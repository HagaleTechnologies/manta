//! SPEC-decode-core.md §4.6 per-callsign confidence, plus the cty/SCP
//! adjustment the spec explicitly defers to this crate (ARCHITECTURE §6.3).

/// SPEC §4.6: geometric mean of per-character confidences times a
/// repetition factor (`r=1 -> 0.5`, `r=2 -> 0.75`, `r=3 -> 0.875`, ...).
///
/// `c_call = (prod cᵢ)^(1/n) * (1 - 0.5^r)`
pub fn c_call(char_confidences: &[f32], reps: u32) -> f32 {
    assert!(
        !char_confidences.is_empty(),
        "a callsign has at least one character"
    );
    let n = char_confidences.len() as f32;
    let log_sum: f32 = char_confidences
        .iter()
        .map(|c| c.max(f32::EPSILON).ln())
        .sum();
    let geo_mean = (log_sum / n).exp();
    let rep_factor = 1.0 - 0.5f32.powi(reps as i32);
    geo_mean * rep_factor
}

/// SCP membership: multiplicative boost capped at 1.0. Absence is neutral
/// -- ARCHITECTURE §6.3: "absence only lowers it [relatively, by not
/// getting the boost], never gates."
pub fn apply_scp_boost(c: f32, in_scp: bool) -> f32 {
    if in_scp {
        (c * 1.15).min(1.0)
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn rep_factor_matches_spec_examples() {
        assert_relative_eq!(c_call(&[1.0], 1), 0.5, epsilon = 1e-6);
        assert_relative_eq!(c_call(&[1.0], 2), 0.75, epsilon = 1e-6);
        assert_relative_eq!(c_call(&[1.0], 3), 0.875, epsilon = 1e-6);
    }

    #[test]
    fn one_low_confidence_character_tanks_the_geometric_mean() {
        let high = c_call(&[1.0, 1.0, 1.0], 3);
        let one_low = c_call(&[1.0, 1.0, 0.1], 3);
        assert!(one_low < high);
    }

    #[test]
    fn scp_boost_raises_confidence_but_never_gates() {
        assert!(apply_scp_boost(0.5, true) > 0.5);
        assert_eq!(apply_scp_boost(0.5, false), 0.5);
    }

    #[test]
    fn scp_boost_is_capped_at_one() {
        assert_eq!(apply_scp_boost(0.95, true), 1.0);
    }
}
