//! MAN-7 diagnostic: how a signal's fractional position inside its channelizer
//! channel affects Demod's measured mark and inter-element-gap durations.
//!
//! Payload "H5" is dit-only (H = ...., 5 = .....), so every mark is a dit and
//! every intra-character gap is exactly one dit -- no dit/dah separation needed.
//! The sweep uses POSITIVE fractional offsets so the contested channel pair is
//! always {k0, k0+1}; V2's real residual is negative (-0.4667), which is the
//! mirror image by construction (the prototype filter is symmetric,
//! manta-dsp/src/proto.rs's `prototype_is_symmetric_and_unity_dc` test).

use manta_decode::envelope::{Demod, DemodConfig, Run};
use manta_decode::HOP_MS;
use manta_dsp::channelizer::Channelizer;
use manta_testkit::scene::{render_scene, SignalSpec};

const FS: f64 = 96_000.0;
const DELTA_HZ: f64 = 93.75;
const K0: usize = 64; // 6000 Hz: the on-center control the ticket used
const WPM: f32 = 35.0;
const SNR_DB: f32 = 20.0;
const DURATION_S: f64 = 30.0;
const NOISE_SEED: u64 = 0x4D41_4E37; // "MAN7"
const WARMUP_S: f64 = 5.0; // channelizer lead-in + Demod's 375-hop init + rails

/// Mean measured (dit_ms, inter_element_gap_ms) for a tone `frac` channels
/// above channel K0's center. `per_hop_argmax` mirrors SPEC §2.5 /
/// `Track::select_channel`: reselect the max-power channel among
/// {K0-1, K0, K0+1} every hop. When false, one fixed channel (the
/// highest-mean-power one) is used for the whole scene, which isolates the
/// filter-shape mechanism from the channel-selection mechanism.
fn measure(frac: f64, per_hop_argmax: bool) -> (f32, f32) {
    let sig = SignalSpec {
        text: "H5".into(),
        loop_text: true,
        wpm: WPM,
        offset_hz: (K0 as f64 + frac) * DELTA_HZ,
        snr_2500_db: SNR_DB,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, _) =
        render_scene(std::slice::from_ref(&sig), FS, DURATION_S, Some(NOISE_SEED)).unwrap();
    let mut ch = Channelizer::new(FS, 0.0).unwrap();
    let hops = ch.process(&iq);
    let owned = [K0 - 1, K0, K0 + 1];

    // Fixed-channel arm: the channel with the largest mean power over the scene.
    let fixed = *owned
        .iter()
        .max_by(|&&a, &&b| {
            let m = |k: usize| hops.iter().map(|h| h.power[k] as f64).sum::<f64>();
            m(a).partial_cmp(&m(b)).unwrap()
        })
        .unwrap();

    let mut demod = Demod::new(DemodConfig::default());
    let mut runs: Vec<Run> = Vec::new();
    for (m, hop) in hops.iter().enumerate() {
        let k = if per_hop_argmax {
            *owned
                .iter()
                .max_by(|&&a, &&b| hop.power[a].partial_cmp(&hop.power[b]).unwrap())
                .unwrap()
        } else {
            fixed
        };
        runs.extend(demod.push(hop.power[k].sqrt(), (m * ch.hop()) as u64));
    }
    runs.extend(demod.finish());

    // Discard warmup: everything before WARMUP_S of channel-output time.
    let warm_hops = (WARMUP_S * 1000.0 / HOP_MS) as u64;
    let steady: Vec<&Run> = runs
        .iter()
        .filter(|r| r.start_ts >= warm_hops * ch.hop() as u64)
        .collect();
    assert!(
        steady.len() > 200,
        "too few steady-state runs: {}",
        steady.len()
    );

    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let marks: Vec<f32> = steady
        .iter()
        .filter(|r| r.mark)
        .map(|r| r.hops as f32 * HOP_MS as f32)
        .collect();
    let mean_mark = mean(&marks);
    // Element gaps are 1 dit; the next-shortest gap (inter-character) is 3 dits.
    // A 2x-mean-mark cut separates them with wide margin at any plausible delta.
    let egaps: Vec<f32> = steady
        .iter()
        .filter(|r| !r.mark)
        .map(|r| r.hops as f32 * HOP_MS as f32)
        .filter(|&g| g < 2.0 * mean_mark)
        .collect();
    assert!(egaps.len() > 100, "too few element gaps: {}", egaps.len());
    (mean_mark, mean(&egaps))
}

/// The invariant MAN-7's fix rests on: Demod's threshold crossings only *move
/// the boundary* between a mark and the gap that follows it -- they do not
/// create or destroy time. So mark + element gap must sum to two true dit
/// periods at every fractional offset, even where the mark alone is wildly
/// inflated.
#[test]
fn mark_and_element_gap_stay_complementary_across_channel_offsets() {
    let true_dit_ms = 1200.0 / WPM; // 34.286 ms at 35 WPM
    let want = 2.0 * true_dit_ms;
    for &frac in &[0.0, 0.25, 0.4667, 0.5] {
        for &argmax in &[false, true] {
            let (mark, egap) = measure(frac, argmax);
            let sum = mark + egap;
            assert!(
                (sum - want).abs() <= 0.05 * want,
                "frac {frac} argmax {argmax}: mark {mark:.2} + egap {egap:.2} = {sum:.2} ms, \
                 want {want:.2} ms (+-5 %)"
            );
        }
    }
}

/// The finding itself, as a measurement: mark overshoot is small on channel
/// center and several times larger near the edge, which is the whole of the
/// reported WPM gap.
#[test]
fn mark_overshoot_grows_toward_the_channel_edge() {
    let true_dit_ms = 1200.0 / WPM;
    let overshoot = |frac: f64| measure(frac, true).0 - true_dit_ms;
    let center = overshoot(0.0);
    let edge = overshoot(0.4667);
    assert!(
        center >= 0.0,
        "on-center overshoot must not be negative: {center:.2} ms"
    );
    assert!(
        edge > 2.0 * center.max(0.5),
        "expected near-edge overshoot to dominate on-center: {center:.2} ms vs {edge:.2} ms"
    );
}
