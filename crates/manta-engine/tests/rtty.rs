//! MAN-314: RTTY (two-tone FSK) is recognised after its dwell and starts no
//! CW track on its tones or keying sidebands (SPEC §2.6), while CW next to
//! it still decodes. Scenes come from `manta_testkit::fsk` at 96 kHz.

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_testkit::fsk::{add_fsk, FskSpec};
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;

const FS: f64 = 96_000.0;
const CENTER: f64 = 14_070_000.0;
const CW_TEXT: &str = "CQ CQ DE K5ARH K5ARH K";

fn cw(offset_hz: f64, snr: f32) -> SignalSpec {
    SignalSpec {
        text: CW_TEXT.into(),
        loop_text: true,
        wpm: 25.0,
        offset_hz,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    }
}

fn rtty(mark_offset_hz: f64, shift_hz: f64, snr: f32) -> FskSpec {
    FskSpec {
        mark_offset_hz,
        shift_hz,
        baud: if shift_hz < 300.0 { 45.45 } else { 50.0 },
        snr_2500_db: snr,
        text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
        start_s: 0.0,
        watterson: None,
        seed: mark_offset_hz as u64,
    }
}

fn scene(cws: &[SignalSpec], rttys: &[FskSpec], duration_s: f64) -> Vec<Complex32> {
    let (mut iq, _) = render_scene(cws, FS, duration_s, Some(11)).unwrap();
    for r in rttys {
        add_fsk(&mut iq, FS, r);
    }
    iq
}

fn decode(iq: &[Complex32]) -> DecodeReport {
    decode_samples(iq, FS, CENTER, &PipelineConfig::default()).unwrap()
}

fn promotions(report: &DecodeReport) -> Vec<(u32, u64, f64)> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackPromoted {
                track_id,
                sample_ts,
                freq_hz,
            } => Some((*track_id, *sample_ts, *freq_hz)),
            _ => None,
        })
        .collect()
}

/// `TrackPromoted` after `after_ts` with a frequency inside `[lo, hi]`.
fn promotions_after(
    report: &DecodeReport,
    after_ts: u64,
    lo: f64,
    hi: f64,
) -> Vec<(u32, u64, f64)> {
    promotions(report)
        .into_iter()
        .filter(|&(_, ts, f)| ts > after_ts && f >= lo && f <= hi)
        .collect()
}

/// Characters decoded after `after_ts` by tracks promoted within `tol_hz`
/// of `freq_hz`.
fn chars_after_near(report: &DecodeReport, after_ts: u64, freq_hz: f64, tol_hz: f64) -> usize {
    let ids: Vec<u32> = promotions(report)
        .into_iter()
        .filter(|&(_, _, f)| (f - freq_hz).abs() <= tol_hz)
        .map(|(id, _, _)| id)
        .collect();
    report
        .events
        .iter()
        .filter(|e| match e {
            DecoderEvent::CharDecoded {
                track_id,
                sample_ts,
                ..
            } => ids.contains(track_id) && *sample_ts > after_ts,
            _ => false,
        })
        .count()
}

fn assert_scenario1(snr: f32) {
    let mark = CENTER + 15_000.0;
    let iq = scene(&[], &[rtty(15_000.0, 170.0, snr)], 40.0);
    let report = decode(&iq);
    assert_eq!(report.rtty.len(), 1, "snr {snr}: {:?}", report.rtty);
    let r = &report.rtty[0];
    assert_eq!(r.shift_hz, 170.0);
    assert!((r.mark_hz - mark).abs() < 25.0, "mark {}", r.mark_hz);
    let t = r.marked_sample_ts as f64 / FS;
    // A hop's `sample_ts` is its first sample, so the hop that completes
    // 20 s of signal (7500 hops, MAN-115's pre-roll skip leaving none of
    // them to the zero padding) is stamped one hop before 20 s.
    let hop = (FS / manta_decode::FO_HZ) as u64;
    assert!(
        r.marked_sample_ts + hop >= (20.0 * FS) as u64 && t <= 22.0,
        "marked at {t} s"
    );
    let late = promotions_after(
        &report,
        r.marked_sample_ts,
        r.space_hz - 1_000.0,
        r.mark_hz + 1_000.0,
    );
    assert!(
        late.is_empty(),
        "snr {snr}: promotions after the mark {late:?}"
    );
    assert!(report.spots.is_empty(), "{:?}", report.spots);
}

#[test]
fn scenario1_170hz_rtty_is_marked_and_starts_no_track() {
    assert_scenario1(10.0);
    assert_scenario1(30.0);
}

