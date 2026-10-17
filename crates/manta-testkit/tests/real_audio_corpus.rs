//! MAN-119: the real-audio corpus as a CI fixture.
//!
//! `audio-corpus/manifest.json` is parsed on every run, so a malformed
//! manifest fails the required legs. The recordings themselves are not in
//! git: set `MANTA_CORPUS_DIR` to a directory holding some of them (the
//! nightly `corpus.yml` fetches them there with `scripts/corpus.py fetch`)
//! and every present IQ recording is streamed through `WavIqSource` end to
//! end. See docs/RUNBOOKS/real-audio-corpus.md.

use std::path::{Path, PathBuf};

use manta_input::{IqSource, WavIqSource};
use num_complex::Complex32;
use serde_json::Value;

/// One `kind: "iq"` manifest entry, as far as this test needs it.
struct IqRecording {
    file: String,
    sample_rate_hz: u32,
    bits_per_sample: u16,
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("audio-corpus/manifest.json")
}

/// Parses the manifest and returns its IQ entries, panicking on any shape
/// error `scripts/corpus.py` would also refuse.
fn load_manifest() -> Vec<IqRecording> {
    let path = manifest_path();
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let manifest: Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let recordings = manifest["recordings"]
        .as_array()
        .unwrap_or_else(|| panic!("{} has no \"recordings\" array", path.display()));
    assert!(
        !recordings.is_empty(),
        "{} lists no recordings",
        path.display()
    );
    let mut iq = Vec::new();
    for rec in recordings {
        let file = rec["file"]
            .as_str()
            .unwrap_or_else(|| panic!("entry without a \"file\" string: {rec}"));
        let rate = rec["sample_rate_hz"]
            .as_u64()
            .filter(|&r| r > 0 && r <= u64::from(u32::MAX))
            .unwrap_or_else(|| panic!("{file}: \"sample_rate_hz\" must be a positive integer"));
        match &rec["sha256"] {
            Value::Null => {}
            Value::String(s) => assert!(
                s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{file}: \"sha256\" must be 64 lowercase hex digits, got {s:?}"
            ),
            other => panic!("{file}: \"sha256\" must be null or a string, got {other}"),
        }
        match rec["kind"].as_str() {
            Some("iq") => {
                let bits = rec["bits_per_sample"]
                    .as_u64()
                    .filter(|b| [16, 24, 32].contains(b))
                    .unwrap_or_else(|| panic!("{file}: \"bits_per_sample\" must be 16, 24 or 32"));
                iq.push(IqRecording {
                    file: file.to_string(),
                    sample_rate_hz: rate as u32,
                    bits_per_sample: bits as u16,
                });
            }
            Some("audio") => {}
            other => panic!("{file}: \"kind\" must be \"iq\" or \"audio\", got {other:?}"),
        }
    }
    iq
}

/// MAN-119: WavIqSource streams every corpus recording present in
/// MANTA_CORPUS_DIR end to end (the ticket's first scenario on the real files).
#[test]
fn every_present_iq_recording_streams_end_to_end() {
    let manifest = load_manifest(); // always: a malformed manifest fails the required legs
    let Some(dir) = std::env::var_os("MANTA_CORPUS_DIR") else {
        eprintln!("skipping: MANTA_CORPUS_DIR is not set (see docs/RUNBOOKS/real-audio-corpus.md)");
        return;
    };
    let dir = PathBuf::from(dir);
    let present: Vec<_> = manifest
        .iter()
        .filter(|r| dir.join(&r.file).exists())
        .collect();
    assert!(
        !present.is_empty(),
        "MANTA_CORPUS_DIR={} holds none of the manifest's recordings ({})",
        dir.display(),
        manifest
            .iter()
            .map(|r| r.file.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for rec in present {
        let path = dir.join(&rec.file);
        let spec = hound::WavReader::open(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", rec.file))
            .spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (2, rec.sample_rate_hz, rec.bits_per_sample),
            "{}",
            rec.file
        );
        let declared = u64::from(
            hound::WavReader::open(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", rec.file))
                .duration(),
        );
        let mut src = WavIqSource::open_with_center_freq_hz(&path, 1.0)
            .unwrap_or_else(|e| panic!("{}: {e:#}", rec.file));
        let mut buf = vec![Complex32::new(0.0, 0.0); 65_536];
        let mut frames: u64 = 0;
        let mut nonzero = false;
        let mut peak: f32 = 0.0;
        loop {
            let n = src
                .read(&mut buf)
                .unwrap_or_else(|e| panic!("{}: {e:#}", rec.file));
            if n == 0 {
                break;
            }
            frames += n as u64;
            for s in &buf[..n] {
                nonzero |= s.re != 0.0 || s.im != 0.0;
                peak = peak.max(s.re.abs()).max(s.im.abs());
            }
        }
        assert_eq!(frames, declared, "{}: streamed frames", rec.file);
        assert!(nonzero, "{}: every sample is zero", rec.file);
        assert!(
            peak <= 1.0,
            "{}: peak |sample| {peak} exceeds 1.0",
            rec.file
        );
        eprintln!("{}: {frames} frames streamed", rec.file);
    }
}
