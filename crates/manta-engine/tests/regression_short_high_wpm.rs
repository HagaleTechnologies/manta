//! MAN-271 (replaces MAN-3): a short, single-shot, high-WPM CW transmission
//! must decode to at least one character -- not close with zero events.
//!
//! Every test here drives the ticket's literal shape: the text keyed ONCE
//! (`loop_text: false`), then silence. MAN-3's original regression recipe
//! looped the text; once MAN-115 cut the warm-up floor that recipe stopped
//! failing for a reason unrelated to any fix, so a looped test can no longer
//! guard this behaviour (one looped guard is kept below anyway). The
//! measured history and both root causes -- the 5-mark speed-tracker quorum
//! at a genuine end-of-signal, and MAN-313's qualification gate never seeing
//! a burst this short -- are in
//! docs/DECISIONS/2026-10-16-man271-short-transmission-decode.md.

use manta_decode::events::DecoderEvent;
use manta_decode::tree::pattern_for;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

const FS: f64 = 96_000.0;

/// The ticket's four named repro tuples:
/// (text, wpm, snr_2500_db, offset_hz, noise seed).
const TUPLES: [(&str, f32, f32, f64, u64); 4] = [
    ("DA", 35.0, 28.3, 14_000.0, 63032404875482),
    ("VE", 39.1, 20.9, 1_000.0, 685126706563701970),
    ("Z5", 37.5, 29.9, -32_000.0, 10751217158967957828),
    ("D5", 34.1, 26.8, 19_000.0, 10012388600385395947),
];

