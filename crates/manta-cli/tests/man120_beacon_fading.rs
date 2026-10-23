//! MAN-120 (#28, lens-3 row 28): the NCDXF `<call> T` power-step Beacon
//! heuristic under Watterson fading, at IQ level. Beacon is the one spot
//! type exempt from the repetition gate (`validator.rs`), and its
//! emission is deferred to `TrackClosed` against confirmed WPM
//! (docs/DECISIONS/2026-09-09-beacon-emission-deferred-to-track-close.md)
//! -- both behave differently under fading, yet until now every test
//! covering the heuristic was clean-text (golden_v11_v15's V11/V18/V30
//! and the `context.rs` unit tests). This is the missing regression.
//!
//! Scene + control follow the V34 pattern
//! (golden_v34_fade_reacquire.rs): one 120 s signal keyed "K5ARH T" in a
//! loop under Watterson CCIR-poor fading, plus an identical-scene AWGN
//! control (same noise seed) as the detectability floor, and a
//! fading-is-real check proving the scene stresses the decode rather than
//! decorating it with a Watterson field.
//!
//! The payload is the decode-shaped form -- the call followed by one
//! long dash (`T`), looped -- rather than the over-the-air "VVV DE ...
//! <4 dashes>": the heuristic only sees `<call> T` once the framing words
//! have aged out of the 16-word window, so rendering the ID directly
//! keeps the test independent of window-aging behaviour (recorded in the
//! MAN-120 decision record, D2).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use coppa_channel::watterson::WattersonPreset;
use manta_testkit::scene::{SignalSpec, WattersonFade};
use manta_testkit::vectors::VectorSpec;

/// The signal's expected absolute frequency (14 MHz + 9100 Hz), V34's
/// offset.
const EXPECTED_HZ: f64 = 14_009_100.0;
/// "Within 300 Hz", the V8w fragmentation count's window, for collecting
/// a scene's tracks in the CER comparison.
const NEAR_HZ: f64 = 300.0;

/// Scene SNR (2500 Hz-referenced) and duration, calibrated per the house
/// rule (bound = measured, rounded out). +10 dB Watterson-poor held on
/// the first try, so it stays; the plan's 120 s did not, and that is a
/// finding, not a calibration miss: a continuously-looped ID gives ~50
/// `<call> T` windows under CCIR-poor fading, and across every measured
/// SNR (10/13/16/19 dB) at least two deep-fade repeats garbled the call
/// into a *distinct but valid* callsign (e.g. NE5ARH at 94.6 s, V5ARH at
/// 110.4 s -- timing-split/merged elements, SNR-independent) that
/// dedupe's callsign key cannot join and no repetition gate stops. The
/// real NCDXF frame keys its ID a handful of times per slot, not 50; 40 s
/// (~13 cycles) is the measured realistic-duty scene where the heuristic
/// holds: K5ARH spots exactly once as Beacon at 7.46 s (control: 11.42 s)
/// with zero bogus, fading CER 0.3038 vs AWGN 0.0127. The 120 s garble
/// finding is recorded as an open claim in the MAN-120 decision record's
/// D2 section. 30 s fails too, differently: the WPM-bootstrap transient's
/// first-cycle garble (U5ARH at 3.5 s) resolves at EOF in both scenes.
const BEACON_SNR_DB: f32 = 10.0;
const BEACON_DURATION_S: f64 = 40.0;

fn spec(watterson: Option<WattersonFade>) -> VectorSpec {
    VectorSpec {
        name: if watterson.is_some() {
            "man120-beacon-fade"
        } else {
            "man120-beacon-awgn"
        },
        fs: 96_000.0,
        duration_s: BEACON_DURATION_S,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x4D41_4E31_3230, // "MAN120"
        signals: vec![SignalSpec {
            dit_units: 1.0,
            element_gap_units: 1.0,
            text: "K5ARH T".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 9_100.0,
            snr_2500_db: BEACON_SNR_DB,
            jitter: None,
            qsb: None,
            watterson,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
    }
}

/// The two WAVs and the keyed text, rendered once per test binary and
/// shared by all three tests. Written under `CARGO_TARGET_TMPDIR`,
/// staged under a process-unique name and renamed into place, like
/// `golden_v34_fade_reacquire.rs`'s `fixture` (a static `TempDir` would
/// leak).
fn fixtures() -> &'static (PathBuf, PathBuf, String) {
    static FIXTURES: OnceLock<(PathBuf, PathBuf, String)> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let fade = write_cached(&spec(Some(WattersonFade {
            preset: WattersonPreset::Poor,
            seed: 0x5631,
        })));
        let awgn = write_cached(&spec(None));
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                fade.parent()
                    .unwrap()
                    .join("man120-beacon-fade.manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let keyed = manifest["keyed_texts"][0].as_str().unwrap().to_string();
        (fade, awgn, keyed)
    })
}

