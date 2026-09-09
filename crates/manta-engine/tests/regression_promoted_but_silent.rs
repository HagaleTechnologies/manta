//! MAN-3 review round 3: `decode_samples`' zero-event error must describe
//! what actually happened, not send the operator back to the detector
//! thresholds in a case those thresholds had nothing to do with.
//!
//! Two ways to reach an empty event stream, and they have *different*
//! causes (see `decode_samples`' `events.is_empty()` branch):
//!
//! 1. No track ever promoted -- genuinely below `detector.on_snr_db`, or
//!    shorter than SPEC §2.1's warmup+confirm floor. "No signal found."
//! 2. A track *did* promote (strong, above threshold, longer than the
//!    floor) but closed before clearing the decoder's own output latency:
//!    the 1 Hz `TrackMeta` cadence, or SPEC §5's mark quorum for a first
//!    character. A signal was found; nothing came out of the decoder yet.
//!
//! Case 2 is reachable with an ordinary strong carrier a few hundred ms
//! longer than the ~2.05 s warmup+confirm floor -- the exact input the
//! review flagged -- and it must not be reported as case 1.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::noise::{add_unit_awgn, amplitude_for_snr_2500};
use num_complex::Complex32;

const FS: f64 = 96_000.0;

/// An unmodulated carrier at `offset_hz`, `duration_s` long, at
/// `snr_2500_db` over unit AWGN -- keyed on with the keyer's own 5 ms
/// raised-cosine rise so the switch-on transient doesn't splatter across
/// the passband.
fn carrier_scene(duration_s: f64, offset_hz: f64, snr_2500_db: f32, seed: u64) -> Vec<Complex32> {
    let n = (duration_s * FS).round() as usize;
    let amp = amplitude_for_snr_2500(snr_2500_db, FS);
    let rise = (0.005 * FS).round() as usize;
    let dphi = std::f64::consts::TAU * offset_hz / FS;
    let mut iq = Vec::with_capacity(n);
    let mut phi = 0.0f64;
    for i in 0..n {
        let env = if i < rise {
            let x = i as f64 / rise as f64;
            (0.5 - 0.5 * (std::f64::consts::PI * x).cos()) as f32
        } else {
            1.0
        };
        let (s, c) = phi.sin_cos();
        iq.push(Complex32::new(amp * env * c as f32, amp * env * s as f32));
        phi += dphi;
        if phi > std::f64::consts::PI {
            phi -= std::f64::consts::TAU;
        } else if phi < -std::f64::consts::PI {
            phi += std::f64::consts::TAU;
        }
    }
    add_unit_awgn(&mut iq, seed);
    iq
}

/// A 2.5 s carrier: 2.0 s warmup + 19 confirm hops leaves it promoted at
/// ~2.05 s, then only ~0.45 s of decoding -- short of the 1 s `TrackMeta`
/// cadence, and a continuous key-down never closes a character. Empty
/// event stream, but the detector did its job.
#[test]
fn promoted_but_silent_input_is_not_reported_as_a_detector_threshold_miss() {
    let iq = carrier_scene(2.5, 8_000.0, 30.0, 0xB0_1E_5E_ED);
    let err = decode_samples(&iq, FS, 0.0, &PipelineConfig::default())
        .expect_err("a 2.5 s carrier emits no events: too short for TrackMeta or a character");
    let msg = err.to_string();
    assert!(
        msg.contains("track(s) promoted") && msg.contains("decoder-output latency"),
        "a promoted-but-silent input must be reported as decoder-output latency, got: {msg}"
    );
    assert!(
        !msg.contains("no track was ever promoted"),
        "a track *was* promoted here; the detector-threshold message is wrong: {msg}"
    );
    assert!(
        !msg.starts_with("no signal found"),
        "a signal was found -- only the decoder had nothing to say yet: {msg}"
    );
}

/// The control: with nothing but noise no track ever promotes, and *that*
/// is the case the detector-threshold message exists for.
#[test]
fn noise_only_input_is_reported_as_a_detector_outcome() {
    let mut iq = vec![Complex32::new(0.0, 0.0); (5.0 * FS) as usize];
    add_unit_awgn(&mut iq, 0x1234_5678_9ABC_DEF0);
    let err = decode_samples(&iq, FS, 0.0, &PipelineConfig::default())
        .expect_err("noise-only input promotes no track and decodes nothing");
    let msg = err.to_string();
    assert!(
        msg.contains("no track was ever promoted") && msg.contains("detector.on_snr_db"),
        "noise-only input must be reported as a detector outcome, got: {msg}"
    );
}
