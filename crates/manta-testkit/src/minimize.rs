//! Failure-minimizer over synthetic scenes (MAN-206).
//!
//! Shrinks a failing [`SceneSpec`] by duration, signals, text and
//! impairments, accepting a step only when the probe returns exactly the
//! original failure signature (one failure per repro; shrinking can turn
//! the target failure into a different one). It runs on `SceneSpec`, which
//! has no constructor from samples or files, so a minimized scene is
//! synthetic by construction; real recorded IQ is never an input.
//!
//! Pass order per round: duration (halve, floor 2 s), signals (ddmin, keep
//! at least one), text (ddmin over words, then over each word's
//! characters, keep at least one), impairments (per signal: drop jitter,
//! QSB, Watterson, Farnsworth, then reset timing to standard). Rounds repeat
//! until one accepts nothing or the probe budget is spent. Each signal's
//! unscaled contribution is cached and mixed with `scene::mix`, which
//! `render_scene` itself uses, so the probe sees exactly the bytes
//! `SceneSpec::render` produces.

use crate::scene::{mix, render_signal};
use crate::scenespec::{SceneSignal, SceneSpec};
use anyhow::{bail, Result};
use num_complex::Complex32;
use std::collections::HashMap;

/// A failure signature: sorted, deduplicated.
pub type Signature = Vec<String>;

/// Most bytes of signal contributions the minimizer keeps cached.
pub const CACHE_BYTES: usize = 1 << 30;

/// Shortest duration the duration pass tries, seconds.
pub const MIN_DURATION_S: f64 = 2.0;

/// The probe budget, counting the first probe of the original scene.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub max_probes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget { max_probes: 200 }
    }
}

/// One attempted change.
#[derive(Debug, Clone)]
pub struct Step {
    pub pass: &'static str,
    pub change: String,
    pub accepted: bool,
    /// What the probe returned for the candidate.
    pub signature: Option<Signature>,
}

/// The minimizer's result.
#[derive(Debug, Clone)]
pub struct Minimized {
    pub original: SceneSpec,
    pub scene: SceneSpec,
    pub signature: Signature,
    pub probes: usize,
    pub steps: Vec<Step>,
}

impl Minimized {
    /// The step log, one line per step.
    pub fn story(&self) -> String {
        self.steps
            .iter()
            .map(|s| {
                let got = match &s.signature {
                    Some(sig) => format!("[{}]", sig.join(", ")),
                    None => "passes".to_string(),
                };
                format!(
                    "{:<11} {:<8} {} -> {got}\n",
                    s.pass,
                    if s.accepted { "accepted" } else { "rejected" },
                    s.change,
                )
            })
            .collect()
    }
}

/// Sort and deduplicate a signature.
pub fn normalize_signature(mut s: Signature) -> Signature {
    s.sort();
    s.dedup();
    s
}

type Contribution = (Vec<Complex32>, String);

struct Runner<'p, P> {
    probe: &'p mut P,
    cache: HashMap<(String, u64), Contribution>,
    target: Signature,
    probes: usize,
    budget: usize,
    steps: Vec<Step>,
}

impl<P> Runner<'_, P>
where
    P: FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
{
    fn render(&mut self, scene: &SceneSpec) -> Result<Vec<Complex32>> {
        scene.validate()?;
        let n = (scene.duration_s * scene.fs).round() as usize;
        // Keep only this duration's contributions, and at most
        // CACHE_BYTES of them; a signal past the cap is rendered per probe.
        let d = scene.duration_s.to_bits();
        self.cache.retain(|k, _| k.1 == d);
        let mut cached_bytes: usize = self
            .cache
            .values()
            .map(|c| c.0.len() * std::mem::size_of::<Complex32>())
            .sum();
        let mut fresh: Vec<Vec<Complex32>> = Vec::new();
        let mut slots: Vec<Result<(String, u64), usize>> = Vec::with_capacity(scene.signals.len());
        for sig in &scene.signals {
            let key = (serde_json::to_string(sig)?, d);
            if self.cache.contains_key(&key) {
                slots.push(Ok(key));
                continue;
            }
            let c = render_signal(&sig.signal_spec(), scene.fs, scene.duration_s)?;
            let bytes = c.0.len() * std::mem::size_of::<Complex32>();
            if cached_bytes + bytes <= CACHE_BYTES {
                cached_bytes += bytes;
                self.cache.insert(key.clone(), c);
                slots.push(Ok(key));
            } else {
                slots.push(Err(fresh.len()));
                fresh.push(c.0);
            }
        }
        let parts: Vec<&[Complex32]> = slots
            .iter()
            .map(|s| match s {
                Ok(k) => self.cache[k].0.as_slice(),
                Err(i) => fresh[*i].as_slice(),
            })
            .collect();
        Ok(mix(&parts, n, scene.noise_seed))
    }

    fn exhausted(&self) -> bool {
        self.probes >= self.budget
    }

    /// Probe a candidate; `Some(true)` when the target failure holds,
    /// `None` when the budget is spent.
    fn holds(
        &mut self,
        pass: &'static str,
        change: String,
        scene: &SceneSpec,
    ) -> Result<Option<bool>> {
        if self.exhausted() {
            return Ok(None);
        }
        let samples = self.render(scene)?;
        self.probes += 1;
        let got = (self.probe)(scene, &samples).map(normalize_signature);
        let accepted = got.as_ref() == Some(&self.target);
        self.steps.push(Step {
            pass,
            change,
            accepted,
            signature: got,
        });
        Ok(Some(accepted))
    }
}

