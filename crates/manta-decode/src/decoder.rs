//! Per-track decode glue: demod -> timing -> beam -> events. SPEC §3–§5.

use crate::beam::{decode_char, BeamConfig};
use crate::envelope::{Demod, DemodConfig, Run};
use crate::events::DecoderEvent;
use crate::timing::{GapClass, GapClassifier, SpeedTracker};
use crate::HOP_MS;

/// Tunables for one track's decode chain: demod + beam + flush threshold. SPEC §9.
#[derive(Debug, Clone)]
pub struct DecodeConfig {
    pub demod: DemodConfig,
    pub beam: BeamConfig,
    /// SPEC §9 decode.flush_gap_dits
    pub flush_gap_dits: f32,
}

// Manual impl: a derived Default would zero flush_gap_dits.
impl Default for DecodeConfig {
    fn default() -> Self {
        DecodeConfig {
            demod: DemodConfig::default(),
            beam: BeamConfig::default(),
            flush_gap_dits: 7.0,
        }
    }
}

const META_INTERVAL_HOPS: u64 = 375; // SPEC §5: TrackMeta at 1 Hz cadence
const WPM_REPORT_DELTA: f32 = 1.0; // SPEC §5: SpeedUpdate on >= 1 WPM change

/// Per-track decode glue: demod -> timing -> beam -> events. SPEC §3–§5.
pub struct TrackDecoder {
    track_id: u32,
    cfg: DecodeConfig,
    demod: Demod,
    tracker: SpeedTracker,
    gaps: GapClassifier,
    /// Runs buffered until the tracker is ready (first 5 marks). They are
    /// drained through gap classification + beam retroactively.
    pending: Vec<Run>,
    cur_marks: Vec<f32>,
    word_flushed: bool,
    last_reported_wpm: Option<f32>,
    freq_hz: f64,
    hop_count: u64,
    last_ts: u64,
    /// Decode-error counter (aborted garble characters). SPEC §4.4.
    pub garble_count: u32,
}

impl TrackDecoder {
    /// A decoder for one track, awaiting its first samples. SPEC §5.
    pub fn new(track_id: u32, cfg: DecodeConfig) -> Self {
        let demod = Demod::new(cfg.demod.clone());
        TrackDecoder {
            track_id,
            cfg,
            demod,
            tracker: SpeedTracker::new(),
            gaps: GapClassifier::new(),
            pending: Vec::new(),
            cur_marks: Vec::new(),
            word_flushed: false,
            last_reported_wpm: None,
            freq_hz: 0.0,
            hop_count: 0,
            last_ts: 0,
            garble_count: 0,
        }
    }

    /// Set the track's absolute RF frequency, reported in TrackMeta events. SPEC §5.
    pub fn set_freq_hz(&mut self, hz: f64) {
        self.freq_hz = hz;
    }

    /// Feed one envelope sample at 375 Hz; returns any events it produced. SPEC §3–§5.
    pub fn push_envelope(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        self.last_ts = sample_ts;
        let runs = self.demod.push(a, sample_ts);
        for run in runs {
            self.on_run(run, &mut events);
        }
        self.check_flush(&mut events);
        self.hop_count += 1;
        if self.hop_count.is_multiple_of(META_INTERVAL_HOPS) {
            if let Some(snr) = self.demod.snr_2500_db() {
                events.push(DecoderEvent::TrackMeta {
                    track_id: self.track_id,
                    snr_2500_db: snr,
                    freq_hz: self.freq_hz,
                });
            }
        }
        events
    }

