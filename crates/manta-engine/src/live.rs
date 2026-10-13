//! MAN-324: the live-state tap. `listen_with_observers` drives a `LiveTap`
//! when `ListenObservers::live_state` is set; it keeps each track's running
//! copy, speed, SNR and frequency, and hands band samples, track snapshots,
//! validation decisions and track closures to a `LiveStateSink` (the
//! daemon's live-state stream). Write-only from the decode path: spots are
//! unchanged. See docs/DECISIONS/2026-10-12-man324-live-state-stream.md.

use crate::track::{ChannelLevel, TrackManager};
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::{CandidateSupport, Decision, Validator};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

/// The newest decoded characters a `TrackSnapshot::text` carries.
pub const LIVE_TEXT_CHARS: usize = 128;
/// The most candidates a `TrackSnapshot` carries, strongest first.
pub const LIVE_MAX_CANDIDATES: usize = 8;

/// Where the decode loop hands live-state records. Implementations must
/// never block: `publish` runs on the decode thread.
pub trait LiveStateSink: Send + Sync {
    /// Cheap; false skips building and publishing records for this batch
    /// (state is still kept, so the next record is complete).
    fn wants_records(&self) -> bool;
    /// Must never block the decode loop.
    fn publish(&self, record: LiveRecord);
}

/// One live-state record, engine side. Every frequency is calibrated.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveRecord {
    /// Once per second of stream time.
    Bands(BandSample),
    /// Once per batch in which a track's decoded text changed.
    Track(TrackSnapshot),
    /// Every validation decision.
    Decision(Decision),
    /// A track closed.
    TrackClosed {
        track_id: u32,
        sample_ts: u64,
        closure: ClosureKind,
    },
}

/// Every channel's level plus the frequency of every track holding a
/// decoder, at one stream instant. Calibrated Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct BandSample {
    pub sample_ts: u64,
    pub channels: Vec<ChannelLevel>,
    pub decoding_track_freqs_hz: Vec<f64>,
}

/// One track's running state.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackSnapshot {
    pub track_id: u32,
    /// Of the track's latest text event.
    pub sample_ts: u64,
    /// Calibrated; from `TrackPromoted`, then every `TrackMeta`.
    pub freq_hz: Option<f64>,
    /// The last `SpeedUpdate`.
    pub wpm: Option<f32>,
    /// The last `TrackMeta`'s 2500 Hz-reference SNR.
    pub snr_2500_db: Option<f32>,
    /// The newest `LIVE_TEXT_CHARS` characters, one space per word
    /// boundary, prosigns dropped.
    pub text: String,
    /// At most `LIVE_MAX_CANDIDATES`, strongest first.
    pub candidates: Vec<CandidateSupport>,
}

#[derive(Default)]
struct LiveTrack {
    freq_hz: Option<f64>,
    wpm: Option<f32>,
    snr_2500_db: Option<f32>,
    text: VecDeque<char>,
    /// A word boundary seen since the last character: the next character
    /// is preceded by one space.
    pending_space: bool,
    last_text_ts: u64,
}

impl LiveTrack {
    fn push_char(&mut self, c: char) {
        if self.pending_space && !self.text.is_empty() {
            self.text.push_back(' ');
        }
        self.pending_space = false;
        self.text.push_back(c.to_ascii_uppercase());
        while self.text.len() > LIVE_TEXT_CHARS {
            self.text.pop_front();
        }
        // Never start the window on a space.
        while self.text.front() == Some(&' ') {
            self.text.pop_front();
        }
    }
}

/// The decode loop's side of the live-state stream.
pub(crate) struct LiveTap {
    sink: Arc<dyn LiveStateSink>,
    tracks: BTreeMap<u32, LiveTrack>,
    dirty: BTreeSet<u32>,
    /// Stream sample timestamp of the next band sample.
    next_tick: u64,
    tick_samples: u64,
    factor: f64,
    /// The newest event sample timestamp seen.
    now_ts: u64,
}