#[test]
fn scenario2_each_standard_shift_is_marked() {
    let shifts = [170.0, 200.0, 425.0, 450.0, 850.0];
    let rttys: Vec<FskSpec> = shifts
        .iter()
        .enumerate()
        .map(|(i, &s)| rtty(3_000.0 * (i as f64 + 1.0), s, 10.0))
        .collect();
    let report = decode(&scene(&[], &rttys, 25.0));
    assert_eq!(report.rtty.len(), 5, "{:?}", report.rtty);
    let mut got: Vec<f64> = report.rtty.iter().map(|r| r.shift_hz).collect();
    got.sort_by(f64::total_cmp);
    assert_eq!(got, shifts.to_vec());
    for r in &report.rtty {
        assert!(r.marked_sample_ts as f64 / FS <= 22.0, "{r:?}");
    }
}

#[test]
fn scenario3_cw_between_two_rtty_signals_is_spotted() {
    let cw_hz = CENTER + 13_000.0;
    let iq = scene(
        &[cw(13_000.0, 10.0)],
        &[rtty(10_000.0, 170.0, 30.0), rtty(16_000.0, 170.0, 30.0)],
        60.0,
    );
    let report = decode(&iq);
    assert_eq!(report.rtty.len(), 2, "{:?}", report.rtty);
    let spot = report
        .spots
        .iter()
        .find(|s| s.callsign == "K5ARH")
        .unwrap_or_else(|| panic!("no K5ARH spot: {:?}", report.spots));
    assert!(
        (spot.freq_hz - cw_hz).abs() <= 50.0,
        "spot at {}",
        spot.freq_hz
    );
    let cw_tracks: Vec<u32> = promotions(&report)
        .into_iter()
        .filter(|&(_, _, f)| (f - cw_hz).abs() <= 60.0)
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(cw_tracks.len(), 1, "CW tracks {cw_tracks:?}");
    let marked = report
        .rtty
        .iter()
        .map(|r| r.marked_sample_ts)
        .max()
        .unwrap();
    assert!(chars_after_near(&report, marked, cw_hz, 60.0) > 0);
}

#[test]
fn cw_starting_after_mark_near_rtty_still_decodes() {
    // RTTY at +12000 (30 dB); CW keyed from t = 30 s, 300 Hz above the mark.
    let duration_s = 60.0;
    let mut iq = scene(&[], &[rtty(12_000.0, 170.0, 30.0)], duration_s);
    let (cw_iq, _) = render_scene(&[cw(12_300.0, 10.0)], FS, duration_s, None).unwrap();
    let start = (30.0 * FS) as usize;
    for (out, s) in iq[start..].iter_mut().zip(cw_iq.iter()) {
        *out += *s;
    }
    let report = decode(&iq);
    assert_eq!(report.rtty.len(), 1, "{:?}", report.rtty);
    assert!(
        report.spots.iter().any(|s| s.callsign == "K5ARH"),
        "no K5ARH spot: {:?}",
        report.spots
    );
}

#[test]
fn strong_rtty_does_not_kill_nearby_cw() {
    let cw_hz = CENTER + 12_600.0;
    let iq = scene(&[cw(12_600.0, 20.0)], &[rtty(12_000.0, 170.0, 45.0)], 60.0);
    let report = decode(&iq);
    assert_eq!(report.rtty.len(), 1, "{:?}", report.rtty);
    let marked = report.rtty[0].marked_sample_ts;
    let n = chars_after_near(&report, marked, cw_hz, 60.0);
    assert!(n >= 40, "only {n} CW characters after the mark");
}

#[test]
fn high_snr_rtty_starts_no_track_after_mark() {
    let iq = scene(
        &[],
        &[rtty(-15_000.0, 170.0, 50.0), rtty(15_000.0, 850.0, 50.0)],
        40.0,
    );
    let report = decode(&iq);
    assert_eq!(report.rtty.len(), 2, "{:?}", report.rtty);
    for r in &report.rtty {
        let late = promotions_after(
            &report,
            r.marked_sample_ts,
            r.space_hz - 1_000.0,
            r.mark_hz + 1_000.0,
        );
        assert!(late.is_empty(), "{r:?}: promotions after the mark {late:?}");
    }
}

#[test]
fn fading_rtty_is_marked() {
    let mut spec = rtty(15_000.0, 170.0, 25.0);
    spec.watterson = Some((coppa_channel::watterson::WattersonPreset::Moderate, 5));
    let report = decode(&scene(&[], &[spec], 45.0));
    assert_eq!(report.rtty.len(), 1, "{:?}", report.rtty);
    assert!(
        report.rtty[0].marked_sample_ts as f64 / FS <= 35.0,
        "{:?}",
        report.rtty
    );
}

