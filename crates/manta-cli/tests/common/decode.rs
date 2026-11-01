use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
pub fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

/// Group `report["events"]` by `trackId`, returning each track's decoded
/// text and its last-reported TrackMeta freq_hz.
pub fn per_track(report: &serde_json::Value) -> BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u64, f64> = BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = ev["trackId"].as_u64().unwrap();
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
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

/// The distinct `track_id`s of `report["events"]`'s `TrackPromoted`
/// events: every promoted track, including one merged or closed before it
/// decoded a character, which `per_track` never sees.
pub fn promoted_track_ids(report: &serde_json::Value) -> BTreeSet<u64> {
    report["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|ev| ev["event"] == "TrackPromoted")
        .map(|ev| ev["trackId"].as_u64().unwrap())
        .collect()
}

/// Rust-only frontend comparison through the production streaming pipeline.
pub fn streaming_report(
    spec: &manta_testkit::vectors::VectorSpec,
    steered: bool,
) -> (
    serde_json::Value,
    manta_testkit::vectors::Manifest,
    manta_engine::TrackLifecycleSnapshot,
) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let source =
        manta_input::WavIqSource::open(&dir.path().join(format!("{}.wav", spec.name))).unwrap();
    let observer = std::sync::Arc::new(manta_engine::TrackLifecycleObserver::default());
    let mut cfg = manta_engine::PipelineConfig::default();
    cfg.decode.steered_frontend = steered;
    let mut events = Vec::new();
    manta_engine::listen_with_observers(
        Box::new(source),
        &cfg,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        manta_engine::ListenObservers {
            track_lifecycle: Some(observer.clone()),
            ..Default::default()
        },
        |event| events.push(event.clone()),
        |_| {},
        |_| {},
    )
    .unwrap();
    (
        serde_json::json!({"events":events}),
        manifest,
        observer.snapshot(),
    )
}