/// Zeller's ddmin (complement form): the smallest sublist of `items`, at
/// least one long, for which `test` holds. `test` returns `None` when the
/// budget is spent, which stops the search with the current list.
fn ddmin<T: Clone>(
    items: Vec<T>,
    test: &mut dyn FnMut(&[T]) -> Result<Option<bool>>,
) -> Result<Vec<T>> {
    let mut cur = items;
    let mut parts = 2usize;
    while cur.len() >= 2 {
        let len = cur.len();
        let chunk = len.div_ceil(parts);
        let mut reduced = false;
        for start in (0..len).step_by(chunk) {
            let end = (start + chunk).min(len);
            let complement: Vec<T> = cur[..start].iter().chain(&cur[end..]).cloned().collect();
            if complement.is_empty() {
                continue;
            }
            match test(&complement)? {
                None => return Ok(cur),
                Some(true) => {
                    cur = complement;
                    parts = (parts - 1).max(2);
                    reduced = true;
                    break;
                }
                Some(false) => {}
            }
        }
        if !reduced {
            if parts >= len {
                break;
            }
            parts = (parts * 2).min(len);
        }
    }
    Ok(cur)
}

/// Shrink `scene` while `probe` keeps returning its original signature.
/// Errors when the original scene does not fail.
pub fn minimize(
    scene: &SceneSpec,
    budget: Budget,
    mut probe: impl FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
) -> Result<Minimized> {
    // Canonicalize through the exact JSON format, which checks it too.
    let original = SceneSpec::from_json(&scene.to_json()?)?;
    let mut r = Runner {
        probe: &mut probe,
        cache: HashMap::new(),
        target: Vec::new(),
        probes: 0,
        budget: budget.max_probes.max(1),
        steps: Vec::new(),
    };
    let samples = r.render(&original)?;
    r.probes += 1;
    let Some(target) = (r.probe)(&original, &samples).map(normalize_signature) else {
        bail!("the scene does not fail: the probe returned no signature");
    };
    r.steps.push(Step {
        pass: "original",
        change: format!(
            "{} signals, {} s",
            original.signals.len(),
            original.duration_s
        ),
        accepted: true,
        signature: Some(target.clone()),
    });
    r.target = target.clone();
    let mut cur = original.clone();
    loop {
        let before = cur.clone();
        cur = duration_pass(&mut r, cur)?;
        cur = signal_pass(&mut r, cur)?;
        cur = text_pass(&mut r, cur)?;
        cur = impairment_pass(&mut r, cur)?;
        if cur == before || r.exhausted() {
            break;
        }
    }
    Ok(Minimized {
        original,
        scene: cur,
        signature: target,
        probes: r.probes,
        steps: r.steps,
    })
}

