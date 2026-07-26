//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6.

pub mod context;
pub mod cty;
pub mod grammar;

pub use context::SpotType;
