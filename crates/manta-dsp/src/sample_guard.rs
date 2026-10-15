//! Input sample guard (MAN-234, SPEC §1.3 step 0): an input sample whose
//! `re` or `im` is NaN, infinite or larger in magnitude than
//! `MAX_SAMPLE_MAGNITUDE` is replaced with silence (`0+0j`) and counted,
//! so no input value can panic the pipeline or latch its floor and gate
//! averages. A usable sample is never written, so clean input stays
//! bit-identical.

use num_complex::Complex32;

/// Largest `|re|` or `|im|` an input sample may have. The prototype filter's
/// absolute tap sum is below 1.34 for every supported N, so a window of
/// samples within this bound gives channel power below 3.6e34, four decades
/// under `f32::MAX`; every source manta reads delivers full scale at 1.0.
pub const MAX_SAMPLE_MAGNITUDE: f32 = 1.0e17;

/// Whether both components are within `±MAX_SAMPLE_MAGNITUDE`. NaN compares
/// false, so it is never usable.
#[inline]
pub fn is_usable(s: Complex32) -> bool {
    s.re.abs() <= MAX_SAMPLE_MAGNITUDE && s.im.abs() <= MAX_SAMPLE_MAGNITUDE
}

/// Replace every unusable sample with `0+0j`; returns how many were
/// replaced. A slice of usable samples is read once and not written.
pub fn replace_unusable(samples: &mut [Complex32]) -> u64 {
    if samples.iter().all(|&s| is_usable(s)) {
        return 0;
    }
    let mut replaced = 0;
    for s in samples.iter_mut().filter(|s| !is_usable(**s)) {
        *s = Complex32::new(0.0, 0.0);
        replaced += 1;
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_and_infinite_components_are_unusable() {
        let bad = [
            Complex32::new(f32::NAN, 0.0),
            Complex32::new(0.0, f32::NAN),
            Complex32::new(f32::INFINITY, 0.0),
            Complex32::new(0.0, f32::INFINITY),
            Complex32::new(f32::NEG_INFINITY, 0.0),
            Complex32::new(0.0, f32::NEG_INFINITY),
            Complex32::new(f32::MAX, 0.0),
            Complex32::new(0.0, -f32::MAX),
        ];
        for s in bad {
            assert!(!is_usable(s), "{s:?} must be unusable");
        }
        let good = [
            Complex32::new(0.0, 0.0),
            Complex32::new(-0.0, -0.0),
            Complex32::new(f32::MIN_POSITIVE / 2.0, -f32::MIN_POSITIVE / 2.0),
            Complex32::new(1.0, -1.0),
            Complex32::new(1.0e16, -1.0e16),
            Complex32::new(MAX_SAMPLE_MAGNITUDE, -MAX_SAMPLE_MAGNITUDE),
            Complex32::new(-MAX_SAMPLE_MAGNITUDE, MAX_SAMPLE_MAGNITUDE),
        ];
        for s in good {
            assert!(is_usable(s), "{s:?} must be usable");
        }
        let next_up = f32::from_bits(MAX_SAMPLE_MAGNITUDE.to_bits() + 1);
        assert!(!is_usable(Complex32::new(next_up, 0.0)));
        assert!(!is_usable(Complex32::new(0.0, -next_up)));
    }

    #[test]
    fn replace_unusable_zeroes_and_counts_each_bad_sample_once() {
        let mut s = vec![
            Complex32::new(0.25, -0.5),
            Complex32::new(f32::NAN, 0.1),
            Complex32::new(-0.0, 1.0e-40),
            Complex32::new(0.3, f32::NEG_INFINITY),
            Complex32::new(f32::INFINITY, f32::NAN),
            Complex32::new(-0.75, 0.125),
        ];
        let before = s.clone();
        assert_eq!(replace_unusable(&mut s), 3);
        for i in [1, 3, 4] {
            assert_eq!(s[i].re.to_bits(), 0.0f32.to_bits());
            assert_eq!(s[i].im.to_bits(), 0.0f32.to_bits());
        }
        for i in [0, 2, 5] {
            assert_eq!(s[i].re.to_bits(), before[i].re.to_bits());
            assert_eq!(s[i].im.to_bits(), before[i].im.to_bits());
        }
    }

    #[test]
    fn replace_unusable_never_writes_a_usable_slice() {
        let mut s: Vec<Complex32> = (0..4096)
            .map(|i| match i % 5 {
                0 => Complex32::new(-0.0, 0.0),
                1 => Complex32::new(f32::MIN_POSITIVE / 4.0, -f32::MIN_POSITIVE / 8.0),
                2 => Complex32::new((i as f32 * 0.37).sin(), (i as f32 * 0.11).cos()),
                3 => Complex32::new(-MAX_SAMPLE_MAGNITUDE, MAX_SAMPLE_MAGNITUDE),
                _ => Complex32::new(i as f32 * 1.0e9, -(i as f32)),
            })
            .collect();
        let before = s.clone();
        assert_eq!(replace_unusable(&mut s), 0);
        for (a, b) in s.iter().zip(before.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn replace_unusable_of_empty_slice_is_zero() {
        assert_eq!(replace_unusable(&mut []), 0);
    }
}
