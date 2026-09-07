//! MAN-100: replay a saved `manta decode --json` report through a fresh
//! `manta-spot::Validator`, without re-running the IQ pipeline.
//!
//! `Validator::ingest` is a pure function of the `DecoderEvent` stream, and
//! `decode --json` already dumps that whole stream (`DecodeReport::events`),
//! so re-feeding a saved report reproduces the same spot list the original
//! `manta decode --json` run produced -- in milliseconds of validator time
//! instead of the minutes the original IQ decode took. Every MAN-100 rule
//! variant was measured with this harness; see
//! `docs/DECISIONS/2026-09-07-man100-variant-arbitration.md` and
//! `wiki/pages/replay-spots-harness.md`.
//!
//! Usage: `replay_spots <report.json> <calls.txt>`
//!
//! `<report.json>` is the stdout of `manta decode --json <wav>` (a
//! `manta_engine::DecodeReport`, serialized). `<calls.txt>` is a
//! newline-separated list of the scene's genuine callsigns (e.g. a fixture
//! manifest's known set), used only to tag each replayed spot
//! genuine/bogus in the printed summary -- it never affects what spots.
//!
//! `DecoderEvent` derives both `Serialize` and `Deserialize`, so this
//! deserializes the saved report's `events` array directly rather than
//! reconstructing it field-by-field -- a hand-rolled reconstruction would
//! silently drift out of sync with the enum's own additive fields (e.g.
//! `CharDecoded::alternatives`, `WordBoundary::confidence`,
//! `TrackMeta::sample_ts`, `TrackClosed::closure`) every time it gained
//! one, exactly the failure mode this rewrite closes.

use manta_decode::events::DecoderEvent;
use manta_spot::Validator;
use std::collections::BTreeSet;
use std::time::Instant;

/// The `sample_ts` values in a saved report are raw-IQ sample indices at
/// the WAV's own sample rate. Every fixture this repo generates (SPEC §7,
/// `manta-testkit::vectors::VectorSpec::fs`) is 96 kHz, so a report
/// replayed here must originate from a 96 kHz source for its 90 s
/// window/gate math to mean what it meant when the report was produced.
const FS: f64 = 96_000.0;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, report_path, calls_path] = args.as_slice() else {
        eprintln!("usage: replay_spots <report.json> <calls.txt>");
        std::process::exit(2);
    };

    let report_text = std::fs::read_to_string(report_path)
        .unwrap_or_else(|e| panic!("reading {report_path}: {e}"));
    let report: serde_json::Value =
        serde_json::from_str(&report_text).expect("report.json must be valid JSON");
    let events: Vec<DecoderEvent> = report["events"]
        .as_array()
        .expect("report must have an \"events\" array")
        .iter()
        .map(|v| serde_json::from_value(v.clone()).expect("valid DecoderEvent JSON"))
        .collect();

    let known_calls: BTreeSet<String> = std::fs::read_to_string(calls_path)
        .unwrap_or_else(|e| panic!("reading {calls_path}: {e}"))
        .lines()
        .map(|l| l.trim().to_ascii_uppercase())
        .filter(|l| !l.is_empty())
        .collect();

    let mut validator = Validator::bundled(FS);
    let start = Instant::now();
    let mut spots = Vec::new();
    for ev in &events {
        spots.extend(validator.ingest(ev));
    }
    let elapsed = start.elapsed();

    let mut distinct = BTreeSet::new();
    let mut bogus = BTreeSet::new();
    for spot in &spots {
        let verdict = if known_calls.contains(&spot.callsign) {
            "genuine"
        } else {
            "bogus"
        };
        println!(
            "{} {:.1} Hz {} wpm {:?} conf={:.3} {verdict}",
            spot.callsign, spot.freq_hz, spot.wpm, spot.spot_type, spot.confidence,
        );
        distinct.insert(spot.callsign.clone());
        if verdict == "bogus" {
            bogus.insert(spot.callsign.clone());
        }
    }
    let validated = distinct.difference(&bogus).count();

    println!(
        "=== ingest {:.1} ms over {} events",
        elapsed.as_secs_f64() * 1000.0,
        events.len()
    );
    println!(
        "=== spots={} distinct={} validated={}/{} bogus={} {:?}",
        spots.len(),
        distinct.len(),
        validated,
        known_calls.len(),
        bogus.len(),
        bogus.iter().collect::<Vec<_>>()
    );
}
