//! Bounded evidence for acoustically supported word readings.

/// A prior scores only readings supplied by the acoustic beam. It must never
/// generate or complete text. The decoder clamps bonuses to its configured cap.
pub trait WordPrior: Send + Sync + std::fmt::Debug {
    fn reading_bonus(&self, reading: &str) -> f32;
}
