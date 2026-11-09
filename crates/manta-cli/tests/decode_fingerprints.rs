//! MAN-524 byte-identity check: one `FINGERPRINT` line (vector, engine, exit
//! code, stdout bytes, SHA-256 of stdout) per `manta decode --json` run over
//! V1-V10, V8w, V31, V34, V51 and VR1-VR8, for both engines. Run once on each
//! build and diff, like `crates/manta-engine/tests/man398_grid.rs`:
//! `cargo test --release -p manta-cli --test decode_fingerprints -- --ignored --nocapture | grep ^FINGERPRINT`
use std::path::{Path, PathBuf};
use std::process::Command;

use manta_dsp::decimate::Decimator;
use manta_testkit::vectors;
use num_complex::Complex32;
use sha2::{Digest, Sha256};

/// Writes one vector's WAV (and optional config) into `dir`; returns the
/// WAV path and any extra `decode` arguments.
fn render(name: &str, dir: &Path) -> (PathBuf, Vec<String>) {
    let plain = |spec: vectors::VectorSpec| {
        vectors::write_fixture_set(&spec, dir).unwrap();
        (dir.join(format!("{}.wav", spec.name)), Vec::new())
    };
    match name {
        "v1" => plain(vectors::v1()),
        "v2" => plain(vectors::v2()),
        "v3" => plain(vectors::v3()),
        "v4" => plain(vectors::v4()),
        "v5" => plain(vectors::v5()),
        "v6" => plain(vectors::v6()),
        "v7" => plain(vectors::v7()),
        "v8" => plain(vectors::v8()),
        "v8w" => plain(vectors::v8w()),
        "v10" => plain(vectors::v10()),
        "v34" => plain(vectors::v34()),
        "vr1" => plain(vectors::vr1()),
        "vr2" => plain(vectors::vr2()),
        "vr3" => plain(vectors::vr3()),
        "vr4" => plain(vectors::vr4()),
        "vr5" => plain(vectors::vr5()),
        "vr6a" => plain(vectors::vr6a()),
        "vr6b" => plain(vectors::vr6b()),
        "vr7" => plain(vectors::vr7()),
        "vr8" => plain(vectors::vr8()),
        "v9" | "v51" => {
            let spec = vectors::v9();
            vectors::write_v9_fixture_set(&spec, dir).unwrap();
            let mut args = Vec::new();
            if name == "v51" {
                let cfg = dir.join("v51.toml");
                std::fs::write(&cfg, "[decode]\nrefine_bw_hz = 30.0\n").unwrap();
                args = vec!["--config".to_string(), cfg.display().to_string()];
            }
            (dir.join("v9.wav"), args)
        }
        "v31" => {
            // As `golden_decimated_capture.rs`: V1 at 192 kHz, decimated by 4.
            let mut spec = vectors::v1();
            spec.fs = 192_000.0;
            let rendered = vectors::render(&spec).unwrap();
            let mut decimator = Decimator::new(192_000.0, 4).unwrap();
            let decimated: Vec<Complex32> = decimator.process(&rendered.samples);
            manta_testkit::wav::write_fixture(
                dir,
                "v31",
                &decimated,
                48_000.0,
                spec.center_freq_hz,
            )
            .unwrap();
            (dir.join("v31.wav"), Vec::new())
        }
        other => panic!("unknown vector {other}"),
    }
}

#[test]
#[ignore]
fn decode_fingerprints() {
    let names = [
        "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v8w", "v9", "v10", "v31", "v34", "v51",
        "vr1", "vr2", "vr3", "vr4", "vr5", "vr6a", "vr6b", "vr7", "vr8",
    ];
    let root = tempfile::tempdir().unwrap();
    let lines: Vec<Vec<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = names
            .iter()
            .map(|&name| {
                let dir = root.path().join(name);
                s.spawn(move || {
                    std::fs::create_dir_all(&dir).unwrap();
                    let (wav, extra) = render(name, &dir);
                    ["legacy", "hsmm"]
                        .iter()
                        .map(|engine| {
                            let out = Command::new(env!("CARGO_BIN_EXE_manta"))
                                .args(["decode", "--json", "--engine", engine])
                                .args(&extra)
                                .arg(&wav)
                                .output()
                                .unwrap();
                            let code = out.status.code().unwrap_or(-1);
                            assert!(
                                out.status.success() && !out.stdout.is_empty(),
                                "{name} {engine}: exit {code}, {} stdout bytes, stderr: {}",
                                out.stdout.len(),
                                String::from_utf8_lossy(&out.stderr)
                            );
                            let sha: String = Sha256::digest(&out.stdout)
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            format!(
                                "FINGERPRINT\t{name}\t{engine}\t{code}\t{}\t{sha}",
                                out.stdout.len()
                            )
                        })
                        .collect()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for line in lines.iter().flatten() {
        println!("{line}");
    }
    assert_eq!(lines.iter().flatten().count(), 2 * names.len());
}