fn off() -> PipelineConfig {
    let mut cfg = PipelineConfig::default();
    cfg.detector.rtty_filter = false;
    cfg
}

fn assert_on_off_equal(iq: &[Complex32], fs: f64, center: f64) {
    let on = decode_samples(iq, fs, center, &PipelineConfig::default()).unwrap();
    let off = decode_samples(iq, fs, center, &off()).unwrap();
    assert!(on.rtty.is_empty(), "{:?}", on.rtty);
    assert!(off.rtty.is_empty());
    assert_eq!(on.events, off.events);
    assert_eq!(on.spots, off.spots);
    assert_eq!(on.text, off.text);
}

#[test]
fn rtty_filter_off_is_todays_behaviour() {
    let v1 = manta_testkit::vectorspec_short();
    let rendered = manta_testkit::vectors::render(&v1).unwrap();
    assert_on_off_equal(&rendered.samples, v1.fs, v1.center_freq_hz);

    let iq = scene(&[], &[rtty(15_000.0, 170.0, 30.0)], 40.0);
    let report = decode_samples(&iq, FS, CENTER, &off()).unwrap();
    assert!(report.rtty.is_empty(), "{:?}", report.rtty);
}

#[test]
fn golden_vectors_unchanged_with_filter_on() {
    // First 60 s of each, to keep CI time bounded.
    for base in [manta_testkit::vectors::v8w(), manta_testkit::vectors::vr3()] {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 60.0,
            ..base
        };
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        assert_on_off_equal(&rendered.samples, spec.fs, spec.center_freq_hz);
    }
}

#[test]
fn rtty_closes_are_bookkeeping() {
    let iq = scene(&[], &[rtty(15_000.0, 170.0, 30.0)], 40.0);
    let report = decode(&iq);
    let r = &report.rtty[0];
    let on_span: Vec<u32> = promotions(&report)
        .into_iter()
        .filter(|&(_, _, f)| f >= r.space_hz - 100.0 && f <= r.mark_hz + 100.0)
        .map(|(id, _, _)| id)
        .collect();
    for e in &report.events {
        if let DecoderEvent::TrackClosed { track_id, closure } = e {
            if on_span.contains(track_id) {
                assert!(
                    matches!(
                        closure,
                        ClosureKind::Bookkeeping {
                            survivor_track_id: None
                        } | ClosureKind::SignalEnded
                    ),
                    "{track_id}: {closure:?}"
                );
            }
        }
    }
}

#[test]
fn decode_report_carries_calibrated_rtty_entries() {
    let iq = scene(&[], &[rtty(15_000.0, 170.0, 30.0)], 25.0);
    let plain = decode(&iq);
    let cfg = PipelineConfig {
        freq_correction_ppm: 10.0,
        ..PipelineConfig::default()
    };
    let cal = decode_samples(&iq, FS, CENTER, &cfg).unwrap();
    let factor = manta_spot::calibration_factor_from_ppm(10.0).unwrap();
    assert_eq!(plain.rtty.len(), 1);
    assert_eq!(cal.rtty.len(), 1);
    assert!((cal.rtty[0].mark_hz - plain.rtty[0].mark_hz * factor).abs() < 1e-6);
    assert!((cal.rtty[0].space_hz - plain.rtty[0].space_hz * factor).abs() < 1e-6);
}

#[test]
fn decode_report_rtty_is_empty_without_rtty() {
    let v1 = manta_testkit::vectorspec_short();
    let rendered = manta_testkit::vectors::render(&v1).unwrap();
    let report = decode_samples(
        &rendered.samples,
        v1.fs,
        v1.center_freq_hz,
        &PipelineConfig::default(),
    )
    .unwrap();
    assert!(report.rtty.is_empty());
}

/// A marked RTTY signal is reported even when no CW decoder ever ran: its
/// tone runs never outlast a confirmation window no CW track can reach.
#[test]
fn decode_report_keeps_rtty_without_cw_output() {
    let iq = scene(&[], &[rtty(15_000.0, 170.0, 30.0)], 4.0);
    let mut cfg = PipelineConfig::default();
    cfg.detector.rtty_dwell_hops = manta_dsp::fsk::BLOCK_HOPS as u64;
    cfg.detector.confirm_hops = 1_000_000;
    let report = decode_samples(&iq, FS, CENTER, &cfg).unwrap();
    assert_eq!(report.rtty.len(), 1, "{:?}", report.rtty);
    assert_eq!(report.rtty[0].shift_hz, 170.0);
    assert!(report.text.is_empty(), "{:?}", report.text);
    assert!(report.spots.is_empty());
    assert_eq!(report.wpm, None);
}
