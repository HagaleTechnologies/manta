//! MAN-9 Phase 1: V8w classical-baseline diagnostic instrument.
//!
//! In-process (no CLI subprocess, no WAV round-trip): renders the V8w scene
//! once per test-binary run and decodes directly via
//! `manta_engine::decode_samples`, the same function `manta decode --json`
//! calls -- so a run's numbers are the production pipeline's numbers, just
//! reached without paying for a subprocess spawn or a float32 WAV
//! write/read (lossless per `manta-testkit`'s `fixture_roundtrips_through_wav`
//! test) on every config variant swept. This is what makes it practical to
//! evaluate the Phase 2-4 rung sweeps' several config values each against
//! the full 120 s / 50-signal / 34-strong-signal scene.

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_testkit::vectors::{v8w, RenderedVector, VectorSpec};
use std::collections::BTreeMap;
use std::sync::OnceLock;

fn v8w_rendered() -> &'static RenderedVector {
    static CACHE: OnceLock<RenderedVector> = OnceLock::new();
    CACHE.get_or_init(|| manta_testkit::vectors::render(&v8w()).unwrap())
}

/// Decode the cached V8w render under `cfg`. In-process equivalent of
/// `manta decode --json` (`decode_wav` is just this + a WAV read).
pub fn decode_v8w_with(cfg: &PipelineConfig) -> DecodeReport {
    let spec = v8w();
    let rendered = v8w_rendered();
    decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, cfg).unwrap()
}

fn track_id_of(ev: &DecoderEvent) -> u32 {
    match ev {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::TrackClosed { track_id } => *track_id,
    }
}

/// Extract the callsign from this project's SPEC §7 payload template
/// "CQ CQ DE <CALL> <CALL> K" (word index 3). Same convention as
/// `golden_v8_v8w.rs`'s helper of the same name.
fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(3)
        .expect("keyed text must follow the 'CQ CQ DE <CALL> <CALL> K' template")
}

/// Per-track decoded text, last-reported `TrackMeta.freq_hz`, and the
/// [min, max] `sample_ts` span across every event this track produced.
/// The span (not `CloseReason`, which is not currently attributable to a
/// specific historical `track_id` from the public event stream) is what
/// the F1-vs-F2 classifier below uses.
pub struct TrackInfo {
    pub text: String,
    pub freq_hz: Option<f64>,
    pub ts_span: Option<(u64, u64)>,
}

