//! MAN-206: the failure-minimizer on a real generator defect. The 129-tap
//! real-passband route leaves a mirror image of V8w's `s09` (-324.6 Hz,
//! Watterson Poor), and the decoder promotes a track near +323 Hz where no
//! signal exists. The minimizer shrinks a 120 s multi-signal scene to a
//! short synthetic repro that still shows exactly that failure.

#[path = "support/ghost.rs"]
mod ghost;

use ghost::ghost_probe;
use manta_testkit::minimize::{minimize, Budget, Minimized};
use manta_testkit::repro;
use manta_testkit::scenespec::{Origin, SceneSignal, SceneSpec};
use manta_testkit::vectors;
use std::time::Instant;

/// V8w's `s09` alone.
fn s09_scene(duration_s: f64) -> SceneSpec {
    let mut s = SceneSpec::from_vector(&vectors::v8w()).with_duration(duration_s);
    s.signals.retain(|g| g.label == "s09");
    s
}

/// 120 s: V8w `s09` plus V8 `s00`..`s03` (direct route, all >= 2 kHz from
/// DC and from s09's mirror), V8w's noise seed, centre 14 MHz.
fn multi_signal_scene() -> SceneSpec {
    let v8 = SceneSpec::from_vector(&vectors::v8());
    let mut s = s09_scene(120.0);
    let interferers: Vec<SceneSignal> = v8.signals[..4].to_vec();
    for g in &interferers {
        assert!(
            g.offset_hz.abs() >= 2000.0 && g.watterson.is_none(),
            "{}",
            g.label
        );
    }
    s.signals.splice(0..0, interferers);
    s
}

fn check_minimized(m: &Minimized, d0: f64, n0: usize) {
    eprintln!("{}", m.story());
    eprintln!(
        "{} probes; minimized scene:\n{}",
        m.probes,
        m.scene.to_json().unwrap()
    );
    assert_eq!(m.signature, ["mirror:s09"]);
    assert!(m.scene.duration_s < d0, "duration {}", m.scene.duration_s);
    assert!(
        m.scene.signals.len() < n0,
        "{} signals",
        m.scene.signals.len()
    );
    assert!(m.scene.signals.iter().any(|g| g.label == "s09"));
    assert_eq!(m.scene.origin, Origin::Synthetic);
    let back = SceneSpec::from_json(&m.scene.to_json().unwrap()).unwrap();
    assert_eq!(back, m.scene);
    let (samples, _) = back.render().unwrap();
    assert_eq!(
        ghost_probe(&back, &samples),
        Some(m.signature.clone()),
        "re-confirm"
    );
}

fn write_repro(m: &Minimized, found_in: &str) {
    let r = repro::from_minimized(m, "v8w-near-dc-mirror-ghost", found_in, "ghost").unwrap();
    let json = r.to_json().unwrap();
    println!("{json}");
    if let Ok(path) = std::env::var("MANTA_REPRO_OUT") {
        std::fs::write(&path, &json).unwrap();
        eprintln!("wrote {path}");
    }
}

#[test]
fn ghost_probe_flags_the_v8w_s09_mirror_at_10_s() {
    let s = s09_scene(10.0);
    let (samples, _) = s.render().unwrap();
    assert_eq!(
        ghost_probe(&s, &samples),
        Some(vec!["mirror:s09".to_string()])
    );
}

#[test]
fn ghost_probe_passes_the_same_signal_on_the_direct_route() {
    let mut s = s09_scene(10.0);
    s.signals[0].watterson = None;
    let (samples, _) = s.render().unwrap();
    assert_eq!(ghost_probe(&s, &samples), None);
}

#[test]
fn ghost_probe_maps_a_decode_error() {
    let mut s = s09_scene(30.0);
    s.signals[0].text = "E".into();
    s.signals[0].jitter = None;
    let (samples, _) = s.render().unwrap();
    assert_eq!(
        ghost_probe(&s, &samples),
        Some(vec!["decode-error".to_string()])
    );
}

#[test]
fn minimizer_shrinks_a_120s_multi_signal_scene() {
    let t = Instant::now();
    let scene = multi_signal_scene();
    assert_eq!(scene.signals.len(), 5);
    let m = minimize(&scene, Budget::default(), ghost_probe).unwrap();
    check_minimized(&m, 120.0, 5);
    eprintln!(
        "minimizer_shrinks_a_120s_multi_signal_scene: {:.1} s",
        t.elapsed().as_secs_f64()
    );
    write_repro(
        &m,
        "manta-testkit vectors::v8w() s09 with vectors::v8() s00-s03 (MAN-206, 2026-11-03)",
    );
}

#[test]
#[ignore = "MAN-206 acceptance: minimizes the 120 s, 50-signal V8w scene (about 6 min); run with --ignored"]
fn minimizer_shrinks_the_v8w_pileup() {
    let t = Instant::now();
    let scene = SceneSpec::from_vector(&vectors::v8w());
    let m = minimize(&scene, Budget::default(), ghost_probe).unwrap();
    check_minimized(&m, 120.0, 50);
    eprintln!(
        "minimizer_shrinks_the_v8w_pileup: {:.1} s",
        t.elapsed().as_secs_f64()
    );
    write_repro(
        &m,
        "manta-testkit vectors::v8w() (MAN-206 research, 2026-11-02)",
    );
}