    /// End of stream: flush the demod and any open character/word. SPEC §5.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        for run in self.demod.finish() {
            self.on_run(run, &mut events);
        }
        if !self.cur_marks.is_empty() && self.tracker.ready() {
            let ts = self.last_ts;
            self.emit_char(ts, &mut events);
            if !self.word_flushed {
                events.push(DecoderEvent::WordBoundary {
                    track_id: self.track_id,
                    sample_ts: ts,
                });
            }
        }
        events
    }

    fn on_run(&mut self, run: Run, events: &mut Vec<DecoderEvent>) {
        if !self.tracker.ready() {
            if run.mark {
                self.tracker.on_mark(run.hops as f32 * HOP_MS as f32);
            }
            self.pending.push(run);
            if self.tracker.ready() {
                // Retroactively assemble the buffered runs; their marks have
                // already fed the tracker, so tracker updates are skipped.
                let drained = std::mem::take(&mut self.pending);
                for r in drained {
                    self.process_run(r, false, events);
                }
                self.demod.set_dit_ms(self.tracker.mu_dit_ms());
            }
            return;
        }
        self.process_run(run, true, events);
    }

    fn process_run(&mut self, run: Run, live: bool, events: &mut Vec<DecoderEvent>) {
        let dur_ms = run.hops as f32 * HOP_MS as f32;
        if run.mark {
            if live {
                self.tracker.on_mark(dur_ms);
                self.demod.set_dit_ms(self.tracker.mu_dit_ms());
                if let Some(w) = self.tracker.wpm() {
                    let report = match self.last_reported_wpm {
                        None => true,
                        Some(prev) => (w - prev).abs() >= WPM_REPORT_DELTA,
                    };
                    if report {
                        self.last_reported_wpm = Some(w);
                        events.push(DecoderEvent::SpeedUpdate {
                            track_id: self.track_id,
                            wpm: w,
                        });
                    }
                }
            }
            self.cur_marks.push(dur_ms);
            self.word_flushed = false;
        } else {
            if self.word_flushed {
                // This gap was already handled by the 7-dit flush.
                return;
            }
            match self.gaps.classify(dur_ms, self.tracker.mu_dit_ms()) {
                GapClass::InterElement => {}
                GapClass::InterChar => self.emit_char(run.start_ts, events),
                GapClass::InterWord => {
                    self.emit_char(run.start_ts, events);
                    events.push(DecoderEvent::WordBoundary {
                        track_id: self.track_id,
                        sample_ts: run.start_ts,
                    });
                }
            }
        }
    }

    /// SPEC §4.2: a trailing space reaching 7*mu_dit forces char + word flush.
    ///
    /// `Demod` holds a run one flip behind `open` for debounce confirmation
    /// (SPEC §3.3): a completed run only surfaces via `push()` once a
    /// further flip evicts it. When the track has gone quiet, the mark
    /// immediately preceding this open space can be stuck in that `held`
    /// slot forever — no further flip is coming to evict it naturally. We
    /// must drain it via `finish()` before committing the character, or the
    /// last mark of the flushed char is silently dropped (and, worse,
    /// re-surfaces later as a bogus extra character when a real `finish()`
    /// eventually runs).
    fn check_flush(&mut self, events: &mut Vec<DecoderEvent>) {
        if self.word_flushed || !self.tracker.ready() || self.cur_marks.is_empty() {
            return;
        }
        if let (Some(hops), Some(ts)) = (
            self.demod.open_space_hops(),
            self.demod.open_space_start_ts(),
        ) {
            let gap_ms = hops as f32 * HOP_MS as f32;
            let flush_dits = self.gaps.flush_threshold_dits(self.cfg.flush_gap_dits);
            if gap_ms >= flush_dits * self.tracker.mu_dit_ms() {
                // Drain any held mark into cur_marks (live: it's a real
                // keyed event and should count for speed tracking); the
                // drained space itself is not separately gap-classified —
                // this forced flush already decides its fate.
                for run in self.demod.finish() {
                    if run.mark {
                        self.process_run(run, true, events);
                    }
                }
                self.emit_char(ts, events);
                if !self.word_flushed {
                    events.push(DecoderEvent::WordBoundary {
                        track_id: self.track_id,
                        sample_ts: ts,
                    });
                }
                self.word_flushed = true;
            }
        }
    }

    fn emit_char(&mut self, sample_ts: u64, events: &mut Vec<DecoderEvent>) {
        if self.cur_marks.is_empty() {
            return;
        }
        // SPEC §4.5: q = clamp(SNR_2500 / 20 dB, 0.3, 1.0)
        let q = self
            .demod
            .snr_2500_db()
            .map(|snr| (snr / 20.0).clamp(0.3, 1.0))
            .unwrap_or(1.0);
        let marks = std::mem::take(&mut self.cur_marks);
        match decode_char(
            &marks,
            self.tracker.mu_dit_ms(),
            self.tracker.mu_dah_ms(),
            q,
            &self.cfg.beam,
        ) {
            Some(cd) => events.push(DecoderEvent::CharDecoded {
                track_id: self.track_id,
                sample_ts,
                glyph: cd.glyph,
                confidence: cd.confidence,
            }),
            None => self.garble_count += 1,
        }
    }
}

