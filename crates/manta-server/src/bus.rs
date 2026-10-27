//! One broadcast channel of validated spots, fanned out to the telnet and
//! JSON/WebSocket servers. ARCHITECTURE §7: "Both servers are thin fan-out
//! consumers of one broadcast channel; slow clients are disconnected,
//! never back-pressure the pipeline." A `tokio::sync::broadcast` channel
//! gives us that for free: `publish` never blocks the (synchronous)
//! decode pipeline, and a subscriber that falls too far behind gets
//! `RecvError::Lagged` instead of stalling the sender -- callers should
//! treat `Lagged` as "disconnect this client," not "catch up."
//!
//! Two channels carry the same spots (MAN-301): telnet and the RBN uplink
//! read [`SpotBus::subscribe`], spots only; the JSON stream reads
//! [`SpotBus::subscribe_json`], which carries every spot plus, with
//! `[rbn_compare]` enabled, each `spotGrade` record in publish order. With
//! no grades published the JSON channel carries exactly the spots.
//!
//! A fourth channel carries relayed spots (MAN-320) to the RBN uplink only:
//! [`SpotBus::publish_relay`] / [`SpotBus::subscribe_relay`]. Telnet, the
//! JSON stream and the live-state stream never read it, so another local
//! program's spots never appear on manta's own outputs.

use crate::relay::RelaySpot;
use manta_spot::Spot;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;

/// Bounds how many unconsumed spots a subscriber may lag behind before
/// it starts missing spots (and its next `recv()` returns `Lagged`).
const CHANNEL_CAPACITY: usize = 1024;

/// MAN-324: the live-state channel's capacity -- how many records a live
/// subscriber may fall behind before it is disconnected. Larger than the
/// spot channels' because the live stream carries roughly 290 times the
/// spot rate (docs/DECISIONS/2026-10-12-man324-live-state-stream.md).
pub const LIVE_CHANNEL_CAPACITY: usize = 4096;

/// Default `[server].history_minutes` (MAN-93).
pub const DEFAULT_HISTORY_MINUTES: u32 = 30;
/// Largest accepted `[server].history_minutes`: one day.
pub const MAX_HISTORY_MINUTES: u32 = 1440;
/// Keep at least this many newest publications, even on quiet bands.
/// Also the minimum number of JSON resume records, including grades.
pub const MIN_HISTORY_SPOTS: usize = 50;
/// Bound history memory and the size of one telnet replay.
/// Also bounds JSON resume history, counting spots and grades together.
pub const MAX_HISTORY_SPOTS: usize = 10_000;

/// Bounds how many distinct callsigns `occurrence_counts` tracks at once
/// (MAN-62). `docs/DECISIONS/2026-09-02-man23-threat-model.md` finding 2
/// already accepts, as inherent to OpenHPSDR/Hermes having no
/// cryptographic framing, that a source able to spoof the HPSDR input can
/// inject fabricated-but-decodable CW -- an unbounded sequence of distinct
/// SYNTHETIC callsigns from that source would otherwise grow this map for
/// the life of the process, with no bound tied to real-world callsign
/// space at all. Real over-the-air traffic, even an unusually busy
/// multi-band contest weekend, plausibly logs on the order of a few
/// thousand distinct callsigns in one session -- 20,000 is generous
/// headroom over any realistic legitimate count while still bounding
/// memory to a small, fixed footprint against adversarial input.
const MAX_OCCURRENCE_ENTRIES: usize = 20_000;

/// One published spot plus its occurrence count *at the moment it was
/// published* -- captured here, not recomputed by a subscriber later, so
/// a subscriber applying an occurrence-based filter (e.g. the telnet
/// server's `set dx filter unique > n`) sees the count each spot actually
/// had when it was published, not whatever the running total has grown to
/// by the time a lagging subscriber gets around to draining it. Querying
/// `SpotBus` for the "current" count of a callsign at filter-evaluation
/// time is exactly the bug this type exists to make impossible to
/// reintroduce: two back-to-back publications for the same callsign would
/// otherwise both observe the *later* count.
#[derive(Debug, Clone)]
pub struct BusSpot {
    pub spot: Spot,
    pub occurrence_count: u32,
}

/// One item on the JSON stream's channel (MAN-301): a spot, or one already
/// serialised `spotGrade` line, written to clients verbatim.
#[derive(Debug, Clone)]
pub enum JsonItem {
    Spot {
        seq: u64,
        spot: BusSpot,
        /// The line this spot was first rendered as, shared by the live
        /// broadcast and the resume history (MAN-137): a replay resends it
        /// byte for byte, even after a `cty.dat` swap (MAN-316) changed
        /// what a fresh rendering would say.
        rendered: Arc<OnceLock<Arc<str>>>,
    },
    Grade {
        seq: u64,
        line: Arc<str>,
    },
    /// MAN-324: one serialized live-state record, sent verbatim. Only ever
    /// on the live channel, never on the spot stream's.
    Live(Arc<str>),
}

/// Bounded, LRU-on-touch tracker for `SpotBus::occurrence_counts` (MAN-62).
/// Each entry carries the callsign's running count plus a monotonic
/// `last_touched` tick, bumped on every publish for that callsign
/// (whether it's a fresh key or an existing one). Once at
/// `MAX_OCCURRENCE_ENTRIES` capacity, inserting a genuinely NEW callsign
/// evicts whichever tracked callsign has gone longest untouched -- a real,
/// currently-active station keeps getting touched (and so stays
/// protected), while adversarial synthetic callsigns that each appear
/// once and never again are exactly what gets evicted first under
/// sustained flood pressure. The eviction scan is O(capacity), but only
/// runs on the rare "insert while full" path, never on an ordinary
/// touch -- deliberately simple over a doubly-linked-list LRU, since
/// eviction pressure here is inherently rare (real callsign cardinality
/// sits far under capacity; sustained floods are the exception, not the
/// steady state).
struct OccurrenceTracker {
    counts: HashMap<String, (u32, u64)>,
    next_touch: u64,
}

