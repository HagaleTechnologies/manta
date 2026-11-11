//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" are measured against the real
//! `manta-spot::Validator`'s output (`report["spots"]`), wired into
//! `decode_samples` in M3's engine-wiring sub-project -- see
//! docs/archive/plans/specs/2026-07-26-m3-engine-wiring-design.md. Previously
//! approximated with text-substring heuristics against raw decoder text,
//! the same way V5/V6 approximated "callsign validated" before this landed
//! -- see docs/archive/plans/specs/2026-07-24-m2-pileup-cpu-budget-design.md.
//!
//! MAN-521 split the one `golden_v8_v8w` binary into `golden_v8`,
//! `golden_v8w` and `golden_vc3` so CI can place each scene on its own
//! shard, and renders each scene once per binary instead of once per test.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// One rendered pileup scene shared by every test in a binary (MAN-521).
pub struct Fixture {
    pub spec: manta_testkit::vectors::VectorSpec,
    pub wav: PathBuf,
    pub manifest: manta_testkit::vectors::Manifest,
    /// Held so the directory is never deleted while the process runs
    /// (statics are never dropped); `sweep_stale` bounds what runs leave.
    _dir: tempfile::TempDir,
}

/// The scene named "V8", "V8w" or "VC3", rendered on first use and shared
/// after that. Each process renders its own copy from the generator, as
/// each test did before MAN-521: nothing is reused from an earlier run, so
/// a generator or coppa change can never be hidden behind an old WAV.
pub fn fixture(scene: &str) -> &'static Fixture {
    static V8: OnceLock<Fixture> = OnceLock::new();
    static V8W: OnceLock<Fixture> = OnceLock::new();
    static VC3: OnceLock<Fixture> = OnceLock::new();
    let cell = match scene {
        "V8" => &V8,
        "V8w" => &V8W,
        "VC3" => &VC3,
        _ => panic!("unknown pileup scene {scene}"),
    };
    cell.get_or_init(|| render(scene))
}

fn render(scene: &str) -> Fixture {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(tmp).unwrap();
    sweep_stale(tmp);
    let dir = tempfile::Builder::new()
        .prefix(&format!("man521-golden-{}-", scene.to_ascii_lowercase()))
        .tempdir_in(tmp)
        .unwrap();
    let (spec, manifest) = match scene {
        "VC3" => {
            let (spec, crashes) = manta_testkit::crash::vc3();
            let m =
                manta_testkit::crash::write_crash_fixture_set(&spec, &crashes, dir.path()).unwrap();
            (spec, m)
        }
        _ => {
            let spec = if scene == "V8" {
                manta_testkit::vectors::v8()
            } else {
                manta_testkit::vectors::v8w()
            };
            let m = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
            (spec, m)
        }
    };
    let wav = dir.path().join(format!("{}.wav", spec.name));
    Fixture {
        spec,
        wav,
        manifest,
        _dir: dir,
    }
}