impl LiveTap {
    /// `fs` sets the band tick (one per `fs` samples of stream time);
    /// `factor` is the frequency calibration applied to band and track
    /// frequencies from the track manager.
    pub(crate) fn new(sink: Arc<dyn LiveStateSink>, fs: f64, factor: f64) -> Self {
        let tick_samples = (fs.round() as u64).max(1);
        Self {
            sink,
            tracks: BTreeMap::new(),
            dirty: BTreeSet::new(),
            next_tick: tick_samples,
            tick_samples,
            factor,
            now_ts: 0,
        }
    }

    /// Updates per-track state from one calibrated event. On `TrackClosed`
    /// it flushes that track's pending snapshot while `validator` still
    /// holds its candidates; call it before `validator.ingest`.
    pub(crate) fn observe(&mut self, ev: &DecoderEvent, validator: &Validator) {
        match ev {
            DecoderEvent::TrackPromoted {
                track_id,
                sample_ts,
                freq_hz,
            } => {
                self.now_ts = self.now_ts.max(*sample_ts);
                self.tracks.entry(*track_id).or_default().freq_hz = Some(*freq_hz);
            }
            DecoderEvent::TrackMeta {
                track_id,
                sample_ts,
                snr_2500_db,
                freq_hz,
            } => {
                self.now_ts = self.now_ts.max(*sample_ts);
                let t = self.tracks.entry(*track_id).or_default();
                t.freq_hz = Some(*freq_hz);
                t.snr_2500_db = Some(*snr_2500_db);
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                self.tracks.entry(*track_id).or_default().wpm = Some(*wpm);
            }
            DecoderEvent::CharDecoded {
                track_id,
                sample_ts,
                glyph,
                ..
            } => {
                self.now_ts = self.now_ts.max(*sample_ts);
                if let Glyph::Char(c) = glyph {
                    let t = self.tracks.entry(*track_id).or_default();
                    t.push_char(*c);
                    t.last_text_ts = *sample_ts;
                    self.dirty.insert(*track_id);
                }
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                ..
            } => {
                self.now_ts = self.now_ts.max(*sample_ts);
                let t = self.tracks.entry(*track_id).or_default();
                t.pending_space = true;
                t.last_text_ts = *sample_ts;
                // The validator's candidate support changes at a word
                // boundary even though the visible text does not yet.
                self.dirty.insert(*track_id);
            }
            DecoderEvent::TrackClosed { track_id, .. } => {
                if self.dirty.remove(track_id) {
                    self.publish_track(*track_id, validator);
                }
            }
        }
    }

    /// Publishes the validator's decisions for the event just ingested,
    /// or drops them while no one is subscribed.
    pub(crate) fn decisions(&mut self, decisions: Vec<Decision>) {
        if decisions.is_empty() || !self.sink.wants_records() {
            return;
        }
        for d in decisions {
            self.sink.publish(LiveRecord::Decision(d));
        }
    }

    /// A track closed: forget it and publish its closure.
    pub(crate) fn closed(&mut self, track_id: u32, closure: ClosureKind) {
        self.tracks.remove(&track_id);
        self.dirty.remove(&track_id);
        if self.sink.wants_records() {
            self.sink.publish(LiveRecord::TrackClosed {
                track_id,
                sample_ts: self.now_ts,
                closure,
            });
        }
    }

    /// End of a decode batch: one snapshot per track whose text changed,
    /// in track-id order.
    pub(crate) fn end_batch(&mut self, validator: &Validator) {
        let dirty = std::mem::take(&mut self.dirty);
        for track_id in dirty {
            self.publish_track(track_id, validator);
        }
    }

    fn publish_track(&self, track_id: u32, validator: &Validator) {
        if !self.sink.wants_records() {
            return;
        }
        let Some(t) = self.tracks.get(&track_id) else {
            return;
        };
        let mut candidates = validator.candidates_for(track_id, self.now_ts);
        candidates.truncate(LIVE_MAX_CANDIDATES);
        self.sink.publish(LiveRecord::Track(TrackSnapshot {
            track_id,
            sample_ts: t.last_text_ts,
            freq_hz: t.freq_hz,
            wpm: t.wpm,
            snr_2500_db: t.snr_2500_db,
            text: t.text.iter().collect(),
            candidates,
        }));
    }

