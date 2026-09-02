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
}
