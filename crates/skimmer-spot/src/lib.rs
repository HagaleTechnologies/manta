//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6.

pub mod confidence;
pub mod context;
pub mod cty;
pub mod grammar;
pub mod scp;

pub use context::SpotType;
