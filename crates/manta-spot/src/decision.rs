//! The why-(not-)spotted record (MAN-324): every decision
//! `Validator::evaluate_candidate` and the beacon path make about a
//! candidate callsign, with its reason. Opt-in through
//! `Validator::record_decisions`; off, the validator allocates nothing for
//! it. See docs/DECISIONS/2026-10-12-man324-live-state-stream.md D4.
//! MAN-207 keeps these decisions, with their evidence, in the bounded
//! observation record (`crate::observation`).

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
    /// A `<call> T` whose call is not on the allowlist. A trailing `T` alone
    /// is not beacon evidence (MAN-201).
    BeaconNoEvidence,
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
    /// No finite positive character confidence supports the candidate (MAN-106).
    NoEvidence,
    /// A candidate held until the callsign lookup answers (MAN-318); a
    /// later decision settles it.
    LookupPending,
    /// The callsign lookup service does not list the call (MAN-318).
    LookupNotFound,
}

impl Reason {
    /// The reason's wire label, as the live-state stream spells it
    /// (`manta_server::live_state::reason_label` delegates here). MAN-198:
    /// the loss funnel's `detail` strings reuse it, and the engine cannot
    /// read `manta-server`.
    pub fn label(self) -> &'static str {
        match self {
            Reason::Blocklist => "blocklist",
            Reason::Notch => "notch",
            Reason::Grammar => "grammar",
            Reason::Cty => "cty",
            Reason::Repetition => "repetition",
            Reason::Variant => "variant",
            Reason::Dedupe => "dedupe",
            Reason::PowerStepGuard => "powerStepGuard",
            Reason::BeaconNoEvidence => "beaconNoEvidence",
            Reason::BeaconPending => "beaconPending",
            Reason::BeaconSpeed => "beaconSpeed",
            Reason::BeaconOverflow => "beaconOverflow",
            Reason::BeaconDiscarded => "beaconDiscarded",
            Reason::NoEvidence => "noEvidence",
            Reason::LookupPending => "lookupPending",
            Reason::LookupNotFound => "lookupNotFound",
        }
    }

    /// Every reason, in declaration order (MAN-207's counters index by it).
    pub const ALL: [Reason; 16] = [
        Reason::Blocklist,
        Reason::Notch,
        Reason::Grammar,
        Reason::Cty,
        Reason::Repetition,
        Reason::Variant,
        Reason::Dedupe,
        Reason::PowerStepGuard,
        Reason::BeaconNoEvidence,
        Reason::BeaconPending,
        Reason::BeaconSpeed,
        Reason::BeaconOverflow,
        Reason::BeaconDiscarded,
        Reason::NoEvidence,
        Reason::LookupPending,
        Reason::LookupNotFound,
    ];

    /// The verdict a decision with this reason carries: `Deferred` while a
    /// beacon or lookup is pending, else `Withheld`.
    pub fn verdict(self) -> Verdict {
        match self {
            Reason::BeaconPending | Reason::LookupPending => Verdict::Deferred,
            _ => Verdict::Withheld,
        }
    }

    /// This reason's position in `ALL`. Exhaustive on purpose: a new
    /// variant fails to compile here until it has a slot, and it must take
    /// the same position in `ALL` (checked at compile time below), or the
    /// MAN-207 counters would index past their arrays.
    pub(crate) const fn index(self) -> usize {
        match self {
            Reason::Blocklist => 0,
            Reason::Notch => 1,
            Reason::Grammar => 2,
            Reason::Cty => 3,
            Reason::Repetition => 4,
            Reason::Variant => 5,
            Reason::Dedupe => 6,
            Reason::PowerStepGuard => 7,
            Reason::BeaconNoEvidence => 8,
            Reason::BeaconPending => 9,
            Reason::BeaconSpeed => 10,
            Reason::BeaconOverflow => 11,
            Reason::BeaconDiscarded => 12,
            Reason::NoEvidence => 13,
            Reason::LookupPending => 14,
            Reason::LookupNotFound => 15,
        }
    }
}

// `ALL[i].index() == i` for every slot, so `ALL` and `index` agree.
const _: () = {
    let mut i = 0;
    while i < Reason::ALL.len() {
        assert!(Reason::ALL[i].index() == i);
        i += 1;
    }
};

/// How a spotted call met its repetition requirement (MAN-200 D12). Set on
/// `Spotted` decisions whose spot is `EmissionState::Confirmed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// Enough separate decodes read the call exactly.
    ExactRepeat,
    /// `[spot] repetition_fusion` combined damaged copies of one station's
    /// self-ID into the call (`lattice::EvidenceLattice`).
    Fused,
}

impl Confirmation {
    /// The live-state spelling: `"exactRepeat"` or `"fused"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactRepeat => "exactRepeat",
            Self::Fused => "fused",
        }
    }
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
    /// MAN-200: how a `Spotted`, confirmed call met its repetition
    /// requirement. `None` for any other decision and for a tentative spot.
    pub confirmation: Option<Confirmation>,
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
        let verdict = reason.verdict();
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
            confirmation: None,
        }
    }

    /// The `Spotted` decision for `spot`.
    pub(crate) fn spotted(spot: &Spot, reps: u32, confirmation: Option<Confirmation>) -> Self {
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
            confirmation,
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

#[cfg(test)]
mod tests {
    use super::Reason;

    #[test]
    fn reason_labels_match_the_live_state_wire() {
        let all = [
            (Reason::Blocklist, "blocklist"),
            (Reason::Notch, "notch"),
            (Reason::Grammar, "grammar"),
            (Reason::Cty, "cty"),
            (Reason::Repetition, "repetition"),
            (Reason::Variant, "variant"),
            (Reason::Dedupe, "dedupe"),
            (Reason::PowerStepGuard, "powerStepGuard"),
            (Reason::BeaconNoEvidence, "beaconNoEvidence"),
            (Reason::BeaconPending, "beaconPending"),
            (Reason::BeaconSpeed, "beaconSpeed"),
            (Reason::BeaconOverflow, "beaconOverflow"),
            (Reason::BeaconDiscarded, "beaconDiscarded"),
            (Reason::NoEvidence, "noEvidence"),
            (Reason::LookupPending, "lookupPending"),
            (Reason::LookupNotFound, "lookupNotFound"),
        ];
        for (r, label) in all {
            assert_eq!(r.label(), label);
        }
    }
}
