//! Bounded word beam. Acoustic pruning always precedes prior scoring.

use crate::beam::CharReading;
use crate::prior::WordPrior;
use crate::tree::Glyph;

const MAX_READING_CHARS: usize = 24;

pub(crate) struct Settlement {
    pub(crate) readings: Vec<(String, f32)>,
    pub(crate) chars: Vec<(Glyph, f32, u64)>,
}

#[derive(Clone, Debug)]
struct Reading {
    text: String,
    score: f32,
    chars: Vec<(Glyph, f32, u64)>,
}

#[derive(Default)]
pub(crate) struct ReadingsBeam {
    held: Vec<Reading>,
    overflow: bool,
}

impl ReadingsBeam {
    pub(crate) fn push(&mut self, candidates: &[CharReading], ts: u64, margin: f32, cap: usize) {
        if self.overflow {
            return;
        }
        if self
            .held
            .first()
            .is_some_and(|r| r.chars.len() >= MAX_READING_CHARS)
        {
            self.held.clear();
            self.overflow = true;
            return;
        }
        let roots = if self.held.is_empty() {
            vec![Reading {
                text: String::new(),
                score: 0.0,
                chars: Vec::new(),
            }]
        } else {
            std::mem::take(&mut self.held)
        };
        let mut next = Vec::new();
        for root in roots {
            for candidate in candidates {
                let mut reading = root.clone();
                // Prosigns retain their event identity but supply no dictionary text.
                reading
                    .text
                    .push(candidate.glyph.text_char().unwrap_or('?'));
                reading.score += candidate.score;
                reading
                    .chars
                    .push((candidate.glyph, candidate.confidence, ts));
                next.push(reading);
            }
        }
        next.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.text.cmp(&b.text))
        });
        if let Some(best) = next.first().map(|r| r.score) {
            next.retain(|r| best - r.score <= margin);
        }
        next.truncate(cap.clamp(1, 16));
        // Subtract a common offset to avoid precision loss on long words.
        if let Some(best) = next.first().map(|r| r.score) {
            for r in &mut next {
                r.score -= best;
            }
        }
        self.held = next;
    }

    fn ranked(&self, prior: Option<&dyn WordPrior>, weight: f32) -> Vec<(&Reading, f32)> {
        let mut ranked: Vec<_> = self
            .held
            .iter()
            .map(|r| {
                let bonus = prior.map_or(0.0, |p| p.reading_bonus(&r.text));
                let bonus = if bonus.is_finite() {
                    bonus.clamp(0.0, weight.max(0.0))
                } else {
                    0.0
                };
                (r, r.score + bonus)
            })
            .collect();
        ranked.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then_with(|| a.text.cmp(&b.text)));
        ranked
    }

    pub(crate) fn snapshot(
        &self,
        prior: Option<&dyn WordPrior>,
        weight: f32,
    ) -> Vec<(String, f32)> {
        let ranked = self.ranked(prior, weight);
        let leader = ranked.first().map_or(0.0, |(_, s)| *s);
        ranked
            .into_iter()
            .map(|(r, s)| (r.text.clone(), (leader - s).max(0.0)))
            .collect()
    }

    pub(crate) fn settle(&mut self, prior: Option<&dyn WordPrior>, weight: f32) -> Settlement {
        // A single ranking supplies both the committed word and diagnostic,
        // even when the reference lists refresh during settlement.
        let ranked = self.ranked(prior, weight);
        let chars = ranked
            .first()
            .map_or_else(Vec::new, |(r, _)| r.chars.clone());
        let leader = ranked.first().map_or(0.0, |(_, s)| *s);
        let readings = ranked
            .into_iter()
            .map(|(r, s)| (r.text.clone(), (leader - s).max(0.0)))
            .collect();
        self.held.clear();
        self.overflow = false;
        Settlement { readings, chars }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct Prior;
    impl WordPrior for Prior {
        fn reading_bonus(&self, s: &str) -> f32 {
            if s == "W1AW" {
                100.0
            } else {
                0.0
            }
        }
    }
    fn candidates() -> Vec<CharReading> {
        vec![
            CharReading {
                glyph: Glyph::Char('R'),
                score: 0.0,
                confidence: 0.6,
            },
            CharReading {
                glyph: Glyph::Char('W'),
                score: -1.0,
                confidence: 0.4,
            },
        ]
    }
    #[test]
    fn close_readings_are_held_across_characters_with_margins() {
        let mut b = ReadingsBeam::default();
        b.push(&candidates(), 10, 3.0, 4);
        b.push(
            &[CharReading {
                glyph: Glyph::Char('1'),
                score: 0.0,
                confidence: 1.0,
            }],
            20,
            3.0,
            4,
        );
        assert_eq!(
            b.snapshot(None, 2.0),
            vec![("R1".into(), 0.0), ("W1".into(), 1.0)]
        );
    }
    #[test]
    fn prior_breaks_acoustic_tie_toward_listed_call() {
        let mut b = ReadingsBeam::default();
        b.push(&candidates(), 10, 3.0, 4);
        for (i, c) in "1AW".chars().enumerate() {
            b.push(
                &[CharReading {
                    glyph: Glyph::Char(c),
                    score: 0.0,
                    confidence: 1.0,
                }],
                20 + i as u64,
                3.0,
                4,
            );
        }
        assert_eq!(b.snapshot(Some(&Prior), 2.0)[0], ("W1AW".into(), 0.0));
        let chars = b.settle(Some(&Prior), 2.0).chars;
        assert_eq!(
            chars
                .iter()
                .map(|(g, _, _)| g.text_char().unwrap())
                .collect::<String>(),
            "W1AW"
        );
        assert_eq!(chars[0].2, 10);
    }
    #[test]
    fn prior_never_rescues_or_substitutes_outside_margin() {
        let mut b = ReadingsBeam::default();
        b.push(&candidates(), 10, 0.5, 4);
        assert_eq!(b.snapshot(Some(&Prior), 100.0), vec![("R".into(), 0.0)]);
    }
    #[test]
    fn held_cap_evicts_deterministically() {
        let mut c = candidates();
        c[1].score = 0.0;
        let mut b = ReadingsBeam::default();
        b.push(&c, 10, 3.0, 1);
        assert_eq!(b.snapshot(None, 2.0), vec![("R".into(), 0.0)]);
    }
    #[test]
    fn settlement_diagnostic_uses_the_committed_ranking() {
        #[derive(Debug)]
        struct Changing(std::sync::atomic::AtomicUsize);
        impl WordPrior for Changing {
            fn reading_bonus(&self, reading: &str) -> f32 {
                let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call < 2 && reading == "W" {
                    2.0
                } else {
                    0.0
                }
            }
        }
        let prior = Changing(std::sync::atomic::AtomicUsize::new(0));
        let mut b = ReadingsBeam::default();
        b.push(&candidates(), 10, 3.0, 4);
        let settled = b.settle(Some(&prior), 2.0);
        assert_eq!(settled.readings[0].0, "W");
        assert_eq!(settled.chars[0].0, Glyph::Char('W'));
        assert_eq!(prior.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn overlong_readings_are_dropped() {
        let mut b = ReadingsBeam::default();
        for _ in 0..26 {
            b.push(&candidates(), 10, 3.0, 4);
        }
        assert!(b.snapshot(None, 2.0).is_empty());
        assert!(b.settle(None, 2.0).chars.is_empty());
        b.push(&candidates(), 10, 3.0, 4);
        assert_eq!(b.snapshot(None, 2.0).len(), 2);
    }
}
