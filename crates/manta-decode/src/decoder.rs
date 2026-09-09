//! Per-track decode glue: demod -> timing -> beam -> events. SPEC §3–§5.

use crate::beam::{decode_char, BeamConfig};
use crate::edge_demod::EdgeDemod;
use crate::envelope::{Demod, DemodConfig, Run};
use crate::events::DecoderEvent;
use crate::evidence::{Evidence, EvidenceConfig, HopEvidence};
use crate::noise::{NoiseConfig, NoiseTracker};
use crate::timing::{GapClass, GapClassifier, SpeedTracker};
use crate::HOP_MS;

/// Decode engine selection (SPEC v2 §0): which timing/beam chain and which
/// keying-decision front end feeds it. `Hsmm` is not reachable until Task 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    Legacy,
    EdgeLegacy,
    Hsmm,
}

impl std::str::FromStr for Engine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "legacy" => Ok(Engine::Legacy),
            "edge-legacy" => Ok(Engine::EdgeLegacy),
            "hsmm" => Ok(Engine::Hsmm),
            o => Err(format!(
                "unknown decode engine {o:?} (legacy | edge-legacy | hsmm)"
            )),
        }
    }
}

/// Tunables for one track's decode chain: demod + beam + flush threshold. SPEC §9.
#[derive(Debug, Clone)]
pub struct DecodeConfig {
    pub demod: DemodConfig,
    pub beam: BeamConfig,
    /// SPEC §9 decode.flush_gap_dits
    pub flush_gap_dits: f32,
    /// SPEC v2 §0: which decode engine drives this track.
    pub engine: Engine,
    /// SPEC v2 §1: `EdgeLegacy`/`Hsmm` evidence front-end tunables.
    pub evidence: EvidenceConfig,
    /// SPEC v2 §2: `EdgeLegacy`/`Hsmm` noise-reference tunables.
    pub noise: NoiseConfig,
}

// Manual impl: a derived Default would zero flush_gap_dits.
impl Default for DecodeConfig {
    fn default() -> Self {
        DecodeConfig {
            demod: DemodConfig::default(),
            beam: BeamConfig::default(),
            flush_gap_dits: 7.0,
            engine: Engine::Legacy,
            evidence: EvidenceConfig::default(),
            noise: NoiseConfig::default(),
        }
    }
}

const META_INTERVAL_HOPS: u64 = 375; // SPEC §5: TrackMeta at 1 Hz cadence
const WPM_REPORT_DELTA: f32 = 1.0; // SPEC §5: SpeedUpdate on >= 1 WPM change

// SPEC §2.3: 10*log10(2500/93.75) -- same bandwidth correction as
// envelope::Demod's private constant of the same name/value, applied here
// to the evidence front end's mark/noise levels for EdgeLegacy/Hsmm.
const SNR_BW_CORR_DB: f32 = 14.3;

/// Per-track decode glue: demod -> timing -> beam -> events. SPEC §3–§5.
pub struct TrackDecoder {
    track_id: u32,
    cfg: DecodeConfig,
    demod: Demod,
    evidence: Evidence,
    noise: NoiseTracker,
    edge: EdgeDemod,
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
    /// SPEC v2 §2.3-derived SNR for the `EdgeLegacy`/`Hsmm` engines (the
    /// `Legacy` engine keeps using `demod.snr_2500_db()` directly). Set by
    /// `snr_from_evidence`, read by `emit_char`'s `q` and by `tick_meta`.
    last_snr: Option<f32>,
    /// Decode-error counter (aborted garble characters). SPEC §4.4.
    pub garble_count: u32,
}

impl TrackDecoder {
    /// A decoder for one track, awaiting its first samples. SPEC §5.
    pub fn new(track_id: u32, cfg: DecodeConfig) -> Self {
        let demod = Demod::new(cfg.demod.clone());
        let evidence = Evidence::new(cfg.evidence.clone());
        let noise = NoiseTracker::new(cfg.noise.clone());
        let edge = EdgeDemod::new(cfg.demod.debounce_ms);
        // SPEC v2 §0 (Task 5, timing.rs Step 6): EdgeLegacy/Hsmm use the
        // SPEC-nominal char-gap threshold; Legacy keeps its documented 1.6
        // deviation (envelope-overshoot correction, timing.rs's
        // CHAR_GAP_DITS doc comment).
        let gaps = match cfg.engine {
            Engine::Legacy => GapClassifier::new(),
            Engine::EdgeLegacy | Engine::Hsmm => {
                GapClassifier::new_with(crate::timing::CHAR_GAP_DITS_NOMINAL)
            }
        };
        TrackDecoder {
            track_id,
            cfg,
            demod,
            evidence,
            noise,
            edge,
            tracker: SpeedTracker::new(),
            gaps,
            pending: Vec::new(),
            cur_marks: Vec::new(),
            word_flushed: false,
            last_reported_wpm: None,
            freq_hz: 0.0,
            hop_count: 0,
            last_ts: 0,
            last_snr: None,
            garble_count: 0,
        }
    }

