//! `CHAR_GAP_DITS` re-derivation sweep (MAN-103, Phase 4 step 3).
//!
//! `docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md` chose
//! `CHAR_GAP_DITS = 1.6` over SPEC §4.2's nominal `2.0` by counting failures
//! over 500 randomly generated scenes, twice, under two independent noise
//! seeds. MAN-103 moves the constant back to `2.0` because the mark-duration
//! inflation that deviation compensated for is fixed at its source, so the
//! same measurement has to be taken again against the corrected timing --
//! otherwise `2.0` is only an argument, not a result. This harness is that
//! measurement, kept in the tree so the next person to touch the constant can
//! re-run it instead of rebuilding it.
//!
//! `#[ignore]`d: one sweep is ~500 full pipeline decodes (several minutes),
//! far too slow for the ordinary `cargo test` gate. It is a measuring
//! instrument, not a regression gate -- the gates for this behaviour are
//! `regression_char_gap_high_wpm.rs`, `wpm_across_channel.rs` and
//! `roundtrip_envelope.rs`.
//!
//! Run one sweep with:
//!
//! ```text
//! cargo test --release -p manta-engine --test char_gap_sweep -- --ignored --nocapture
//! ```
//!
//! `CHAR_GAP_DITS` is a private constant, so sweeping *values* means editing
//! `crates/manta-decode/src/timing.rs` and re-running this harness once per
//! candidate -- exactly how the 2026-07-18 table was produced.

use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_decode::tree::pattern_for;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::keyer::{key_text, KeyerSpec};
use manta_testkit::scene::{render_scene, SignalSpec};

const CASES: usize = 500;
/// The two independent stream seeds the 2026-07-18 sweep used, in spirit:
/// one full 500-case sweep each, so a difference has to reproduce twice.
const STREAM_SEEDS: [u64; 2] = [0x00A1_2026_0718, 0x00B2_2026_0907];
/// Same failure criterion `roundtrip_iq.rs`'s proptest applies.
const CER_FAIL: f64 = 0.25;

const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// SplitMix64 -- deterministic, dependency-free, and reproducible from the
/// seed alone (proptest's own RNG is not addressable from outside a
/// `proptest!` block).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    /// Uniform in `[lo, hi]`.
    fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + (hi - lo) * u as f32
    }
}

fn total_marks(text: &str) -> usize {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter_map(pattern_for)
        .map(str::len)
        .sum()
}

/// Same generated space as `roundtrip_iq.rs`'s `text_strategy`: 1-3 words of
/// 2-6 characters, mixed first character, at least 5 marks total.
fn gen_text(rng: &mut Rng) -> String {
    loop {
        let words = 1 + rng.below(3);
        let mut out: Vec<String> = Vec::new();
        for w in 0..words {
            let charset: Vec<char> = if w == 0 { MIXED_FIRST } else { REST }.chars().collect();
            let rest: Vec<char> = REST.chars().collect();
            let mut s = String::new();
            s.push(charset[rng.below(charset.len())]);
            let tail = 1 + rng.below(5);
            for _ in 0..tail {
                s.push(rest[rng.below(rest.len())]);
            }
            out.push(s);
        }
        let text = out.join(" ");
        if total_marks(&text) >= 5 {
            return text;
        }
    }
}

/// One case: the same (text, wpm, snr, offset, noise seed) space the
/// 2026-07-18 sweep drew from, `jitter: None`.
fn run_case(rng: &mut Rng) -> Option<(String, f32, i32, f32, f64)> {
    let fs = 96_000.0;
    let text = gen_text(rng);
    let wpm = rng.range_f32(10.0, 40.0);
    let snr = rng.range_f32(15.0, 30.0);
    let offset_khz = rng.below(81) as i32 - 40;
    let noise_seed = rng.next_u64();

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
    let (iq, texts) =
        render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
    let cer_val = match decode_samples(&iq, fs, 0.0, &PipelineConfig::default()) {
        // "no signal found" counts as a failure, as it did in 2026-07-18's
        // sweep (the proptest's `.unwrap()` panicked on it).
        Err(_) => 1.0,
        Ok(report) => cer(&texts[0], &report.text),
    };
    (cer_val >= CER_FAIL).then_some((text, wpm, offset_khz, snr, cer_val))
}

/// Envelope-layer case: text -> keyed envelope (375 Hz) -> `TrackDecoder`,
/// the layer `CHAR_GAP_DITS` actually lives in. Same generated space as the
/// IQ sweep minus the front-end parameters (offset/SNR/noise seed), and the
/// criterion is exact: this chain decodes at CER 0 across the whole 10-40 WPM
/// range (`roundtrip_envelope.rs`), so any non-zero CER is a real gap- or
/// mark-classification failure rather than front-end noise.
fn run_case_envelope(rng: &mut Rng) -> Option<(String, f32, f64)> {
    let text = gen_text(rng);
    let wpm = rng.range_f32(10.0, 40.0);
    let (env, keyed) = key_text(&text, &KeyerSpec::new(wpm), 375.0).unwrap();
    let mut dec = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for (i, &a) in env.iter().enumerate() {
        events.extend(dec.push_envelope(a, i as u64 * 256));
    }
    let tail = (8.0 * 1200.0 / wpm * 0.375) as usize + 450;
    for i in 0..tail {
        events.extend(dec.push_envelope(0.0, (env.len() + i) as u64 * 256));
    }
    events.extend(dec.finish());
    let cer_val = cer(&keyed, &events_to_text(&events));
    (cer_val > 0.0).then_some((text, wpm, cer_val))
}

#[test]
#[ignore]
fn char_gap_dits_failure_sweep_envelope() {
    for seed in STREAM_SEEDS {
        let mut rng = Rng(seed);
        let mut failures = Vec::new();
        for _ in 0..CASES {
            if let Some(f) = run_case_envelope(&mut rng) {
                failures.push(f);
            }
        }
        println!(
            "envelope seed {seed:#x}: {} failures / {CASES}",
            failures.len()
        );
        for (text, wpm, cer_val) in &failures {
            println!("  FAIL text {text:?} wpm {wpm:.2} CER {cer_val:.4}");
        }
    }
}

#[test]
#[ignore]
fn char_gap_dits_failure_sweep_iq() {
    for seed in STREAM_SEEDS {
        let mut rng = Rng(seed);
        let mut failures = Vec::new();
        for _ in 0..CASES {
            if let Some(f) = run_case(&mut rng) {
                failures.push(f);
            }
        }
        println!("iq seed {seed:#x}: {} failures / {CASES}", failures.len());
        for (text, wpm, offset_khz, snr, cer_val) in &failures {
            println!(
                "  FAIL text {text:?} wpm {wpm:.2} offset {offset_khz} kHz snr {snr:.1} \
                 CER {cer_val:.4}"
            );
        }
    }
}