impl OccurrenceTracker {
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
            next_touch: 0,
        }
    }

    /// Records one occurrence of `callsign` and returns its new count.
    fn touch(&mut self, callsign: &str) -> u32 {
        self.next_touch += 1;
        let touch = self.next_touch;
        if let Some(entry) = self.counts.get_mut(callsign) {
            entry.0 += 1;
            entry.1 = touch;
            return entry.0;
        }
        if self.counts.len() >= MAX_OCCURRENCE_ENTRIES {
            if let Some(lru_key) = self
                .counts
                .iter()
                .min_by_key(|(_, (_, last_touched))| *last_touched)
                .map(|(key, _)| key.clone())
            {
                self.counts.remove(&lru_key);
            }
        }
        self.counts.insert(callsign.to_string(), (1, touch));
        1
    }
}

impl JsonItem {
    /// Per-session stream position; live-state records have none.
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Spot { seq, .. } | Self::Grade { seq, .. } => Some(*seq),
            Self::Live(_) => None,
        }
    }
}

trait Aged {
    fn heard_ts(&self) -> u64;
}
impl Aged for BusSpot {
    fn heard_ts(&self) -> u64 {
        self.spot.sample_ts
    }
}
struct StreamRecord {
    heard_ts: u64,
    item: JsonItem,
}
impl Aged for StreamRecord {
    fn heard_ts(&self) -> u64 {
        self.heard_ts
    }
}
struct StreamHistory {
    records: History<StreamRecord>,
    last_seq: u64,
}

/// In-window ceiling evictions in the independent telnet and JSON histories.
pub struct PublishOutcome {
    pub history_truncated: usize,
    pub stream_history_truncated: usize,
}

/// Whether a resume request can recover every missing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeOutcome {
    NoRequest,
    Resumed,
    Gap,
    Reset,
}

/// Classify a cursor against the snapshot and the retained history (MAN-137).
pub fn resume_outcome(
    session_matches: bool,
    since: Option<u64>,
    upto: u64,
    history_from: u64,
) -> ResumeOutcome {
    match since {
        None => ResumeOutcome::NoRequest,
        Some(n) if !session_matches || n > upto => ResumeOutcome::Reset,
        Some(n) if n < history_from.saturating_sub(1) => ResumeOutcome::Gap,
        Some(_) => ResumeOutcome::Resumed,
    }
}

/// One atomic resume decision and its retained records, in send order.
pub struct Resume {
    pub outcome: ResumeOutcome,
    pub history_from: u64,
    pub records: Vec<JsonItem>,
}

struct History<T> {
    entries: VecDeque<T>,
    /// Highest heard timestamp ever published; late entries cannot rewind it.
    newest_sample_ts: u64,
}

impl<T: Aged> History<T> {
    fn push(&mut self, entry: T, window_samples: u64) -> usize {
        self.newest_sample_ts = self.newest_sample_ts.max(entry.heard_ts());
        self.entries.push_back(entry);
        let mut truncated = 0;
        while self.entries.len() > MIN_HISTORY_SPOTS {
            let front = self.entries.front().expect("history is nonempty");
            let in_window =
                self.newest_sample_ts.saturating_sub(front.heard_ts()) <= window_samples;
            let over_ceiling = self.entries.len() > MAX_HISTORY_SPOTS;
            // Zero restores the old count-only history even for equal timestamps.
            if window_samples != 0 && in_window && !over_ceiling {
                break;
            }
            if over_ceiling && in_window && window_samples != 0 {
                truncated += 1;
            }
            self.entries.pop_front();
        }
        truncated
    }
}

pub struct SpotBus {
    tx: broadcast::Sender<BusSpot>,
    json_tx: broadcast::Sender<JsonItem>,
    /// MAN-324: the live-state stream's own channel.
    live_tx: broadcast::Sender<JsonItem>,
    /// MAN-320: relayed spots, read by the RBN uplink only.
    relay_tx: broadcast::Sender<RelaySpot>,
    epoch: SystemTime,
    session_nonce: u128,
    sample_rate_hz: f64,
    recent: Mutex<History<BusSpot>>,
    stream: Mutex<StreamHistory>,
    history_minutes: u32,
    window_samples: u64,
    occurrence_counts: Mutex<OccurrenceTracker>,
}

