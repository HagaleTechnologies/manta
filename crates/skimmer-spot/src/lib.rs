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