    /// Set the track's absolute RF frequency, reported in TrackMeta events. SPEC §5.
    pub fn set_freq_hz(&mut self, hz: f64) {
        self.freq_hz = hz;
    }

    /// One hop: linear amplitude, linear power, optional spectral noise
    /// reference power (SPEC v2 §2.2), input sample counter.
    pub fn push_hop(
        &mut self,
        amp: f32,
        power: f32,
        spectral_ref_power: Option<f32>,
        sample_ts: u64,
    ) -> Vec<DecoderEvent> {
        self.last_ts = sample_ts;
        match self.cfg.engine {
            Engine::Legacy => self.push_envelope_legacy(amp, sample_ts),
            Engine::EdgeLegacy => {
                let n = self.noise.push(power, spectral_ref_power).max(1e-18).sqrt();
                let mut events = Vec::new();
                if let Some(ev) = self.evidence.push(amp, n, sample_ts) {
                    self.snr_from_evidence(&ev);
                    for run in self.edge.push(&ev) {
                        self.on_run(run, &mut events);
                    }
                    self.check_flush_edge(&mut events);
                }
                self.tick_meta(&mut events);
                events
            }
            Engine::Hsmm => self.push_hop_hsmm(amp, power, spectral_ref_power, sample_ts), // Task 8
        }
    }

