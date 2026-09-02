//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::blocklist::Blocklist;
use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::dedupe::Dedupe;
use crate::gate::RepetitionGate;
use crate::grammar;
use crate::notch::NotchList;
use crate::scp;
use manta_decode::events::DecoderEvent;
use manta_decode::tree::{Glyph, Prosign};
use std::collections::{BTreeMap, VecDeque};

/// How many recently-completed words a track remembers for context
/// parsing. Calls/context keywords always appear within a handful of
/// words of each other in practice; this bound keeps `TrackState` small
/// without needing a time-based window here (the repetition gate and
/// dedupe windows, which *do* need to be time-based, live in `gate.rs`/
/// `dedupe.rs`).
const WORD_WINDOW: usize = 16;

/// A validated spot, ready for `manta-server` to serialize and emit.
/// No wall-clock timestamp -- that conversion happens at the
/// `manta-server` boundary (SPEC-decode-core.md §5), not here.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Spot {
    pub callsign: String,
    pub freq_hz: f64,
    pub snr_db: f32,
    pub wpm: f32,
    pub spot_type: SpotType,
    pub confidence: f32,
    pub track_id: u32,
    pub sample_ts: u64,
}

#[derive(Default)]
struct Word {
    text: String,
    confidences: Vec<f32>,
    /// Set once this word has been offered to the validation pipeline as a
    /// context-match candidate, so a later, unrelated word boundary that
    /// re-scans a growing window doesn't re-process it.
    attempted: bool,
    /// The `SpotType` this word was last processed as, if any. An
    /// allowlisted word can spot immediately with no context yet (type
    /// `Unknown`); if a trailing word later completes a real context
    /// pattern (e.g. "K5ARH" then "UP" completing `<call> UP` -> `De`),
    /// that's a genuinely new type worth a reclassification, not a
    /// re-attempt of the same evaluation -- `attempted` alone must not
    /// permanently block it (MAN-28 round 8 review).
    last_spot_type: Option<SpotType>,
    /// The repetition count computed the first time this word was
    /// evaluated. A reclassification (see `last_spot_type`) reuses this
    /// rather than calling `RepetitionGate::record` again -- the word was
    /// decoded once, not twice, so re-recording would let an ordinary,
    /// non-exempt callsign spot after a type change alone inflated its
    /// rep count to 2 (MAN-28 round 9 review).
    last_reps: u32,
}

#[derive(Default)]
struct TrackState {
    words: VecDeque<Word>,
    current: Word,
    freq_hz: f64,
    snr_db: f32,
    wpm: f32,
    /// Set once a real `TrackMeta` event has been received. `freq_hz`/
    /// `snr_db` hold bogus `0.0` defaults until then (decoder.rs emits
    /// `TrackMeta` only every 375 hops -- a fast decode can complete
    /// chars/words before the first one ever arrives), so no spot may be
    /// emitted before this is true (MAN-28 round 8 review).
    has_meta: bool,
    /// The most recent `sample_ts` seen for this track (any event kind).
    /// `try_spot` is normally only invoked by a `WordBoundary`, but a
    /// candidate held back by `has_meta` must be retried the moment
    /// metadata arrives even if no further word ever completes -- this is
    /// the timestamp that retry uses (MAN-28 round 9 review).
    last_sample_ts: u64,
}

/// A `freq_correction_ppm` value that doesn't yield a finite, positive
/// calibration factor. Rejected before construction so an invalid config
/// value (NaN, infinity, or a ppm so negative it flips the correction
/// negative or zero) can never poison an emitted spot's frequency or its
/// dedupe bucket (MAN-29).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InvalidCalibration {
    pub ppm: f64,
    pub factor: f64,
}

impl std::fmt::Display for InvalidCalibration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names the actual reason -- a ppm outside the supported range can
        // have a factor that's perfectly finite and positive, so that
        // message would be actively misleading for it (MAN-29 review
        // round 3).
        if !self.ppm.is_finite() {
            write!(f, "freq_correction_ppm {} is not finite", self.ppm)
        } else if self.ppm.abs() > MAX_ABS_PPM {
            write!(
                f,
                "freq_correction_ppm {} is outside the supported range [-{MAX_ABS_PPM}, {MAX_ABS_PPM}]",
                self.ppm
            )
        } else {
            write!(
                f,
                "freq_correction_ppm {} yields calibration factor {}, which must be finite and positive",
                self.ppm, self.factor
            )
        }
    }
}

