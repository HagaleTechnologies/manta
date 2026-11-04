//! Committed synthetic repros: format, digest and lint (MAN-206).
//!
//! A repro is a small JSON file holding a minimized [`SceneSpec`], the
//! failure signature its probe returns, and per-target digests of its
//! rendered samples. The lint keeps the repro directory synthetic-only: no
//! audio or IQ files, no file over 8 KiB, and no name that points at the
//! local real-audio corpus.

use crate::minimize::{Minimized, Signature};
use crate::scenespec::{Origin, SceneSpec, GENERATOR_REVISION};
use anyhow::{bail, ensure, Context, Result};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `Repro::schema`.
pub const REPRO_SCHEMA: &str = "manta-testkit/repro/v1";
/// Largest allowed repro file.
pub const MAX_REPRO_BYTES: u64 = 8 * 1024;

const CORPUS_MANIFEST: &str = include_str!("../audio-corpus/manifest.json");

/// What replay must find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// The failure is still present: the probe returns `signature`.
    Fails,
    /// The failure was fixed: the probe returns no signature.
    Passes,
}

/// One committed repro.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repro {
    pub schema: String,
    pub origin: Origin,
    pub name: String,
    pub found_in: String,
    pub generator_revision: u32,
    pub expect: Expect,
    pub probe: String,
    pub signature: Signature,
    /// FNV-1a digest of the rendered samples (hex), keyed by
    /// [`digest_key`].
    pub digests: BTreeMap<String, String>,
    pub scene: SceneSpec,
}

/// FNV-1a over the f32 bit patterns of `s` (re, then im, little-endian).
pub fn samples_digest(s: &[Complex32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in s {
        for v in [c.re, c.im] {
            for b in v.to_bits().to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    h
}

/// `"{ARCH}-{OS}"`: faded scenes go through coppa's SIMD-dispatched FFT, so
/// digests are recorded per target.
pub fn digest_key() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Build a repro from a minimizer result: renders the parsed-back scene and
/// records this target's digest. The caller re-confirms the signature.
pub fn from_minimized(m: &Minimized, name: &str, found_in: &str, probe: &str) -> Result<Repro> {
    let scene = SceneSpec::from_json(&m.scene.to_json()?)?;
    let (samples, _) = scene.render()?;
    let mut digests = BTreeMap::new();
    digests.insert(digest_key(), format!("{:016x}", samples_digest(&samples)));
    Ok(Repro {
        schema: REPRO_SCHEMA.into(),
        origin: Origin::Synthetic,
        name: name.into(),
        found_in: found_in.into(),
        generator_revision: GENERATOR_REVISION,
        expect: Expect::Fails,
        probe: probe.into(),
        signature: m.signature.clone(),
        digests,
        scene,
    })
}

impl Repro {
    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    /// Parse, then check the schema and the scene.
    pub fn from_json(text: &str) -> Result<Self> {
        let r: Repro = serde_json::from_str(text)?;
        ensure!(
            r.schema == REPRO_SCHEMA,
            "unknown repro schema '{}'",
            r.schema
        );
        r.scene.validate()?;
        Ok(r)
    }
}

fn corpus_markers() -> Vec<String> {
    let mut m: Vec<String> = [".wav", ".cf32", ".ci16", "audio-corpus"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(CORPUS_MANIFEST) {
        for rec in v["recordings"].as_array().into_iter().flatten() {
            for key in ["file", "sha256"] {
                if let Some(s) = rec[key].as_str() {
                    m.push(s.to_ascii_lowercase());
                }
            }
        }
    }
    m
}

/// Lint one repro's identity fields (`name`, `found_in`, `probe`, and every
/// signal's label and text) against the real-audio corpus.
pub fn lint_repro(r: &Repro) -> Result<()> {
    let mut fields = vec![&r.name, &r.found_in, &r.probe];
    for s in &r.scene.signals {
        fields.push(&s.label);
        fields.push(&s.text);
    }
    for field in fields {
        let low = field.to_ascii_lowercase();
        for marker in corpus_markers() {
            if low.contains(&marker) {
                bail!("repro '{}' names real recorded audio ('{marker}')", r.name);
            }
        }
    }
    Ok(())
}

/// Lint a repro directory and parse every repro in it, sorted by path.
/// Only `*.json` and `README.md` are allowed.
pub fn lint_dir(dir: &Path) -> Result<Vec<(PathBuf, Repro)>> {
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "README.md" {
            continue;
        }
        ensure!(
            p.is_file() && name.ends_with(".json"),
            "{} is not a repro: only *.json and README.md belong here",
            p.display()
        );
        let len = std::fs::metadata(&p)?.len();
        ensure!(
            len <= MAX_REPRO_BYTES,
            "{} is {len} bytes, over {MAX_REPRO_BYTES}",
            p.display()
        );
        let text = std::fs::read_to_string(&p)?;
        let r = Repro::from_json(&text).with_context(|| format!("parsing {}", p.display()))?;
        lint_repro(&r).with_context(|| p.display().to_string())?;
        out.push((p, r));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minimize::{minimize, Budget};
    use crate::vectors;

    fn small_repro() -> Repro {
        let mut scene = SceneSpec::from_vector(&vectors::v1()).with_duration(4.0);
        scene.fs = 8000.0;
        scene.signals[0].offset_hz = 1500.0;
        let m = minimize(&scene, Budget { max_probes: 4 }, |_, _| {
            Some(vec!["x".into()])
        })
        .unwrap();
        from_minimized(&m, "unit", "unit test", "none").unwrap()
    }

    #[test]
    fn repro_round_trips_and_rejects_unknown_fields() {
        let r = small_repro();
        let j = r.to_json().unwrap();
        assert_eq!(Repro::from_json(&j).unwrap(), r);
        let bad = j.replacen("\"name\"", "\"iq_path\": \"x\", \"name\"", 1);
        assert!(Repro::from_json(&bad).is_err());
    }

    #[test]
    fn lint_rejects_a_wav_or_cf32_file_in_the_directory() {
        for f in ["capture.wav", "capture.cf32"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(d.path().join(f), b"RIFF").unwrap();
            assert!(lint_dir(d.path()).is_err(), "{f}");
        }
    }

    #[test]
    fn lint_rejects_corpus_names_and_hashes() {
        let mut r = small_repro();
        r.found_in = "B2_20251129_000000_7080kHz.source.wav".into();
        assert!(lint_repro(&r).is_err());
        let mut r = small_repro();
        r.name = "74eba71e00fede9cab8067f9ff5a043396066015bbf8c23232788279b9f1c5f2".into();
        assert!(lint_repro(&r).is_err());
        let mut r = small_repro();
        r.scene.signals[0].label = "wpx_cw_iq_96khz.wav".into();
        assert!(lint_repro(&r).is_err());
        assert!(lint_repro(&small_repro()).is_ok());
    }

    #[test]
    fn lint_rejects_files_over_8_kib() {
        let d = tempfile::tempdir().unwrap();
        let mut r = small_repro();
        r.found_in = "x".repeat(9000);
        std::fs::write(d.path().join("big.json"), r.to_json().unwrap()).unwrap();
        assert!(lint_dir(d.path()).is_err());
    }

    #[test]
    fn lint_accepts_readme_and_json() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("README.md"), "repros").unwrap();
        std::fs::write(d.path().join("a.json"), small_repro().to_json().unwrap()).unwrap();
        let got = lint_dir(d.path()).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn digest_key_is_arch_dash_os() {
        let k = digest_key();
        assert_eq!(
            k,
            format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
        );
        assert!(k.contains('-'));
    }
}
