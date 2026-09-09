//! Decoder output event stream. SPEC §5.

use crate::tree::Glyph;

fn one() -> f32 {
    1.0
}

/// Decoder output events, in emission order. SPEC §5.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event")]
pub enum DecoderEvent {
    CharDecoded {
        track_id: u32,
        sample_ts: u64,
        glyph: Glyph,
        confidence: f32,
        /// v2 §4.9: up to two competing glyphs at this position with their
        /// posterior mass. Empty for the legacy engine.
        #[serde(default)]
        alternatives: Vec<(Glyph, f32)>,
    },
    WordBoundary {
        track_id: u32,
        sample_ts: u64,
        /// v2 §4.9: posterior that this gap is a word gap. 1.0 for legacy.
        #[serde(default = "one")]
        confidence: f32,
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

impl DecoderEvent {
    pub fn char_decoded(track_id: u32, sample_ts: u64, glyph: Glyph, confidence: f32) -> Self {
        DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            glyph,
            confidence,
            alternatives: Vec::new(),
        }
    }
    pub fn word_boundary(track_id: u32, sample_ts: u64) -> Self {
        DecoderEvent::WordBoundary {
            track_id,
            sample_ts,
            confidence: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Glyph;

    #[test]
    fn legacy_json_without_new_fields_still_deserializes() {
        let json = r#"{"event":"CharDecoded","track_id":1,"sample_ts":10,"glyph":{"Char":"A"},"confidence":0.5}"#;
        let e: DecoderEvent = serde_json::from_str(json).unwrap();
        match e {
            DecoderEvent::CharDecoded { alternatives, .. } => assert!(alternatives.is_empty()),
            _ => panic!(),
        }
        let json = r#"{"event":"WordBoundary","track_id":1,"sample_ts":10}"#;
        let e: DecoderEvent = serde_json::from_str(json).unwrap();
        match e {
            DecoderEvent::WordBoundary { confidence, .. } => assert_eq!(confidence, 1.0),
            _ => panic!(),
        }
    }

    #[test]
    fn constructors_fill_defaults() {
        let e = DecoderEvent::char_decoded(1, 10, Glyph::Char('A'), 0.5);
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"CharDecoded","track_id":1,"sample_ts":10,"glyph":{"Char":"A"},"confidence":0.5,"alternatives":[]}"#
        );
        let e = DecoderEvent::word_boundary(1, 10);
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"WordBoundary","track_id":1,"sample_ts":10,"confidence":1.0}"#
        );
    }
}
