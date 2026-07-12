//! Text -> keyed CW envelope: raised-cosine edges, optional timing jitter.
//! SPEC §7 preamble.

use crate::gaussian_pair;
use anyhow::{bail, Result};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use skimmer_decode::tree::pattern_for;

/// Per-segment timing jitter for the keyer. SPEC §7.
#[derive(Debug, Clone, Copy)]
pub struct Jitter {
    /// Fractional sigma per timing segment (SPEC §7: 8 % where stated).
    pub sigma: f32,
    pub seed: u64,
}

/// Keying parameters: speed, edge shape, optional jitter. SPEC §7.
#[derive(Debug, Clone, Copy)]
pub struct KeyerSpec {
    pub wpm: f32,
    /// Raised-cosine rise/fall, contained inside the element. SPEC §7: 5 ms.
    pub rise_ms: f64,
    pub jitter: Option<Jitter>,
}

impl KeyerSpec {
    /// A clean keyer at `wpm` with 5 ms raised-cosine edges and no jitter. SPEC §7.
    pub fn new(wpm: f32) -> Self {
        KeyerSpec {
            wpm,
            rise_ms: 5.0,
            jitter: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    on: bool,
    dur_ms: f64,
}

struct SegmentBuilder {
    segs: Vec<Segment>,
    rng: Option<(ChaCha8Rng, f64)>,
}

impl SegmentBuilder {
    fn new(jitter: Option<Jitter>) -> Self {
        let rng = jitter.map(|j| (ChaCha8Rng::seed_from_u64(j.seed), j.sigma as f64));
        SegmentBuilder {
            segs: Vec::new(),
            rng,
        }
    }

    fn push(&mut self, on: bool, nominal_ms: f64) {
        let dur_ms = match &mut self.rng {
            None => nominal_ms,
            Some((rng, sigma)) => {
                let (z, _) = gaussian_pair(rng);
                nominal_ms * (1.0 + *sigma * z.clamp(-3.0, 3.0))
            }
        };
        self.segs.push(Segment { on, dur_ms });
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// Append one word's segments. `unit` = dit ms. Returns Err on unknown chars.
fn push_word(b: &mut SegmentBuilder, word: &str, unit: f64) -> Result<()> {
    let chars: Vec<char> = word.chars().collect();
    for (ci, c) in chars.iter().enumerate() {
        let Some(pattern) = pattern_for(*c) else {
            bail!("character {c:?} has no Morse encoding");
        };
        let els: Vec<char> = pattern.chars().collect();
        for (ei, e) in els.iter().enumerate() {
            b.push(true, if *e == '.' { unit } else { 3.0 * unit });
            if ei < els.len() - 1 {
                b.push(false, unit);
            }
        }
        if ci < chars.len() - 1 {
            b.push(false, 3.0 * unit);
        }
    }
    Ok(())
}

fn render(segs: &[Segment], rise_ms: f64, fs: f64, total_samples: Option<usize>) -> Vec<f32> {
    let total_ms: f64 = segs.iter().map(|s| s.dur_ms).sum();
    let n = total_samples.unwrap_or((total_ms / 1000.0 * fs).round() as usize);
    let mut env = vec![0.0f32; n];
    let mut seg_idx = 0usize;
    let mut seg_start_ms = 0.0f64;
    for (i, v) in env.iter_mut().enumerate() {
        let t_ms = i as f64 * 1000.0 / fs;
        while seg_idx < segs.len() && t_ms >= seg_start_ms + segs[seg_idx].dur_ms {
            seg_start_ms += segs[seg_idx].dur_ms;
            seg_idx += 1;
        }
        if seg_idx >= segs.len() {
            break; // zero tail
        }
        let seg = segs[seg_idx];
        if !seg.on {
            continue;
        }
        let t_in = t_ms - seg_start_ms;
        let rise = rise_ms.min(seg.dur_ms / 2.0);
        let up = if t_in < rise {
            0.5 * (1.0 - (std::f64::consts::PI * t_in / rise).cos())
        } else {
            1.0
        };
        let t_rem = seg.dur_ms - t_in;
        let down = if t_rem < rise {
            0.5 * (1.0 - (std::f64::consts::PI * t_rem / rise).cos())
        } else {
            1.0
        };
        *v = up.min(down) as f32;
    }
    env
}

/// Key `text` once. Returns (envelope at fs, normalized keyed text). SPEC §7.
pub fn key_text(text: &str, spec: &KeyerSpec, fs: f64) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let unit = 1200.0 / spec.wpm as f64;
    let mut b = SegmentBuilder::new(spec.jitter);
    let words: Vec<&str> = norm.split(' ').collect();
    for (wi, w) in words.iter().enumerate() {
        push_word(&mut b, w, unit)?;
        if wi < words.len() - 1 {
            b.push(false, 7.0 * unit);
        }
    }
    let env = render(&b.segs, spec.rise_ms, fs, None);
    Ok((env, norm))
}

/// Key `text` repeatedly (7-dit gaps between repetitions) until `duration_s`.
/// Characters are keyed only if they fit entirely (pinned decision 13). SPEC §7.
pub fn key_text_loop(
    text: &str,
    spec: &KeyerSpec,
    fs: f64,
    duration_s: f64,
) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let unit = 1200.0 / spec.wpm as f64;
    let budget_ms = duration_s * 1000.0;
    let mut b = SegmentBuilder::new(spec.jitter);
    let mut keyed = String::new();
    let mut elapsed = 0.0f64;
    'outer: loop {
        let words: Vec<&str> = norm.split(' ').collect();
        for (wi, w) in words.iter().enumerate() {
            let chars: Vec<char> = w.chars().collect();
            for (ci, c) in chars.iter().enumerate() {
                // Try the character into a scratch builder to measure it.
                let mut scratch = SegmentBuilder {
                    segs: Vec::new(),
                    rng: b.rng.take(),
                };
                push_word(&mut scratch, &c.to_string(), unit)?;
                let char_ms: f64 = scratch.segs.iter().map(|s| s.dur_ms).sum();
                b.rng = scratch.rng.take();
                if elapsed + char_ms > budget_ms {
                    break 'outer;
                }
                b.segs.extend(scratch.segs);
                elapsed += char_ms;
                keyed.push(*c);
                if ci < chars.len() - 1 {
                    b.push(false, 3.0 * unit);
                    elapsed += b.segs.last().unwrap().dur_ms;
                }
            }
            if wi < words.len() - 1 {
                b.push(false, 7.0 * unit);
                elapsed += b.segs.last().unwrap().dur_ms;
                keyed.push(' ');
            }
        }
        b.push(false, 7.0 * unit);
        elapsed += b.segs.last().unwrap().dur_ms;
        keyed.push(' ');
        if elapsed >= budget_ms {
            break;
        }
    }
    let n = (duration_s * fs).round() as usize;
    let env = render(&b.segs, spec.rise_ms, fs, Some(n));
    Ok((env, keyed.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn on_runs_ms(env: &[f32], fs: f64) -> Vec<f64> {
        // Durations of contiguous >0.5 stretches, in ms.
        let mut runs = Vec::new();
        let mut count = 0usize;
        for &v in env {
            if v > 0.5 {
                count += 1;
            } else if count > 0 {
                runs.push(count as f64 * 1000.0 / fs);
                count = 0;
            }
        }
        if count > 0 {
            runs.push(count as f64 * 1000.0 / fs);
        }
        runs
    }

    #[test]
    fn paris_mark_durations_at_20wpm() {
        // PARIS = .--. .- .-. .. ... => 10 dits, 4 dahs.
        let (env, text) = key_text("PARIS", &KeyerSpec::new(20.0), FS).unwrap();
        assert_eq!(text, "PARIS");
        let runs = on_runs_ms(&env, FS);
        assert_eq!(runs.len(), 14);
        // Above-half-amplitude width of a raised-cosine-edged element equals
        // its nominal duration minus rise_ms (5 ms lost at half-height).
        let dits = runs.iter().filter(|&&r| r < 100.0).count();
        let dahs = runs.iter().filter(|&&r| r >= 100.0).count();
        assert_eq!((dits, dahs), (10, 4));
        for r in &runs {
            let nominal = if *r < 100.0 { 60.0 } else { 180.0 };
            assert!(
                (r - (nominal - 5.0)).abs() < 1.0,
                "run {r} vs nominal {nominal}"
            );
        }
    }

    #[test]
    fn edges_are_raised_cosine_not_clicks() {
        let (env, _) = key_text("E", &KeyerSpec::new(20.0), FS).unwrap();
        // No sample-to-sample step exceeds what a 5 ms raised cosine allows.
        let max_step = (std::f64::consts::PI / (0.005 * FS) / 2.0) as f32 * 1.1;
        for w in env.windows(2) {
            assert!(
                (w[1] - w[0]).abs() <= max_step,
                "click: {} -> {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn word_gap_is_seven_dits() {
        let (env, _) = key_text("E E", &KeyerSpec::new(20.0), FS).unwrap();
        // envelope: dit, 7-dit gap, dit => total 9 dits = 540 ms
        assert_eq!(env.len(), (0.540 * FS) as usize);
    }

    #[test]
    fn loop_pads_to_duration_and_truncates_whole_chars() {
        // "CQ K" at 20 WPM = 43 dit units = 2.58 s per repetition, +7u gap:
        // two full repetitions end at 5.58 s, so 6 s holds exactly "CQ K CQ K"
        // and a truncated start of the third.
        let (env, text) = key_text_loop("CQ K", &KeyerSpec::new(20.0), FS, 6.0).unwrap();
        assert_eq!(env.len(), (6.0 * FS) as usize);
        assert!(text.starts_with("CQ K CQ K"), "{text}");
        // Truncation is at character granularity: no empty words.
        for word in text.split(' ') {
            assert!(!word.is_empty());
        }
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let spec = KeyerSpec {
            wpm: 20.0,
            rise_ms: 5.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: 42,
            }),
        };
        let (a, _) = key_text("PARIS", &spec, FS).unwrap();
        let (b, _) = key_text("PARIS", &spec, FS).unwrap();
        assert_eq!(a, b, "same seed must give identical envelopes");
        let runs = on_runs_ms(&a, FS);
        for r in &runs {
            let nominal = if *r < 100.0 { 60.0 } else { 180.0 };
            // 8 % sigma clamped at 3 sigma => within 24 % + edge loss
            assert!((r - (nominal - 5.0)).abs() < nominal * 0.25, "run {r}");
        }
        let (c, _) = key_text(
            "PARIS",
            &KeyerSpec {
                jitter: Some(Jitter {
                    sigma: 0.08,
                    seed: 43,
                }),
                ..spec
            },
            FS,
        )
        .unwrap();
        assert_ne!(a, c, "different seed must differ");
    }

    #[test]
    fn unknown_character_errors() {
        assert!(key_text("A#B", &KeyerSpec::new(20.0), FS).is_err());
    }
}
