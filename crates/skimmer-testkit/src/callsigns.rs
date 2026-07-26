//! Deterministic fixture callsigns for SPEC §7 V8/V8w pileup scenes.
//! ChaCha8-seeded (pinned decision 2), not 50 hand-picked real-looking
//! calls.

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use std::collections::BTreeSet;

const PREFIXES: [&str; 25] = [
    "W2", "W3", "W4", "W5", "W6", "W7", "W8", "W9", "W0", "K1", "K2", "K3", "K4", "K6", "K7", "N3",
    "N4", "N5", "N6", "N7", "AA1", "AB2", "AC3", "VE3", "VE7",
];
const SUFFIX_LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const PILEUP_CALLS_SEED: u64 = 0x534B_494D_5638; // "SKIMV8"

/// 50 unique, deterministic fixture callsigns for pileup scenes (SPEC §7
/// V8/V8w). Uses `crate::u01` (lib.rs, already `pub(crate)`) for the same
/// hand-rolled ChaCha8 conversion every other generator in this crate uses
/// (pinned decision 2) -- no local reimplementation.
pub(crate) fn pileup_calls() -> Vec<String> {
    let mut rng = ChaCha8Rng::seed_from_u64(PILEUP_CALLS_SEED);
    let mut calls = BTreeSet::new();
    while calls.len() < 50 {
        let prefix = PREFIXES[(crate::u01(&mut rng) * PREFIXES.len() as f64) as usize];
        let suffix: String = (0..3)
            .map(|_| {
                SUFFIX_LETTERS[(crate::u01(&mut rng) * SUFFIX_LETTERS.len() as f64) as usize]
                    as char
            })
            .collect();
        calls.insert(format!("{prefix}{suffix}"));
    }
    calls.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_50_unique_deterministic_calls() {
        let a = pileup_calls();
        let b = pileup_calls();
        assert_eq!(a.len(), 50);
        assert_eq!(a, b, "must be deterministic across calls");
        let unique: BTreeSet<&String> = a.iter().collect();
        assert_eq!(unique.len(), 50, "all 50 calls must be unique");
    }
}