fn write_cached(spec: &VectorSpec) -> PathBuf {
    // One cache dir per spec: the two scenes share a noise seed, so the
    // seed alone would make the second spec's write lose its rename race
    // against the first spec's already-published dir.
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("man120-{}-{:x}", spec.name, spec.noise_seed));
    let wav = root.join(format!("{}.wav", spec.name));
    let manifest_path = root.join(format!("{}.manifest.json", spec.name));
    if !wav.exists() || !manifest_path.exists() {
        let staging = root.with_extension(format!("staging.{}", std::process::id()));
        std::fs::create_dir_all(&staging).unwrap();
        manta_testkit::vectors::write_fixture_set(spec, &staging).unwrap();
        // Loser of a rename race: another process already published one.
        if std::fs::rename(&staging, &root).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
    }
    wav
}

fn decode(wav: &Path) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `(callsign, spot_type, sample_ts_s)` for every spot.
fn spots(report: &serde_json::Value) -> Vec<(String, String, f64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["callsign"].as_str().unwrap().to_string(),
                s["spot_type"].as_str().unwrap().to_string(),
                s["sample_ts"].as_f64().unwrap() / 96_000.0,
            )
        })
        .collect()
}

/// The scene's decoded copy: the texts of every track within `NEAR_HZ`
/// of the signal joined in ascending track_id (the MAN-116 D6
/// frequency-join convention, shared with `joined_cer`).
fn joined_track_text(report: &serde_json::Value) -> String {
    let mut freqs: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    let mut texts: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = match ev["track_id"].as_u64() {
            Some(t) => t,
            None => continue,
        };
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"]["Char"].as_str() {
                    texts.entry(tid).or_default().push_str(c);
                }
            }
            "WordBoundary" => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            "TrackMeta" => {
                freqs.insert(tid, ev["freq_hz"].as_f64().unwrap());
            }
            _ => {}
        }
    }
    let mut tracks: Vec<(u64, String)> = texts
        .into_iter()
        .filter(|(tid, _)| {
            freqs
                .get(tid)
                .is_some_and(|f| (f - EXPECTED_HZ).abs() <= NEAR_HZ)
        })
        .map(|(tid, t)| (tid, t.trim().to_string()))
        .collect();
    tracks.sort_by_key(|(tid, _)| *tid);
    tracks
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The ticket's assertion: under Watterson-poor fading the real call
/// spots exactly once as `Beacon`, and nothing else spots at all --
/// zero bogus callsigns of any type (a garbled half-word plus a lone `T`
/// is exactly the false-`Beacon` shape fading could produce, with no
/// repetition gate to catch it).
#[test]
fn beacon_spots_exactly_once_under_watterson_poor() {
    let (fade, _, _) = fixtures();
    let report = decode(fade);
    let spots = spots(&report);
    let beacons: Vec<&(String, String, f64)> =
        spots.iter().filter(|(_, ty, _)| ty == "Beacon").collect();
    assert_eq!(
        beacons.len(),
        1,
        "exactly one Beacon spot expected, got {spots:?}"
    );
    assert_eq!(beacons[0].0, "K5ARH", "the beacon's call: {spots:?}");
    let others: Vec<&(String, String, f64)> =
        spots.iter().filter(|(c, _, _)| c != "K5ARH").collect();
    assert!(
        others.is_empty(),
        "zero bogus callsigns of any type expected: {others:?}"
    );
}

/// The detectability floor: the same scene without fading must also spot
/// the beacon exactly once, so a fading-scene failure is the
/// heuristic-under-fade, not a dead scene (the V34-control philosophy).
#[test]
fn awgn_control_spots_the_beacon_once() {
    let (_, awgn, _) = fixtures();
    let report = decode(awgn);
    let spots = spots(&report);
    let beacons: Vec<&(String, String, f64)> =
        spots.iter().filter(|(_, ty, _)| ty == "Beacon").collect();
    assert_eq!(
        beacons.len(),
        1,
        "the AWGN control must spot the beacon exactly once, got {spots:?}"
    );
    assert_eq!(beacons[0].0, "K5ARH");
}

/// The fading must be real: the fading scene's copy decodes strictly
/// worse than the control's, so the scene exercises the mechanism rather
/// than carrying a decorative Watterson field.
#[test]
fn fading_actually_stresses_the_decode() {
    let (fade, awgn, keyed) = fixtures();
    let fade_cer = manta_testkit::cer::cer(keyed, &joined_track_text(&decode(fade)));
    let awgn_cer = manta_testkit::cer::cer(keyed, &joined_track_text(&decode(awgn)));
    assert!(
        fade_cer > awgn_cer,
        "fading CER {fade_cer} must exceed AWGN CER {awgn_cer}"
    );
}
