//! CW keying state machine, timing, and Morse decode (SPEC-decode-core §3–§5).

pub mod beam;
pub mod decoder;
pub mod envelope;
pub mod events;
pub mod timing;
pub mod tree;

/// Channel output (envelope) rate, invariant across input rates. SPEC §1.1.
pub const FO_HZ: f64 = 375.0;
/// Hop period in milliseconds. SPEC §1.1.
pub const HOP_MS: f64 = 8.0 / 3.0;

/// The single normative ms->hop conversion: round half-up. SPEC §1.1.
pub fn ms_to_hops(ms: f64) -> u32 {
    (ms * 0.375 + 0.5).floor() as u32
}

#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn ms_to_hops_rounds_half_up() {
        // SPEC §1.1 single normative rounding rule; examples from SPEC §2.3–§3.3.
        assert_eq!(ms_to_hops(50.0), 19); // confirm window
        assert_eq!(ms_to_hops(12.0), 5); // debounce (4.5 -> 5)
        assert_eq!(ms_to_hops(500.0), 188); // A_ref window (187.5 -> 188)
        assert_eq!(ms_to_hops(1000.0), 375);
        assert_eq!(ms_to_hops(5000.0), 1875); // hang
    }
}
