//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6.

pub mod confidence;
pub mod context;
pub mod cty;
pub mod dedupe;
pub mod gate;
pub mod grammar;
pub mod scp;
pub mod validator;

pub use context::SpotType;
pub use validator::{Spot, Validator};

/// AD1C's `cty.dat` country/prefix table, vendored under `data/` -- see
/// `data/SOURCES.md` for provenance and refresh instructions.
pub const CTY_DAT: &str = include_str!("../data/cty.dat");

/// The `MASTER.SCP` super-check-partial callsign list, vendored under
/// `data/` -- see `data/SOURCES.md` for provenance and refresh
/// instructions.
pub const MASTER_SCP: &str = include_str!("../data/master.scp");
