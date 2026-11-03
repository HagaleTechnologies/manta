//! A serializable, synthetic-only scene description (MAN-206).
//!
//! `SceneSpec` is what the failure-minimizer shrinks and what a committed
//! repro stores. It has no sample buffer and no constructor from samples or
//! files, and `origin` has one variant, `synthetic`, so a scene can only
//! describe something this testkit renders. Floats serialize as strings
//! written with Rust's shortest round-trip `Display` and read with
//! `str::parse`, so a scene renders the same bytes after a JSON round trip
//! (a plain JSON number changed 44 of 381 shipped values' bits).
//!
//! It represents what `vectors::render` renders. V9's drift staircase, the
//! RTTY/FT8/non-CW generators and MAN-312 crash scenes are not represented.

use crate::keyer::Jitter;
use crate::scene::{render_scene, QsbSine, SignalSpec, WattersonFade};
use crate::vectors::VectorSpec;
use anyhow::{ensure, Result};
use coppa_channel::watterson::WattersonPreset;
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

/// `SceneSpec::schema`.
pub const SCENE_SCHEMA: &str = "manta-testkit/scene/v1";
/// Bump when a change moves rendered bytes for an unchanged `SceneSpec`.
pub const GENERATOR_REVISION: u32 = 1;

/// Largest sample rate a scene may ask for, S/s.
pub const MAX_FS: f64 = 1_000_000.0;
/// Longest scene, seconds.
pub const MAX_DURATION_S: f64 = 600.0;

/// Where a scene came from. Only synthetic scenes exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Synthetic,
}

/// A whole scene: sample rate, duration, centre, noise and signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSpec {
    pub schema: String,
    pub origin: Origin,
    #[serde(with = "exact_f64")]
    pub fs: f64,
    #[serde(with = "exact_f64")]
    pub duration_s: f64,
    #[serde(with = "exact_f64")]
    pub center_freq_hz: f64,
    pub noise_seed: Option<u64>,
    pub signals: Vec<SceneSignal>,
}

