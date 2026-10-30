//! The why-(not-)spotted record (MAN-324): every decision
//! `Validator::evaluate_candidate` and the beacon path make about a
//! candidate callsign, with its reason. Opt-in through
//! `Validator::record_decisions`; off, the validator allocates nothing for
//! it. See docs/DECISIONS/2026-10-12-man324-live-state-stream.md D4.

use crate::context::SpotType;
use crate::support::Support;
use crate::validator::Spot;

/// What validation decided about one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Published as a spot.
    Spotted,
    /// Not published (this time); see the `Reason`.
    Withheld,
    /// A beacon candidate captured now and judged when its track ends, or
    /// (MAN-318) a candidate held until the callsign lookup answers.
    Deferred,
}

/// Why a candidate was withheld or deferred. `None` on a `Spotted`
/// decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The call is on the operator's `[spot]` blocklist.
    Blocklist,
    /// The frequency is inside a `[spot]` notch.
    Notch,
    /// Not a valid callsign shape (`grammar::is_plausible`).
    Grammar,
    /// No `cty.dat` prefix allocates it.
    Cty,
    /// Fewer than the candidate pattern tier requires. Not final: a later
    /// repetition produces a new decision.
    Repetition,
    /// A better-supported variant on the same track wins (MAN-100).
    Variant,
    /// The same call was spotted recently on this frequency.
    Dedupe,
    /// A power-step CQ/DE framing candidate burned by the MAN-105 guard.
    PowerStepGuard,
    /// A beacon candidate deferred to its track's end.
    BeaconPending,
    /// A deferred beacon rejected at track end: speed unconfirmed or
    /// implausibly fast.
    BeaconSpeed,
    /// A deferred beacon evicted because its track held too many.
    BeaconOverflow,
    /// A deferred beacon dropped when its track closed for bookkeeping
    /// with no survivor.
    BeaconDiscarded,
    /// A candidate held until the callsign lookup answers (MAN-318); a
    /// later decision settles it.
    LookupPending,
    /// The callsign lookup service does not list the call (MAN-318).
    LookupNotFound,
}

/// The variant that beat a candidate withheld for `Reason::Variant`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rival {
    pub call: String,
    pub support: Support,
}

/// One validation decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub track_id: u32,
    pub sample_ts: u64,
    pub call: String,
    /// The validator's calibrated frequency.
    pub freq_hz: f64,
    pub spot_type: SpotType,
    pub verdict: Verdict,
    /// `None` only for `Verdict::Spotted`.
    pub reason: Option<Reason>,
    /// The repetition gate's count for this decode; `None` when the
    /// decision came before the gate ran.
    pub reps: Option<u32>,
    /// Admission requirement at the configured validation level.
    pub required_reps: u32,
    /// None for an allowlisted call or a beacon.
    pub pattern_tier: Option<crate::PatternTier>,
    pub rival: Option<Rival>,
    /// The emitted spot, `Verdict::Spotted` only.
    pub spot: Option<Spot>,
}

impl Decision {
    /// A `Withheld` (or `Deferred`, for `BeaconPending`/`LookupPending`) decision with no
    /// repetition count, rival or spot yet.
    pub(crate) fn withheld(
        track_id: u32,
        sample_ts: u64,
        call: &str,
        freq_hz: f64,
        spot_type: SpotType,
        reason: Reason,
    ) -> Self {
        let verdict = if matches!(reason, Reason::BeaconPending | Reason::LookupPending) {
            Verdict::Deferred
        } else {
            Verdict::Withheld
        };
        Self {
            track_id,
            sample_ts,
            call: call.to_string(),
            freq_hz,
            spot_type,
            verdict,
            reason: Some(reason),
            reps: None,
            // Validator::decide fills the per-candidate admission metadata.
            required_reps: 1,
            pattern_tier: None,
            rival: None,
            spot: None,
        }
    }

    /// The `Spotted` decision for `spot`.
    pub(crate) fn spotted(spot: &Spot, reps: u32) -> Self {
        Self {
            track_id: spot.track_id,
            sample_ts: spot.sample_ts,
            call: spot.callsign.clone(),
            freq_hz: spot.freq_hz,
            spot_type: spot.spot_type,
            verdict: Verdict::Spotted,
            reason: None,
            reps: Some(reps),
            // Validator::decide fills the per-candidate admission metadata.
            required_reps: 1,
            pattern_tier: None,
            rival: None,
            spot: Some(spot.clone()),
        }
    }

    pub(crate) fn with_reps(mut self, reps: u32) -> Self {
        self.reps = Some(reps);
        self
    }
}

/// One candidate callsign the validator's variant arbitration is weighing
/// on a track, with its support (`Validator::candidates_for`).
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateSupport {
    pub call: String,
    pub support: Support,
}
