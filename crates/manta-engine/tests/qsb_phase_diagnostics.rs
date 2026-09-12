//! MAN-8 diagnostic (not a gate): decode V6 end-to-end and report where its
//! character errors and speed-tracking behavior live relative to QSB phase.
//! `cargo test -p manta-engine --test qsb_phase_diagnostics -- --ignored --nocapture`
//!
//! This is an instrument, not an acceptance test -- its only hard assertion
//! is a sanity floor (enough data was produced to say anything at all). Its
//! purpose is the printed per-octant tables, which drove the MAN-8 rung
//! selection recorded in docs/DECISIONS/2026-09-04-man8-v6-qsb-decode-fix.md.

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::{align, EditOp};

/// QSB phase octant (0..8) a given sample timestamp falls into, at V6's
/// rate_hz = 0.2 (5 s period).
fn octant(sample_ts: u64, fs: f64) -> usize {
    let phase = (0.2 * (sample_ts as f64 / fs)).fract();
    ((phase * 8.0) as usize).min(7)
}

/// Every `CharDecoded` event's (sample_ts, glyph char), in emission order.
fn char_decoded_events(events: &[DecoderEvent]) -> Vec<(u64, Option<char>)> {
    events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::CharDecoded {
                sample_ts, glyph, ..
            } => Some((*sample_ts, glyph.text_char())),
            _ => None,
        })
        .collect()
}

/// Every `SpeedUpdate` event's (approximate sample_ts via nearest preceding
/// CharDecoded, wpm) -- `SpeedUpdate` itself carries no timestamp (SPEC §5),
/// so this pairs each with the last known position in the stream.
fn wpm_series(events: &[DecoderEvent]) -> Vec<(u64, f32)> {
    let mut out = Vec::new();
    let mut last_ts = 0u64;
    for e in events {
        match e {
            DecoderEvent::CharDecoded { sample_ts, .. } => last_ts = *sample_ts,
            DecoderEvent::SpeedUpdate { wpm, .. } => out.push((last_ts, *wpm)),
            _ => {}
        }
    }
    out
}

#[test]
#[ignore]
fn v6_error_distribution_by_qsb_phase() {
    let spec = manta_testkit::vectors::v6();
    let (iq, texts) = manta_testkit::scene::render_scene(
        &spec.signals,
        spec.fs,
        spec.duration_s,
        Some(spec.noise_seed),
    )
    .unwrap();
    let report =
        decode_samples(&iq, spec.fs, spec.center_freq_hz, &PipelineConfig::default()).unwrap();

    // 1. Errors per QSB phase octant. Align the decoded text against the
    // expected text, then attribute each aligned position to the octant of
    // the nearest CharDecoded event covering that position in the decode.
    let chars = char_decoded_events(&report.events);
    let ops = align(&texts[0], &report.text);
    let mut err = [0u32; 8];
    let mut tot = [0u32; 8];
    // decoded_idx -> sample_ts via the nth CharDecoded event with a glyph
    // char (matches events_to_text's construction, which only pushes
    // decoded_idx-bearing chars).
    let decoded_ts: Vec<u64> = chars
        .iter()
        .filter_map(|&(ts, c)| c.map(|_| ts))
        .collect();
    for op in &ops {
        let ts = match op {
            EditOp::Match { decoded_idx, .. } | EditOp::Substitute { decoded_idx, .. } => {
                decoded_ts.get(*decoded_idx).copied()
            }
            EditOp::Insert { decoded_idx } => decoded_ts.get(*decoded_idx).copied(),
            EditOp::Delete { .. } => None, // no decoded position to anchor to
        };
        let Some(ts) = ts else { continue };
        let oct = octant(ts, spec.fs);
        tot[oct] += 1;
        if !matches!(op, EditOp::Match { .. }) {
            err[oct] += 1;
        }
    }
    let rates: Vec<String> = (0..8)
        .map(|i| {
            if tot[i] == 0 {
                format!("oct{i}: n/a")
            } else {
                format!("oct{i}: {:.3} ({}/{})", err[i] as f64 / tot[i] as f64, err[i], tot[i])
            }
        })
        .collect();
    println!("QSB octant error rates: {}", rates.join(", "));

    // 2. Measured WPM vs QSB phase, from SpeedUpdate events.
    let wpm_table: Vec<String> = wpm_series(&report.events)
        .iter()
        .map(|&(ts, wpm)| format!("(oct{}, {wpm:.1})", octant(ts, spec.fs)))
        .collect();
    println!("wpm-vs-phase samples: {}", wpm_table.len());
    if let Some(sample) = wpm_table.chunks(20).next() {
        println!("wpm-vs-phase (first 20): {}", sample.join(", "));
    }

    // Deliberately weak: this test exists to print, and to fail loudly if
    // the decode collapses entirely (e.g. a regression upstream of MAN-8).
    assert!(
        tot.iter().sum::<u32>() > 100,
        "diagnostic produced too little data ({} aligned positions with a decoded_ts)",
        tot.iter().sum::<u32>()
    );
}