impl std::error::Error for InvalidCalibration {}

/// Widest physically-plausible oscillator drift a real source could report
/// (a few hundred ppm covers even a badly-drifted uncalibrated RTL-SDR
/// crystal; SPEC's own worked example is 10 ppm). Bounding `ppm` this way
/// keeps the derived factor comfortably inside `[0.999, 1.001]`, which
/// rules out the overflow-to-infinity a finite-but-absurd ppm (e.g.
/// `f64::MAX`) would otherwise produce once multiplied against a real RF
/// frequency -- a factor merely being finite and positive isn't enough
/// (MAN-29 review round 2).
const MAX_ABS_PPM: f64 = 1_000.0;

/// Converts a ppm frequency-correction setting (config key
/// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4) into the
/// multiplicative factor applied to a spot's reported frequency:
/// `factor = 1.0 + ppm * 1e-6`. Errors if `ppm` is outside
/// `[-MAX_ABS_PPM, MAX_ABS_PPM]`, or the result isn't finite and positive
/// (MAN-29).
pub fn calibration_factor_from_ppm(ppm: f64) -> Result<f64, InvalidCalibration> {
    let factor = 1.0 + ppm * 1e-6;
    if !ppm.is_finite() || ppm.abs() > MAX_ABS_PPM || !factor.is_finite() || factor <= 0.0 {
        return Err(InvalidCalibration { ppm, factor });
    }
    Ok(factor)
}

/// Per-reason counts of spots suppressed by an operator override (MAN-31).
/// ARCHITECTURE §8: "Every dropped/evicted/suppressed item is counted. No
/// silent loss anywhere in the pipeline." Exposed via
/// `Validator::suppression_counts` for the future M3 metrics endpoint to
/// read, mirroring `manta_engine::track::CloseCounts` -- nothing wires it
/// externally yet, since the Prometheus text endpoint itself is explicit
/// M3 scope (ROADMAP.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuppressionCounts {
    pub blocklist: u64,
    pub notch: u64,
}

pub struct Validator {
    cty: cty::Table,
    scp: Option<scp::Set>,
    tracks: BTreeMap<u32, TrackState>,
    gate: RepetitionGate,
    dedupe: Dedupe,
    freq_calibration: f64,
    allowlist: std::collections::BTreeSet<String>,
    blocklist: Blocklist,
    notch: NotchList,
    suppression_counts: SuppressionCounts,
}

impl Validator {
    pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self {
        Self {
            cty: cty::Table::parse(cty_dat),
            scp: master_scp.map(scp::Set::parse),
            tracks: BTreeMap::new(),
            gate: RepetitionGate::new(fs),
            dedupe: Dedupe::new(fs),
            freq_calibration: 1.0,
            allowlist: std::collections::BTreeSet::new(),
            blocklist: Blocklist::default(),
            notch: NotchList::default(),
            suppression_counts: SuppressionCounts::default(),
        }
    }

    /// A production `Validator` backed by this crate's bundled `cty.dat`/
    /// `MASTER.SCP` snapshot (`crate::CTY_DAT`/`crate::MASTER_SCP`).
    pub fn bundled(fs: f64) -> Self {
        Self::new(fs, crate::CTY_DAT, Some(crate::MASTER_SCP))
    }