impl SpotBus {
    /// `epoch` is the wall-clock instant corresponding to `sample_ts == 0`
    /// in the decode pipeline (session start); `sample_rate_hz` converts a
    /// spot's sample-count timestamp to elapsed seconds since `epoch`.
    /// `epoch` must always be a genuine wall-clock instant -- it feeds
    /// `unix_ts_for`, which becomes the JSON stream's `timestamp` and the
    /// telnet stream's RBN Zulu time, both observed by real clients as
    /// "when this was heard." `session_nonce` is a SEPARATE concern: an
    /// opaque value used only for spot-`id` uniqueness (see
    /// `session_nonce()`). Deliberately two parameters, not one derived
    /// from the other -- an earlier version derived both from the same
    /// value (a content hash of the replayed file, for deterministic
    /// reruns), which produced a technically-unique but *fabricated*
    /// wall-clock timestamp with no relation to real time. Don't
    /// reconflate them.
    pub fn new(sample_rate_hz: f64, epoch: SystemTime, session_nonce: u128) -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (json_tx, _json_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (live_tx, _live_rx) = broadcast::channel(LIVE_CHANNEL_CAPACITY);
        let (relay_tx, _relay_rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            json_tx,
            live_tx,
            relay_tx,
            epoch,
            session_nonce,
            sample_rate_hz,
            recent: Mutex::new(History {
                entries: VecDeque::with_capacity(MIN_HISTORY_SPOTS),
                newest_sample_ts: 0,
            }),
            stream: Mutex::new(StreamHistory {
                records: History {
                    entries: VecDeque::with_capacity(MIN_HISTORY_SPOTS),
                    newest_sample_ts: 0,
                },
                last_seq: 0,
            }),
            history_minutes: DEFAULT_HISTORY_MINUTES,
            window_samples: 0,
            occurrence_counts: Mutex::new(OccurrenceTracker::new()),
        }
        .with_history_minutes(DEFAULT_HISTORY_MINUTES)
    }

    /// Retain the heard-time window plus the floor, up to the ceiling.
    /// Config bounds minutes to 0..=MAX_HISTORY_MINUTES; larger values saturate.
    pub fn with_history_minutes(mut self, minutes: u32) -> Self {
        self.history_minutes = minutes;
        self.window_samples =
            (f64::from(self.history_minutes()) * 60.0 * self.sample_rate_hz).ceil() as u64;
        self
    }

    pub fn history_minutes(&self) -> u32 {
        self.history_minutes
    }

    /// Publishes one validated spot to every current subscriber. No-op
    /// (not an error) when there are zero subscribers -- a spot server
    /// with no connected clients is the common case, not a failure.
    /// Returns ceiling evictions still inside the window, for the caller to count.
    /// Ordinary window ageing and zero-minute count-only eviction are not loss.
    pub fn publish(&self, spot: Spot) -> PublishOutcome {
        // Increment and read back under one lock acquisition -- this *is*
        // the fix: the occurrence count a spot carries is fixed at the
        // instant of this publish, before any subscriber can observe it.
        let occurrence_count = {
            let mut counts = self
                .occurrence_counts
                .lock()
                .expect("occurrence_counts lock poisoned");
            counts.touch(&spot.callsign)
        };
        let bus_spot = BusSpot {
            spot,
            occurrence_count,
        };
        let truncated = {
            let mut recent = self.recent.lock().expect("recent lock poisoned");
            // `recent` retains the SAME occurrence_count as the live
            // broadcast, not just the bare Spot -- otherwise a caller
            // replaying history (`sh/dx`) has no way to apply the same
            // occurrence-based filter the live stream already honors, and
            // a spot suppressed live could leak through history replay,
            // uncounted (round-11 review finding).
            recent.push(bus_spot.clone(), self.window_samples)
        };
        // Seq assignment, history push and nonblocking send share one lock.
        // Subscribe snapshots under this same lock: replay ends exactly where
        // the receiver starts. No other lock is acquired while this is held.
        let stream_history_truncated = {
            let mut stream = self.stream.lock().expect("stream lock poisoned");
            stream.last_seq += 1;
            let item = JsonItem::Spot {
                seq: stream.last_seq,
                spot: bus_spot.clone(),
                rendered: Arc::default(),
            };
            let n = stream.records.push(
                StreamRecord {
                    heard_ts: bus_spot.spot.sample_ts,
                    item: item.clone(),
                },
                self.window_samples,
            );
            let _ = self.json_tx.send(item);
            n
        };
        let _ = self.tx.send(bus_spot);
        PublishOutcome {
            history_truncated: truncated,
            stream_history_truncated,
        }
    }

    /// Publishes one serialised `spotGrade` line (MAN-301) to JSON-stream
    /// subscribers only. Telnet and the uplink never see it.
    /// The renderer must not call back into this bus: it runs under the
    /// stream lock, with seq assignment, history push and nonblocking send.
    pub fn publish_grade(&self, render: impl FnOnce(u64) -> Arc<str>) -> usize {
        let mut stream = self.stream.lock().expect("stream lock poisoned");
        stream.last_seq += 1;
        let seq = stream.last_seq;
        let item = JsonItem::Grade {
            seq,
            line: render(seq),
        };
        let heard_ts = stream.records.newest_sample_ts;
        let n = stream.records.push(
            StreamRecord {
                heard_ts,
                item: item.clone(),
            },
            self.window_samples,
        );
        let _ = self.json_tx.send(item);
        n
    }

    /// Subscribe and snapshot under the publication lock, so every live
    /// record has seq greater than the snapshot. Broadcast send never blocks.
    pub fn subscribe_json_with_seq(&self) -> (broadcast::Receiver<JsonItem>, u64) {
        let stream = self.stream.lock().expect("stream lock poisoned");
        (self.json_tx.subscribe(), stream.last_seq)
    }

    /// Decimal session identity shared with the spot IDs.
    pub fn session_id(&self) -> String {
        self.session_nonce.to_string()
    }

