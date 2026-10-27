//! MAN-402 regression: high-speed dah-heavy text.
//!
//! On main before MAN-402, every row of the MAN-398 grid (`man398_grid.rs`)
//! keying looped "MO", "0000", "OO" or "T0M" at 30 WPM or more decoded as
//! garbage or nothing: "MO" at 33 WPM as `H5555...5I`, "0000" at 37.5 WPM as
//! an empty string. Their 90-120 ms dahs sit under the 150 ms dit ceiling, so
//! the five-mark speed bootstrap read them as dits, and a stream of equal
//! dahs never supplies the longer mark that would correct it. `SpeedTracker`
//! now resolves that bootstrap from the completed gaps between its marks
//! before the warmup replay. Mechanism, alternatives and measurements:
//! docs/DECISIONS/2026-10-25-man402-dah-heavy-bootstrap.md.
//!
//! The fixtures are the grid's own rows, regenerated here from its PRNG so
//! no offset or SNR is rounded; the hermetic cases live in `manta-decode`'s
//! `timing.rs` and `decoder.rs` tests.

use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

/// `man398_grid.rs`'s text list, in its order (the row's text is
/// `TEXTS[row % 24]`).
const TEXTS: [&str; 24] = [
    "AU",
    "HH",
    "5NN",
    "K7X",
    "CQ",
    "TEST",
    "W1AW",
    "TU",
    "73",
    "EE",
    "SS",
    "IS",
    "CQ TEST W5AU",
    "599 001",
    "5NN 0042",
    "E 0000",
    "TEST 000",
    "OO",
    "MO",
    "T0M",
    "R 5NN TU",
    "CQ CQ DE K1ABC",
    "0000",
    "EIS 5",
];

const DAH_HEAVY: [&str; 4] = ["MO", "0000", "OO", "T0M"];

/// The rows at WPM >= 30 keying a `DAH_HEAVY` text, as the grid draws them.
const HIGH_SPEED_ROWS: [usize; 17] = [
    19, 43, 46, 65, 66, 67, 70, 91, 114, 162, 186, 190, 214, 234, 238, 258, 286,
];

#[derive(Debug, Clone, Copy)]
struct Case {
    row: usize,
    text: &'static str,
    wpm: f32,
    offset_khz: f64,
    snr_2500_db: f32,
    seed: u64,
}

/// All 288 grid rows, with `man398_grid.rs`'s xorshift draws in its order and
/// arithmetic.
fn grid_cases() -> Vec<Case> {
    let mut x: u64 = 0xD1B54A32D192ED03;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..288)
        .map(|row| {
            let text = TEXTS[row % TEXTS.len()];
            let wpm = 12.0 + (next() % 2800) as f32 / 100.0;
            let mut offset_khz = (next() % 7800) as f64 / 100.0 - 39.0;
            if offset_khz.abs() < 1.0 {
                offset_khz = 5.0;
            }
            let snr_2500_db = 12.0 + (next() % 1800) as f32 / 100.0;
            let seed = next();
            Case {
                row,
                text,
                wpm,
                offset_khz,
                snr_2500_db,
                seed,
            }
        })
        .collect()
}

fn high_speed_dah_heavy() -> Vec<Case> {
    grid_cases()
        .into_iter()
        .filter(|c| DAH_HEAVY.contains(&c.text) && c.wpm >= 30.0)
        .collect()
}

fn case(row: usize) -> Case {
    grid_cases()[row]
}