    /// Once per second of stream time, publishes a band sample. Reads and
    /// resets the track manager's occupancy window either way, so each
    /// sample covers about one second.
    pub(crate) fn maybe_tick(&mut self, tm: &mut TrackManager, stream_ts: u64) {
        if stream_ts < self.next_tick {
            return;
        }
        while self.next_tick <= stream_ts {
            self.next_tick += self.tick_samples;
        }
        let mut channels = tm.take_channel_levels();
        if !self.sink.wants_records() {
            return;
        }
        for c in &mut channels {
            c.freq_hz *= self.factor;
        }
        let decoding_track_freqs_hz = tm
            .decoding_track_freqs_hz()
            .into_iter()
            .map(|f| f * self.factor)
            .collect();
        self.sink.publish(LiveRecord::Bands(BandSample {
            sample_ts: stream_ts,
            channels,
            decoding_track_freqs_hz,
        }));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Records every published record; `wants` toggles subscription.
    #[derive(Default)]
    pub(crate) struct RecordingSink {
        pub(crate) records: Mutex<Vec<LiveRecord>>,
        pub(crate) wants: AtomicBool,
    }

    impl RecordingSink {
        pub(crate) fn wanting() -> Arc<Self> {
            let s = Arc::new(Self::default());
            s.wants.store(true, Ordering::Relaxed);
            s
        }
        pub(crate) fn take(&self) -> Vec<LiveRecord> {
            std::mem::take(&mut *self.records.lock().unwrap())
        }
    }

    impl LiveStateSink for RecordingSink {
        fn wants_records(&self) -> bool {
            self.wants.load(Ordering::Relaxed)
        }
        fn publish(&self, record: LiveRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    const FS: f64 = 96_000.0;

    fn validator() -> Validator {
        Validator::bundled(FS)
    }

    fn chars(tap: &mut LiveTap, v: &Validator, track_id: u32, text: &str, ts: &mut u64) {
        for c in text.chars() {
            if c == ' ' {
                tap.observe(&DecoderEvent::word_boundary(track_id, *ts), v);
            } else {
                tap.observe(
                    &DecoderEvent::char_decoded(track_id, *ts, Glyph::Char(c), 0.9),
                    v,
                );
            }
            *ts += 100;
        }
    }

    fn tracks(records: &[LiveRecord]) -> Vec<&TrackSnapshot> {
        records
            .iter()
            .filter_map(|r| match r {
                LiveRecord::Track(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_keeps_the_newest_128_characters_with_one_space_per_word_boundary() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 1, "CQ  DE W1AW ", &mut ts);
        tap.end_batch(&v);
        let recs = sink.take();
        assert_eq!(tracks(&recs)[0].text, "CQ DE W1AW");
        let long: String = (0..60).map(|_| "AB ").collect();
        chars(&mut tap, &v, 1, &long, &mut ts);
        chars(&mut tap, &v, 1, "XY", &mut ts);
        tap.end_batch(&v);
        let recs = sink.take();
        let text = &tracks(&recs)[0].text;
        assert_eq!(text.chars().count(), LIVE_TEXT_CHARS);
        assert!(text.ends_with("AB AB XY"), "{text}");
        assert!(!text.starts_with(' '));
    }

    #[test]
    fn prosigns_are_dropped_from_text() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 1, "TEST", &mut ts);
        tap.observe(
            &DecoderEvent::char_decoded(
                1,
                ts,
                Glyph::Prosign(manta_decode::tree::Prosign::Ar),
                0.9,
            ),
            &v,
        );
        tap.end_batch(&v);
        assert_eq!(tracks(&sink.take())[0].text, "TEST");
    }

    #[test]
    fn frequency_comes_from_promotion_then_track_meta() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        tap.observe(
            &DecoderEvent::TrackPromoted {
                track_id: 1,
                sample_ts: 0,
                freq_hz: 14_010_000.0,
            },
            &v,
        );
        let mut ts = 10;
        chars(&mut tap, &v, 1, "A", &mut ts);
        tap.end_batch(&v);
        assert_eq!(tracks(&sink.take())[0].freq_hz, Some(14_010_000.0));
        tap.observe(
            &DecoderEvent::TrackMeta {
                track_id: 1,
                sample_ts: 20,
                snr_2500_db: 12.0,
                freq_hz: 14_010_050.0,
            },
            &v,
        );
        chars(&mut tap, &v, 1, "B", &mut ts);
        tap.end_batch(&v);
        let recs = sink.take();
        assert_eq!(tracks(&recs)[0].freq_hz, Some(14_010_050.0));
        assert_eq!(tracks(&recs)[0].snr_2500_db, Some(12.0));
    }

    #[test]
    fn wpm_and_snr_are_null_until_their_first_event() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 1, "A", &mut ts);
        tap.end_batch(&v);
        let recs = sink.take();
        let t = tracks(&recs)[0];
        assert_eq!((t.freq_hz, t.wpm, t.snr_2500_db), (None, None, None));
        tap.observe(&DecoderEvent::SpeedUpdate { track_id: 1, wpm: 21.4 }, &v);
        chars(&mut tap, &v, 1, "B", &mut ts);
        tap.end_batch(&v);
        assert_eq!(tracks(&sink.take())[0].wpm, Some(21.4));
    }

