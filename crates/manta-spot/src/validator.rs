//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::dedupe::Dedupe;
use crate::gate::RepetitionGate;
use crate::grammar;
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
}

#[derive(Default)]
struct TrackState {
    words: VecDeque<Word>,
    current: Word,
    freq_hz: f64,
    snr_db: f32,
    wpm: f32,
}

pub struct Validator {
    cty: cty::Table,
    scp: Option<scp::Set>,
    tracks: BTreeMap<u32, TrackState>,
    gate: RepetitionGate,
    dedupe: Dedupe,
}

impl Validator {
    pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self {
        Self {
            cty: cty::Table::parse(cty_dat),
            scp: master_scp.map(scp::Set::parse),
            tracks: BTreeMap::new(),
            gate: RepetitionGate::new(fs),
            dedupe: Dedupe::new(fs),
        }
    }

    /// A production `Validator` backed by this crate's bundled `cty.dat`/
    /// `MASTER.SCP` snapshot (`crate::CTY_DAT`/`crate::MASTER_SCP`).
    pub fn bundled(fs: f64) -> Self {
        Self::new(fs, crate::CTY_DAT, Some(crate::MASTER_SCP))
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
                track.snr_db = *snr_2500_db;
                track.freq_hz = *freq_hz;
                Vec::new()
            }
        }
    }

    fn try_spot(&mut self, track_id: u32, sample_ts: u64) -> Vec<Spot> {
        let Some((candidate, spot_type)) = self.tracks.get(&track_id).and_then(|track| {
            let joined: String = track
                .words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            context::parse(&joined)
        }) else {
            return Vec::new();
        };

        let (freq_hz, snr_db, wpm) = {
            let track = self.tracks.get(&track_id).unwrap();
            (track.freq_hz, track.snr_db, track.wpm)
        };
        let char_confidences = {
            let track = self.tracks.get_mut(&track_id).unwrap();
            let Some(word) = track.words.iter_mut().rev().find(|w| w.text == candidate) else {
                return Vec::new();
            };
            if word.attempted {
                return Vec::new();
            }
            word.attempted = true;
            word.confidences.clone()
        };

        if !grammar::is_plausible(&candidate) {
            return Vec::new();
        }
        if !self.cty.is_allocated(&candidate) {
            return Vec::new();
        }

        let reps = self.gate.record(track_id, &candidate, sample_ts) as u32;
        let mut confidence = confidence::c_call(&char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        if reps < 2 {
            return Vec::new();
        }
        if !self
            .dedupe
            .should_emit(&candidate, freq_hz, snr_db, spot_type, sample_ts)
        {
            return Vec::new();
        }

        vec![Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            track_id,
            sample_ts,
        }]
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

    #[test]
    fn full_pipeline_spots_a_repeated_valid_callsign() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
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
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
    }
}
