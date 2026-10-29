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
//! Usage: `replay_spots <report.json> <calls.txt> [sample_rate_hz]
//! [--allowlist <CALL>]... [--blocklist <path>] [--notch <path>]`
//!
//! `<report.json>` is the stdout of `manta decode --json <wav>` (a
//! `manta_engine::DecodeReport`, serialized). `<calls.txt>` is a
//! newline-separated list of the scene's genuine callsigns (e.g. a fixture
//! manifest's known set), used only to tag each replayed spot
//! genuine/bogus in the printed summary -- it never affects what spots.
//! `[sample_rate_hz]` defaults to 96 kHz (every fixture this repo
//! generates, SPEC §7 / `manta-testkit::vectors::VectorSpec::fs`) but MUST
//! be overridden to the WAV's actual rate for a report produced with
//! `manta decode --capture-rate-hz <n>` -- `DecodeReport` carries no
//! sample-rate field of its own (Codex review, PR #133), so this can't be
//! inferred from the report; passing the wrong rate silently doubles or
//! halves the 60 s message gap, 90 s repetition window, and 10 min dedupe
//! window against what the original run actually used.
//!
//! The remaining flags mirror `manta decode`'s own of the same name
//! (Codex review, PR #133): `DecodeReport` doesn't preserve the original
//! run's allowlist/blocklist/notch settings either, and those affect
//! admission, suppression, and dedupe buckets in the real validator just
//! as much as the sample rate does -- a report produced with any of them
//! non-default needs the SAME values passed here, or the replayed spot
//! list won't match the original run's.
//!
//! Deliberately NOT mirrored: `--freq-correction-ppm`. Unlike the other
//! flags, this one must NEVER be passed through, even if the original run
//! used it (Codex review, PR #133 round 5): `decode_samples`
//! (`manta-engine/src/lib.rs`) mutates every serialized `TrackMeta`/
//! `TrackPromoted` `freq_hz` by the calibration factor AFTER its own
//! `Validator` pass, specifically so the JSON report's events already
//! carry corrected frequencies -- applying the same correction again here
//! would double it, moving notch/dedupe buckets and producing a spot list
//! that doesn't match the original run.
//!
//! `DecoderEvent` derives both `Serialize` and `Deserialize`, so this
//! deserializes the saved report's `events` array directly rather than
//! reconstructing it field-by-field -- a hand-rolled reconstruction would
//! silently drift out of sync with the enum's own additive fields (e.g.
//! `CharDecoded::alternatives`, `WordBoundary::confidence`,
//! `TrackMeta::sample_ts`, `TrackClosed::closure`) every time it gained
//! one, exactly the failure mode this rewrite closes.

use manta_decode::events::DecoderEvent;
use manta_spot::{Blocklist, NotchList, Validator};
use std::collections::BTreeSet;
use std::time::Instant;

/// The `sample_ts` values in a saved report are raw-IQ sample indices at
/// the WAV's own sample rate -- see the module doc's `[sample_rate_hz]`
/// note for why this can't just be read out of the report itself.
const DEFAULT_FS: f64 = 96_000.0;

struct Args {
    report_path: String,
    calls_path: String,
    fs: f64,
    allowlist: Vec<String>,
    blocklist: Option<String>,
    notch: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: replay_spots <report.json> <calls.txt> [sample_rate_hz] \
         [--allowlist <CALL>]... [--blocklist <path>] [--notch <path>]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut allowlist = Vec::new();
    let mut blocklist = None;
    let mut notch = None;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--allowlist" => {
                allowlist.push(raw.get(i + 1).unwrap_or_else(|| usage()).clone());
                i += 2;
            }
            "--blocklist" => {
                blocklist = Some(raw.get(i + 1).unwrap_or_else(|| usage()).clone());
                i += 2;
            }
            "--notch" => {
                notch = Some(raw.get(i + 1).unwrap_or_else(|| usage()).clone());
                i += 2;
            }
            other if other.starts_with("--") => usage(),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let (report_path, calls_path, fs) = match positional.as_slice() {
        [report_path, calls_path] => (report_path.clone(), calls_path.clone(), DEFAULT_FS),
        [report_path, calls_path, sample_rate_hz] => {
            let fs: f64 = sample_rate_hz.parse().unwrap_or_else(|e| {
                eprintln!("invalid sample_rate_hz {sample_rate_hz:?}: {e}");
                std::process::exit(2);
            });
            if !fs.is_finite() || fs <= 0.0 {
                eprintln!("sample_rate_hz must be a finite, positive number, got {fs}");
                std::process::exit(2);
            }
            (report_path.clone(), calls_path.clone(), fs)
        }
        _ => usage(),
    };

    Args {
        report_path,
        calls_path,
        fs,
        allowlist,
        blocklist,
        notch,
    }
}

fn main() {
    let args = parse_args();

    let report_text = std::fs::read_to_string(&args.report_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", args.report_path));
    let report: serde_json::Value =
        serde_json::from_str(&report_text).expect("report.json must be valid JSON");
    let events: Vec<DecoderEvent> = report["events"]
        .as_array()
        .expect("report must have an \"events\" array")
        .iter()
        .map(|v| serde_json::from_value(v.clone()).expect("valid DecoderEvent JSON"))
        .collect();

    let known_calls: BTreeSet<String> = std::fs::read_to_string(&args.calls_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", args.calls_path))
        .lines()
        .map(|l| l.trim().to_ascii_uppercase())
        .filter(|l| !l.is_empty())
        .collect();

    // No `.with_freq_correction_ppm(...)` here -- see the module doc's
    // "Deliberately NOT mirrored" note: the report's own freq_hz values
    // are already calibration-corrected, so applying it again would
    // double it.
    let mut validator = Validator::bundled(args.fs);
    for call in &args.allowlist {
        validator.allowlist(call);
    }
    if let Some(path) = &args.blocklist {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        validator = validator.with_blocklist(Blocklist::parse(&text));
    }
    if let Some(path) = &args.notch {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        validator = validator.with_notch(NotchList::parse(&text));
    }

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