/// One single-shot tuple rendered and decoded through the full pipeline.
fn decode_tuple(
    text: &str,
    wpm: f32,
    snr: f32,
    offset_hz: f64,
    seed: u64,
    duration_s: f64,
    cfg: &PipelineConfig,
) -> Result<(usize, String), String> {
    let sig = SignalSpec {
        text: text.into(),
        loop_text: false,
        wpm,
        offset_hz,
        snr_2500_db: snr,
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
    };
    let (iq, _) = render_scene(std::slice::from_ref(&sig), FS, duration_s, Some(seed)).unwrap();
    match decode_samples(&iq, FS, 0.0, cfg) {
        Ok(report) => Ok((
            report
                .events
                .iter()
                .filter(|e| matches!(e, DecoderEvent::CharDecoded { .. }))
                .count(),
            report.text,
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The Gherkin's literal case: every ticket tuple, keyed once, decodes to at
/// least one character with the default config. On main every one of these
/// returned `Err("no signal found (no keyed CW signal in the input)")`.
#[test]
fn ticket_tuples_single_shot_decode_at_least_one_character() {
    for &(text, wpm, snr, off, seed) in &TUPLES {
        let (chars, decoded) =
            decode_tuple(text, wpm, snr, off, seed, 3.0, &PipelineConfig::default())
                .unwrap_or_else(|e| panic!("{text}: decode failed: {e}"));
        assert!(chars >= 1, "{text}: {chars} CharDecoded, text {decoded:?}");
        assert!(!decoded.is_empty(), "{text}: empty text");
        // The first character is the known, out-of-scope promotion-time
        // corruption (see the decision record's known limits); the LAST
        // character is asserted exact.
        assert_eq!(
            decoded.chars().last(),
            text.chars().last(),
            "{text}: decoded {decoded:?}"
        );
    }
}

/// The same single-shot tuples across input durations: the decode must not
/// depend on how much silence follows the burst. Below ~1.1 s the decoder has
/// not yet seen Demod's 375-hop init window -- a pre-existing general limit,
/// out of scope (see the decision record). On main every duration errored.
#[test]
fn ticket_tuples_single_shot_decode_across_durations() {
    for &(text, wpm, snr, off, seed) in &TUPLES {
        for dur in [1.2, 1.5, 2.0, 6.0, 15.0] {
            let (chars, decoded) =
                decode_tuple(text, wpm, snr, off, seed, dur, &PipelineConfig::default())
                    .unwrap_or_else(|e| panic!("{text} @ {dur}s: decode failed: {e}"));
            assert!(
                chars >= 1,
                "{text} @ {dur}s: {chars} CharDecoded, text {decoded:?}"
            );
        }
    }
}

/// MAN-3's historical looped recipe (12 s, `loop_text: true`) still decodes
/// the keyed text verbatim -- the fix must not disturb looped traffic.
#[test]
fn ticket_tuples_looped_still_decode_fully() {
    for &(text, wpm, snr, off, seed) in &TUPLES {
        let sig = SignalSpec {
            text: text.into(),
            loop_text: true,
            wpm,
            offset_hz: off,
            snr_2500_db: snr,
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
        };
        let (iq, _) = render_scene(std::slice::from_ref(&sig), FS, 12.0, Some(seed)).unwrap();
        let report = decode_samples(&iq, FS, 0.0, &PipelineConfig::default()).unwrap();
        let chars = report
            .events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::CharDecoded { .. }))
            .count();
        assert!(chars >= 1, "{text}: {chars} CharDecoded");
        assert!(
            report.text.contains(text),
            "{text}: decoded {:?}",
            report.text
        );
    }
}

/// With `non_cw_filter` off, the decoder-side root cause is isolated from
/// the qualification gate: "DA" and "VE" (4 and 3 surviving marks) decoded
/// to zero characters on main because the speed tracker never reached its
/// 5-mark quorum. The finish-time bootstrap fixes exactly this.
#[test]
fn filter_off_single_shot_decodes_dit_heavy_short_texts() {
    let mut cfg = PipelineConfig::default();
    cfg.detector.non_cw_filter = false;
    for &(text, wpm, snr, off, seed) in &TUPLES {
        let (chars, decoded) = decode_tuple(text, wpm, snr, off, seed, 3.0, &cfg)
            .unwrap_or_else(|e| panic!("{text}: decode failed: {e}"));
        assert!(chars >= 1, "{text}: {chars} CharDecoded, text {decoded:?}");
    }
}

/// SplitMix64 -- deterministic, dependency-free, reproducible from the seed
/// alone (copied from `char_gap_sweep.rs`, which explains why).
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

/// A 2-character text from the Gherkin's domain: first character from the
/// proptest's mixed set, second alphanumeric, redrawn until at least 5
/// SPEC-table marks (the proptest filter the ticket's cases came from).
fn gen_text(rng: &mut Rng) -> String {
    const MIXED_FIRST: &str = "ACDFGKLNPQRVWXYZ";
    const REST: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let first: Vec<char> = MIXED_FIRST.chars().collect();
    let second: Vec<char> = REST.chars().collect();
    loop {
        let mut s = String::new();
        s.push(first[rng.below(first.len())]);
        s.push(second[rng.below(second.len())]);
        if total_marks(&s) >= 5 {
            return s;
        }
    }
}

/// One randomly drawn case from the Gherkin's domain. Returns
/// `(text, wpm, snr, offset_hz, seed)`; the draw order is exactly as listed.
fn draw_case(rng: &mut Rng) -> (String, f32, f32, f64, u64) {
    let text = gen_text(rng);
    let wpm = rng.range_f32(34.0, 39.0);
    let snr = rng.range_f32(20.0, 30.0);
    let offset_khz = rng.below(81) as i32 - 40;
    // Channel 0 is the DC dead channel (issue #12); a draw of 0 moves to
    // +1 kHz.
    let offset_hz = if offset_khz == 0 {
        1_000.0
    } else {
        offset_khz as f64 * 1000.0
    };
    let seed = rng.next_u64();
    (text, wpm, snr, offset_hz, seed)
}

/// A small, always-run slice of the Gherkin's domain: zero-output cases must
/// be zero. On main, 112 of the first 200 draws of this exact generator
/// decoded zero characters.
#[test]
fn two_char_domain_single_shot_never_decodes_nothing() {
    let mut rng = Rng(0x271_2026_1016);
    let mut zero = Vec::new();
    for _ in 0..24 {
        let (text, wpm, snr, off, seed) = draw_case(&mut rng);
        match decode_tuple(&text, wpm, snr, off, seed, 3.0, &PipelineConfig::default()) {
            Ok((chars, decoded)) => {
                if chars == 0 {
                    zero.push(format!(
                        "{text} wpm {wpm:.1} snr {snr:.1} off {off} Hz: {decoded:?}"
                    ));
                }
            }
            Err(e) => zero.push(format!(
                "{text} wpm {wpm:.1} snr {snr:.1} off {off} Hz: Err({e})"
            )),
        }
    }
    assert!(
        zero.is_empty(),
        "{} of 24 cases decoded zero characters:\n{}",
        zero.len(),
        zero.join("\n")
    );
}

/// The measuring instrument behind the decision record's numbers: the full
/// 200-case domain sweep, single-shot. Prints the zero-output, not-exact and
/// exact counts. Run with:
///
/// ```text
/// cargo test --release -p manta-engine --test regression_short_high_wpm \
///     two_char_domain_single_shot_sweep_200 -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measuring instrument, ~75 s release"]
fn two_char_domain_single_shot_sweep_200() {
    let mut rng = Rng(0x271_2026_1016);
    let (mut zero, mut not_exact, mut exact) = (0usize, 0usize, 0usize);
    for _ in 0..200 {
        let (text, wpm, snr, off, seed) = draw_case(&mut rng);
        let (chars, decoded) =
            match decode_tuple(&text, wpm, snr, off, seed, 3.0, &PipelineConfig::default()) {
                Ok((chars, decoded)) => (chars, decoded),
                Err(e) => {
                    zero += 1;
                    println!(
                    "ZERO-OUTPUT {text} wpm {wpm:.1} snr {snr:.1} off {off} seed {seed}: Err({e})"
                );
                    continue;
                }
            };
        if chars == 0 {
            zero += 1;
            println!("ZERO-OUTPUT {text} wpm {wpm:.1} snr {snr:.1} off {off} seed {seed}");
        } else if decoded.contains(&text) {
            exact += 1;
        } else {
            not_exact += 1;
        }
    }
    println!(
        "single-shot 200-case sweep: {zero} zero-output, {not_exact} not exact, {exact} exact"
    );
    assert_eq!(zero, 0, "zero-output cases must be zero");
}
