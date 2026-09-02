//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6. Plus operator suppression overrides
//! (bad-call blocklist, notched frequencies -- MAN-31) orthogonal to
//! that pipeline.

pub mod blocklist;
pub mod confidence;
pub mod context;
pub mod cty;
pub mod dedupe;
pub mod gate;
pub mod grammar;
pub mod notch;
pub mod scp;
pub mod validator;

pub use blocklist::Blocklist;
pub use context::SpotType;
pub use notch::{FreqRange, NotchList};
pub use validator::{calibration_factor_from_ppm, InvalidCalibration, Spot, Validator};

/// AD1C's `cty.dat` country/prefix table, vendored under `data/` -- see
/// `data/SOURCES.md` for provenance and refresh instructions.
pub const CTY_DAT: &str = include_str!("../data/cty.dat");

/// The `MASTER.SCP` super-check-partial callsign list, vendored under
/// `data/` -- see `data/SOURCES.md` for provenance and refresh
/// instructions.
pub const MASTER_SCP: &str = include_str!("../data/master.scp");