/// Removes `man521-golden-*` directories older than a day. No golden run
/// lasts that long, so a concurrent run's directory is never touched.
fn sweep_stale(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .to_string_lossy()
            .starts_with("man521-golden-")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `manta decode --json [extra..] <wav>`; the assert text is the old
/// `decode_report`'s and `decode_report_hsmm`'s.
pub fn decode_json(wav: &Path, extra: &[&str]) -> serde_json::Value {
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

/// Extract the callsign from this project's SPEC §7 payload template
/// "CQ CQ DE <CALL> <CALL> K" (word index 3).
pub fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(3)
        .expect("keyed text must follow the 'CQ CQ DE <CALL> <CALL> K' template")
}

/// MAN-120 (FU-3 of MAN-116): CER here is scored over the frequency-
/// joined copy of every track within this distance of the expected
/// signal, in ascending track_id -- the same convention `manta bench
/// sensitivity` scores with (`bench.rs`'s `MATCH_HZ`), so a fading-
/// fragmented station is scored on its joined copy, not on whichever
/// single track was nearest, and fragmentation can never inflate these
/// numbers (MAN-116 decision D6).
pub const CER_JOIN_HZ: f64 = 150.0;

/// `per_track`'s entries as `joined_cer` tuples; tracks with no reported
/// frequency are skipped (they can never sit inside a match window).
pub fn track_refs(tracks: &BTreeMap<u64, (String, Option<f64>)>) -> Vec<(u64, f64, &str)> {
    tracks
        .iter()
        .filter_map(|(&id, (text, freq))| freq.map(|f| (id, f, text.as_str())))
        .collect()
}

/// `(callsign, track_id)` pairs from `report["spots"]` -- the real
/// `manta-spot::Validator`'s output, not a text-heuristic approximation.
pub fn spotted_calls(report: &serde_json::Value) -> Vec<(String, u64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["dxCall"].as_str().unwrap().to_string(),
                s["trackId"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Anti-vacuity floor for `v8w_pileup_fading_spots_no_bogus_callsigns`: a
/// change that simply stopped emitting spots would satisfy "0 bogus" while
/// destroying the skimmer. Measured baseline at e398d46 is 22/50 genuine
/// calls validated; MAN-100's arbitration fix measures 20/50. 15 left
/// room for a legitimate precision/recall trade while still catching a
/// collapse. MAN-213 raised it to 20 to encode its acceptance criterion,
/// "at least the pre-existing ~20 of 50": `main`'s pre-MAN-103 baseline
/// is 21/50, and MAN-103 + MAN-213's fade-tracking rails measure 28/50
/// (docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md).
/// MAN-95 measures 27/50 at normal validation and 28/50 at minimal; the
/// unlisted synthetic AB2KLK pattern accounts for the difference. This
/// is NOT a SPEC criterion -- SPEC §7's V8w row states no recall bar -- it
/// is a guard on this test's own meaning.
pub const MIN_V8W_VALIDATED: usize = 20;

/// The V8w-scene "no bogus callsigns" check shared by V8w and VC3: the
/// distinct callsigns spotted that are not in the scene (sorted), the
/// genuine calls validated, and the distinct calls spotted.
pub fn bogus_and_validated(
    report: &serde_json::Value,
    manifest: &manta_testkit::vectors::Manifest,
    name: &str,
) -> (Vec<String>, usize, usize) {
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "{name} fixture must have 50 unique callsigns"
    );

    let spots = spotted_calls(report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();
    let mut bogus: Vec<String> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .map(|c| c.to_string())
        .collect();
    bogus.sort_unstable();
    let validated = known_calls.iter().filter(|c| spotted.contains(**c)).count();
    (bogus, validated, spotted.len())
}

/// The bogus/validated failures for a V8w-scene report, evaluated and
/// reported together, deliberately: two sequential `assert!`s would make
/// the second one unreachable whenever the first fails -- exactly the
/// defect MAN-101 exists to fix one level up.
pub fn v8w_scene_failures(
    report: &serde_json::Value,
    manifest: &manta_testkit::vectors::Manifest,
    name: &str,
) -> Vec<String> {
    let (bogus, validated, n_spotted) = bogus_and_validated(report, manifest, name);
    let mut failures: Vec<String> = Vec::new();
    if !bogus.is_empty() {
        failures.push(format!(
            "SPEC §7 {name} requires 0 bogus callsigns; got {} of {n_spotted} distinct spotted: {bogus:?}",
            bogus.len(),
        ));
    }
    if validated < MIN_V8W_VALIDATED {
        failures.push(format!(
            "only {validated}/50 genuine callsigns validated (floor {MIN_V8W_VALIDATED}); \
             0 bogus is vacuous if recall has collapsed"
        ));
    }
    failures
}

/// VC3 tracks promoted at most: the V8w scene promotes 52 without crashes,
/// `"full"` measured 53 with them, and `"off"` 1694.
pub const MAX_VC3_PROMOTED: usize = 60;

/// MAN-317: the opt-in prior must preserve the existing precision and recall
/// bars on the same pinned scenes, including the VC3 crash realization.
/// Before MAN-521 one test looped over V8, V8w and VC3; each scene's binary
/// now calls this for its own scene.
pub fn prior_on_stays_at_zero_bogus(scene: &str) {
    let fx = fixture(scene);
    let manifest = &fx.manifest;
    let dir = tempfile::tempdir().unwrap();
    for weight in [0.0, 1.0, 2.0] {
        let config = dir.path().join("prior.toml");
        std::fs::write(
            &config,
            format!("[decode]\nprior = \"on\"\nprior_weight = {weight}\n"),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["decode", "--json", "--config"])
            .arg(&config)
            .arg(&fx.wav)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report = serde_json::from_slice(&out.stdout).unwrap();
        let (bogus, validated, _) = bogus_and_validated(&report, manifest, scene);
        eprintln!(
            "MAN-317 {scene} weight={weight}: bogus={}, validated={validated}",
            bogus.len()
        );
        assert!(bogus.is_empty(), "{scene} prior {weight}: {bogus:?}");
        let floor = if scene == "V8" { 45 } else { MIN_V8W_VALIDATED };
        assert!(
            validated >= floor,
            "{scene} prior {weight}: {validated} < {floor}"
        );
        if scene == "VC3" {
            let promoted = report["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event"] == "TrackPromoted")
                .count();
            assert!(promoted <= MAX_VC3_PROMOTED);
        }
    }
}

/// MAN-200: the opt-in repetition fusion must preserve the existing
/// precision and recall bars on the same pinned scenes, including the VC3
/// crash realization. The validated counts go in the decision record.
/// Before MAN-521 one test looped over V8, V8w and VC3; each scene's binary
/// now calls this for its own scene.
pub fn repetition_fusion_on_stays_at_zero_bogus(scene: &str) {
    let fx = fixture(scene);
    let manifest = &fx.manifest;
    let dir = tempfile::tempdir().unwrap();
    let wav = &fx.wav;
    let config = dir.path().join("fusion.toml");
    std::fs::write(&config, "[spot]\nrepetition_fusion = true\n").unwrap();
    let mut validated_by_mode = Vec::new();
    for fusion in [false, true] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        cmd.args(["decode", "--json"]);
        if fusion {
            cmd.arg("--config").arg(&config);
        }
        let out = cmd.arg(wav).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report = serde_json::from_slice(&out.stdout).unwrap();
        let (bogus, validated, _) = bogus_and_validated(&report, manifest, scene);
        eprintln!(
            "MAN-200 {scene} repetition_fusion={fusion}: bogus={}, validated={validated}",
            bogus.len()
        );
        assert!(bogus.is_empty(), "{scene} fusion {fusion}: {bogus:?}");
        let floor = if scene == "V8" { 45 } else { MIN_V8W_VALIDATED };
        assert!(
            validated >= floor,
            "{scene} fusion {fusion}: {validated} < {floor}"
        );
        if scene == "VC3" {
            let promoted = report["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["event"] == "TrackPromoted")
                .count();
            assert!(promoted <= MAX_VC3_PROMOTED);
        }
        validated_by_mode.push(validated);
    }
    eprintln!(
        "MAN-200 {scene}: validated off={} on={}",
        validated_by_mode[0], validated_by_mode[1]
    );
}
