//! MAN-198: the stage-attributed loss funnel behind `manta oracle --funnel`.
//!
//! For each truth opportunity (one call on one frequency over one span of
//! time) the funnel charges exactly one earliest failed pipeline stage --
//! input coverage, detectable energy, admitted track, usable timing, text
//! contains call, self-ID recognized, sufficient evidence, emitted,
//! delivered -- or none, when a spot for the call was delivered. The
//! evidence comes from one production `listen_with_observers` pass with a
//! write-only [`FunnelTrace`] attached; decoding and spots are unchanged.
//! Diagnostic only: nothing here is a recall score. See
//! docs/DECISIONS/2026-11-03-man198-loss-funnel.md.

mod attribute;
mod bypass;
mod collect;
mod trace;

pub use attribute::{
    attribute, summarize, Births, CloseTally, DecisionFact, Evidence, Geometry, Opportunity,
    Record, RecordSpot, SpotFact, Stage, StageCounts, Summary, Tolerances, TrackFacts, TrackTally,
    TruthLayer, VetoFact, TRACK_TEXT_CHARS,
};
pub use bypass::{
    run_bypass, run_synthetic_bypass, summarize_bypass, Bypass, BypassRecord, BypassSummary,
    SyntheticTruth, TruthWord,
};
pub use collect::{run_funnel, FunnelReport};
pub use trace::{FunnelTrace, SpawnVeto, TraceLog, TrackCloseReason, TrackTraceRecord, VetoKey};
