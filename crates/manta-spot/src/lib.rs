//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6. Plus operator suppression overrides
//! (bad-call blocklist, notched frequencies -- MAN-31) orthogonal to
//! that pipeline.

pub mod blocklist;
pub mod confidence;
pub mod context;
pub mod cty;
pub mod decision;
pub mod dedupe;
pub mod gate;
pub mod grammar;
pub mod lookup;
pub mod message;
pub mod notch;
pub mod pattern;
pub mod reference;
pub mod scp;
pub mod support;
pub mod validator;
pub mod variant;

pub use blocklist::Blocklist;
pub use context::SpotType;
pub use decision::{CandidateSupport, Decision, Reason, Rival, Verdict};
pub use gate::{RepetitionCounts, REPETITION_BUCKETS};
pub use lookup::{CallsignLookup, Existence, LookupStatus};
pub use notch::{FreqRange, NotchList};
pub use pattern::{PatternTier, ValidationLevel};
pub use support::Support;
pub use validator::{
    calibration_factor_from_ppm, EmissionState, InvalidCalibration, Spot, SuppressionCounts,
    Validator,
};

/// AD1C's `cty.dat` country/prefix table, vendored under `data/` -- see
/// `data/SOURCES.md` for provenance and refresh instructions. Override with `--cty`.
pub const CTY_DAT: &str = include_str!("../data/cty.dat");

/// The `MASTER.SCP` super-check-partial callsign list, vendored under
/// `data/` -- see `data/SOURCES.md` for provenance and refresh
/// instructions. Override with `--scp`.
pub const MASTER_SCP: &str = include_str!("../data/master.scp");

/// Per-entity ADIF DXCC entity numbers keyed on `cty.dat`'s primary-prefix
/// field, vendored under `data/` -- see `data/SOURCES.md` for provenance and
/// `scripts/gen-dxcc-table.sh` for regeneration. Separate from `cty.dat`
/// because AD1C's `cty.dat` carries no ADIF entity numbers at all (MAN-136,
/// broad-review decision D10).
pub const DXCC_TSV: &str = include_str!("../data/dxcc.tsv");

pub mod vintage;

/// Callsign-pattern rarity list derived from `MASTER.SCP`; see `data/SOURCES.md`.
pub const PATTERNS_LST: &str = include_str!("../data/patterns.lst");