/// Fade preset names in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadePreset {
    Good,
    Moderate,
    Poor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneJitter {
    #[serde(with = "exact_f32")]
    pub sigma: f32,
    pub seed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneQsb {
    #[serde(with = "exact_f32")]
    pub rate_hz: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneFade {
    pub preset: FadePreset,
    pub seed: u64,
}

/// One signal: a stable label plus every `SignalSpec` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSignal {
    pub label: String,
    pub text: String,
    pub loop_text: bool,
    #[serde(with = "exact_f32")]
    pub wpm: f32,
    #[serde(with = "exact_f64")]
    pub offset_hz: f64,
    #[serde(with = "exact_f32")]
    pub snr_2500_db: f32,
    pub jitter: Option<SceneJitter>,
    pub qsb: Option<SceneQsb>,
    pub watterson: Option<SceneFade>,
    #[serde(with = "exact_opt_f32")]
    pub char_wpm: Option<f32>,
    #[serde(with = "exact_f32")]
    pub weight: f32,
    #[serde(with = "exact_f32")]
    pub char_gap_units: f32,
    #[serde(with = "exact_f32")]
    pub word_gap_units: f32,
    #[serde(with = "exact_f64")]
    pub rise_ms: f64,
    #[serde(with = "exact_f32")]
    pub dit_units: f32,
    #[serde(with = "exact_f32")]
    pub element_gap_units: f32,
}

impl SceneSignal {
    /// Label `label` on a copy of `s`.
    pub fn from_signal(label: String, s: &SignalSpec) -> Self {
        SceneSignal {
            label,
            text: s.text.clone(),
            loop_text: s.loop_text,
            wpm: s.wpm,
            offset_hz: s.offset_hz,
            snr_2500_db: s.snr_2500_db,
            jitter: s.jitter.map(|j| SceneJitter {
                sigma: j.sigma,
                seed: j.seed,
            }),
            qsb: s.qsb.map(|q| SceneQsb { rate_hz: q.rate_hz }),
            watterson: s.watterson.map(|w| SceneFade {
                preset: match w.preset {
                    WattersonPreset::Good => FadePreset::Good,
                    WattersonPreset::Moderate => FadePreset::Moderate,
                    WattersonPreset::Poor => FadePreset::Poor,
                },
                seed: w.seed,
            }),
            char_wpm: s.char_wpm,
            weight: s.weight,
            char_gap_units: s.char_gap_units,
            word_gap_units: s.word_gap_units,
            rise_ms: s.rise_ms,
            dit_units: s.dit_units,
            element_gap_units: s.element_gap_units,
        }
    }

    /// The `SignalSpec` this signal renders as.
    pub fn signal_spec(&self) -> SignalSpec {
        SignalSpec {
            text: self.text.clone(),
            loop_text: self.loop_text,
            wpm: self.wpm,
            offset_hz: self.offset_hz,
            snr_2500_db: self.snr_2500_db,
            jitter: self.jitter.as_ref().map(|j| Jitter {
                sigma: j.sigma,
                seed: j.seed,
            }),
            qsb: self.qsb.as_ref().map(|q| QsbSine { rate_hz: q.rate_hz }),
            watterson: self.watterson.as_ref().map(|w| WattersonFade {
                preset: match w.preset {
                    FadePreset::Good => WattersonPreset::Good,
                    FadePreset::Moderate => WattersonPreset::Moderate,
                    FadePreset::Poor => WattersonPreset::Poor,
                },
                seed: w.seed,
            }),
            char_wpm: self.char_wpm,
            weight: self.weight,
            char_gap_units: self.char_gap_units,
            word_gap_units: self.word_gap_units,
            rise_ms: self.rise_ms,
            dit_units: self.dit_units,
            element_gap_units: self.element_gap_units,
        }
    }
}

impl SceneSpec {
    /// Describe a golden vector, with `spec.rise_ms` applied to every signal
    /// as `vectors::render` does. Labels are `s00`, `s01`, ... in order.
    pub fn from_vector(spec: &VectorSpec) -> Self {
        SceneSpec {
            schema: SCENE_SCHEMA.to_string(),
            origin: Origin::Synthetic,
            fs: spec.fs,
            duration_s: spec.duration_s,
            center_freq_hz: spec.center_freq_hz,
            noise_seed: Some(spec.noise_seed),
            signals: spec
                .signals
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    SceneSignal::from_signal(
                        format!("s{i:02}"),
                        &SignalSpec {
                            rise_ms: spec.rise_ms,
                            ..s.clone()
                        },
                    )
                })
                .collect(),
        }
    }

    /// A copy with a different duration.
    pub fn with_duration(&self, d: f64) -> Self {
        SceneSpec {
            duration_s: d,
            ..self.clone()
        }
    }

    /// The signals as `SignalSpec`s, in order.
    pub fn signal_specs(&self) -> Vec<SignalSpec> {
        self.signals.iter().map(SceneSignal::signal_spec).collect()
    }

    /// Check the schema and that labels are unique.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == SCENE_SCHEMA,
            "unknown scene schema {:?}",
            self.schema
        );
        let mut labels: Vec<&str> = self.signals.iter().map(|s| s.label.as_str()).collect();
        labels.sort_unstable();
        let n = labels.len();
        labels.dedup();
        ensure!(labels.len() == n, "signal labels are not unique");
        ensure!(
            self.fs.is_finite() && self.fs > 0.0 && self.fs <= MAX_FS,
            "fs must be in (0, {MAX_FS}]"
        );
        ensure!(
            self.duration_s.is_finite()
                && self.duration_s > 0.0
                && self.duration_s <= MAX_DURATION_S,
            "duration_s must be in (0, {MAX_DURATION_S}]"
        );
        ensure!(
            self.center_freq_hz.is_finite(),
            "center_freq_hz must be finite"
        );
        Ok(())
    }

    /// Render with `render_scene`: samples and keyed text per signal.
    pub fn render(&self) -> Result<(Vec<Complex32>, Vec<String>)> {
        self.validate()?;
        render_scene(
            &self.signal_specs(),
            self.fs,
            self.duration_s,
            self.noise_seed,
        )
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse and validate JSON.
    pub fn from_json(text: &str) -> Result<Self> {
        let s: SceneSpec = serde_json::from_str(text)?;
        s.validate()?;
        Ok(s)
    }
}

/// `f64` as a shortest round-trip decimal string.
pub(crate) mod exact_f64 {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        let t = String::deserialize(d)?;
        t.parse().map_err(D::Error::custom)
    }
}

/// `f32` as a shortest round-trip decimal string.
pub(crate) mod exact_f32 {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        let t = String::deserialize(d)?;
        t.parse().map_err(D::Error::custom)
    }
}

