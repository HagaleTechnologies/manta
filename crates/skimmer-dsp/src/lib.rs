//! Channel extraction and frequency estimation for skimmer.
//!
//! At M0 this crate holds the Kaiser prototype designer (SPEC §1.2 — NEW code,
//! coppa-dsp has no FIR designer), a single-channel extractor shim, and an
//! FFT-peak frequency estimator. The M2 PFB replaces `single` and `freqest`.

pub mod freqest;
pub mod proto;
pub mod single;