    #[test]
    fn one_track_state_per_dirty_track_per_batch_in_track_id_order() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 3, "ABC", &mut ts);
        chars(&mut tap, &v, 1, "XYZ", &mut ts);
        tap.observe(&DecoderEvent::SpeedUpdate { track_id: 2, wpm: 20.0 }, &v);
        tap.end_batch(&v);
        let recs = sink.take();
        let ids: Vec<u32> = tracks(&recs).iter().map(|t| t.track_id).collect();
        assert_eq!(ids, [1, 3], "one record per dirty track, none for a speed-only track");
        tap.end_batch(&v);
        assert!(sink.take().is_empty(), "nothing changed in the second batch");
    }

    #[test]
    fn track_closed_flushes_pending_text_before_the_closed_record() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 1, "W1AW", &mut ts);
        let closed = DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        };
        tap.observe(&closed, &v);
        tap.closed(1, ClosureKind::SignalEnded);
        tap.end_batch(&v);
        let recs = sink.take();
        assert_eq!(recs.len(), 2, "{recs:?}");
        assert!(matches!(&recs[0], LiveRecord::Track(t) if t.text == "W1AW"));
        assert!(matches!(
            recs[1],
            LiveRecord::TrackClosed {
                track_id: 1,
                closure: ClosureKind::SignalEnded,
                ..
            }
        ));
    }

    #[test]
    fn nothing_is_published_without_a_subscriber_but_state_is_kept() {
        let sink = Arc::new(RecordingSink::default());
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let v = validator();
        let mut ts = 0;
        chars(&mut tap, &v, 1, "CQ DE", &mut ts);
        tap.end_batch(&v);
        tap.decisions(vec![]);
        assert!(sink.take().is_empty());
        sink.wants.store(true, Ordering::Relaxed);
        chars(&mut tap, &v, 1, " W1AW", &mut ts);
        tap.end_batch(&v);
        assert_eq!(tracks(&sink.take())[0].text, "CQ DE W1AW");
    }

    #[test]
    fn band_ticks_once_per_stream_second_across_batches() {
        let sink = RecordingSink::wanting();
        let mut tap = LiveTap::new(sink.clone(), FS, 1.0);
        let mut tm = TrackManager::new(
            64,
            FS,
            14_000_000.0,
            crate::DetectorConfig::default(),
            manta_decode::decoder::DecodeConfig::default(),
        );
        let fs = FS as u64;
        // Batches of ~43 ms over 3.5 s, plus one 2 s batch: one sample per
        // crossed second.
        let mut ts = 0;
        while ts < fs * 7 / 2 {
            ts += 4096;
            tap.maybe_tick(&mut tm, ts);
        }
        assert_eq!(sink.take().len(), 3);
        ts += 2 * fs;
        tap.maybe_tick(&mut tm, ts);
        let recs = sink.take();
        assert_eq!(recs.len(), 1, "a long batch gives one sample, not one per second");
        let LiveRecord::Bands(b) = &recs[0] else {
            panic!("{recs:?}")
        };
        assert_eq!(b.channels.len(), 64);
        assert_eq!(b.sample_ts, ts);
    }
}