pub fn per_track(events: &[DecoderEvent]) -> BTreeMap<u32, TrackInfo> {
    let mut texts: BTreeMap<u32, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u32, f64> = BTreeMap::new();
    let mut spans: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for ev in events {
        let tid = track_id_of(ev);
        match ev {
            DecoderEvent::CharDecoded {
                glyph, sample_ts, ..
            } => {
                if let Some(c) = glyph.text_char() {
                    texts.entry(tid).or_default().push(c);
                }
                let e = spans.entry(tid).or_insert((*sample_ts, *sample_ts));
                e.0 = e.0.min(*sample_ts);
                e.1 = e.1.max(*sample_ts);
            }
            DecoderEvent::WordBoundary { sample_ts, .. } => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
                let e = spans.entry(tid).or_insert((*sample_ts, *sample_ts));
                e.0 = e.0.min(*sample_ts);
                e.1 = e.1.max(*sample_ts);
            }
            DecoderEvent::TrackMeta { freq_hz, .. } => {
                freqs.insert(tid, *freq_hz);
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for (tid, text) in texts {
        out.insert(
            tid,
            TrackInfo {
                text: text.trim().to_string(),
                freq_hz: freqs.get(&tid).copied(),
                ts_span: spans.get(&tid).copied(),
            },
        );
    }
    out
}

/// For each expected signal (`spec.signals` order), the decoded track
/// whose last-reported `freq_hz` is closest to that signal's expected
/// absolute frequency. Same "one track per expected signal, always"
/// convention as `golden_v8_v8w.rs::match_tracks_by_freq` -- this is why
/// fragmentation inflates CER instead of being invisible to the matcher.
fn match_by_freq<'a>(
    spec: &VectorSpec,
    tracks: &'a BTreeMap<u32, TrackInfo>,
) -> Vec<(&'a str, Option<f64>)> {
    spec.signals
        .iter()
        .map(|s| {
            let expected_freq = spec.center_freq_hz + s.offset_hz;
            tracks
                .values()
                .min_by(|a, b| {
                    let da = (a.freq_hz.unwrap_or(f64::MAX) - expected_freq).abs();
                    let db = (b.freq_hz.unwrap_or(f64::MAX) - expected_freq).abs();
                    da.partial_cmp(&db).unwrap()
                })
                .map(|t| (t.text.as_str(), t.freq_hz))
                .unwrap_or(("", None))
        })
        .collect()
}

/// Per-signal diagnostic row: SNR/WPM (scene ground truth), CER, length
/// ratio, and track-cluster size -- the artifact `ROADMAP.md`'s M4
/// acceptance criterion ("fusion beats classical-only CER by a measured,
/// documented margin") has to diff against.
#[derive(Debug, Clone)]
pub struct SignalRow {
    pub idx: usize,
    pub call: String,
    pub snr_2500_db: f32,
    pub wpm: f32,
    pub cer: f64,
    pub len_ratio: f64,
    pub tracks_within_300hz: usize,
}

fn tracks_near(
    spec: &VectorSpec,
    tracks: &BTreeMap<u32, TrackInfo>,
    expected_freq_hz: f64,
    within_hz: f64,
) -> usize {
    let _ = spec;
    tracks
        .values()
        .filter(|t| {
            t.freq_hz
                .is_some_and(|f| (f - expected_freq_hz).abs() <= within_hz)
        })
        .count()
}

/// Full 50-row diagnostic table for one decode.
pub fn per_signal_rows(spec: &VectorSpec, report: &DecodeReport) -> Vec<SignalRow> {
    let tracks = per_track(&report.events);
    let matched = match_by_freq(spec, &tracks);
    spec.signals
        .iter()
        .enumerate()
        .map(|(i, sig)| {
            let expected_freq_hz = spec.center_freq_hz + sig.offset_hz;
            let (decoded, _freq) = matched[i];
            let expected = &report_expected_text(spec, i);
            let cer = manta_testkit::cer::cer(expected, decoded);
            let len_ratio = if expected.chars().count() == 0 {
                0.0
            } else {
                decoded.chars().count() as f64 / expected.chars().count() as f64
            };
            SignalRow {
                idx: i,
                call: call_from_keyed_text(expected).to_string(),
                snr_2500_db: sig.snr_2500_db,
                wpm: sig.wpm,
                cer,
                len_ratio,
                tracks_within_300hz: tracks_near(spec, &tracks, expected_freq_hz, 300.0),
            }
        })
        .collect()
}

/// Ground-truth keyed text for signal `i`. All V8w signals key the same
/// deterministic looped template, so this is reconstructible from the spec
/// alone without re-rendering (the real per-signal keyed text lives in
/// `RenderedVector::keyed_texts`, but callers that only have a `VectorSpec`
/// -- e.g. a sweep loop that re-renders nothing -- use the cached render).
fn report_expected_text(_spec: &VectorSpec, i: usize) -> String {
    v8w_rendered().keyed_texts[i].clone()
}

/// Strong-signal (>= +6 dB) pass count and median CER -- the two numbers
/// every rung's accept test in the MAN-9 plan is judged against.
pub fn strong_signal_stats(spec: &VectorSpec, report: &DecodeReport) -> (usize, f64, Vec<f64>) {
    let rows = per_signal_rows(spec, report);
    let mut cers: Vec<f64> = rows
        .iter()
        .filter(|r| r.snr_2500_db >= 6.0)
        .map(|r| r.cer)
        .collect();
    cers.sort_by(f64::total_cmp);
    let passes = cers.iter().filter(|&&c| c < 0.10).count();
    let median = if cers.is_empty() {
        0.0
    } else if cers.len() % 2 == 1 {
        cers[cers.len() / 2]
    } else {
        (cers[cers.len() / 2 - 1] + cers[cers.len() / 2]) / 2.0
    };
    (passes, median, cers)
}

/// Signals whose matched track is fragmented: decoded/expected length
/// ratio < 0.3 (ignore-comment convention: idx 25/41/44 at 0.09-0.22 vs.
/// 1.0-1.3 for full-duration captures).
pub fn fragmented_signal_indices(spec: &VectorSpec, report: &DecodeReport) -> Vec<usize> {
    per_signal_rows(spec, report)
        .iter()
        .filter(|r| r.snr_2500_db >= 6.0 && r.len_ratio < 0.3)
        .map(|r| r.idx)
        .collect()
}

/// Discriminates the two fragmentation hypotheses for the strong signals
/// with `len_ratio < 0.3` (ignore-comment: idx 25, 41, 44):
///
/// F1 (sequential drop/reacquire): the tracks near one expected frequency
///     are DISJOINT in decoded-event time (no two spans overlap).
/// F2 (concurrent spectral spread): their event-time spans OVERLAP.
///
/// Reports only -- the verdict per signal is transcribed into the pin doc
/// and selects Phase 5's branch (see that phase's doc comment for why:
/// F1 wants a longer track-hang, F2 wants a wider merge radius, and
/// applying the wrong one for a given signal's failure mode is a wasted,
/// possibly-regressive rung).
#[test]
#[ignore]
fn v8w_fragmentation_is_sequential_or_concurrent() {
    let spec = v8w();
    let report = decode_v8w_with(&PipelineConfig::default());
    let tracks = per_track(&report.events);

    let fragmented = fragmented_signal_indices(&spec, &report);
    assert!(
        !fragmented.is_empty(),
        "expected the known fragmented signals (idx 25/41/44 at the MAN-9 \
         baseline); scene or decoder changed enough that this classifier's \
         premise no longer holds -- re-derive the fragmented set before \
         trusting Phase 5's branch selection"
    );

    for idx in fragmented {
        let sig = &spec.signals[idx];
        let expected_freq_hz = spec.center_freq_hz + sig.offset_hz;
        let cluster: Vec<&TrackInfo> = tracks
            .values()
            .filter(|t| {
                t.freq_hz
                    .is_some_and(|f| (f - expected_freq_hz).abs() <= 300.0)
            })
            .collect();
        let spans: Vec<(u64, u64)> = cluster.iter().filter_map(|t| t.ts_span).collect();
        let mut overlaps = 0usize;
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                let (a, b) = (spans[i], spans[j]);
                if a.0 <= b.1 && b.0 <= a.1 {
                    overlaps += 1;
                }
            }
        }
        let verdict = if overlaps == 0 {
            "F1 (sequential)"
        } else {
            "F2 (concurrent)"
        };
        println!(
            "idx {idx}: {} tracks within 300 Hz, {overlaps} overlapping pairs -> {verdict}",
            cluster.len()
        );
    }
}