    /// Decide and copy a resume suffix under one lock; eviction cannot race
    /// the decision. Never replay beyond the connection's subscription snapshot.
    pub fn json_resume(&self, session: Option<&str>, since: Option<u64>, upto: u64) -> Resume {
        let stream = self.stream.lock().expect("stream lock poisoned");
        let history_from = stream
            .records
            .entries
            .front()
            .and_then(|r| r.item.seq())
            .unwrap_or(stream.last_seq.saturating_add(1))
            .min(upto.saturating_add(1));
        let outcome = resume_outcome(
            session == Some(self.session_id().as_str()),
            since,
            upto,
            history_from,
        );
        let records = stream
            .records
            .entries
            .iter()
            .filter(|r| {
                let seq = r.item.seq().expect("only spot stream records in history");
                outcome != ResumeOutcome::NoRequest
                    && seq <= upto
                    && (outcome != ResumeOutcome::Resumed || seq > since.unwrap_or(0))
            })
            .map(|r| r.item.clone())
            .collect();
        Resume {
            outcome,
            history_from,
            records,
        }
    }

    /// Spots only: the telnet server, the RBN uplink and the comparator.
    /// MAN-324: publishes one serialized live-state record to the live
    /// channel. Never blocks; with no subscriber the line is dropped and
    /// this returns false.
    pub fn publish_live(&self, line: Arc<str>) -> bool {
        self.live_tx.send(JsonItem::Live(line)).is_ok()
    }

    /// MAN-324: a receiver on the live-state channel.
    pub fn subscribe_live(&self) -> broadcast::Receiver<JsonItem> {
        self.live_tx.subscribe()
    }

    /// MAN-324: live-state subscribers right now; the decode loop builds
    /// records only when this is nonzero.
    pub fn live_receiver_count(&self) -> usize {
        self.live_tx.receiver_count()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusSpot> {
        self.tx.subscribe()
    }

    /// MAN-320: publishes one relayed spot to the RBN uplink's relay
    /// receivers. No history, no occurrence count, no sequence number: a
    /// relayed spot is another decoder's output and touches none of manta's
    /// own spot state. Returns false (not an error) with no subscriber.
    pub fn publish_relay(&self, spot: RelaySpot) -> bool {
        self.relay_tx.send(spot).is_ok()
    }

    /// MAN-320: a receiver on the relay channel. Only the RBN uplink
    /// subscribes.
    pub fn subscribe_relay(&self) -> broadcast::Receiver<RelaySpot> {
        self.relay_tx.subscribe()
    }

    /// Spots and grades in publish order: the JSON/WebSocket stream.
    pub fn subscribe_json(&self) -> broadcast::Receiver<JsonItem> {
        self.json_tx.subscribe()
    }

    /// Opaque session identity -- combined (alongside `track_id`/
    /// `sample_ts`) into JSON spot `id`s to keep them unique across
    /// stations and restarts, not just within one session (see
    /// `spot_message::SpotMessage::from_spot`). Deliberately independent
    /// of `epoch`/`unix_ts_for` -- this value need only be unlikely to
    /// repeat, never truthful as a point in time, so callers are free to
    /// derive it from something with no wall-clock meaning at all (e.g. a
    /// content hash of a replayed file, for deterministic reruns).
    pub fn session_nonce(&self) -> u128 {
        self.session_nonce
    }

    /// The last `n` published spots, oldest first (`sh/dx` backing store),
    /// each carrying the occurrence_count it had when published -- see
    /// `publish`'s doc comment for why history must retain this, not just
    /// the bare `Spot`.
    pub fn recent(&self, n: usize) -> Vec<BusSpot> {
        self.select_recent(|_| true, n)
    }

    /// Newest `limit` matching retained entries, in publication order (oldest first).
    /// Filters under the history lock and clones only selected entries. `keep`
    /// must not call a method that locks history; `unix_ts_for` is safe.
    pub fn select_recent(
        &self,
        mut keep: impl FnMut(&BusSpot) -> bool,
        limit: usize,
    ) -> Vec<BusSpot> {
        let recent = self.recent.lock().expect("recent lock poisoned");
        let mut selected: Vec<_> = recent
            .entries
            .iter()
            .rev()
            .filter(|entry| keep(entry))
            .take(limit)
            .cloned()
            .collect();
        selected.reverse();
        selected
    }

    /// Converts a spot's `sample_ts` (samples since session start) to a
    /// Unix wall-clock timestamp, using this bus's `epoch`/`sample_rate_hz`.
    pub fn unix_ts_for(&self, sample_ts: u64) -> i64 {
        let elapsed = Duration::from_secs_f64(sample_ts as f64 / self.sample_rate_hz);
        (self.epoch + elapsed)
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("epoch predates the Unix epoch")
            .as_secs() as i64
    }
}

/// Total items (spots, or on the JSON channel spots and grades) a subscriber will never receive once disconnected right
/// after a `Lagged(n)` error: `n` (already evicted from the channel) plus
/// whatever's still retained in `rx`'s own buffer (`rx.len()`) that a
/// caller choosing to disconnect (ARCHITECTURE §7: slow clients are
/// disconnected, never back-pressured) will never drain. Recording only
/// `n` under-counts real loss -- ARCHITECTURE §8 requires every dropped
/// item counted, not just the evicted portion (round-9 review finding).
/// Shared by the telnet, JSON/TCP, and WebSocket handlers so all three
/// count loss the same way.
pub fn total_lag_loss<T: Clone>(n: u64, rx: &broadcast::Receiver<T>) -> u64 {
    n + rx.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

    fn live_bus() -> SpotBus {
        SpotBus::new(96_000.0, SystemTime::UNIX_EPOCH, 1)
    }

    #[test]
    fn json_records_carry_consecutive_seq_across_spots_and_grades() {
        let bus = live_bus();
        let mut rx = bus.subscribe_json();
        bus.publish(sample_spot(0));
        bus.publish_grade(|_| Arc::from("grade"));
        bus.publish(sample_spot(1));
        for seq in 1..=3 {
            assert_eq!(rx.try_recv().unwrap().seq(), Some(seq));
        }
    }

    #[test]
    fn subscribe_json_with_seq_reports_the_newest_seq_before_subscribing() {
        let bus = live_bus();
        bus.publish(sample_spot(0));
        bus.publish(sample_spot(1));
        let (mut rx, s0) = bus.subscribe_json_with_seq();
        assert_eq!(s0, 2);
        bus.publish(sample_spot(2));
        assert_eq!(rx.try_recv().unwrap().seq(), Some(3));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn json_resume_replays_retained_records_after_since_in_seq_order() {
        let bus = live_bus();
        bus.publish(sample_spot(0));
        bus.publish_grade(|_| Arc::from("grade"));
        bus.publish(sample_spot(1));
        bus.publish(sample_spot(2));
        let r = bus.json_resume(Some(&bus.session_id()), Some(1), 4);
        assert_eq!(r.outcome, ResumeOutcome::Resumed);
        assert_eq!(
            r.records
                .iter()
                .filter_map(JsonItem::seq)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(matches!(&r.records[0], JsonItem::Grade { line, .. } if &**line == "grade"));
    }

    #[test]
    fn resume_outcome_table() {
        use ResumeOutcome::*;
        for (matches, since, upto, first, expected) in [
            (false, None, 0, 1, NoRequest),
            (true, Some(0), 0, 1, Resumed),
            (true, Some(4), 8, 5, Resumed),
            (true, Some(8), 8, 5, Resumed),
            (true, Some(3), 8, 5, Gap),
            (false, Some(5), 8, 5, Reset),
            (true, Some(9), 8, 5, Reset),
        ] {
            assert_eq!(resume_outcome(matches, since, upto, first), expected);
        }
    }

    #[test]
    fn json_history_holds_spots_and_grades_under_one_ten_thousand_record_ceiling() {
        let bus = live_bus();
        let mut truncated = 0;
        for _ in 0..6000 {
            truncated += bus.publish(sample_spot(0)).stream_history_truncated;
            truncated += bus.publish_grade(|_| Arc::from("grade"));
        }
        let r = bus.json_resume(None, Some(0), 12000);
        assert_eq!(r.history_from, 2001);
        assert_eq!(r.records.len(), 10000);
        assert_eq!(truncated, 2000);
    }

    #[test]
    fn json_history_keeps_the_fifty_newest_records_outside_the_window() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(1);
        for ts in 0..60 {
            bus.publish(sample_spot(ts * 3600));
            bus.publish_grade(|_| Arc::from("grade"));
        }
        let r = bus.json_resume(Some("0"), Some(0), 120);
        assert_eq!(r.records.len(), 50);
        assert_eq!(r.history_from, 71);
    }

    #[test]
    fn a_grade_ages_with_the_newest_heard_time_at_its_publish() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(1);
        bus.publish(sample_spot(0));
        bus.publish_grade(|_| Arc::from("grade"));
        assert_eq!(
            bus.stream
                .lock()
                .unwrap()
                .records
                .entries
                .back()
                .unwrap()
                .heard_ts,
            0
        );
        for ts in 3600..3660 {
            bus.publish(sample_spot(ts));
        }
        let r = bus.json_resume(None, Some(0), 62);
        assert_eq!(r.history_from, 3);
        assert!(r
            .records
            .iter()
            .all(|item| matches!(item, JsonItem::Spot { .. })));
    }

    #[test]
    fn sh_dx_history_never_holds_grades_and_keeps_fifty_spots() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(1);
        for ts in 0..100 {
            bus.publish(sample_spot(ts * 3600));
            bus.publish_grade(|_| Arc::from("grade"));
        }
        assert_eq!(
            retained_times(&bus),
            (50..100).map(|ts| ts * 3600).collect::<Vec<_>>()
        );
    }