fn spec(c: &Case) -> SignalSpec {
    SignalSpec {
        text: c.text.to_string(),
        loop_text: true,
        wpm: c.wpm,
        offset_hz: c.offset_khz * 1000.0,
        snr_2500_db: c.snr_2500_db,
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

/// Render the grid's 96 kHz scene and decode it with the default pipeline.
/// Returns the keyed text (as `render_scene` truncated it) and the report.
fn decode(c: &Case, duration_s: f64) -> (String, DecodeReport) {
    let fs = 96_000.0;
    let (iq, keyed) =
        render_scene(std::slice::from_ref(&spec(c)), fs, duration_s, Some(c.seed)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    (keyed[0].clone(), report)
}

/// The ticket's acceptance at 30 s: CER under 0.25, at least five exact
/// whitespace-delimited copies of the word, and the decode ending in five of
/// them. A scene can end mid-word, so one trailing proper prefix of the word
/// is dropped first; nothing else is trimmed.
fn assert_word_bounded_copy(c: &Case) {
    let (keyed, report) = decode(c, 30.0);
    let decoded = report.text;
    let word = c.text;
    let ctx = format!(
        "row {} {word:?} @ {} WPM / {} kHz / {} dB (seed {})",
        c.row, c.wpm, c.offset_khz, c.snr_2500_db, c.seed
    );
    let e = cer(&keyed, &decoded);
    assert!(
        e < 0.25,
        "{ctx}: CER {e:.4}\nkeyed {keyed:?}\ndecoded {decoded:?}"
    );
    let words = decoded.split_whitespace().filter(|&w| w == word).count();
    assert!(
        words >= 5,
        "{ctx}: {words} word-bounded copies, CER {e:.4}\ndecoded {decoded:?}"
    );
    let mut toks: Vec<&str> = decoded.split_whitespace().collect();
    if toks
        .last()
        .is_some_and(|w| *w != word && word.starts_with(w))
    {
        toks.pop();
    }
    assert!(
        toks.len() >= 5 && toks[toks.len() - 5..].iter().all(|&w| w == word),
        "{ctx}: decode does not end in word-bounded {word:?}, CER {e:.4}\ndecoded {decoded:?}"
    );
}

/// The selection is the grid's, unchanged: a drift in the PRNG port or the
/// filter fails here, not as a silently different fixture set.
#[test]
fn fixtures_match_the_man398_grid() {
    let rows: Vec<usize> = high_speed_dah_heavy().iter().map(|c| c.row).collect();
    assert_eq!(rows, HIGH_SPEED_ROWS);
    let mo = case(66);
    assert_eq!((mo.text, mo.seed), ("MO", 9030173154109632877));
    assert_eq!(format!("{:.2}", mo.wpm), "33.04");
    assert_eq!(format!("{:.2}", mo.offset_khz), "-15.28");
    assert_eq!(format!("{:.2}", mo.snr_2500_db), "24.13");
    let zeros = case(70);
    assert_eq!((zeros.text, zeros.seed), ("0000", 9381957236689149590));
    assert_eq!(format!("{:.2}", zeros.wpm), "37.52");
    assert_eq!(format!("{:.2}", zeros.offset_khz), "30.07");
    assert_eq!(format!("{:.2}", zeros.snr_2500_db), "28.61");
}

/// Ticket scenario 1: looped "MO" at 33.04 WPM, -15.28 kHz, 24.13 dB
/// (grid row 66; was `H5555555555555555555555555555I`, CER 1.0).
#[test]
fn ticket_mo_at_33_wpm_decodes_word_bounded() {
    assert_word_bounded_copy(&case(66));
}

/// Ticket scenario 2: looped "0000" at 37.52 WPM, +30.07 kHz, 28.61 dB
/// (grid row 70; was an empty decode, CER 1.0).
#[test]
fn ticket_zeros_at_37_wpm_decode_word_bounded() {
    assert_word_bounded_copy(&case(70));
}

/// Every high-speed dah-heavy grid row, including the ticket's other
/// families (`T0M` at 36 WPM was "E", `OO` at 36.6 WPM "5S"). Rows run on
/// scoped threads: each is an independent 30 s decode.
#[test]
fn high_speed_dah_heavy_grid_rows_decode_word_bounded() {
    let cases = high_speed_dah_heavy();
    assert_eq!(cases.len(), HIGH_SPEED_ROWS.len());
    std::thread::scope(|s| {
        let handles: Vec<_> = cases
            .iter()
            .map(|c| s.spawn(move || assert_word_bounded_copy(c)))
            .collect();
        let failed: Vec<usize> = handles
            .into_iter()
            .zip(&cases)
            .filter_map(|(h, c)| h.join().is_err().then_some(c.row))
            .collect();
        assert!(failed.is_empty(), "rows failed (panics above): {failed:?}");
    });
}

/// Deterministic decode: the two ticket fixtures, decoded twice on the same
/// build, give identical text and identical ordered events.
#[test]
fn ticket_fixtures_decode_deterministically() {
    for row in [66, 70] {
        let c = case(row);
        let (_, a) = decode(&c, 30.0);
        let (_, b) = decode(&c, 30.0);
        assert_eq!(a.text, b.text, "row {row}");
        assert_eq!(a.events, b.events, "row {row}");
    }
}
