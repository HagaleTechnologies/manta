//! MAN-9 Phase 1: discriminates the two fragmentation hypotheses for the
//! V8w pileup's 3/34 strong signals with `len_ratio < 0.3` (the golden
//! test's ignore-comment: idx 25, 41, 44). `#[ignore]`d diagnostic
//! instrument, not a gate -- decides which branch of Phase 5's track-
//! continuity fix applies. See
//! docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md.
//!
//! - F1 (sequential drop/reacquire): a fade holds the signal below
//!   `off_snr_db` longer than `hang_hops`, the track closes
//!   `HangExpired`, and the returning signal spawns a new, unrelated
//!   `track_id`. Predicts: the cluster's tracks are DISJOINT in
//!   decoded-event time.
//! - F2 (concurrent spectral spread): CCIR-poor's delay spread/Doppler
//!   smears the signal across several channels, sustaining more than one
//!   simultaneously-open track. Predicts: tracks OVERLAP in time.
//!
//! `cargo test -p manta-engine --test v8w_fading_diagnostics -- --ignored
//! --nocapture`

use manta_decode::events::DecoderEvent;
use manta_engine::PipelineConfig;
use manta_testkit::vectors::VectorSpec;
use std::collections::BTreeMap;

const NEAR_HZ: f64 = 300.0;
const FRAGMENTED_LEN_RATIO: f64 = 0.3;

/// CR-gamma: `text` is `Some` only once a `CharDecoded`/`WordBoundary` event
/// has actually been seen for this track -- mirrors `golden_v8_v8w.rs`'s
/// `per_track` exactly. `nearest_track` (used for the CER-shaped
/// `len_ratio` computation below) filters to this population so it can
/// never select a `TrackMeta`-only fragment the golden harness cannot see.
/// `tracks_near` (cluster/overlap counting) deliberately keeps every entry,
/// `TrackMeta`-only ones included -- that is the population this file's own
/// "N tracks within 300 Hz" figures describe.
#[derive(Debug, Default)]
struct TrackSummary {
    text: Option<String>,
    freq_hz: Option<f64>,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
}

fn per_track(events: &[DecoderEvent]) -> BTreeMap<u32, TrackSummary> {
    let mut out: BTreeMap<u32, TrackSummary> = BTreeMap::new();
    let touch = |out: &mut BTreeMap<u32, TrackSummary>, track_id: u32, ts: u64| {
        let e = out.entry(track_id).or_default();
        e.first_ts = Some(e.first_ts.map_or(ts, |f| f.min(ts)));
        e.last_ts = Some(e.last_ts.map_or(ts, |l| l.max(ts)));
    };
    for ev in events {
        match ev {
            DecoderEvent::CharDecoded {
                track_id,
                sample_ts,
                glyph,
                ..
            } => {
                if let Some(c) = glyph.text_char() {
                    out.entry(*track_id)
                        .or_default()
                        .text
                        .get_or_insert_with(String::new)
                        .push(c);
                }
                touch(&mut out, *track_id, *sample_ts);
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
            } => {
                let e = out.entry(*track_id).or_default();
                let t = e.text.get_or_insert_with(String::new);
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
                touch(&mut out, *track_id, *sample_ts);
            }
            DecoderEvent::TrackMeta {
                track_id, freq_hz, ..
            } => {
                out.entry(*track_id).or_default().freq_hz = Some(*freq_hz);
            }
            _ => {}
        }
    }
    out
}

fn expected_freq(spec: &VectorSpec, idx: usize) -> f64 {
    spec.center_freq_hz + spec.signals[idx].offset_hz
}

/// Nearest-by-frequency track AMONG THOSE WITH A DECODED-TEXT EVENT only --
/// same restriction as `golden_v8_v8w.rs`'s `match_tracks_by_freq` (CR-gamma),
/// so a closer-in-frequency `TrackMeta`-only fragment can never be picked
/// over the real, farther track that actually carries the signal's text.
fn nearest_track(
    tracks: &BTreeMap<u32, TrackSummary>,
    expected_freq_hz: f64,
) -> Option<&TrackSummary> {
    tracks
        .values()
        .filter(|t| t.text.is_some())
        .min_by(|a, b| {
            let da = (a.freq_hz.unwrap_or(f64::MAX) - expected_freq_hz).abs();
            let db = (b.freq_hz.unwrap_or(f64::MAX) - expected_freq_hz).abs();
            da.partial_cmp(&db).unwrap()
        })
}

