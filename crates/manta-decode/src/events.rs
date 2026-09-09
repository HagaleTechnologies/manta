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
        /// Round 9 (PR #154): whether a consumer holding deferred,
        /// per-identity evidence for this track_id may treat this closure
        /// as the identity's true final state. See `ClosureKind`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ClosureKind {
    /// A genuine, observed end of signal: overall stream EOF, a
    /// sustained observed power drop (`HangExpired`), or no character
    /// decoded for the GC timeout (`Silent` -- not itself proof the RF
    /// signal is gone, but by the time `TrackManager::finish` or
    /// `step_hop`'s closure path fires this reason, no further evidence
    /// for this exact `track_id` will ever arrive either way).
    SignalEnded,
    /// Pure track-bookkeeping, not evidence the signal itself ended. If
    /// `survivor_track_id` is `Some`, this track's identity may continue
    /// there (`Merged`); if `None`, it simply stopped being tracked with
    /// no successor (`Evicted`).
    Bookkeeping { survivor_track_id: Option<u32> },
}