    #[test]
    fn history_from_is_clamped_to_one_past_the_snapshot() {
        let bus = live_bus().with_history_minutes(0);
        for ts in 0..60 {
            bus.publish(sample_spot(ts));
        }
        let r = bus.json_resume(None, None, 0);
        assert_eq!(r.history_from, 1);
        assert!(r.records.is_empty());
    }

    #[test]
    fn live_subscribers_receive_live_lines_only() {
        let bus = live_bus();
        let mut live = bus.subscribe_live();
        bus.publish(sample_spot(1));
        bus.publish_grade(|_| Arc::from("{\"kind\":\"spotGrade\"}"));
        bus.publish_live(Arc::from("{\"kind\":\"bandState\"}"));
        match live.try_recv() {
            Ok(JsonItem::Live(line)) => assert_eq!(&*line, "{\"kind\":\"bandState\"}"),
            other => panic!("{other:?}"),
        }
        assert!(live.try_recv().is_err());
    }

    #[test]
    fn spot_and_json_subscribers_never_receive_live_lines() {
        let bus = live_bus();
        let mut spots = bus.subscribe();
        let mut json = bus.subscribe_json();
        bus.publish_live(Arc::from("{}"));
        assert!(spots.try_recv().is_err());
        assert!(json.try_recv().is_err());
    }

