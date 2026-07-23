//! Channel extraction and frequency estimation for skimmer.
//!
//! At M0 this crate holds the Kaiser prototype designer (SPEC §1.2 — NEW code,
//! coppa-dsp has no FIR designer), a single-channel extractor shim, and an
//! FFT-peak frequency estimator. `channelizer` is the M2 full N-channel WOLA
//! polyphase filterbank (SPEC §1.1-1.3) that supersedes `single`/`freqest`.

pub mod channelizer;
pub mod floor;
pub mod freqest;
pub mod hilbert;
pub mod proto;
pub mod single;