fn duration_pass<P>(r: &mut Runner<'_, P>, mut cur: SceneSpec) -> Result<SceneSpec>
where
    P: FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
{
    loop {
        let d = (cur.duration_s / 2.0).max(MIN_DURATION_S);
        if d >= cur.duration_s {
            return Ok(cur);
        }
        let cand = cur.with_duration(d);
        match r.holds("duration", format!("{} s -> {d} s", cur.duration_s), &cand)? {
            Some(true) => cur = cand,
            _ => return Ok(cur),
        }
    }
}

fn signal_pass<P>(r: &mut Runner<'_, P>, cur: SceneSpec) -> Result<SceneSpec>
where
    P: FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
{
    let base = cur.clone();
    let kept = ddmin(cur.signals.clone(), &mut |sub: &[SceneSignal]| {
        let cand = SceneSpec {
            signals: sub.to_vec(),
            ..base.clone()
        };
        let labels: Vec<&str> = sub.iter().map(|s| s.label.as_str()).collect();
        r.holds("signals", format!("keep {}", labels.join(",")), &cand)
    })?;
    Ok(SceneSpec {
        signals: kept,
        ..cur
    })
}

fn with_signal(scene: &SceneSpec, i: usize, sig: SceneSignal) -> SceneSpec {
    let mut s = scene.clone();
    s.signals[i] = sig;
    s
}

fn text_pass<P>(r: &mut Runner<'_, P>, mut cur: SceneSpec) -> Result<SceneSpec>
where
    P: FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
{
    for i in 0..cur.signals.len() {
        let words: Vec<String> = cur.signals[i]
            .text
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let base = cur.clone();
        let label = cur.signals[i].label.clone();
        let kept = ddmin(words, &mut |sub: &[String]| {
            let mut sig = base.signals[i].clone();
            sig.text = sub.join(" ");
            let change = format!("{label} text '{}'", sig.text);
            r.holds("text", change, &with_signal(&base, i, sig))
        })?;
        cur.signals[i].text = kept.join(" ");
        for w in 0..kept.len() {
            let words: Vec<String> = cur.signals[i].text.split(' ').map(str::to_string).collect();
            let chars: Vec<char> = words[w].chars().collect();
            let base = cur.clone();
            let kept_chars = ddmin(chars, &mut |sub: &[char]| {
                let mut ws = words.clone();
                ws[w] = sub.iter().collect();
                let mut sig = base.signals[i].clone();
                sig.text = ws.join(" ");
                let change = format!("{label} text '{}'", sig.text);
                r.holds("text", change, &with_signal(&base, i, sig))
            })?;
            let mut ws = words.clone();
            ws[w] = kept_chars.iter().collect();
            cur.signals[i].text = ws.join(" ");
        }
    }
    Ok(cur)
}

fn impairment_pass<P>(r: &mut Runner<'_, P>, mut cur: SceneSpec) -> Result<SceneSpec>
where
    P: FnMut(&SceneSpec, &[Complex32]) -> Option<Signature>,
{
    type Edit = (&'static str, fn(&SceneSignal) -> Option<SceneSignal>);
    let edits: [Edit; 5] = [
        ("drop jitter", |s| {
            s.jitter.as_ref().map(|_| SceneSignal {
                jitter: None,
                ..s.clone()
            })
        }),
        ("drop qsb", |s| {
            s.qsb.as_ref().map(|_| SceneSignal {
                qsb: None,
                ..s.clone()
            })
        }),
        ("drop watterson", |s| {
            s.watterson.as_ref().map(|_| SceneSignal {
                watterson: None,
                ..s.clone()
            })
        }),
        ("drop farnsworth", |s| {
            s.char_wpm.map(|_| SceneSignal {
                char_wpm: None,
                ..s.clone()
            })
        }),
        ("standard timing", |s| {
            let std = SceneSignal {
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
                dit_units: 1.0,
                element_gap_units: 1.0,
                ..s.clone()
            };
            (std != *s).then_some(std)
        }),
    ];
    for i in 0..cur.signals.len() {
        for (name, edit) in edits {
            let Some(sig) = edit(&cur.signals[i]) else {
                continue;
            };
            let cand = with_signal(&cur, i, sig);
            let change = format!("{} {name}", cur.signals[i].label);
            match r.holds("impairment", change, &cand)? {
                Some(true) => cur = cand,
                Some(false) => {}
                None => return Ok(cur),
            }
        }
    }
    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenespec::{FadePreset, Origin, SceneFade, SceneJitter, SCENE_SCHEMA};

    fn sig(label: &str, offset_hz: f64) -> SceneSignal {
        SceneSignal {
            label: label.into(),
            text: "CQ DE K1ABC Q K".into(),
            loop_text: true,
            wpm: 25.0,
            offset_hz,
            snr_2500_db: 10.0,
            jitter: Some(SceneJitter {
                sigma: 0.08,
                seed: 1,
            }),
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
            dit_units: 1.0,
            element_gap_units: 1.0,
        }
    }

    fn scene() -> SceneSpec {
        SceneSpec {
            schema: SCENE_SCHEMA.into(),
            origin: Origin::Synthetic,
            fs: 8000.0,
            duration_s: 6.0,
            center_freq_hz: 7_000_000.0,
            noise_seed: Some(5),
            signals: vec![
                sig("a", -2000.0),
                sig("b", -700.0),
                sig("c", 900.0),
                sig("d", 2500.0),
            ],
        }
    }

    fn has(s: &SceneSpec, label: &str) -> bool {
        s.signals.iter().any(|g| g.label == label)
    }

    #[test]
    fn drops_every_signal_the_failure_does_not_need() {
        let m = minimize(&scene(), Budget::default(), |s, _| {
            has(s, "b").then(|| vec!["needs-b".to_string()])
        })
        .unwrap();
        let labels: Vec<_> = m.scene.signals.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["b"]);
        assert_eq!(m.signature, ["needs-b"]);
    }

    #[test]
    fn keeps_a_signal_whose_removal_changes_the_signature() {
        let m = minimize(&scene(), Budget::default(), |s, _| {
            has(s, "b").then(|| {
                let mut v = vec!["needs-b".to_string()];
                if has(s, "c") {
                    v.push("c-present".into());
                }
                v
            })
        })
        .unwrap();
        let labels: Vec<_> = m.scene.signals.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["b", "c"]);
    }

    #[test]
    fn shortens_duration_while_the_failure_holds() {
        let m = minimize(&scene(), Budget::default(), |s, _| {
            (s.duration_s >= 3.0).then(|| vec!["long".to_string()])
        })
        .unwrap();
        assert_eq!(m.scene.duration_s, 3.0);
        let tried: Vec<_> = m
            .steps
            .iter()
            .filter(|s| s.pass == "duration")
            .map(|s| (s.change.clone(), s.accepted))
            .collect();
        assert_eq!(tried[0], ("6 s -> 3 s".to_string(), true));
        assert_eq!(tried[1], ("3 s -> 2 s".to_string(), false));
    }

    #[test]
    fn shortens_text_to_the_words_and_characters_that_matter() {
        let m = minimize(&scene(), Budget::default(), |s, _| {
            s.signals
                .iter()
                .any(|g| g.label == "b" && g.text.contains('Q'))
                .then(|| vec!["q-in-b".to_string()])
        })
        .unwrap();
        let b = m.scene.signals.iter().find(|g| g.label == "b").unwrap();
        assert_eq!(b.text, "Q");
    }

    #[test]
    fn drops_impairments_the_failure_does_not_need() {
        let mut s = scene();
        s.signals[1].watterson = Some(SceneFade {
            preset: FadePreset::Good,
            seed: 3,
        });
        let m = minimize(&s, Budget::default(), |s, _| {
            s.signals
                .iter()
                .any(|g| g.label == "b" && g.watterson.is_some())
                .then(|| vec!["faded-b".to_string()])
        })
        .unwrap();
        let b = m.scene.signals.iter().find(|g| g.label == "b").unwrap();
        assert!(b.jitter.is_none());
        assert!(b.watterson.is_some());
    }

    #[test]
    fn respects_the_probe_budget() {
        let mut calls = 0usize;
        let m = minimize(&scene(), Budget { max_probes: 5 }, |s, _| {
            calls += 1;
            has(s, "b").then(|| vec!["needs-b".to_string()])
        })
        .unwrap();
        assert_eq!(calls, 5);
        assert_eq!(m.probes, 5);
    }

    #[test]
    fn reports_every_probe_in_the_step_log() {
        let m = minimize(&scene(), Budget::default(), |s, _| {
            has(s, "b").then(|| vec!["needs-b".to_string()])
        })
        .unwrap();
        assert_eq!(m.steps.len(), m.probes);
        assert!(m.story().lines().count() == m.steps.len());
    }

    #[test]
    fn refuses_a_scene_that_does_not_fail() {
        assert!(minimize(&scene(), Budget::default(), |_, _| None).is_err());
    }

    #[test]
    fn probe_sees_the_bytes_scenespec_render_produces() {
        let mut checked = 0usize;
        minimize(&scene(), Budget { max_probes: 40 }, |s, samples| {
            assert_eq!(samples, s.render().unwrap().0.as_slice());
            checked += 1;
            has(s, "b").then(|| vec!["needs-b".to_string()])
        })
        .unwrap();
        assert!(checked > 5);
    }

    #[test]
    fn is_deterministic() {
        let run = || {
            minimize(&scene(), Budget::default(), |s, _| {
                has(s, "c").then(|| vec!["needs-c".to_string()])
            })
            .unwrap()
        };
        let (a, b) = (run(), run());
        assert_eq!(a.scene, b.scene);
        assert_eq!(a.probes, b.probes);
    }
}
