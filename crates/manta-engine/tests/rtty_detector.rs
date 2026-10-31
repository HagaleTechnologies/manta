//! MAN-314: `manta_dsp::fsk::FskDetector` driven by a real channelizer and
//! noise floor over `manta_testkit` scenes (SPEC §2.6). Lives here, not in
//! manta-dsp, because manta-testkit depends on manta-dsp.

use manta_dsp::channelizer::{power_db, Channelizer};
use manta_dsp::floor::FloorBank;
use manta_dsp::fsk::{FskDetector, FskSignal};
use manta_testkit::fsk::{add_fsk, FskSpec};
use manta_testkit::scene::render_scene;
use num_complex::Complex32;

const FS: f64 = 96_000.0;

fn detect(iq: &[Complex32], fs: f64) -> (Vec<FskSignal>, Vec<bool>) {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let n = ch.n_channels();
    let mut floor = FloorBank::new(n);
    let mut det = FskDetector::new(n, 20);
    for chunk in iq.chunks(4096) {
        for h in ch.process(chunk) {
            let p: Vec<f64> = h.power.iter().map(|&x| power_db(x)).collect();
            floor.update(&p);
            det.push_hop(&h, &p, &floor);
        }
    }
    let hard = (0..n).map(|k| det.in_hard_span(k)).collect();
    (det.signals().to_vec(), hard)
}

fn rtty(mark_offset_hz: f64, shift_hz: f64, snr: f32) -> FskSpec {
    FskSpec {
        mark_below_space: false,
        mark_offset_hz,
        shift_hz,
        baud: if shift_hz < 300.0 { 45.45 } else { 50.0 },
        snr_2500_db: snr,
        text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
        start_s: 0.0,
        watterson: None,
        seed: 9,
    }
}

#[test]
fn each_standard_shift_marks_with_correct_nominal() {
    // (shift, measured-shift range from the MAN-314 sweep at 0 dB)
    let cases = [
        (170.0, 142.5, 181.5),
        (200.0, 183.0, 220.5),
        (425.0, 406.0, 441.8),
        (450.0, 426.7, 466.2),
        (850.0, 839.6, 876.6),
    ];
    let (mut iq, _) = render_scene(&[], FS, 25.0, Some(3)).unwrap();
    for (i, &(shift, _, _)) in cases.iter().enumerate() {
        add_fsk(&mut iq, FS, &rtty(3_000.0 * (i as f64 + 1.0), shift, 10.0));
    }
    let (signals, _) = detect(&iq, FS);
    assert_eq!(signals.len(), 5, "{signals:#?}");
    for (s, &(shift, lo, hi)) in signals.iter().zip(cases.iter()) {
        assert_eq!(s.nominal_shift_hz, shift, "{s:?}");
        assert!(
            (lo..=hi).contains(&s.measured_shift_hz),
            "{shift} Hz shift measured {}",
            s.measured_shift_hz
        );
    }
}

#[test]
fn golden_cw_scenes_never_mark() {
    for base in [manta_testkit::vectors::v8(), manta_testkit::vectors::v8w()] {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 60.0,
            ..base
        };
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let (signals, hard) = detect(&rendered.samples, spec.fs);
        assert!(signals.is_empty(), "{}: {signals:?}", spec.name);
        assert!(hard.iter().all(|h| !h));
    }
}

#[test]
fn deterministic_across_runs() {
    let (mut iq, _) = render_scene(&[], FS, 22.0, Some(4)).unwrap();
    add_fsk(&mut iq, FS, &rtty(5_000.0, 170.0, 20.0));
    let a = detect(&iq, FS);
    let b = detect(&iq, FS);
    assert_eq!(a.0.len(), 1);
    assert_eq!(a, b);
}
