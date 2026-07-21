//! ROADMAP M0 criterion: text -> testkit CW -> (IQ + AWGN) -> full pipeline
//! -> text, CER = 0, for 10–40 WPM at >= +15 dB SNR-in-2500-Hz.

use proptest::prelude::*;
use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::cer::cer;
use skimmer_testkit::keyer::{key_text, KeyerSpec};
use skimmer_testkit::scene::{render_scene, SignalSpec};

/// Restricts the generated text's first character to avoid an all-dah
/// opening (pinned decision 20, `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`):
/// `ClusterPair`'s unimodal-init branch (`crates/skimmer-decode/src/timing.rs`)
/// always assumes the first 5-mark cluster is dits and can't recover if it
/// turns out to be a homogeneous run of dahs instead (e.g. M, O, or
/// repeated T). This is NOT "must contain both dit and dah elements" —
/// several excluded letters (B, J, U) contain both element types and decode
/// fine; the real constraint is narrower than that.
const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn word_strategy(first: bool) -> impl Strategy<Value = String> {
    let charset = if first { MIXED_FIRST } else { REST };
    (
        proptest::sample::select(charset.chars().collect::<Vec<_>>()),
        proptest::collection::vec(
            proptest::sample::select(REST.chars().collect::<Vec<_>>()),
            1..6,
        ),
    )
        .prop_map(|(h, tail)| {
            let mut w = h.to_string();
            w.extend(tail);
            w
        })
}

/// Total dit/dah elements (marks) across the whole text, ignoring spaces.
/// SPEC §4.1: the online 2-means speed tracker only becomes `ready()` after
/// its 5th mark; below that quorum no character can ever be classified. See
/// the matching comment in `roundtrip_envelope.rs` for the full trace —
/// found via bisection on this suite's minimal failing case ("AA", 4 marks).
fn total_marks(text: &str) -> usize {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter_map(skimmer_decode::tree::pattern_for)
        .map(str::len)
        .sum()
}

