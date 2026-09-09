//! Decoder output event stream. SPEC §5.

use crate::tree::Glyph;

/// Decoder output events, in emission order. SPEC §5.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "event")]
pub enum DecoderEvent {
    CharDecoded {
        track_id: u32,
        sample_ts: u64,
        glyph: Glyph,
        confidence: f32,
    },
    WordBoundary {
        track_id: u32,
        sample_ts: u64,
    },
    SpeedUpdate {
        track_id: u32,
        wpm: f32,
    },
    TrackMeta {
        track_id: u32,
        snr_2500_db: f32,
        freq_hz: f64,
    },
    /// A track has been promoted from CANDIDATE to ACTIVE (SPEC §2.1) --
    /// the detector decided this looks like a real signal worth
    /// demodulating, at the exact hop it made that decision. Unlike every
    /// other variant, this doesn't depend on the decoder producing
    /// anything downstream (a full decode, a periodic TrackMeta update, or
    /// even a real event surviving to `TrackClosed`'s `has_emitted`
    /// filter) -- it's the detector's own ground truth for "found a
    /// candidate," which `manta_engine::doctor()`'s `NoSignal` check needs
    /// directly rather than inferring it from decode-timing side effects
    /// (see docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md).
    TrackPromoted {
        track_id: u32,
        sample_ts: u64,
        freq_hz: f64,
    },
    /// A track has closed (any `CloseReason`: Unconfirmed/HangExpired/
    /// Silent/Merged/Evicted) and will never emit another event under
    /// this `track_id` -- `TrackManager::next_id` never reuses one.
    /// MAN-19: added so per-track_id state downstream (`manta-spot`'s
    /// `Validator::tracks`, `RepetitionGate::seen`) has a signal to free
    /// it; without this neither had one, so both grew unboundedly for the
    /// life of the process under sustained track churn.
    TrackClosed {
        track_id: u32,
    },
}