    /// Sets the per-source frequency-calibration correction (config key
    /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4 -- the
    /// oscillator-accuracy setting that section names as out of scope for
    /// the ±10 Hz decode-accuracy figure it defines, ARCHITECTURE §6 step
    /// 5). Corrects a systematically drifted source clock/LO (legacy
    /// precedent: CW Skimmer/SkimSrv's `FreqCalibration=` .ini key, though
    /// that key is a raw multiplier -- this crate's contract is ppm, per
    /// the spec). Applied to a spot's reported frequency before emission
    /// (MAN-29). Errors if `ppm` is outside the supported range
    /// `[-1000, 1000]` (any physically-plausible oscillator drift; see
    /// `MAX_ABS_PPM`), or doesn't yield a finite, positive correction
    /// factor within that range.
    pub fn with_freq_correction_ppm(mut self, ppm: f64) -> Result<Self, InvalidCalibration> {
        self.freq_calibration = calibration_factor_from_ppm(ppm)?;
        Ok(self)
    }

    /// Adds `call` to the operator's Watch List (MAN-28): an explicitly
    /// allowlisted callsign bypasses grammar/cty validation and the
    /// repetition gate entirely, matching CW Skimmer's Watch List
    /// behavior (Aggregator manual Appendix A2). Checked after the MAN-31
    /// suppression overrides below -- an explicit blocklist/notch entry is
    /// the more specific, deliberate override and is never silently
    /// defeated by a broader allowlist entry.
    pub fn allowlist(&mut self, call: &str) {
        self.allowlist.insert(call.to_ascii_uppercase());
    }

    /// Sets the operator's bad-callsign blocklist (MAN-31). Empty by
    /// default -- no suppression until the operator supplies one.
    pub fn with_blocklist(mut self, blocklist: Blocklist) -> Self {
        self.blocklist = blocklist;
        self
    }

    /// Sets the operator's notched-frequency list (MAN-31). Empty by
    /// default -- no suppression until the operator supplies one.
    pub fn with_notch(mut self, notch: NotchList) -> Self {
        self.notch = notch;
        self
    }

    /// Per-reason counts of operator-suppressed spots so far (MAN-31,
    /// ARCHITECTURE §8).
    pub fn suppression_counts(&self) -> SuppressionCounts {
        self.suppression_counts
    }

    /// Feeds one decoder event in. Returns zero or more validated spots
    /// (almost always zero -- a spot only comes out on the event that
    /// completes a passing candidate's word).
    pub fn ingest(&mut self, event: &DecoderEvent) -> Vec<Spot> {
        match event {
            DecoderEvent::CharDecoded {
                track_id,
                glyph,
                confidence,
                ..
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                match glyph {
                    Glyph::Char(c) => {
                        track.current.text.push(c.to_ascii_uppercase());
                        track.current.confidences.push(*confidence);
                    }
                    Glyph::Prosign(Prosign::Err) => {
                        // SPEC §4.4: operator-error prosign discards the
                        // current word buffer back to the previous
                        // boundary.
                        track.current = Word::default();
                    }
                    Glyph::Prosign(_) => {}
                }
                Vec::new()
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                if !track.current.text.is_empty() {
                    let word = std::mem::take(&mut track.current);
                    track.words.push_back(word);
                    if track.words.len() > WORD_WINDOW {
                        track.words.pop_front();
                    }
                }
                track.last_sample_ts = *sample_ts;
                self.try_spot(*track_id, *sample_ts)
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                self.tracks.entry(*track_id).or_default().wpm = *wpm;
                Vec::new()
            }
            DecoderEvent::TrackMeta {
                track_id,
                snr_2500_db,
                freq_hz,
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                let had_meta = track.has_meta;
                track.snr_db = *snr_2500_db;
                track.freq_hz = *freq_hz;
                track.has_meta = true;
                let sample_ts = track.last_sample_ts;
                // Retry: a candidate held back by the has_meta gate is
                // otherwise only ever re-evaluated by a later
                // WordBoundary, which a short transmission may never
                // produce again -- silently losing the pending exemption
                // (MAN-28 round 9 review). No-op once already true.
                if had_meta {
                    Vec::new()
                } else {
                    self.try_spot(*track_id, sample_ts)
                }
            }
        }
    }