/// Reports `TrackManager::close_counts` for the default-config V8w decode,
/// corroborating the F1 hypothesis (a nonzero `hang_expired` count) when
/// present. Aggregate, not per-cluster-attributed (the public event stream
/// has no per-`track_id` `CloseReason`), but a nonzero count here plus a
/// disjoint-span cluster from the classifier above is two independent
/// signals pointing the same direction.
#[test]
#[ignore]
fn v8w_close_counts_report() {
    let report = decode_v8w_with(&PipelineConfig::default());
    println!("close_counts: {:?}", report.close_counts);
}

// --- MAN-9 scratch sweep harness -------------------------------------
//
// Not part of the plan's Phase 1 deliverable: a throwaway driver used to
// evaluate each rung's candidate values against the full V8w scene
// in-process (one render, N decodes, no CLI subprocess/WAV round-trip per
// candidate -- see this file's header comment). Removed before the ticket
// ships; every number it prints is transcribed into the pin doc as it's
// produced.
#[test]
#[ignore]
fn man9_sweep_rung1_debounce_dits() {
    let spec = v8w();
    for &debounce_dits in &[0.0f32, 0.15, 0.25, 0.35] {
        let mut cfg = PipelineConfig::default();
        cfg.decode.demod.debounce_dits = debounce_dits;
        let report = decode_v8w_with(&cfg);
        let (passes, median, _) = strong_signal_stats(&spec, &report);
        println!("debounce_dits={debounce_dits:.2}: passes={passes}/34 median_cer={median:.4}");
    }
}

