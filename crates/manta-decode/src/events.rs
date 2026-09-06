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
