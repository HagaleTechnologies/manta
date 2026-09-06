//! One broadcast channel of validated spots, fanned out to the telnet and
//! JSON/WebSocket servers. ARCHITECTURE §7: "Both servers are thin fan-out
//! consumers of one broadcast channel; slow clients are disconnected,
//! never back-pressure the pipeline." A `tokio::sync::broadcast` channel
//! gives us that for free: `publish` never blocks the (synchronous)
//! decode pipeline, and a subscriber that falls too far behind gets
//! `RecvError::Lagged` instead of stalling the sender -- callers should
//! treat `Lagged` as "disconnect this client," not "catch up."

use manta_spot::Spot;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;

/// Bounds how many unconsumed spots a subscriber may lag behind before
/// it starts missing spots (and its next `recv()` returns `Lagged`).
const CHANNEL_CAPACITY: usize = 1024;

/// How many of the most recently published spots `recent()` can return --
/// backs the telnet server's `sh/dx` command (MAN-12 scope, per the
/// 2026-09-02 ticket clarification: real command grammar, not just
/// "don't disconnect").
const RECENT_HISTORY_CAP: usize = 50;

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

pub struct SpotBus {
    tx: broadcast::Sender<BusSpot>,
    epoch: SystemTime,
    session_nonce: u128,
    sample_rate_hz: f64,
    recent: Mutex<VecDeque<BusSpot>>,
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
        Self {
            tx,
            epoch,
            session_nonce,
            sample_rate_hz,
            recent: Mutex::new(VecDeque::with_capacity(RECENT_HISTORY_CAP)),
            occurrence_counts: Mutex::new(OccurrenceTracker::new()),
        }
    }

    /// Publishes one validated spot to every current subscriber. No-op
    /// (not an error) when there are zero subscribers -- a spot server
    /// with no connected clients is the common case, not a failure.
    pub fn publish(&self, spot: Spot) {
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
        {
            let mut recent = self.recent.lock().expect("recent lock poisoned");
            if recent.len() == RECENT_HISTORY_CAP {
                recent.pop_front();
            }
            // `recent` retains the SAME occurrence_count as the live
            // broadcast, not just the bare Spot -- otherwise a caller
            // replaying history (`sh/dx`) has no way to apply the same
            // occurrence-based filter the live stream already honors, and
            // a spot suppressed live could leak through history replay,
            // uncounted (round-11 review finding).
            recent.push_back(bus_spot.clone());
        }
        let _ = self.tx.send(bus_spot);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusSpot> {
        self.tx.subscribe()
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
        let recent = self.recent.lock().expect("recent lock poisoned");
        let skip = recent.len().saturating_sub(n);
        recent.iter().skip(skip).cloned().collect()
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

/// Total spots a subscriber will never receive once disconnected right
/// after a `Lagged(n)` error: `n` (already evicted from the channel) plus
/// whatever's still retained in `rx`'s own buffer (`rx.len()`) that a
/// caller choosing to disconnect (ARCHITECTURE §7: slow clients are
/// disconnected, never back-pressured) will never drain. Recording only
/// `n` under-counts real loss -- ARCHITECTURE §8 requires every dropped
/// item counted, not just the evicted portion (round-9 review finding).
/// Shared by the telnet, JSON/TCP, and WebSocket handlers so all three
/// count loss the same way.
pub fn total_lag_loss(n: u64, rx: &broadcast::Receiver<BusSpot>) -> u64 {
    n + rx.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

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
}