#[test]
#[ignore]
fn man9_sweep_rung2_beam_width() {
    let spec = v8w();
    // Coordinate descent, not a full 4x3 grid: q_low fixed at the spec
    // default (0.6) while width_low_q sweeps. Once the winner is known, a
    // second pass sweeps q_low at that fixed width -- 6 decodes instead
    // of 12, ~200 s each on this 2-core sandbox. Recorded as a
    // compute-budget adaptation in the pin doc, not a silent scope cut:
    // MAN-9 Rung 1 showed zero V8w sensitivity to its own swept
    // parameter, so a reduced-but-still-empirical Rung 2 sweep is a
    // reasonable trade against another ~40 minutes of full-grid decode
    // time for a lever the plan itself flags as comparatively low-risk
    // (width is provably non-inert but bounded in effect per character).
    for &width_low_q in &[4usize, 8, 12, 16] {
        let mut cfg = PipelineConfig::default();
        cfg.decode.beam.width_low_q = width_low_q;
        cfg.decode.beam.q_low = 0.6;
        let report = decode_v8w_with(&cfg);
        let (passes, median, _) = strong_signal_stats(&spec, &report);
        println!("width_low_q={width_low_q} q_low=0.60: passes={passes}/34 median_cer={median:.4}");
    }
}

/// Diagnostic: is `q_low = 0.6` ever actually reached by V8w's per-track
/// demod-rail `q`? All four `man9_sweep_rung2_beam_width` cells came back
/// byte-identical, which is the signature of `q_low` sitting above every
/// observed `q` (so `effective_width` never selects `width_low_q`) rather
/// than width being a truly inert lever. A much more permissive `q_low`
/// sanity-checks which explanation holds.
#[test]
#[ignore]
fn man9_sweep_rung2_q_low_sanity() {
    let spec = v8w();
    for &q_low in &[0.6f32, 0.8, 0.95] {
        let mut cfg = PipelineConfig::default();
        cfg.decode.beam.width_low_q = 16;
        cfg.decode.beam.q_low = q_low;
        let report = decode_v8w_with(&cfg);
        let (passes, median, _) = strong_signal_stats(&spec, &report);
        println!("width_low_q=16 q_low={q_low:.2}: passes={passes}/34 median_cer={median:.4}");
    }
}

#[test]
#[ignore]
fn man9_sweep_rung3_mark_admission() {
    use manta_decode::timing::MarkAdmission;
    let spec = v8w();
    for &(lo, hi) in &[
        (0.0f32, f32::INFINITY), // off (SPEC default)
        (0.55, 1.9),
        (0.45, 2.2),
        (0.35, 2.6),
    ] {
        let mut cfg = PipelineConfig::default();
        cfg.decode.mark_admission = MarkAdmission { lo, hi };
        let report = decode_v8w_with(&cfg);
        let (passes, median, _) = strong_signal_stats(&spec, &report);
        println!("admission=({lo:.2},{hi:.2}): passes={passes}/34 median_cer={median:.4}");
    }
}

#[test]
#[ignore]
fn man9_sweep_phase5_merge_radius() {
    let spec = v8w();
    for &radius in &[1.0f64, 1.5, 2.0, 2.5] {
        let mut cfg = PipelineConfig::default();
        cfg.detector.merge_radius_channels = radius;
        let report = decode_v8w_with(&cfg);
        let (passes, median, _) = strong_signal_stats(&spec, &report);
        let fragmented = fragmented_signal_indices(&spec, &report);
        println!(
            "merge_radius={radius:.1}: passes={passes}/34 median_cer={median:.4} fragmented={:?}",
            fragmented
        );
    }
}
