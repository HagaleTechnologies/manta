//! MAN-206: the receive-side analytic conversion (`AudioIqSource`, the
//! 511-tap Hilbert FIR at 48 kHz) checked against the independent
//! generator. Not part of the generator gate: it feeds `Re(z)` of an
//! independent keyed tone through `AudioIqSource` and compares the result
//! with `z` delayed by the FIR's group delay, inside MAN-273's
//! [600, 23400] Hz contract. An image of -70 dBc bounds the power error
//! near 0.003 dB.

use manta_dsp::hilbert::HILBERT_MIN_IMAGE_REJECTION_DB;
use manta_input::{AudioIqSource, IqSource};
use manta_testkit::crosscheck;
use manta_testkit::measure;
use manta_testkit::scene::SignalSpec;
use num_complex::Complex32;

const FS: f64 = 48_000.0;
const DELAY: usize = 255;

fn sig(offset_hz: f64) -> SignalSpec {
    SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    }
}

fn through_audio_source(real: Vec<f32>) -> Vec<Complex32> {
    let n = real.len();
    let mut src =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap();
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
    loop {
        let got = src.read(&mut buf).unwrap();
        if got == 0 {
            break;
        }
        out.extend_from_slice(&buf[..got]);
    }
    out
}

#[test]
fn audio_iq_source_matches_the_independent_analytic_tone() {
    let verified = crosscheck::verified().unwrap_or_else(|report| {
        panic!("generator cross-check failed, so the independent tone is not trusted:\n{report}")
    });
    for f in [750.0, 1500.0, 3000.0, 12_000.0, 20_000.0] {
        let (z, _) = crosscheck::independent_tone(verified, &sig(f), FS, 4.0).unwrap();
        let out = through_audio_source(z.iter().map(|c| c.re).collect());
        let lag = measure::group_delay_samples(&z, &out, 4 * DELAY);
        assert_eq!(lag, DELAY as i64, "{f} Hz: group delay");
        let n = out.len().min(z.len()) - DELAY;
        let p = &out[DELAY..DELAY + n];
        let q = &z[..n];
        let image = measure::image_dbc(p, f, FS, 100.0);
        assert!(
            image <= -HILBERT_MIN_IMAGE_REJECTION_DB,
            "{f} Hz: image {image:.1} dBc"
        );
        let power_db = 10.0 * (measure::total_power(p) / measure::total_power(q)).log10();
        assert!(power_db.abs() <= 0.01, "{f} Hz: power {power_db:.4} dB");
        let mag = |x: &[Complex32]| -> Vec<f64> {
            let a = x.iter().map(|c| c.norm() as f64).fold(0.0f64, f64::max);
            x.iter().map(|c| c.norm() as f64 / a).collect()
        };
        let rp = measure::on_runs(&mag(p), crosscheck::ON_RUN_FRACTION);
        let rq = measure::on_runs(&mag(q), crosscheck::ON_RUN_FRACTION);
        assert_eq!(rp.len(), rq.len(), "{f} Hz: on-run count");
        for (a, b) in rp.iter().zip(&rq) {
            assert!(
                a.0.abs_diff(b.0) <= 1 && a.1.abs_diff(b.1) <= 1,
                "{f} Hz: run {a:?} vs {b:?}"
            );
        }
        eprintln!(
            "{f} Hz: image {image:.1} dBc, power {power_db:.5} dB, {} runs",
            rp.len()
        );
    }
}