    #[test]
    fn live_receiver_count_follows_subscribers() {
        let bus = live_bus();
        assert_eq!(bus.live_receiver_count(), 0);
        let a = bus.subscribe_live();
        let b = bus.subscribe_live();
        assert_eq!(bus.live_receiver_count(), 2);
        drop(a);
        assert_eq!(bus.live_receiver_count(), 1);
        drop(b);
        assert_eq!(bus.live_receiver_count(), 0);
        let _json = bus.subscribe_json();
        assert_eq!(bus.live_receiver_count(), 0);
    }

    #[test]
    fn a_lagging_live_subscriber_is_told_to_disconnect_after_4096() {
        let bus = live_bus();
        let mut rx = bus.subscribe_live();
        let published = LIVE_CHANNEL_CAPACITY + 10;
        for i in 0..published {
            bus.publish_live(Arc::from(format!("{i}")));
        }
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                assert_eq!(n, 10);
                assert_eq!(total_lag_loss(n, &rx), published as u64);
            }
            other => panic!("{other:?}"),
        }
    }

    fn sample_spot(sample_ts: u64) -> Spot {
        Spot {
            callsign: "K5ARH".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 20.0,
            wpm: 25.0,
            spot_type: SpotType::De,
            confidence: 0.9,
            track_id: 1,
            sample_ts,
            rst: None,
            qrl_query: false,
        }
    }

    #[tokio::test]
    async fn a_subscriber_receives_a_published_spot() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        bus.publish(sample_spot(0));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.spot.callsign, "K5ARH");
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_does_not_panic_or_block() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        bus.publish(sample_spot(0));
    }

    #[tokio::test]
    async fn each_subscriber_gets_its_own_copy() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(sample_spot(0));

        assert_eq!(rx1.recv().await.unwrap().spot.callsign, "K5ARH");
        assert_eq!(rx2.recv().await.unwrap().spot.callsign, "K5ARH");
    }

    #[tokio::test]
    async fn a_lagging_subscriber_is_told_to_disconnect_instead_of_stalling_the_sender() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        // Publish well past the channel capacity without the subscriber
        // ever draining -- this must not block `publish`.
        for i in 0..(CHANNEL_CAPACITY as u64 + 10) {
            bus.publish(sample_spot(i));
        }

        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn total_lag_loss_includes_spots_still_retained_after_a_lagged_disconnect() {
        // Regression (round-9 review): `Lagged(n)` only reports the count
        // already EVICTED past the broadcast channel's capacity -- a
        // subscriber this far behind can still have up to
        // (CHANNEL_CAPACITY - n) more spots retained in the buffer that
        // it will never see once the caller disconnects it (ARCHITECTURE
        // §7: slow clients are disconnected, never back-pressured).
        // Recording only `n` under-counts total loss; ARCHITECTURE §8
        // requires every dropped item counted, not just the evicted
        // portion.
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        // Publish exactly 10 past capacity: 10 evicted, CHANNEL_CAPACITY
        // (1024) still retained -- total loss for a client about to be
        // disconnected without ever having read anything is exactly the
        // 1034 spots published.
        let total_published = CHANNEL_CAPACITY as u64 + 10;
        for i in 0..total_published {
            bus.publish(sample_spot(i));
        }

        let n = match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => n,
            other => panic!("expected Lagged, got {other:?}"),
        };
        assert_eq!(total_lag_loss(n, &rx), total_published);
    }

    #[tokio::test]
    async fn json_subscribers_receive_spots_and_grades_in_publish_order() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe_json();

        bus.publish(sample_spot(0));
        bus.publish_grade(|_| Arc::from("{\"kind\":\"spotGrade\",\"n\":1}"));
        bus.publish(sample_spot(1));
        bus.publish_grade(|_| Arc::from("{\"kind\":\"spotGrade\",\"n\":2}"));

        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(match rx.recv().await.unwrap() {
                JsonItem::Spot { spot: s, .. } => format!("spot:{}", s.spot.sample_ts),
                JsonItem::Grade { line, .. } => format!("grade:{line}"),
                JsonItem::Live(line) => format!("live:{line}"),
            });
        }
        assert_eq!(
            seen,
            vec![
                "spot:0".to_string(),
                "grade:{\"kind\":\"spotGrade\",\"n\":1}".to_string(),
                "spot:1".to_string(),
                "grade:{\"kind\":\"spotGrade\",\"n\":2}".to_string(),
            ]
        );
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn spot_subscribers_never_receive_grades() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        bus.publish_grade(|_| Arc::from("{\"kind\":\"spotGrade\"}"));
        bus.publish(sample_spot(7));
        bus.publish_grade(|_| Arc::from("{\"kind\":\"spotGrade\"}"));

        assert_eq!(rx.recv().await.unwrap().spot.sample_ts, 7);
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn with_no_grades_published_the_json_channel_carries_exactly_the_spots() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut spots = bus.subscribe();
        let mut json = bus.subscribe_json();

        for i in 0..5u64 {
            bus.publish(sample_spot(i));
        }

        for _ in 0..5 {
            let a = spots.recv().await.unwrap();
            let b = match json.recv().await.unwrap() {
                JsonItem::Spot { spot: s, .. } => s,
                JsonItem::Grade { line, .. } | JsonItem::Live(line) => {
                    panic!("unexpected line {line}")
                }
            };
            assert_eq!(a.spot, b.spot);
            assert_eq!(a.occurrence_count, b.occurrence_count);
        }
        assert!(matches!(
            json.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn recent_returns_the_last_n_published_spots_oldest_first() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        for i in 0..5u64 {
            let mut spot = sample_spot(i);
            spot.track_id = i as u32;
            bus.publish(spot);
        }

        let recent = bus.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(
            recent.iter().map(|s| s.spot.track_id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[tokio::test]
    async fn recent_caps_at_the_available_history() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        bus.publish(sample_spot(0));
        bus.publish(sample_spot(1));

        assert_eq!(bus.recent(10).len(), 2);
    }

    #[tokio::test]
    async fn recent_spots_carry_their_publish_time_occurrence_count() {
        // Regression (round-11 review): `sh/dx` replays history from
        // `recent()` with no occurrence-count information, so the telnet
        // server couldn't apply `set dx filter unique > n` to history the
        // same way it already applies it to the live stream -- a spot
        // suppressed live could leak through `sh/dx` immediately after,
        // uncounted. `recent()` must carry the SAME occurrence_count each
        // spot had when published (see `BusSpot`'s own doc comment on why
        // that has to be captured at publish time, not recomputed later).
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut first = sample_spot(0);
        first.callsign = "K5ARH".to_string();
        let mut second = sample_spot(1);
        second.callsign = "K5ARH".to_string();
        bus.publish(first);
        bus.publish(second);

        let recent = bus.recent(2);
        assert_eq!(recent[0].occurrence_count, 1);
        assert_eq!(recent[1].occurrence_count, 2);
    }

    #[tokio::test]
    async fn published_spot_carries_its_occurrence_count_at_publish_time() {
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        bus.publish(sample_spot(0));
        assert_eq!(rx.recv().await.unwrap().occurrence_count, 1);

        bus.publish(sample_spot(1));
        assert_eq!(rx.recv().await.unwrap().occurrence_count, 2);
    }

    #[tokio::test]
    async fn occurrence_count_is_fixed_at_publish_even_if_the_subscriber_hasnt_drained_yet() {
        // The bug this type exists to make impossible: two back-to-back
        // publications for the same callsign, drained together by a
        // subscriber that was momentarily behind. Each must still carry
        // the count it actually had *when published*, not the running
        // total by the time the subscriber gets to it.
        let bus = SpotBus::new(96_000.0, SystemTime::now(), 0);
        let mut rx = bus.subscribe();

        bus.publish(sample_spot(0));
        bus.publish(sample_spot(1));

        assert_eq!(rx.recv().await.unwrap().occurrence_count, 1);
        assert_eq!(rx.recv().await.unwrap().occurrence_count, 2);
    }

    #[test]
    fn occurrence_tracker_stays_bounded_under_an_unbounded_sequence_of_distinct_callsigns() {
        // MAN-62's core scenario: a source able to inject fabricated CW
        // for an unbounded sequence of distinct synthetic callsigns must
        // not grow this map without bound.
        let mut tracker = OccurrenceTracker::new();
        for i in 0..(MAX_OCCURRENCE_ENTRIES + 5_000) {
            tracker.touch(&format!("SYNTH{i}"));
        }
        assert_eq!(tracker.counts.len(), MAX_OCCURRENCE_ENTRIES);
    }

    #[test]
    fn occurrence_tracker_evicts_the_least_recently_touched_entry_first() {
        let mut tracker = OccurrenceTracker::new();
        for i in 0..MAX_OCCURRENCE_ENTRIES {
            tracker.touch(&format!("FILL{i}"));
        }
        assert_eq!(tracker.counts.get("FILL0").unwrap().0, 1);

        // One more distinct callsign past capacity must evict the
        // longest-untouched entry (FILL0, touched first and never since),
        // not some other arbitrary one.
        tracker.touch("NEWCOMER");
        assert_eq!(tracker.counts.len(), MAX_OCCURRENCE_ENTRIES);
        assert!(
            !tracker.counts.contains_key("FILL0"),
            "the least-recently-touched entry must be evicted first"
        );
        assert!(tracker.counts.contains_key("NEWCOMER"));
    }

    #[test]
    fn occurrence_tracker_protects_a_repeatedly_touched_callsign_during_a_flood() {
        // A genuinely-active real callsign that keeps getting touched must
        // survive sustained eviction pressure from a flood of one-shot
        // synthetic callsigns -- this IS the trade-off the ticket's own
        // technical notes called out as needing a real design decision:
        // LRU-on-touch (not FIFO-by-insertion) is what makes it hold.
        let mut tracker = OccurrenceTracker::new();
        tracker.touch("REAL_STATION"); // inserted first -- FIFO would evict it first

        for i in 0..MAX_OCCURRENCE_ENTRIES {
            tracker.touch(&format!("FLOOD{i}"));
            if i % 100 == 0 {
                tracker.touch("REAL_STATION"); // stays fresh throughout
            }
        }

        assert!(
            tracker.counts.contains_key("REAL_STATION"),
            "a repeatedly-touched real callsign must survive sustained flood eviction pressure"
        );
        assert!(tracker.counts.get("REAL_STATION").unwrap().0 > 1);
    }

    #[test]
    fn occurrence_tracker_counts_correctly_alongside_bounded_eviction() {
        let mut tracker = OccurrenceTracker::new();
        assert_eq!(tracker.touch("K5ARH"), 1);
        assert_eq!(tracker.touch("K5ARH"), 2);
        assert_eq!(tracker.touch("JA1ABC"), 1);
        assert_eq!(tracker.touch("K5ARH"), 3);
    }

    #[test]
    fn unix_ts_for_converts_sample_count_using_sample_rate_and_epoch() {
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = SpotBus::new(1000.0, epoch, 0); // 1000 Hz -> 1 sample = 1 ms
                                                  // 5000 samples at 1000 Hz = 5 seconds past epoch.
        assert_eq!(bus.unix_ts_for(5000), 1_700_000_005);
    }

    #[test]
    fn session_nonce_returns_exactly_what_the_caller_supplied() {
        // session_nonce is opaque and independent of epoch/unix_ts_for --
        // a caller may derive it from anything (a content hash, a random
        // value, nanoseconds-since-epoch for a live session), and SpotBus
        // must never reinterpret or recompute it.
        let bus = SpotBus::new(1000.0, SystemTime::now(), 0xDEAD_BEEF_u128);
        assert_eq!(bus.session_nonce(), 0xDEAD_BEEF_u128);
    }

    #[test]
    fn unix_ts_for_is_unaffected_by_session_nonce() {
        // Regression: an earlier version derived session_nonce FROM epoch,
        // so the two could never vary independently. A caller replaying a
        // file must be able to hold epoch fixed (real wall-clock time)
        // while session_nonce varies (content-derived), or vice versa.
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let a = SpotBus::new(1000.0, epoch, 111);
        let b = SpotBus::new(1000.0, epoch, 222);
        assert_ne!(a.session_nonce(), b.session_nonce());
        assert_eq!(a.unix_ts_for(0), b.unix_ts_for(0));
    }

    fn retained_times(bus: &SpotBus) -> Vec<u64> {
        bus.recent(usize::MAX)
            .iter()
            .map(|e| e.spot.sample_ts)
            .collect()
    }

    #[test]
    fn history_keeps_every_spot_heard_within_the_window() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0);
        for ts in 0..3600 {
            bus.publish(sample_spot(ts));
        }
        assert_eq!(retained_times(&bus), (1799..3600).collect::<Vec<_>>());
    }

    #[test]
    fn history_never_drops_below_the_fifty_newest_spots() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(1);
        for ts in 0..60 {
            bus.publish(sample_spot(ts * 3600));
        }
        assert_eq!(
            retained_times(&bus),
            (10..60).map(|ts| ts * 3600).collect::<Vec<_>>()
        );
    }

    #[test]
    fn zero_minutes_keeps_exactly_the_fifty_newest_like_before_man93() {
        for spacing in [0, 1] {
            let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(0);
            for i in 0..60 {
                let mut spot = sample_spot(i * spacing);
                spot.track_id = i as u32;
                assert_eq!(bus.publish(spot).history_truncated, 0);
            }
            assert_eq!(
                bus.recent(usize::MAX)
                    .iter()
                    .map(|e| e.spot.track_id)
                    .collect::<Vec<_>>(),
                (10..60).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn history_is_capped_at_the_ceiling_and_publish_reports_in_window_evictions() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0);
        let mut truncated = 0;
        for i in 0..MAX_HISTORY_SPOTS + 5 {
            let mut spot = sample_spot(1);
            spot.track_id = i as u32;
            truncated += bus.publish(spot).history_truncated;
        }
        let entries = bus.recent(usize::MAX);
        assert_eq!(entries.len(), MAX_HISTORY_SPOTS);
        assert_eq!(entries[0].spot.track_id, 5);
        assert_eq!(truncated, 5);
    }

    #[test]
    fn ageing_out_of_the_window_is_not_reported_as_truncation() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0);
        for i in 0..100 {
            assert_eq!(bus.publish(sample_spot(i * 600)).history_truncated, 0);
        }
        assert_eq!(bus.recent(usize::MAX).len(), MIN_HISTORY_SPOTS);
    }

    #[test]
    fn an_out_of_order_publish_never_evicts_an_entry_inside_the_window() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0).with_history_minutes(1);
        for ts in 100..200 {
            bus.publish(sample_spot(ts));
        }
        bus.publish(sample_spot(0));
        assert_eq!(
            retained_times(&bus),
            (139..200).chain([0]).collect::<Vec<_>>()
        );
        bus.publish(sample_spot(200));
        assert_eq!(
            retained_times(&bus),
            (140..200).chain([0, 200]).collect::<Vec<_>>()
        );
        // The high-water mark cannot move backwards on a late publish.
        assert_eq!(bus.recent.lock().unwrap().newest_sample_ts, 200);
    }

    #[test]
    fn select_recent_returns_the_newest_matching_entries_oldest_first() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0);
        for ts in 0..10 {
            bus.publish(sample_spot(ts));
        }
        let even = |entry: &BusSpot| entry.spot.sample_ts.is_multiple_of(2);
        let selected = bus.select_recent(even, 3);
        assert_eq!(
            selected
                .iter()
                .map(|e| e.spot.sample_ts)
                .collect::<Vec<_>>(),
            [4, 6, 8]
        );
        assert!(bus.select_recent(even, 0).is_empty());
        assert_eq!(
            bus.select_recent(even, 100)
                .iter()
                .map(|e| e.spot.sample_ts)
                .collect::<Vec<_>>(),
            [0, 2, 4, 6, 8]
        );
    }

    #[test]
    fn new_defaults_to_thirty_minutes_and_huge_values_do_not_panic() {
        let bus = SpotBus::new(1.0, SystemTime::UNIX_EPOCH, 0);
        assert_eq!(bus.history_minutes(), DEFAULT_HISTORY_MINUTES);
        assert_eq!(DEFAULT_HISTORY_MINUTES, 30);
        let bus = bus.with_history_minutes(u32::MAX);
        bus.publish(sample_spot(u64::MAX));
        assert_eq!(bus.recent(1).len(), 1);
    }
}
