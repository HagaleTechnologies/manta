//! SPEC §7 V34 "fade-good-reacquire" (MAN-108): one Watterson-good fading
//! signal stays on one track through its fades (SPEC §2.4's DORMANT
//! re-acquisition), and a `[detector] reacquire_ms = 0` control still
//! fragments the same WAV, proving the scene exercises the mechanism (as
//! V51's refine-off control does for the refiner). See
//! docs/DECISIONS/2026-10-17-man108-dormant-track-reacquisition.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The signal's expected absolute frequency (14 MHz + 9100 Hz).
const EXPECTED_HZ: f64 = 14_009_100.0;
/// "Within 300 Hz", the V8w fragmentation count's window.
const NEAR_HZ: f64 = 300.0;

/// The V34 WAV and its keyed text, rendered once per test binary and
/// shared by both tests. Written under `CARGO_TARGET_TMPDIR`, staged under
/// a process-unique name and renamed into place, like
/// `tests/common/mod.rs`'s `fixture_wav` (a static `TempDir` would leak).
fn fixture() -> &'static (PathBuf, String) {
    static FIXTURE: OnceLock<(PathBuf, String)> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let spec = manta_testkit::vectors::v34();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("man108-v34-{:x}", spec.noise_seed));
        let wav = root.join("v34.wav");
        let manifest_path = root.join("v34.manifest.json");
        if !wav.exists() || !manifest_path.exists() {
            let staging = root.with_extension(format!("staging.{}", std::process::id()));
            std::fs::create_dir_all(&staging).unwrap();
            manta_testkit::vectors::write_fixture_set(&spec, &staging).unwrap();
            // Loser of a rename race: another process already published one.
            if std::fs::rename(&staging, &root).is_err() {
                let _ = std::fs::remove_dir_all(&staging);
            }
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let keyed = manifest["keyed_texts"][0].as_str().unwrap().to_string();
        (wav, keyed)
    })
}

fn decode(config: Option<&Path>) -> serde_json::Value {
    let (wav, _) = fixture();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    cmd.args(["decode", "--json"]);
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    let out = cmd.arg(wav).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Distinct `track_id`s promoted within `NEAR_HZ` of the signal.
fn promoted_near(report: &serde_json::Value) -> BTreeSet<u64> {
    report["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|ev| ev["event"] == "TrackPromoted")
        .filter(|ev| (ev["frequency"].as_f64().unwrap() - EXPECTED_HZ).abs() <= NEAR_HZ)
        .map(|ev| ev["trackId"].as_u64().unwrap())
        .collect()
}

/// `track_id`'s decoded text, words split at `WordBoundary`.
fn track_text(report: &serde_json::Value, track_id: u64) -> String {
    let mut text = String::new();
    for ev in report["events"].as_array().unwrap() {
        if ev["trackId"].as_u64() != Some(track_id) {
            continue;
        }
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"].as_str().filter(|g| g.chars().count() == 1) {
                    text.push_str(c);
                }
            }
            "WordBoundary" if !text.is_empty() && !text.ends_with(' ') => text.push(' '),
            _ => {}
        }
    }
    text.trim().to_string()
}

/// Measured on the MAN-108 branch: 1 track, W1AW spotted from it at
/// 20.9 s, CER 0.1546 against the keyed text -- equal to the CER of the
/// `reacquire_ms = 0` control's three tracks' texts joined in id order, so
/// the one track decodes today's text. The bound is that rounded up to the
/// next 0.05.
const V34_MAX_CER: f64 = 0.20;

#[test]
fn v34_fading_signal_stays_on_one_track() {
    let report = decode(None);
    let promoted = promoted_near(&report);
    assert_eq!(
        promoted.len(),
        1,
        "V34's fading signal must stay on one track: promoted {promoted:?}"
    );
    let id = *promoted.iter().next().unwrap();
    let spots: Vec<(String, u64)> = report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["dxCall"].as_str().unwrap().to_string(),
                s["trackId"].as_u64().unwrap(),
            )
        })
        .collect();
    let w1aw: Vec<u64> = spots
        .iter()
        .filter(|(c, _)| c == "W1AW")
        .map(|&(_, t)| t)
        .collect();
    assert!(!w1aw.is_empty(), "W1AW must be spotted: {spots:?}");
    assert!(
        w1aw.iter().all(|&t| t == id),
        "every W1AW spot must carry the one track's id {id}: {spots:?}"
    );
    let (_, keyed) = fixture();
    let decoded = track_text(&report, id);
    let cer = manta_testkit::cer::cer(keyed, &decoded);
    assert!(
        cer <= V34_MAX_CER,
        "V34 CER must be <= {V34_MAX_CER}, got {cer} (decoded {decoded:?})"
    );
}

#[test]
fn v34_control_without_reacquire_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("reacquire0.toml");
    std::fs::write(&cfg, "[detector]\nreacquire_ms = 0\n").unwrap();
    let promoted = promoted_near(&decode(Some(&cfg)));
    // Measured on the MAN-108 branch: 3, as on `35deb42`.
    assert!(
        promoted.len() >= 2,
        "with reacquire_ms = 0 the V34 scene must still fragment (else it no \
         longer exercises re-acquisition): promoted {promoted:?}"
    );
}
