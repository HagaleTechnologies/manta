//! MAN-493: plain-data keying truth for the synthetic oracle-bypass
//! experiments (`known-transitions`, `known-transcript`). Everything here
//! is data -- no engine types -- because `manta-engine` can only reach
//! this crate as a dev-dependency: the engine's synthetic runner takes
//! what these builders produce as arguments. The segment math mirrors
//! `keyer`'s own (`units_ms`, `push_word`) over
//! `manta_decode::tree::pattern_for`, so a timeline built here keys the
//! same text at the same instants `keyer::key_text` would; the module's
//! test pins that equivalence.

use crate::keyer::KeyerSpec;
use anyhow::Result;
use manta_decode::tree::pattern_for;

/// One on/off keying segment: (key down, duration in ms).
pub type Segment = (bool, f64);

/// One known word with the input-sample timestamp it starts at.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptItem {
    pub word: String,
    pub start_ts: u64,
    /// Each character's keyed span, `(first element start, last element
    /// end)` in input samples, so a consumer can place characters and the
    /// word end at their real times.
    pub chars: Vec<(u64, u64)>,
}

/// `keyer`'s private unit math, mirrored: content unit (dit) and spacing
/// unit (inter-character/word gap), in ms. SPEC §7 V10's Farnsworth
/// derivation.
fn units_ms(word_wpm: f32, char_wpm: Option<f32>) -> (f64, f64) {
    let content_wpm = char_wpm.unwrap_or(word_wpm);
    let content_unit = 1200.0 / content_wpm as f64;
    let gap_unit = match char_wpm {
        None => content_unit,
        Some(_) => {
            let target_word_ms = 50.0 * 1200.0 / word_wpm as f64;
            (target_word_ms - 31.0 * content_unit) / 19.0
        }
    };
    (content_unit, gap_unit)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// The on/off keying timeline of `text` under `spec`, exactly the
/// segments `keyer::key_text` renders: one (on, ms) pair per mark and
/// gap, in order. Errors on a character with no Morse encoding.
pub fn keying_segments(text: &str, spec: &KeyerSpec) -> Result<Vec<Segment>> {
    let norm = normalize(text);
    let (unit, gap_unit) = units_ms(spec.wpm, spec.char_wpm);
    let mut segs: Vec<Segment> = Vec::new();
    let words: Vec<&str> = norm.split(' ').collect();
    for (wi, w) in words.iter().enumerate() {
        let chars: Vec<char> = w.chars().collect();
        for (ci, c) in chars.iter().enumerate() {
            let Some(pattern) = pattern_for(*c) else {
                anyhow::bail!("character '{}' has no Morse encoding", c.escape_debug());
            };
            let els: Vec<char> = pattern.chars().collect();
            for (ei, e) in els.iter().enumerate() {
                segs.push((
                    true,
                    if *e == '.' {
                        spec.dit_units as f64 * unit
                    } else {
                        spec.weight as f64 * unit
                    },
                ));
                if ei < els.len() - 1 {
                    segs.push((false, spec.element_gap_units as f64 * unit));
                }
            }
            if ci < chars.len() - 1 {
                segs.push((false, spec.char_gap_units as f64 * gap_unit));
            }
        }
        if wi < words.len() - 1 {
            segs.push((false, spec.word_gap_units as f64 * gap_unit));
        }
    }
    Ok(segs)
}

/// Sample a segment timeline at the decoder's hop rate (`hop_hz`, 375 Hz
/// at every production rate): returns (input sample timestamp, amplitude
/// 0 or 1) per hop, starting `t0_s` seconds into the stream at `fs`
/// samples per second, for `duration_s` seconds of stream. The hard
/// edges are the point -- each hop's keying state is the truth the
/// decoder would have to infer.
pub fn envelope_at_hops(
    segments: &[Segment],
    hop_hz: f64,
    fs: f64,
    t0_s: f64,
    duration_s: f64,
) -> Vec<(u64, f32)> {
    let t_end_ms = duration_s * 1000.0;
    let samples_per_hop = fs / hop_hz;
    let t0 = t0_s * fs;
    let mut out = Vec::new();
    let mut i = 0u64;
    loop {
        let t_ms = i as f64 * 1000.0 / hop_hz;
        if t_ms >= t_end_ms {
            break;
        }
        let mut acc = 0.0f64;
        let mut on = false;
        for &(seg_on, dur) in segments {
            if t_ms < acc + dur {
                on = seg_on;
                break;
            }
            acc += dur;
        }
        out.push((
            (t0 + i as f64 * samples_per_hop).round() as u64,
            if on { 1.0 } else { 0.0 },
        ));
        i += 1;
    }
    out
}

/// The known words with their leading input-sample timestamps: word
/// starts where a word gap (or the stream start) precedes it, in the same
/// normalization the keyer uses, timed by the same unit math
/// [`keying_segments`] walks.
pub fn transcript_items(
    text: &str,
    spec: &KeyerSpec,
    fs: f64,
    t0_s: f64,
) -> Result<Vec<TranscriptItem>> {
    let norm = normalize(text);
    let (unit, gap_unit) = units_ms(spec.wpm, spec.char_wpm);
    let word_gap_ms = spec.word_gap_units as f64 * gap_unit;
    let char_gap_ms = spec.char_gap_units as f64 * gap_unit;
    let mut items = Vec::new();
    let mut t_ms = 0.0f64;
    for (wi, w) in norm.split(' ').enumerate() {
        if wi > 0 {
            t_ms += word_gap_ms;
        }
        let ts = |ms: f64| ((t0_s + ms / 1000.0) * fs).round() as u64;
        let mut item = TranscriptItem {
            word: w.to_string(),
            start_ts: ts(t_ms),
            chars: Vec::new(),
        };
        let chars: Vec<char> = w.chars().collect();
        for (ci, c) in chars.iter().enumerate() {
            let char_start_ms = t_ms;
            let Some(pattern) = pattern_for(*c) else {
                anyhow::bail!("character '{}' has no Morse encoding", c.escape_debug());
            };
            let els: Vec<char> = pattern.chars().collect();
            for (ei, e) in els.iter().enumerate() {
                t_ms += if *e == '.' {
                    spec.dit_units as f64 * unit
                } else {
                    spec.weight as f64 * unit
                };
                if ei < els.len() - 1 {
                    t_ms += spec.element_gap_units as f64 * unit;
                }
            }
            item.chars.push((ts(char_start_ms), ts(t_ms)));
            if ci < chars.len() - 1 {
                t_ms += char_gap_ms;
            }
        }
        items.push(item);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyer::key_text;

    const FS: f64 = 96_000.0;
    const HOP_HZ: f64 = 375.0;

    #[test]
    fn envelope_offset_preserves_keying() {
        let segs = keying_segments("CQ CQ DE W1AW W1AW K", &KeyerSpec::new(20.0)).unwrap();
        let duration_s = segs.iter().map(|s| s.1).sum::<f64>() / 1000.0 + 1.0;
        let base = envelope_at_hops(&segs, HOP_HZ, FS, 0.0, duration_s);
        let shifted = envelope_at_hops(&segs, HOP_HZ, FS, 10.0, duration_s);
        assert_eq!(base.len(), shifted.len());
        for ((ts, amp), (shifted_ts, shifted_amp)) in base.iter().zip(&shifted) {
            assert_eq!(*shifted_ts, ts + 960_000);
            assert_eq!(amp, shifted_amp, "amplitude changed at sample {ts}");
        }
    }

    /// The reconstructed timeline recovers `keyer::key_text`'s keyed
    /// characters and word gaps at the right hops: every >0.5 stretch of
    /// the keyer's own envelope is ON in the timeline and every long
    /// (>= 5-dit) gap matches, at the hop the timeline names.
    #[test]
    fn keying_timeline_matches_the_keyer() {
        let spec = KeyerSpec::new(20.0);
        let text = "CQ CQ DE W1AW W1AW K";
        let (env, norm) = key_text(text, &spec, FS).unwrap();
        let segs = keying_segments(text, &spec).unwrap();
        // Total keyed time matches the keyer's envelope length.
        let total_ms: f64 = segs.iter().map(|s| s.1).sum();
        assert!(
            (env.len() as f64 / FS * 1000.0 - total_ms).abs() < 1.0,
            "keyer {} samples vs timeline {total_ms} ms",
            env.len()
        );
        assert_eq!(norm, "CQ CQ DE W1AW W1AW K");
        // Hop-sampling the timeline agrees with the keyer's own envelope
        // at every hop the keyer is unambiguous about: disagreements are
        // allowed only within one rise (5 ms, rounded up) of a segment
        // boundary, where the keyer's raised cosine crosses half
        // amplitude on one side of the boundary and the timeline's hard
        // edge on the other.
        let hops = envelope_at_hops(&segs, HOP_HZ, FS, 0.0, total_ms / 1000.0);
        let mut bounds = Vec::new();
        let mut acc = 0.0f64;
        for &(_, dur) in &segs {
            bounds.push(acc);
            acc += dur;
        }
        bounds.push(acc);
        let per_hop = FS / HOP_HZ; // 256 samples
        let mut disagree = 0usize;
        for &(ts, on) in &hops {
            let i = (ts as f64 / per_hop).round() as usize * per_hop as usize;
            let mid = i + (per_hop as usize) / 2;
            if mid >= env.len() {
                continue;
            }
            let t_ms = mid as f64 * 1000.0 / FS;
            let near_bound = bounds.iter().any(|&b| (t_ms - b).abs() < 6.0);
            if !near_bound && (env[mid] > 0.5) != (on > 0.5) {
                disagree += 1;
            }
        }
        assert_eq!(disagree, 0, "{disagree} hops disagree with the keyer");

        // Words start where the timeline says: each word's first mark.
        let items = transcript_items(text, &spec, FS, 0.0).unwrap();
        let words: Vec<&str> = norm.split(' ').collect();
        assert_eq!(items.len(), words.len());
        for (item, w) in items.iter().zip(&words) {
            assert_eq!(item.word, *w);
            // The sample the item names is ON in the keyer's envelope
            // (the word's first mark), give or take the rise.
            let i = item.start_ts as usize;
            assert!(
                env[i..(i + 300).min(env.len())].iter().any(|&v| v > 0.5),
                "{} at {} not on",
                w,
                i
            );
        }
        // Word gaps: consecutive items are at least a word gap apart
        // (7 dits = 420 ms at 20 WPM) for distinct words.
        assert!(items[1].start_ts > items[0].start_ts + 300 * (FS as u64 / 1000));
    }
}