    /// Feed one envelope sample at 375 Hz; returns any events it produced. SPEC §3–§5.
    pub fn push_envelope(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent> {
        self.push_hop(a, a * a, None, sample_ts)
    }

    /// The `Legacy` engine's demod -> timing -> beam chain. SPEC §3–§5.
    fn push_envelope_legacy(&mut self, a: f32, sample_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        self.last_ts = sample_ts;
        let runs = self.demod.push(a, sample_ts);
        for run in runs {
            self.on_run(run, &mut events);
        }
        self.check_flush(&mut events);
        self.tick_meta(&mut events);
        events
    }

    /// Task 8 placeholder: HSMM engine, not implemented yet.
    #[allow(unused_variables)]
    fn push_hop_hsmm(
        &mut self,
        amp: f32,
        power: f32,
        spectral_ref_power: Option<f32>,
        sample_ts: u64,
    ) -> Vec<DecoderEvent> {
        unimplemented!("Task 8")
    }

    /// SPEC §5: TrackMeta at the 1 Hz cadence. `Legacy` reads its SNR
    /// straight from the keying rails; `EdgeLegacy`/`Hsmm` use the SPEC v2
    /// §2.3 evidence-derived estimate stashed by `snr_from_evidence`.
    fn tick_meta(&mut self, events: &mut Vec<DecoderEvent>) {
        self.hop_count += 1;
        if self.hop_count % META_INTERVAL_HOPS == 0 {
            let snr = if self.cfg.engine == Engine::Legacy {
                self.demod.snr_2500_db()
            } else {
                self.last_snr
            };
            if let Some(snr) = snr {
                events.push(DecoderEvent::TrackMeta {
                    track_id: self.track_id,
                    snr_2500_db: snr,
                    freq_hz: self.freq_hz,
                });
            }
        }
    }

    /// SPEC v2 §2.3: SNR from the evidence front end's own mark/noise
    /// levels, `20*log10(mark_level/noise_level) - 14.3` (same bandwidth
    /// correction as `envelope::Demod::snr_2500_db`).
    fn snr_from_evidence(&mut self, ev: &HopEvidence) {
        self.last_snr =
            Some(20.0 * (ev.mark_level / ev.noise_level.max(1e-18)).log10() - SNR_BW_CORR_DB);
    }

    /// End of stream: flush the demod and any open character/word. SPEC §5.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        if self.cfg.engine == Engine::EdgeLegacy {
            for ev in self.evidence.flush() {
                self.snr_from_evidence(&ev);
                for run in self.edge.push(&ev) {
                    self.on_run(run, &mut events);
                }
            }
            for run in self.edge.finish() {
                self.on_run(run, &mut events);
            }
        } else {
            for run in self.demod.finish() {
                self.on_run(run, &mut events);
            }
        }
        if !self.cur_marks.is_empty() && self.tracker.ready() {
            let ts = self.last_ts;
            self.emit_char(ts, &mut events);
            if !self.word_flushed {
                events.push(DecoderEvent::word_boundary(self.track_id, ts));
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
                    events.push(DecoderEvent::word_boundary(self.track_id, run.start_ts));
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
                    events.push(DecoderEvent::word_boundary(self.track_id, ts));
                }
                self.word_flushed = true;
            }
        }
    }

    /// SPEC §4.2: a trailing space reaching 7*mu_dit forces char + word
    /// flush, for the `EdgeLegacy` engine (see `check_flush`'s doc comment
    /// for the full rationale). `EdgeDemod` holds a run one flip behind
    /// `open` for debounce confirmation, same as `Demod`; `self.edge.finish()`
    /// drains it before committing the character.
    fn check_flush_edge(&mut self, events: &mut Vec<DecoderEvent>) {
        if self.word_flushed || !self.tracker.ready() || self.cur_marks.is_empty() {
            return;
        }
        if let Some((hops, ts)) = self.edge.open_space() {
            let gap_ms = hops as f32 * HOP_MS as f32;
            let flush_dits = self.gaps.flush_threshold_dits(self.cfg.flush_gap_dits);
            if gap_ms >= flush_dits * self.tracker.mu_dit_ms() {
                for run in self.edge.finish() {
                    if run.mark {
                        self.process_run(run, true, events);
                    }
                }
                self.emit_char(ts, events);
                if !self.word_flushed {
                    events.push(DecoderEvent::word_boundary(self.track_id, ts));
                }
                self.word_flushed = true;
            }
        }
    }

    fn emit_char(&mut self, sample_ts: u64, events: &mut Vec<DecoderEvent>) {
        if self.cur_marks.is_empty() {
            return;
        }
        // SPEC §4.5: q = clamp(SNR_2500 / 20 dB, 0.3, 1.0). Legacy reads the
        // keying-rail SNR directly; EdgeLegacy/Hsmm use the evidence-derived
        // estimate (SPEC v2 §2.3) stashed in `last_snr`.
        let snr = if self.cfg.engine == Engine::Legacy {
            self.demod.snr_2500_db()
        } else {
            self.last_snr
        };
        let q = snr.map(|snr| (snr / 20.0).clamp(0.3, 1.0)).unwrap_or(1.0);
        let marks = std::mem::take(&mut self.cur_marks);
        match decode_char(
            &marks,
            self.tracker.mu_dit_ms(),
            self.tracker.mu_dah_ms(),
            q,
            &self.cfg.beam,
        ) {
            Some(cd) => events.push(DecoderEvent::char_decoded(
                self.track_id,
                sample_ts,
                cd.glyph,
                cd.confidence,
            )),
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

    /// Rectangular envelope helper with `depth_db` keying depth (the existing
    /// `rect_envelope` is 0/1; deep keying is what breaks the legacy chain).
    /// Applies the same 4-hop linear ramp as evidence.rs's `keyed()` test
    /// helper on every edge, emulating the SPEC v2 §1.2 channel filter --
    /// without it a rectangular envelope doesn't exercise the legacy bias
    /// this test exists to catch.
    fn rect_envelope_depth(text: &str, dit_hops: u32, depth_db: f32) -> Vec<f32> {
        let lo = 10f32.powf(-depth_db / 20.0);
        let v: Vec<f32> = rect_envelope(text, dit_hops)
            .into_iter()
            .map(|v| if v > 0.5 { 1.0 } else { lo })
            .collect();
        let mut out = v.clone();
        for i in 1..v.len() {
            if (v[i] - v[i - 1]).abs() > 0.1 {
                for j in 0..4 {
                    let k = i + j;
                    if k < out.len() {
                        let f = (j as f32 + 0.5) / 4.0;
                        out[k] = v[i - 1] + f * (v[i] - v[i - 1]);
                    }
                }
            }
        }
        out
    }

    fn decode_with(engine: Engine, env: &[f32]) -> String {
        let cfg = DecodeConfig {
            engine,
            ..Default::default()
        };
        let mut dec = TrackDecoder::new(1, cfg);
        let mut events = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            events.extend(dec.push_envelope(a, i as u64 * 256));
        }
        events.extend(dec.finish());
        events_to_text(&events)
    }

    #[test]
    fn edge_legacy_decodes_contest_speed_deep_keying() {
        // 40 WPM = 11.25 hops/dit; use 11 hops. 45 dB depth. Legacy merges words here.
        //
        // [Finding, Task 5 execution]: as originally drafted (env starting
        // directly with the first mark, no lead-in) this test loses the
        // leading "C" entirely -- NOT the timing/beam defect this test
        // exists to catch, but a distinct, separately-diagnosed artifact of
        // `NoiseTracker`'s temporal-minimum EMA (tau_ms = 40 ms, SPEC v2
        // §2.1, Task 3, calibrated against 60 s of real-style noise)
        // starting from a stone-cold state: with no prior sample at all, its
        // very first smoothed value is initialized directly from that first
        // sample (`Evidence::push`/`NoiseTracker::push` both special-case
        // "no history yet" as `= power`, not an EMA blend), so if that first
        // sample happens to be a mark (as here), the noise estimate starts
        // pinned near the MARK level and only decays on later low samples --
        // confirmed by instrumented trace that it doesn't cross below the
        // evidence gate's `present = mark_level >= 2*noise_level` threshold
        // until ~23 hops into "C"'s own trailing 3-dit gap, well after "C"'s
        // marks are over. A real track is never this cold: `NoiseTracker`
        // has been accumulating real channel history, mark and noise alike,
        // for as long as the track has existed before its first character
        // starts keying. A tiny (4-hop) noise-floor lead-in restores that:
        // it makes the very FIRST sample ever pushed a noise-floor sample
        // instead of a mark, so `NoiseTracker`'s min and `Evidence`'s a_s
        // both init near the true floor immediately (verified: the gate is
        // open from "C"'s first element) -- without needing anywhere near
        // NoiseTracker's ~60-hop convergence time, since the min only ever
        // needs ONE low sample to seed it, not a sustained low stretch. 4
        // hops is deliberately kept under EdgeDemod's 5-hop debounce
        // (`ms_to_hops(12.0)`) so it merges into the first mark run instead
        // of surfacing as its own space `Run` -- a longer, separately-
        // surfaced lead-in space was tried first and broke word-boundary
        // detection for the REST of the message: it fed one huge (~18-dit)
        // outlier into `GapClassifier`'s Farnsworth long-gap statistics
        // before any genuine word gap did, anchoring its bimodal "hi"
        // (word-gap) cluster at that outlier and pushing the char/word
        // boundary (`sqrt(lo*hi)`) above every real 7-dit word gap in the
        // scene, merging every word after the first. This does not touch
        // any Task 2-4 code; it is a test-scene fix only.
        let lo = 10f32.powf(-45.0 / 20.0);
        let mut env: Vec<f32> = vec![lo; 4];
        env.extend(rect_envelope_depth("CQ TEST W5AU W5AU TEST", 11, 45.0));
        assert_eq!(
            decode_with(Engine::EdgeLegacy, &env),
            "CQ TEST W5AU W5AU TEST"
        );
        // Documents the defect this engine fixes: fed the SAME deep-keyed
        // scene, the Legacy chain's geometric-mean threshold (SPEC v1 §3.2)
        // merges words together (its E_hi/E_lo rails can't track the deep
        // keying depth correctly at contest speed).
        assert_ne!(decode_with(Engine::Legacy, &env), "CQ TEST W5AU W5AU TEST");
    }

    #[test]
    fn legacy_engine_is_untouched() {
        let env = rect_envelope("CQ CQ DE W1AW", 18);
        assert_eq!(decode_with(Engine::Legacy, &env), "CQ CQ DE W1AW");
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
        // hop 198; the closing inter-char space starts there (pinned
        // decision 11: CharDecoded ts = start of the closing space run).
        assert_eq!(ts, 198 * 256);
    }

    #[test]
    fn text_assembly_drops_prosigns_and_collapses_spaces() {
        use crate::tree::{Glyph, Prosign};
        let ev = vec![
            DecoderEvent::char_decoded(1, 0, Glyph::Char('A'), 1.0),
            DecoderEvent::word_boundary(1, 1),
            DecoderEvent::char_decoded(1, 2, Glyph::Prosign(Prosign::Ar), 1.0),
            DecoderEvent::word_boundary(1, 3),
            DecoderEvent::char_decoded(1, 4, Glyph::Char('B'), 1.0),
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