    /// Gathers every candidate word worth evaluating this event: at most
    /// one from context parsing, plus every allowlisted word in the
    /// window not yet attempted. Independent sources, not one-or-the-
    /// other by priority (MAN-28 round 7 review): a stale, already-
    /// attempted context match elsewhere in the 16-word window must never
    /// block discovery of a different, freshly-allowlisted word -- the
    /// whole window is scanned (not just the newest word) so a qualifying
    /// word is found the moment it's allowlisted, `word.attempted`
    /// (checked in `evaluate_candidate`) prevents re-processing one this
    /// already spotted or rejected.
    fn candidates(&self, track_id: u32) -> Vec<(String, SpotType)> {
        let Some(track) = self.tracks.get(&track_id) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();

        let joined: String = track
            .words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(context_match) = context::parse(&joined) {
            candidates.push(context_match);
        }

        // MAN-28 Watch List: an allowlisted word is found independently of
        // context parsing -- including with no recognized CQ/DE/UP/beacon
        // pattern at all, the primary real-world case (an NCDXF beacon
        // transmits its callsign followed by power-step dashes, no
        // framing words). `SpotType::Unknown` is the context-parse-
        // documented fallback for exactly this case.
        for word in &track.words {
            if self.allowlist.contains(&word.text)
                && !candidates.iter().any(|(c, _)| *c == word.text)
            {
                candidates.push((word.text.clone(), SpotType::Unknown));
            }
        }

        candidates
    }

    fn try_spot(&mut self, track_id: u32, sample_ts: u64) -> Vec<Spot> {
        // No real TrackMeta yet -- freq_hz/snr_db still hold bogus 0.0
        // defaults. Bail without marking anything attempted, so pending
        // candidates are simply re-evaluated once metadata does arrive.
        if !self.tracks.get(&track_id).is_some_and(|t| t.has_meta) {
            return Vec::new();
        }
        self.candidates(track_id)
            .into_iter()
            .filter_map(|(candidate, spot_type)| {
                self.evaluate_candidate(track_id, sample_ts, candidate, spot_type)
            })
            .collect()
    }

    fn evaluate_candidate(
        &mut self,
        track_id: u32,
        sample_ts: u64,
        candidate: String,
        spot_type: SpotType,
    ) -> Option<Spot> {
        let (freq_hz, snr_db, wpm) = {
            let track = self.tracks.get(&track_id)?;
            (
                track.freq_hz * self.freq_calibration,
                track.snr_db,
                track.wpm,
            )
        };
        let (char_confidences, reclassifying) = {
            let track = self.tracks.get_mut(&track_id)?;
            let word = track.words.iter_mut().rev().find(|w| w.text == candidate)?;
            if word.attempted {
                // A prior attempt with the SAME type is a genuine
                // re-attempt (nothing new to evaluate). A prior attempt
                // with a DIFFERENT, non-Unknown-losing type means
                // trailing context has since completed a real pattern --
                // a reclassification, not a re-attempt; let it through so
                // dedupe's type-changed override (below) can decide
                // whether to emit the corrected spot. Reclassification
                // only ever promotes (e.g. Unknown -> De, or De -> a
                // still-more-specific type): a transition INTO Unknown
                // must never be accepted -- that isn't newly-arrived
                // context, it's an already-contextualized word's framing
                // word aging out of the window, which must not erase the
                // classification it already earned (MAN-28 round 11
                // review).
                if word.last_spot_type == Some(spot_type) || spot_type == SpotType::Unknown {
                    return None;
                }
            }
            let reclassifying = word.attempted;
            word.attempted = true;
            word.last_spot_type = Some(spot_type);
            (word.confidences.clone(), reclassifying)
        };

        // Operator suppression overrides (MAN-31) -- orthogonal to, and
        // checked ahead of, both the automatic validation pipeline and the
        // MAN-28 allowlist below: an explicit blocklist/notch entry is the
        // operator's more specific, deliberate override and must not be
        // silently defeated by a broader allowlist entry. Each hit is
        // counted (ARCHITECTURE §8) so it reads as a deliberate
        // suppression, not silent coverage loss.
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            return None;
        }

        // MAN-28 Watch List: an allowlisted callsign bypasses grammar/cty
        // validation and the repetition gate below entirely.
        let is_allowlisted = self.allowlist.contains(&candidate);

        if !is_allowlisted {
            if !grammar::is_plausible(&candidate) {
                return None;
            }
            if !self.cty.is_allocated(&candidate) {
                return None;
            }
        }