/// `Option<f32>` as a string or `null`.
pub(crate) mod exact_opt_f32 {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => s.serialize_some(&format!("{x}")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| t.parse().map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors;

    pub(crate) fn all_vectors() -> Vec<VectorSpec> {
        use vectors::*;
        vec![
            v1(),
            v2(),
            v3(),
            v4(),
            v5(),
            v6(),
            v7(),
            v8(),
            v8w(),
            v9(),
            v10(),
            v34(),
            vr1(),
            vr2(),
            vr3(),
            vr4(),
            vr5(),
            vr6a(),
            vr6b(),
            vr7(),
            vr8(),
            vw1(),
            vw2(),
            vw3(),
            vw4(),
            vw5(),
        ]
    }

    fn float_bits(s: &SceneSpec) -> Vec<u64> {
        let mut b = vec![
            s.fs.to_bits(),
            s.duration_s.to_bits(),
            s.center_freq_hz.to_bits(),
        ];
        for g in &s.signals {
            b.extend([
                g.wpm.to_bits() as u64,
                g.offset_hz.to_bits(),
                g.snr_2500_db.to_bits() as u64,
                g.char_wpm.map(|c| c.to_bits() as u64).unwrap_or(u64::MAX),
                g.weight.to_bits() as u64,
                g.char_gap_units.to_bits() as u64,
                g.word_gap_units.to_bits() as u64,
                g.rise_ms.to_bits(),
                g.dit_units.to_bits() as u64,
                g.element_gap_units.to_bits() as u64,
                g.jitter
                    .as_ref()
                    .map(|j| j.sigma.to_bits() as u64)
                    .unwrap_or(u64::MAX),
                g.qsb
                    .as_ref()
                    .map(|q| q.rate_hz.to_bits() as u64)
                    .unwrap_or(u64::MAX),
            ]);
        }
        b
    }

    #[test]
    fn every_shipped_vector_round_trips_bit_for_bit() {
        for v in all_vectors() {
            let s = SceneSpec::from_vector(&v);
            let back = SceneSpec::from_json(&s.to_json().unwrap()).unwrap();
            assert_eq!(float_bits(&s), float_bits(&back), "{}", v.name);
            assert_eq!(s, back, "{}", v.name);
        }
    }

    #[test]
    fn scene_render_matches_vectors_render() {
        for v in [vectors::v1(), vectors::v8()] {
            let short = VectorSpec {
                duration_s: 20.0,
                ..v.clone()
            };
            let want = vectors::render(&short).unwrap();
            let (got, texts) = SceneSpec::from_vector(&v)
                .with_duration(20.0)
                .render()
                .unwrap();
            assert_eq!(got, want.samples, "{}", v.name);
            assert_eq!(texts, want.keyed_texts, "{}", v.name);
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let s = SceneSpec::from_vector(&vectors::v1()).to_json().unwrap();
        let bad = s.replacen("\"origin\"", "\"samples\": [], \"origin\"", 1);
        assert!(SceneSpec::from_json(&bad).is_err());
        let bad = s.replacen("\"label\"", "\"wav\": \"x.wav\", \"label\"", 1);
        assert!(SceneSpec::from_json(&bad).is_err());
    }

    #[test]
    fn origin_other_than_synthetic_is_rejected() {
        let s = SceneSpec::from_vector(&vectors::v1()).to_json().unwrap();
        let bad = s.replace("\"synthetic\"", "\"recorded\"");
        assert!(SceneSpec::from_json(&bad).is_err());
    }

    #[test]
    fn labels_are_unique_and_stable() {
        let s = SceneSpec::from_vector(&vectors::v8w());
        let labels: Vec<_> = s.signals.iter().map(|g| g.label.clone()).collect();
        assert_eq!(labels.first().map(String::as_str), Some("s00"));
        assert_eq!(labels.last().map(String::as_str), Some("s49"));
        assert!(s.validate().is_ok());
        let mut dup = s.clone();
        dup.signals[1].label = "s00".into();
        assert!(dup.validate().is_err());
    }

    #[test]
    fn non_finite_or_huge_sizes_are_rejected() {
        let s = SceneSpec::from_vector(&vectors::v1()).to_json().unwrap();
        for bad in ["\"inf\"", "\"NaN\"", "\"1e12\""] {
            let j = s.replacen(
                "\"duration_s\": \"120\"",
                &format!("\"duration_s\": {bad}"),
                1,
            );
            assert_ne!(j, s);
            assert!(SceneSpec::from_json(&j).is_err(), "{bad}");
        }
    }

    #[test]
    fn floats_are_strings_in_json() {
        let j = SceneSpec::from_vector(&vectors::v8w()).to_json().unwrap();
        assert!(j.contains("\"offset_hz\": \""), "{j}");
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        fn no_float(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Number(n) => n.is_u64() || n.is_i64(),
                serde_json::Value::Array(a) => a.iter().all(no_float),
                serde_json::Value::Object(o) => o.values().all(no_float),
                _ => true,
            }
        }
        assert!(no_float(&v));
    }
}
