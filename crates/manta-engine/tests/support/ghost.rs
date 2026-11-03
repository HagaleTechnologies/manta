//! MAN-206 ghost probe, shared by `minimizer.rs` and `synthetic_repros.rs`
//! via `#[path]`.

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scenespec::SceneSpec;
use num_complex::Complex32;

/// How far a promoted track may sit from a signal (or its mirror) and
/// still be attributed to it, Hz.
pub const MATCH_HZ: f64 = 150.0;

/// One entry per promoted track farther than 150 Hz from every scene signal:
/// "mirror:<label>" when within 150 Hz of -offset of that signal (the
/// real-passband image), else "unexplained". None when there is none.
/// A decode error is Some(["decode-error"]) (MAN-206 D9).
pub fn ghost_probe(scene: &SceneSpec, samples: &[Complex32]) -> Option<Vec<String>> {
    let report = match decode_samples(
        samples,
        scene.fs,
        scene.center_freq_hz,
        &PipelineConfig::default(),
    ) {
        Ok(r) => r,
        Err(_) => return Some(vec!["decode-error".to_string()]),
    };
    let mut sig: Vec<String> = report
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz - scene.center_freq_hz),
            _ => None,
        })
        .filter(|off| {
            scene
                .signals
                .iter()
                .all(|s| (off - s.offset_hz).abs() > MATCH_HZ)
        })
        .map(|off| {
            scene
                .signals
                .iter()
                .find(|s| (off + s.offset_hz).abs() <= MATCH_HZ)
                .map(|s| format!("mirror:{}", s.label))
                .unwrap_or_else(|| "unexplained".to_string())
        })
        .collect();
    sig.sort();
    sig.dedup();
    (!sig.is_empty()).then_some(sig)
}