        // A reclassification is the same decode re-typed, not a new one --
        // reuse its already-recorded repetition count instead of calling
        // `gate.record` again, which would otherwise let a type change
        // alone inflate an ordinary, non-exempt callsign's rep count past
        // the repetition gate after only one real decode (MAN-28 round 9
        // review).
        let reps = if reclassifying {
            let track = self.tracks.get(&track_id)?;
            track
                .words
                .iter()
                .rev()
                .find(|w| w.text == candidate)
                .map(|w| w.last_reps)
                .unwrap_or(0)
        } else {
            self.gate.record(track_id, &candidate, sample_ts) as u32
        };
        {
            let track = self.tracks.get_mut(&track_id)?;
            if let Some(word) = track.words.iter_mut().rev().find(|w| w.text == candidate) {
                word.last_reps = reps;
            }
        }
        let mut confidence = confidence::c_call(&char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        // ARCHITECTURE §6.4 exempts BEACON-tagged messages from the
        // repetition requirement: NCDXF-style beacons ID once per cycle,
        // so a single decode must still spot (MAN-28).
        if !is_allowlisted && spot_type != SpotType::Beacon && reps < 2 {
            return None;
        }
        if !self
            .dedupe
            .should_emit(&candidate, freq_hz, snr_db, spot_type, sample_ts)
        {
            return None;
        }

        Some(Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            track_id,
            sample_ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

    fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for c in text.chars() {
            events.push(DecoderEvent::CharDecoded {
                track_id,
                sample_ts: ts,
                glyph: Glyph::Char(c),
                confidence: 0.95,
            });
            ts += 100;
        }
        events.push(DecoderEvent::WordBoundary {
            track_id,
            sample_ts: ts,
        });
        ts += 100;
        (events, ts)
    }

    fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for word in words {
            let (mut w_events, next_ts) = word_events(track_id, word, ts);
            events.append(&mut w_events);
            ts = next_ts;
        }
        events
    }

    fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
        events.iter().flat_map(|e| v.ingest(e)).collect()
    }

    /// Real telemetry, so `try_spot`'s `has_meta` gate (MAN-28 round 8)
    /// doesn't hold back every spot in tests that don't otherwise care
    /// about metadata timing.
    fn seed_meta(v: &mut Validator, track_id: u32) {
        v.ingest(&DecoderEvent::TrackMeta {
            track_id,
            snr_2500_db: 20.0,
            freq_hz: 14_000_000.0,
        });
    }

    #[test]
    fn full_pipeline_spots_a_repeated_valid_callsign() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
        assert_eq!(spots[0].track_id, 1);
    }

    #[test]
    fn ungrammatical_text_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let words = ["DE", "12345", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert!(spots.is_empty());
    }

    #[test]
    fn error_prosign_discards_current_word() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let events = vec![
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 0,
                glyph: Glyph::Char('D'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 100,
                glyph: Glyph::Char('E'),
                confidence: 0.9,
            },
            DecoderEvent::WordBoundary {
                track_id: 1,
                sample_ts: 200,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 300,
                glyph: Glyph::Char('K'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 400,
                glyph: Glyph::Prosign(Prosign::Err),
                confidence: 0.0,
            },
        ];
        // after the <ERR> prosign, the partial "K" must be gone.
        for e in &events {
            v.ingest(e);
        }
        let track = v.tracks.get(&1).unwrap();
        assert!(track.current.text.is_empty());
        assert_eq!(track.words.len(), 1);
        assert_eq!(track.words[0].text, "DE");
    }

    #[test]
    fn bundled_validator_spots_a_real_repeated_callsign() {
        let mut v = Validator::bundled(FS);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
    }

    /// MAN-29: a configured per-source frequency-calibration correction
    /// (config key `input.freq_correction_ppm`, SPEC-decode-core.md §1.4)
    /// corrects a spot's reported frequency before emission -- distinct
    /// from the ~10 Hz decode-accuracy figure (ARCHITECTURE §6 step 5),
    /// which is decode precision, not a drifted source clock/LO.
    #[test]
    fn calibration_ppm_corrects_emitted_spot_frequency() {
        const RAW_FREQ_HZ: f64 = 14_027_000.0;
        const PPM: f64 = 10.0; // SPEC's own worked example (§1.4).

        let mut v = Validator::new(FS, CTY_FIXTURE, None)
            .with_freq_correction_ppm(PPM)
            .unwrap();
        v.ingest(&DecoderEvent::TrackMeta {
            track_id: 1,
            snr_2500_db: 20.0,
            freq_hz: RAW_FREQ_HZ,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));

        assert_eq!(spots.len(), 1);
        let expected = RAW_FREQ_HZ * (1.0 + PPM * 1e-6);
        assert!(
            (spots[0].freq_hz - expected).abs() < 1e-6,
            "spot freq_hz {} should equal raw {RAW_FREQ_HZ} * (1 + {PPM}ppm) = {expected}",
            spots[0].freq_hz
        );
    }

    #[test]
    fn default_calibration_is_identity() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        v.ingest(&DecoderEvent::TrackMeta {
            track_id: 1,
            snr_2500_db: 20.0,
            freq_hz: 14_027_000.0,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));

        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].freq_hz, 14_027_000.0);
    }

    #[test]
    fn calibration_factor_from_ppm_zero_is_identity() {
        assert_eq!(calibration_factor_from_ppm(0.0), Ok(1.0));
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_nan() {
        assert!(calibration_factor_from_ppm(f64::NAN).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_infinity() {
        assert!(calibration_factor_from_ppm(f64::INFINITY).is_err());
        assert!(calibration_factor_from_ppm(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_a_factor_that_hits_zero_or_goes_negative() {
        // ppm = -1_000_000 drives factor to exactly 0.0; anything more
        // negative flips it negative.
        assert!(calibration_factor_from_ppm(-1_000_000.0).is_err());
        assert!(calibration_factor_from_ppm(-2_000_000.0).is_err());
    }

    /// MAN-29 review round 2: a finite ppm whose derived factor is itself
    /// finite and positive can still overflow to infinity once multiplied
    /// against a real RF frequency (e.g. `f64::MAX` -> factor ~1.8e302).
    /// Reject ppm outside any physically-plausible oscillator drift, not
    /// just outside "finite and positive".
    #[test]
    fn calibration_factor_from_ppm_rejects_absurdly_large_finite_ppm() {
        assert!(calibration_factor_from_ppm(f64::MAX).is_err());
        assert!(calibration_factor_from_ppm(1e300).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_accepts_realistic_oscillator_drift() {
        // SPEC's own worked example (10 ppm) and a generously bad cheap-SDR
        // crystal (a few hundred ppm) both stay accepted.
        assert!(calibration_factor_from_ppm(10.0).is_ok());
        assert!(calibration_factor_from_ppm(-500.0).is_ok());
    }

    #[test]
    fn with_freq_correction_ppm_rejects_an_invalid_factor_before_use() {
        match Validator::new(FS, CTY_FIXTURE, None).with_freq_correction_ppm(f64::NAN) {
            Ok(_) => panic!("expected NaN ppm to be rejected"),
            Err(err) => assert!(err.ppm.is_nan()),
        }
    }

    /// MAN-29 review round 3: a ppm just outside the supported range
    /// (whose derived factor is otherwise perfectly finite and positive)
    /// must not be reported with the "must be finite and positive"
    /// message -- that's not why it failed, and the message must name the
    /// actual supported range so a caller can fix their input.
    #[test]
    fn invalid_calibration_display_names_the_range_when_ppm_is_out_of_range() {
        let err = calibration_factor_from_ppm(1_001.0).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("must be finite and positive"),
            "1001 ppm's factor (1.001001) IS finite and positive -- the real problem is the \
             ppm range, message was: {msg}"
        );
        assert!(
            msg.contains("1000"),
            "expected the message to name the supported range, got: {msg}"
        );
    }

    #[test]
    fn invalid_calibration_display_reports_non_finite_ppm() {
        let err = calibration_factor_from_ppm(f64::NAN).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }
}