/// Assemble plain text: chars joined, word boundaries as single spaces,
/// prosigns dropped (SPEC §4.4 telnet-facing convention).
pub fn events_to_text(events: &[DecoderEvent]) -> String {
    let mut s = String::new();
    for e in events {
        match e {
            DecoderEvent::CharDecoded { glyph, .. } => {
                if let Some(c) = glyph.text_char() {
                    s.push(c);
                }
            }
            DecoderEvent::WordBoundary { .. } if !s.is_empty() && !s.ends_with(' ') => {
                s.push(' ');
            }
            _ => {}
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::pattern_for;

    /// Render text as an ideal rectangular envelope at 375 Hz.
    /// 25 WPM => dit = 48 ms = 18 hops exactly (no rounding error).
    fn rect_envelope(text: &str, dit_hops: u32) -> Vec<f32> {
        let mut env = Vec::new();
        let mut push = |level: f32, hops: u32| {
            for _ in 0..hops {
                env.push(level);
            }
        };
        // MAN-6: one dit of leading silence so the first mark has a
        // genuinely observed rising edge. `Demod` discards its leading
        // (un-anchored) run; without this, every test here would lose its
        // first element.
        push(0.0, dit_hops);
        let words: Vec<&str> = text.split_whitespace().collect();
        for (wi, word) in words.iter().enumerate() {
            let chars: Vec<char> = word.chars().collect();
            for (ci, c) in chars.iter().enumerate() {
                let pat = pattern_for(*c).unwrap();
                let els: Vec<char> = pat.chars().collect();
                for (ei, e) in els.iter().enumerate() {
                    push(1.0, if *e == '.' { dit_hops } else { 3 * dit_hops });
                    if ei < els.len() - 1 {
                        push(0.0, dit_hops);
                    }
                }
                if ci < chars.len() - 1 {
                    push(0.0, 3 * dit_hops);
                }
            }
            if wi < words.len() - 1 {
                push(0.0, 7 * dit_hops);
            }
        }
        push(0.0, 8 * dit_hops); // tail so the last word flushes by timeout
        env
    }

    fn decode(text: &str) -> (String, Vec<DecoderEvent>) {
        let env = rect_envelope(text, 18);
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        dec.set_freq_hz(14_012_340.0);
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        (events_to_text(&events), events)
    }

    /// MAN-6 regression, hermetic. A track promoted mid-element makes the
    /// demod's init window open mid-dah; the resulting un-anchored fragment
    /// used to become sample #1 of SpeedTracker's 5-mark bootstrap, tipping
    /// ClusterPair::initialize's largest-ratio-gap split onto a bad,
    /// self-consistent fixed point: mu_dit collapses, every mark
    /// reclassifies as a dah, every inter-element gap promotes to
    /// inter-character, and the output becomes an endless "TT TTT TT TTT
    /// ..." that never re-syncs. Reproduced here with ZERO noise, which is
    /// the point: this is a deterministic timing-bootstrap defect, not a
    /// noise-robustness limit. See
    /// docs/DECISIONS/2026-09-04-man6-leading-partial-run-and-badlock-recovery.md.
    ///
    /// `skip_hops` starts the fresh `TrackDecoder` (simulating a
    /// track-promotion attach) partway through a real element: at
    /// `dit_hops = 25`, "A" = dit(25) gap(25) dah(75), so its dah spans
    /// hops [dit_hops+50, dit_hops+125) once `rect_envelope`'s one-dit
    /// leading silence is accounted for. `skip_hops = dit_hops + 125 - 7`
    /// starts 7 hops before that dah ends, leaving a 7-hop fragment: above
    /// the 5-hop debounce floor, and (66.67/18.7 ~= 3.6 > 200/66.67 = 3.0)
    /// large enough to win `ClusterPair::initialize`'s largest-ratio-gap
    /// split against the real dit/dah population.
    fn decode_from_hop(text: &str, dit_hops: u32, skip_hops: usize) -> (String, Vec<DecoderEvent>) {
        let env = rect_envelope(text, dit_hops);
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env.iter().skip(skip_hops).enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        (events_to_text(&events), events)
    }

    #[test]
    fn mid_element_start_does_not_lock_bad_timing() {
        // 10 repetitions of "AU"; start 7 hops before the end of A's first
        // dah (see decode_from_hop's doc comment for the derivation).
        let text = "AU AU AU AU AU AU AU AU AU AU";
        let (decoded, events) = decode_from_hop(text, 25, 25 + 125 - 7);
        assert!(
            !decoded.contains('T'),
            "MAN-6 bad timing lock: every element decoded as a lone dah -- {decoded:?}"
        );
        assert!(
            decoded.contains("AU AU AU"),
            "expected the looped text to decode -- {decoded:?}"
        );
        // 25 hops/dit = 66.67 ms = 18.0 WPM. A bad lock reports ~60 WPM
        // (mu_dit clamped to the 20 ms floor).
        let wpm = events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
                _ => None,
            })
            .next_back()
            .expect("no SpeedUpdate emitted");
        assert!((wpm - 18.0).abs() < 2.0, "wpm {wpm}");
    }

    #[test]
    fn mid_element_start_error_does_not_grow_with_duration() {
        // The ticket's actual acceptance criterion: error must stabilize or
        // shrink as the scene lengthens, not accumulate. Under the bad lock
        // the 'T' count grew linearly with duration; after the fix it is
        // zero at every length.
        for reps in [4usize, 10, 24] {
            let text = std::iter::repeat_n("AU", reps)
                .collect::<Vec<_>>()
                .join(" ");
            let (decoded, _) = decode_from_hop(&text, 25, 25 + 125 - 7);
            let t_count = decoded.chars().filter(|&c| c == 'T').count();
            assert_eq!(t_count, 0, "reps {reps}: garbled 'T' stream -- {decoded:?}");
        }
    }

    #[test]
    fn decodes_single_word() {
        let (text, _) = decode("PARIS");
        assert_eq!(text, "PARIS");
    }

    #[test]
    fn decodes_words_with_boundaries() {
        let (text, _) = decode("CQ CQ DE W1AW");
        assert_eq!(text, "CQ CQ DE W1AW");
    }

    #[test]
    fn first_characters_are_not_lost() {
        // Tracker init consumes the first 5 marks; the pending-run buffer must
        // decode them retroactively.
        let (text, _) = decode("CQ TEST");
        assert!(text.starts_with("CQ"), "got {text:?}");
    }

    #[test]
    fn emits_speed_and_meta_events() {
        let (_, events) = decode("CQ CQ DE W1AW W1AW K");
        assert!(events.iter().any(
            |e| matches!(e, DecoderEvent::SpeedUpdate { wpm, .. } if (*wpm - 25.0).abs() < 3.0)
        ));
        assert!(events.iter().any(
            |e| matches!(e, DecoderEvent::TrackMeta { freq_hz, .. } if *freq_hz == 14_012_340.0)
        ));
    }

    #[test]
    fn trailing_word_flushes_by_timeout_not_eof() {
        // The 7-dit rule (SPEC §4.2) must close "K" before the stream ends.
        let env = rect_envelope("CQ K", 18);
        let cut = env.len() - 18; // stop 1 dit short of the synthetic tail's end
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env[..cut].iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        // No finish(): the flush must already have happened via the timeout.
        assert_eq!(events_to_text(&events), "CQ K");
    }

    #[test]
    fn char_timestamp_is_end_of_last_mark() {
        // Note: a lone "E" can never decode — the tracker needs 5 marks.
        // "PARIS" gives P (.--.) = 4 marks; the 5th mark (A's dit) makes the
        // tracker ready and the pending buffer drains retroactively.
        let (_, events) = decode("PARIS");
        let ts = events
            .iter()
            .find_map(|e| match e {
                DecoderEvent::CharDecoded { sample_ts, .. } => Some(*sample_ts),
                _ => None,
            })
            .unwrap();
        // P = dit(18) g(18) dah(54) g(18) dah(54) g(18) dit(18) = ends at
        // hop 198 (relative to the first real mark); the closing inter-char
        // space starts there (pinned decision 11: CharDecoded ts = start of
        // the closing space run). MAN-6's leading dit of silence in
        // `rect_envelope` shifts every absolute timestamp by dit_hops: 198 +
        // 18 = 216.
        assert_eq!(ts, 216 * 256);
    }

    #[test]
    fn text_assembly_drops_prosigns_and_collapses_spaces() {
        use crate::tree::{Glyph, Prosign};
        let ev = vec![
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 0,
                glyph: Glyph::Char('A'),
                confidence: 1.0,
            },
            DecoderEvent::WordBoundary {
                track_id: 1,
                sample_ts: 1,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 2,
                glyph: Glyph::Prosign(Prosign::Ar),
                confidence: 1.0,
            },
            DecoderEvent::WordBoundary {
                track_id: 1,
                sample_ts: 3,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 4,
                glyph: Glyph::Char('B'),
                confidence: 1.0,
            },
        ];
        assert_eq!(events_to_text(&ev), "A B");
    }

    #[test]
    fn all_dah_opener_decodes_correctly() {
        // Pinned decision 20 regression, exercised end-to-end. At 24
        // hops/dit (dit = 64 ms, ~18.75 WPM), a homogeneous run of dahs
        // averages 192 ms -- unambiguously over the SPEC §4.1 150 ms dit
        // ceiling, so unimodal init must assume dahs, not the pre-fix
        // default of dits (which decoded "TTTTT" as "5").
        let env = rect_envelope("TTTTT", 24);
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        assert_eq!(events_to_text(&events), "TTTTT");
    }
}
