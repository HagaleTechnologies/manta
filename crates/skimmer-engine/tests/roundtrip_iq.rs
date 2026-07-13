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
    #[test]
    fn iq_roundtrip_with_noise(
        text in text_strategy(),
        wpm in 10.0f32..=40.0,
        snr in 15.0f32..=30.0,
        offset_khz in -40i32..=40,
        noise_seed in any::<u64>(),
    ) {
        let fs = 96_000.0;
        // Scene long enough for the whole text + flush tail.
        let (probe_env, _) = key_text(&text, &KeyerSpec::new(wpm), fs).unwrap();
        let duration_s = probe_env.len() as f64 / fs + 1.5;
        let sig = SignalSpec {
            text: text.clone(),
            loop_text: false,
            wpm,
            offset_hz: offset_khz as f64 * 1000.0,
            snr_2500_db: snr,
            jitter: None,
        };
        let (iq, texts) = render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
        let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
        prop_assert_eq!(
            cer(&texts[0], &report.text), 0.0,
            "wpm {} snr {} offset {} kHz: keyed {:?} decoded {:?}",
            wpm, snr, offset_khz, texts[0], report.text
        );
        // V1's frequency criterion, generalized: within 10 Hz.
        prop_assert!((report.freq_hz - offset_khz as f64 * 1000.0).abs() <= 10.0);
    }
}