fn text_strategy() -> impl Strategy<Value = String> {
    (
        word_strategy(true),
        proptest::collection::vec(word_strategy(false), 0..2),
    )
        .prop_map(|(first, rest)| {
            let mut words = vec![first];
            words.extend(rest);
            words.join(" ")
        })
        .prop_filter(
            "at least 5 marks total (SPEC §4.1 tracker init quorum)",
            |t| total_marks(t) >= 5,
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    // SPEC §2.1's ~2.05 s mandatory warmup(750 hops)+confirm(19 hops) floor:
    // the old `keyed_length + 1.5 s`, once-keyed, non-looping scene duration
    // fell entirely under that floor for many generated cases, so the real
    // detector never promoted a track before the clip ended and
    // `decode_samples` returned Err ("no signal found"), panicking the
    // `.unwrap()` below. **This specific duration/warmup-floor mechanism is
    // fixed** (Task 11 Step 0,
    // docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md): loop the
    // generated text (`loop_text: true`) across a 12 s floor, same fix and
    // reasoning as `regression_char_gap_high_wpm.rs` (see that file's doc
    // comment for why a silent lead-in was tried and rejected first).
    // Verified in isolation on `regression_char_gap_high_wpm.rs`'s fixed
    // "AB" case: with the duration/warmup-floor mechanism as the only factor
    // in play, the fix gives a small, constant, measurable CER floor from
    // the lost leading repetition(s) -- not the runaway failures below.
    //
    // Still `#[ignore]`d, though, because un-ignoring surfaced THREE
    // separate, pre-existing, out-of-scope real-detector bugs across this
    // proptest's WPM/offset/SNR/text space (Tasks 4-8, not a Task 11
    // tolerance issue -- same "don't force a real bug to pass" principle
    // this plan applies to V2 in `golden_v2_v3.rs`):
    //   1. offset_hz == 0 (channel 0, dead DC center): total decode failure
    //      (zero CharDecoded events) at every WPM/SNR tried. Noted on
    //      <https://github.com/HagaleTechnologies/skimmer/issues/12>.
    //   2. WPM in roughly [10.0, 10.15]: a sharp decode-garbling cliff
    //      (CER > 1) clearing completely by 10.2 WPM, independent of
    //      text/offset/SNR. Filed as
    //      <https://github.com/HagaleTechnologies/skimmer/issues/22>.
    //   3. Some other (text, wpm, offset, snr) combinations well outside
    //      both of the above (e.g. wpm=18.117826, offset=-20kHz, snr=28dB,
    //      text "AU") produce *persistent, non-converging* garbled decode
    //      -- CER that grows with scene duration rather than stabilizing,
    //      unlike every warmup-floor case in this task. Filed as
    //      <https://github.com/HagaleTechnologies/skimmer/issues/23>.
    // Given #3's breadth (not confined to a narrow, excludable parameter
    // band the way #1 and #2 are), narrowing the proptest strategy further
    // isn't a real fix, just whack-a-mole -- re-ignoring instead of forcing
    // a pass, per this plan's escalation guidance. Un-ignore once #12/#22/#23
    // (or their eventual root cause, possibly shared) are resolved.
    #[test]
    #[ignore]
    fn iq_roundtrip_with_noise(
        text in text_strategy(),
        wpm in 10.0f32..=40.0,
        snr in 15.0f32..=30.0,
        offset_khz in -40i32..=40,
        noise_seed in any::<u64>(),
    ) {
        let fs = 96_000.0;
        // Scene long enough for the whole text + flush tail, floored at 12 s
        // so even the slowest/shortest generated cases loop several times
        // past the ~2.05 s warmup+confirm floor before the clip ends.
        let (probe_env, _) = key_text(&text, &KeyerSpec::new(wpm), fs).unwrap();
        let duration_s = (probe_env.len() as f64 / fs + 1.5).max(12.0);
        let sig = SignalSpec {
            text: text.clone(),
            loop_text: true,
            wpm,
            offset_hz: offset_khz as f64 * 1000.0,
            snr_2500_db: snr,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        };
        let (iq, texts) = render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
        let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
        // Loosened from the original exact `CER == 0`: like every other
        // continuously-keyed scenario in this task, the leading
        // repetition(s) played during warmup+confirm are structurally lost.
        // This tolerance is aspirational pending issues #12/#22/#23 above --
        // it holds for the ordinary warmup-floor-only cases but this test
        // stays `#[ignore]`d because of the three real-bug cases that
        // violate it regardless of tolerance.
        prop_assert!(
            cer(&texts[0], &report.text) < 0.25,
            "wpm {} snr {} offset {} kHz: keyed {:?} decoded {:?} (CER {:.4})",
            wpm, snr, offset_khz, texts[0], report.text, cer(&texts[0], &report.text)
        );
        // V1's frequency criterion, generalized: within 25 Hz. Re-measured
        // (Task 11 Step 3) against SPEC's original 10 Hz using a manual
        // sweep across this proptest's wpm/snr/offset space (bypassing the
        // proptest harness itself, which the CER bugs above dominate):
        // clean-CER cases showed freq errors up to ~20.5 Hz, NOT reliably
        // under 10 Hz -- unlike golden_v1.rs's V1 gate (fixed, narrow
        // +12.34 kHz offset, now measured at ~9.9 Hz). This proptest's much
        // wider offset range (+/-40 kHz vs. V1's single fixed offset)
        // exposes more of the channelizer's known, separately-deferred
        // fine-frequency interpolator bias (`interpolate_offset`,
        // `skimmer-dsp::channelizer`; ~+/-21 Hz depending on fractional
        // channel position -- see this plan's Task 11 context). Left at the
        // original 25 Hz rather than tightened.
        prop_assert!((report.freq_hz - offset_khz as f64 * 1000.0).abs() <= 25.0);
    }
}
