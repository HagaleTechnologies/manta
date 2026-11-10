//! MAN-120: the NCDXF `<call> T` power-step Beacon heuristic under
//! Watterson fading at IQ level. MAN-201 D8 now requires the decoded
//! call itself to be allowlisted for this shape, and listed IDs spot
//! immediately. Unlisted `V V V <call>` beacons retain the separate
//! repetition-exempt path deferred to TrackClosed.
//!
//!
//! Scene + control follow the V34 pattern
//! (golden_v34_fade_reacquire.rs): one 40 s signal keyed "K5ARH T" in a
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

/// Historical 40-second scene at +10 dB, SNR referenced to 2500 Hz.
/// MAN-120 measured garbles beyond this duration before MAN-201 D8.
/// MAN-532 now pins the full 120-second scene separately at +6/+10 dB.
/// See docs/DECISIONS/2026-11-09-man532-beacon-fading-regression.md.
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
    decode_with(wav, &["--allowlist", "K5ARH"])
}

/// MAN-201 D8: the beacon reading needs the call on the allowlist; every
/// positive assertion here decodes with it listed.
fn decode_with(wav: &Path, extra: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .args(extra)
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

/// `(dxCall, spotType, sampleTs in seconds)` for every spot.
fn spots(report: &serde_json::Value) -> Vec<(String, String, f64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["dxCall"].as_str().unwrap().to_string(),
                s["spotType"].as_str().unwrap().to_string(),
                s["sampleTs"].as_f64().unwrap() / 96_000.0,
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
        let tid = match ev["trackId"].as_u64() {
            Some(t) => t,
            None => continue,
        };
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"].as_str().filter(|g| g.chars().count() == 1) {
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
                freqs.insert(tid, ev["frequency"].as_f64().unwrap());
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

/// A listed call survives fading without emitting a call outside the scene.
/// Type changes can legitimately emit more than one Beacon.
#[test]
fn faded_allowlisted_beacon_emits_only_the_scene_call() {
    let (fade, _, _) = fixtures();
    let report = decode(fade);
    let spots = spots(&report);
    // MAN-201 D8 (D12, the trailing-`T` exemption): a listed call spots at
    // once, each occurrence, not once at its track's close. A faded `T` makes
    // an `unknown` read, and dedupe's type change lets a later `beacon`
    // through, so the count is not one (measured: 2 `beacon`, 1 `unknown`).
    assert!(
        spots.iter().any(|(_, ty, _)| ty == "beacon"),
        "the beacon must spot, got {spots:?}"
    );
    assert!(
        spots.iter().all(|(call, _, _)| call == "K5ARH"),
        "every spot must be the listed call: {spots:?}"
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
        spots.iter().filter(|(_, ty, _)| ty == "beacon").collect();
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
/// MAN-201 D8: a trailing `T` alone is not beacon evidence.
#[test]
fn an_unlisted_call_is_not_a_beacon() {
    let (fade, awgn, _) = fixtures();
    for wav in [fade, awgn] {
        let got = spots(&decode_with(wav, &[]));
        assert!(
            got.iter().all(|(_, spot_type, _)| spot_type != "beacon"),
            "{got:?}"
        );
    }
}

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
