//! Placeholder detector (design doc §2): a deliberately minimal stand-in
//! for SPEC §2's real order-statistic detector + track manager (a later M2
//! sub-project). Picks the loudest channel once, over a fixed calibration
//! window, and never re-evaluates -- exactly enough to keep M0/M1's
//! single-track decode path working through the new channelizer.

use num_complex::Complex32;
use skimmer_dsp::channelizer::Channelizer;

/// Run `ch` over `calib_iq`, return the channel index with the highest
/// average power across the resulting hops, or `None` if no hops were
/// produced (calibration window shorter than one filter length).
/// Temporary: no caller yet until Tasks 6/7 wire this into decode_samples/decode_wav and listen.
#[allow(dead_code)]
pub(crate) fn calibrate_channel(ch: &mut Channelizer, calib_iq: &[Complex32]) -> Option<usize> {
    let hops = ch.process(calib_iq);
    if hops.is_empty() {
        return None;
    }
    let n = ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in &hops {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    let mut k0 = 0;
    for (k, &p) in avg_power.iter().enumerate() {
        if p > avg_power[k0] {
            k0 = k;
        }
    }
    Some(k0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn finds_the_loudest_channel() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 96_000, 1.0, FS); // 1 s calibration window
        let k0 = calibrate_channel(&mut ch, &iq).unwrap();
        assert_eq!(k0, 64); // 6000 / 93.75 = 64 exactly
    }

    #[test]
    fn none_on_too_short_input() {
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let iq = tone(6_000.0, 10, 1.0, FS); // far shorter than one filter length
        assert!(calibrate_channel(&mut ch, &iq).is_none());
    }
}
