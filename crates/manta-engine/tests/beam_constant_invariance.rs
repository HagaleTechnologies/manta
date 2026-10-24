//! σ / beam-width decode-invariance pins (MAN-120, SPEC §4.3).
//!
//! Why σ cannot change the decode (MAN-9 plan, decision 5): every
//! hypothesis surviving a beam step has consumed the same marks, so σ
//! enters each candidate's score as a common positive factor -- it cannot
//! reorder the beam, change the truncation set, or flip the final argmax.
//!
//! These tests pin that property (and the beam-width floor: width >= 2
//! measured identical to the default 4) on one AWGN and one Watterson-poor
//! scene. They deliberately compare the assembled text and the glyph
//! stream, NOT the full event stream: `CharDecoded::confidence` and the
//! validator confidences derived from it legitimately move with σ
//! (measured: spot confidence 0.55 -> 0.20 as σ goes 0.10 -> 1.50 with
//! byte-identical text). Do not "fix" a failure here by asserting less of
//! the decode -- a text/glyph difference is a real invariance break.
//! MAN-9 measured width 1 changing V5's decode, so width 1 is deliberately
//! not asserted either way (scene-dependent); the floor pinned here is
//! "width 2 behaves like width 4".

use coppa_channel::watterson::WattersonPreset;
use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec, WattersonFade};
use num_complex::Complex32;

/// 60 s of looped "CQ CQ DE W1AW W1AW K", 20 WPM, +20 dB (2500 Hz-ref),
/// offset +12_340 Hz -- V1's scene parameters at half duration.
fn scene(watterson: Option<WattersonFade>) -> Vec<Complex32> {
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    };
    render_scene(&[sig], 96_000.0, 60.0, Some(0x534B_494D_5631))
        .expect("scene renders")
        .0
}

/// Both scene variants, rendered once per test: AWGN, and the same signal
/// under Watterson CCIR-poor fading (coppa pinned commit, fixed seed).
fn scenes() -> Vec<(&'static str, Vec<Complex32>)> {
    vec![
        ("awgn", scene(None)),
        (
            "watterson-poor",
            scene(Some(WattersonFade {
                preset: WattersonPreset::Poor,
                seed: 0x5635,
            })),
        ),
    ]
}

fn decode(iq: &[Complex32], width: usize, sigma: f32) -> DecodeReport {
    let mut cfg = PipelineConfig::default();
    cfg.decode.beam.width = width;
    cfg.decode.beam.sigma = sigma;
    decode_samples(iq, 96_000.0, 14_000_000.0, &cfg).expect("decode succeeds")
}

/// The glyph stream: every decoded glyph and word boundary, with its
/// track, in order -- everything the decode says that σ must not touch.
fn glyph_stream(report: &DecodeReport) -> Vec<String> {
    let mut out = Vec::new();
    for ev in &report.events {
        match ev {
            DecoderEvent::CharDecoded {
                track_id, glyph, ..
            } => out.push(format!("{track_id}:{glyph:?}")),
            DecoderEvent::WordBoundary { track_id, .. } => out.push(format!("{track_id}:WB")),
            _ => {}
        }
    }
    out
}

/// σ is decode-invariant: an 8x-15x bracket around the default 0.25
/// leaves the assembled text and the glyph stream byte-identical on both
/// the clean and the faded scene. (Re-measured at MAN-120 in-process and,
/// via `[decode] timing_sigma` on V1/V5 with SHA-256 text comparison, by
/// the research phase; MAN-9 plan decision 5 proves it from the scoring
/// rule.)
#[test]
fn sigma_does_not_change_text_or_glyph_stream() {
    for (name, iq) in scenes() {
        let baseline = decode(&iq, 4, 0.25);
        assert!(
            !baseline.text.is_empty(),
            "{name}: the scene must decode to something for the pin to mean anything"
        );
        for sigma in [0.10, 1.00] {
            let moved = decode(&iq, 4, sigma);
            assert_eq!(
                moved.text, baseline.text,
                "{name}: sigma {sigma} changed the decode text"
            );
            assert_eq!(
                glyph_stream(&moved),
                glyph_stream(&baseline),
                "{name}: sigma {sigma} changed the glyph stream"
            );
        }
    }
}

/// Beam width is the non-inert sibling (SPEC §4.4): width 1 starves the
/// beam and has changed a fading decode (V5, MAN-9); widths 2-16 measured
/// identical to the default 4. This pins the floor: width 2 and width 8
/// must match width 4's text and glyph stream on both scenes.
#[test]
fn beam_width_two_or_more_matches_default_four() {
    for (name, iq) in scenes() {
        let baseline = decode(&iq, 4, 0.25);
        for width in [2, 8] {
            let moved = decode(&iq, width, 0.25);
            assert_eq!(
                moved.text, baseline.text,
                "{name}: width {width} changed the decode text"
            );
            assert_eq!(
                glyph_stream(&moved),
                glyph_stream(&baseline),
                "{name}: width {width} changed the glyph stream"
            );
        }
    }
}
