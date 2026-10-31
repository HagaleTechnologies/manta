//! MAN-318: an external check that a callsign exists, asked as validation's
//! last step. manta-spot only asks; the daemon answers (QRZ.com, in
//! `manta-cli`'s `callsign_lookup`). With no lookup set the validator never
//! asks, so file input stays byte-identical. See
//! docs/DECISIONS/2026-10-27-man318-callsign-lookup.md.

/// What is known about one callsign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existence {
    /// The service lists the call: spot it.
    Exists,
    /// The service answered that it does not list the call: withhold it.
    NotFound,
    /// No answer (failed, timed out, skipped): spot it anyway.
    Unknown,
}

/// `CallsignLookup::check`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStatus {
    Known(Existence),
    /// A lookup is queued or in flight; `settled` reports its answer.
    Pending,
}

/// Runs on the decode thread: every method returns without waiting on the
/// network, except `settle_all`. Each `check` is one would-be spot, so an
/// implementation can count spots per answer.
pub trait CallsignLookup: Send + Sync {
    /// `call` is the base callsign (a valid portable suffix stripped).
    fn check(&self, call: &str) -> LookupStatus;
    /// Calls that were `Pending` and now have an answer, oldest request
    /// first; `Unknown` for one whose deadline passed unanswered.
    fn settled(&self) -> Vec<(String, Existence)>;
    /// Waits until every pending call is settled (each at most its own
    /// deadline), then reports them as `settled` does. Called once, when
    /// decoding ends.
    fn settle_all(&self) -> Vec<(String, Existence)>;
}
