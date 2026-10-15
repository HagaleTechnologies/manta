//! Decoder output event stream. SPEC §5.

use crate::timing::TimingEstimates;
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
        /// The hop's input-stream sample counter (MAN-102 review round 2,
        /// finding 1). Was implicitly `0` for every `TrackMeta` (see
        /// `manta_engine::track::event_sample_ts`'s old tie-break comment)
        /// until this field existed, which sorted every `TrackMeta` ahead
        /// of the *entire* batch it was emitted in -- including
        /// `CharDecoded`/`WordBoundary` events from earlier, real hops in
        /// that same batch -- letting a just-reset/just-reported SNR value
        /// retroactively attach to characters decoded before it, with the
        /// magnitude of the error depending on the caller's chunk size
        /// (`decode_samples` batches differently than `listen`). A real
        /// timestamp lets the existing `(sample_ts, track_id)` resequence
        /// (SPEC §6 rule 6) place it correctly, the same treatment
        /// `TrackClosed` already gets for an analogous problem.
        /// `#[serde(default)]` (Codex review, PR #134 round 3) so an event
        /// log from before this field existed still deserializes -- `0`
        /// reproduces exactly the old synthetic-timestamp behavior this
        /// field replaces.
        #[serde(default)]
        sample_ts: u64,
        snr_2500_db: f32,
        /// The track's centroid at the hop `sample_ts` names (MAN-211).
        freq_hz: f64,
    },
    /// The track's learned sender timing (MAN-315 D8): dot, dash,
    /// element-gap, char-gap and word-gap durations, mean and variance in
    /// dits of `dit_ms`, `null` for a class with no observations. Emitted
    /// on the `TrackMeta` cadence when a class gained an observation since
    /// the last one or a drift re-init wiped timing already reported (the
    /// wiped classes then read `null`), and at track finish. Legacy and
    /// EdgeLegacy only. The
    /// estimates are flattened into the event object. SPEC §5.
    TimingUpdate {
        track_id: u32,
        sample_ts: u64,
        #[serde(flatten)]
        timing: TimingEstimates,
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
    /// Silent/Merged/Evicted/Rtty) and will never emit another event under
    /// this `track_id` -- `TrackManager::next_id` never reuses one. The
    /// normative schema, including both `closure` kinds and when this is
    /// emitted at all, is SPEC-decode-core.md §5; §6 rule 6 orders a merge
    /// loser's `TrackClosed` before its survivor's (MAN-174).
    /// MAN-19: added so per-track_id state downstream (`manta-spot`'s
    /// `Validator::tracks`, `RepetitionGate::seen`) has a signal to free
    /// it; without this neither had one, so both grew unboundedly for the
    /// life of the process under sustained track churn.
    TrackClosed {
        track_id: u32,
        /// Round 9 (PR #154): whether a consumer holding deferred,
        /// per-identity evidence for this track_id may treat this closure
        /// as the identity's true final state. See `ClosureKind`.
        /// `#[serde(default)]` (SPEC v2 §5 merge, MAN-166, 2026-09-09):
        /// PR #154 predates this branch's `Deserialize` derive on
        /// `DecoderEvent`, so old `TrackClosed` JSON without this field
        /// must still deserialize -- defaults to the conservative
        /// `SignalEnded` reading, matching every consumer's implicit
        /// assumption before `ClosureKind` existed.
        #[serde(default)]
        closure: ClosureKind,
    },
}

/// Whether a `TrackClosed` reflects a genuine end-of-signal, or is pure
/// track-bookkeeping that says nothing about whether the physical RF
/// signal is actually gone. Round 9 (PR #154): `manta-spot`'s deferred
/// Beacon-candidate judgment (`Validator::resolve_pending_beacons`) must
/// only ever run against a genuine end-of-signal -- a track closed via
/// Merged/Evicted bookkeeping can have its identity's real signal
/// continuing on a survivor track, or simply drop out of tracking
/// entirely (still transmitting, just no longer followed), and treating
/// that moment as "final" reopens exactly the WPM-oscillation false-
/// positive risk the deferred-judgment design exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClosureKind {
    /// A genuine, observed end of signal: overall stream EOF
    /// (`TrackManager::finish`) or a sustained observed power drop
    /// (`HangExpired`). Deferred evidence is judged now.
    SignalEnded,
    /// Pure track-bookkeeping, not evidence the signal itself ended. If
    /// `survivor_track_id` is `Some`, this track's identity may continue
    /// there (`Merged`) and deferred evidence moves to it; if `None`, it
    /// simply stopped being tracked with no successor (`Evicted`,
    /// `Silent` -- no character decoded for the GC timeout, which a
    /// continuous carrier also satisfies, round 10 -- `Rtty`, or a source
    /// discontinuity, MAN-73) and deferred evidence is discarded and
    /// counted. See SPEC-decode-core.md §5.
    Bookkeeping { survivor_track_id: Option<u32> },
}

impl Default for ClosureKind {
    /// See `TrackClosed::closure`'s `#[serde(default)]` doc comment.
    fn default() -> Self {
        ClosureKind::SignalEnded
    }
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
        // PR #154's `ClosureKind` predates this branch's `Deserialize` derive --
        // old `TrackClosed` JSON without `closure` must still deserialize.
        let json = r#"{"event":"TrackClosed","track_id":1}"#;
        let e: DecoderEvent = serde_json::from_str(json).unwrap();
        match e {
            DecoderEvent::TrackClosed { closure, .. } => {
                assert_eq!(closure, ClosureKind::SignalEnded)
            }
            _ => panic!(),
        }
    }

    fn timing_update() -> DecoderEvent {
        use crate::timing::ClassEstimate;
        let c = |mean_dits, var_dits2, n| {
            Some(ClassEstimate {
                mean_dits,
                var_dits2,
                n,
            })
        };
        DecoderEvent::TimingUpdate {
            track_id: 2,
            sample_ts: 4_128_768,
            timing: TimingEstimates {
                dit_ms: 54.5,
                dot: c(0.875, 0.0625, 118),
                dash: c(4.0, 0.25, 131),
                element_gap: c(1.125, 0.03125, 176),
                char_gap: c(4.5, 0.5, 61),
                word_gap: None,
            },
        }
    }

    /// MAN-315 D8: the estimates are flattened into the event object.
    #[test]
    fn timing_update_json_shape() {
        assert_eq!(
            serde_json::to_string(&timing_update()).unwrap(),
            r#"{"event":"TimingUpdate","track_id":2,"sample_ts":4128768,"dit_ms":54.5,"dot":{"mean_dits":0.875,"var_dits2":0.0625,"n":118},"dash":{"mean_dits":4.0,"var_dits2":0.25,"n":131},"element_gap":{"mean_dits":1.125,"var_dits2":0.03125,"n":176},"char_gap":{"mean_dits":4.5,"var_dits2":0.5,"n":61},"word_gap":null}"#
        );
    }

    #[test]
    fn timing_update_roundtrips() {
        let e = timing_update();
        let json = serde_json::to_string(&e).unwrap();
        let back: DecoderEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
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