/// CR-7 (MAN-9 pin doc): strictly less than, not `<=` -- `radius_hz` is
/// called with `NEAR_HZ` (300.0), exactly the scene's own
/// `MIN_SEPARATION_HZ`, so an inclusive bound could count a neighboring
/// signal's legitimate track as one of this signal's fragments.
fn tracks_near(
    tracks: &BTreeMap<u32, TrackSummary>,
    expected_freq_hz: f64,
    radius_hz: f64,
) -> Vec<&TrackSummary> {
    tracks
        .values()
        .filter(|t| {
            t.freq_hz
                .is_some_and(|f| (f - expected_freq_hz).abs() < radius_hz)
        })
        .collect()
}

/// Strong (`snr_2500_db >= 6.0`) signal indices whose nearest-track
/// `len_ratio` (matching the golden test's own definition) is below
/// `FRAGMENTED_LEN_RATIO`. Same 25/41/44 set the ignore-comment records,
/// re-derived here from the native event stream (no CLI/JSON round trip).
fn fragmented_signal_indices(
    tracks: &BTreeMap<u32, TrackSummary>,
    spec: &VectorSpec,
    keyed_texts: &[String],
) -> Vec<usize> {
    spec.signals
        .iter()
        .enumerate()
        .filter(|(_, s)| s.snr_2500_db >= 6.0)
        .filter_map(|(i, s)| {
            let expected = spec.center_freq_hz + s.offset_hz;
            let decoded_len = nearest_track(tracks, expected)
                .and_then(|t| t.text.as_deref())
                .map(|t| t.trim().chars().count())
                .unwrap_or(0);
            let expected_len = keyed_texts[i].chars().count();
            let len_ratio = decoded_len as f64 / expected_len as f64;
            (len_ratio < FRAGMENTED_LEN_RATIO).then_some(i)
        })
        .collect()
}

/// `(first_ts, last_ts)` for every track in `cluster` that actually has a
/// timestamped event (`CharDecoded`/`WordBoundary`). CR-2 (round-2 review):
/// `TrackMeta` carries no `sample_ts`, so a TrackMeta-only track -- exactly
/// what a short fading fragment produces -- has `first_ts`/`last_ts` both
/// `None`. Defaulting those to `(0, 0)` would make every such track
/// trivially overlap every other one at the origin, flipping the F1/F2
/// verdict to "F2 concurrent" regardless of the tracks' real relationship.
/// Excluding them (rather than defaulting) means the span set only ever
/// contains tracks whose overlap is actually measured.
fn timestamped_spans(cluster: &[&TrackSummary]) -> Vec<(u64, u64)> {
    cluster
        .iter()
        .filter_map(|t| match (t.first_ts, t.last_ts) {
            (Some(first), Some(last)) => Some((first, last)),
            _ => None,
        })
        .collect()
}

/// Count of pairs in `spans` whose `[first_ts, last_ts]` intervals
/// overlap -- evidence for F2 (concurrent spectral spread) over F1
/// (sequential drop/reacquire, which predicts disjoint spans).
fn count_overlapping_pairs(spans: &[(u64, u64)]) -> usize {
    let mut overlaps = 0;
    for i in 0..spans.len() {
        for j in (i + 1)..spans.len() {
            let (a0, a1) = spans[i];
            let (b0, b1) = spans[j];
            if a0 <= b1 && b0 <= a1 {
                overlaps += 1;
            }
        }
    }
    overlaps
}

#[test]
#[ignore]
fn v8w_fragmentation_is_sequential_or_concurrent() {
    let spec = manta_testkit::vectors::v8w();
    let rendered = manta_testkit::vectors::render(&spec).unwrap();
    let report = manta_engine::decode_samples(
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
        &PipelineConfig::default(),
    )
    .unwrap();
    let tracks = per_track(&report.events);

    let fragmented = fragmented_signal_indices(&tracks, &spec, &rendered.keyed_texts);
    for &idx in &fragmented {
        let expected = expected_freq(&spec, idx);
        let cluster = tracks_near(&tracks, expected, NEAR_HZ);
        let spans = timestamped_spans(&cluster);
        let overlaps = count_overlapping_pairs(&spans);
        let centers: Vec<Option<f64>> = cluster.iter().map(|t| t.freq_hz).collect();
        println!(
            "idx {idx}: {} tracks within {NEAR_HZ} Hz, {overlaps} overlapping pairs, \
             freqs {centers:?}, spans {spans:?} (verdict: {})",
            cluster.len(),
            if overlaps == 0 {
                "F1 sequential"
            } else {
                "F2 concurrent"
            }
        );
    }
    // Reports only -- the verdict per signal is transcribed into the pin
    // doc and selects Phase 5's branch. This assertion only guards against
    // the scene or decoder changing out from under this diagnostic.
    assert!(
        !fragmented.is_empty(),
        "expected the known fragmented signals (idx 25/41/44 per the golden \
         test's ignore-comment); scene or decoder behavior changed"
    );
}
