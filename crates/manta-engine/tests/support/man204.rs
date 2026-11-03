//! MAN-204 test support: the research's reference geometry (a +30 dB
//! station at the centre of channel 107 and a +10 dB station 40 Hz away,
//! 96 kS/s, 60 s), rendered one station at a time and summed in a fixed
//! order, plus a per-track call counter. Shared by `man204_cancellation.rs`
//! and `man204_cancellation_sweep.rs`.
#![allow(dead_code)]

use manta_decode::decoder::events_to_text;
use manta_decode::events::DecoderEvent;
use manta_testkit::scene::{render_scene, SignalSpec, MASTER_SCALE};
use num_complex::Complex32;
use std::collections::BTreeMap;
use std::f64::consts::TAU;

pub const FS: f64 = 96_000.0;
pub const CENTER: f64 = 14_030_000.0;
pub const DUR: f64 = 60.0;
/// Station A's offset: the centre of channel 107.
pub const A_OFF: f64 = 10_031.25;
/// Half a 93.75 Hz channel: A at `A_OFF + MID` sits between two channels.
pub const MID: f64 = 46.875;
pub const A_CALL: &str = "W1AW";
pub const B_CALL: &str = "K5ARH";
pub const A_TEXT: &str = "CQ TEST DE W1AW W1AW K";
pub const B_TEXT: &str = "CQ TEST DE K5ARH K5ARH K";

pub fn station(text: &str, wpm: f32, offset_hz: f64, snr_2500_db: f32) -> SignalSpec {
    SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: text.into(),
        loop_text: true,
        wpm,
        offset_hz,
        snr_2500_db,
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

/// Station A: 25 WPM at `offset_hz`, `snr` dB.
pub fn a(offset_hz: f64, snr: f32) -> SignalSpec {
    station(A_TEXT, 25.0, offset_hz, snr)
}

/// Station B: 20 WPM at `offset_hz`, `snr` dB.
pub fn b(offset_hz: f64, snr: f32) -> SignalSpec {
    station(B_TEXT, 20.0, offset_hz, snr)
}

/// One station, noiseless.
pub fn render_station(spec: &SignalSpec, dur: f64) -> Vec<Complex32> {
    render_scene(std::slice::from_ref(spec), FS, dur, None)
        .unwrap()
        .0
}

/// The testkit's AWGN alone.
pub fn noise(dur: f64, seed: u64) -> Vec<Complex32> {
    render_scene(&[], FS, dur, Some(seed)).unwrap().0
}

/// An unkeyed carrier at `offset_hz`, `snr` dB (2500 Hz), f64 phase.
pub fn steady(offset_hz: f64, snr: f32, dur: f64) -> Vec<Complex32> {
    let amp = (manta_testkit::noise::amplitude_for_snr_2500(snr, FS) * MASTER_SCALE) as f64;
    let n = (dur * FS).round() as usize;
    (0..n)
        .map(|k| {
            let ph = (offset_hz / FS * k as f64).rem_euclid(1.0) * TAU;
            Complex32::new((amp * ph.cos()) as f32, (amp * ph.sin()) as f32)
        })
        .collect()
}

/// Elementwise sum, in argument order.
pub fn sum(parts: &[Vec<Complex32>]) -> Vec<Complex32> {
    let mut out = parts[0].clone();
    for p in &parts[1..] {
        for (x, y) in out.iter_mut().zip(p) {
            *x += y;
        }
    }
    out
}

/// A plus B plus noise, 60 s.
pub fn pair(a_off: f64, a_snr: f32, b_off: f64, b_snr: f32, seed: u64) -> Vec<Complex32> {
    sum(&[
        render_station(&a(a_off, a_snr), DUR),
        render_station(&b(b_off, b_snr), DUR),
        noise(DUR, seed),
    ])
}

/// A alone plus noise, 60 s.
pub fn alone(a_off: f64, a_snr: f32, seed: u64) -> Vec<Complex32> {
    sum(&[render_station(&a(a_off, a_snr), DUR), noise(DUR, seed)])
}

fn track_of(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::ReadingsHeld { track_id, .. }
        | DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::TimingUpdate { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

/// Decoded text per track id.
pub fn texts(events: &[DecoderEvent]) -> BTreeMap<u32, String> {
    let mut by: BTreeMap<u32, Vec<DecoderEvent>> = BTreeMap::new();
    for e in events {
        by.entry(track_of(e)).or_default().push(e.clone());
    }
    by.into_iter()
        .map(|(id, ev)| (id, events_to_text(&ev)))
        .collect()
}

/// Occurrences of `call` in every track's decoded text.
pub fn calls(events: &[DecoderEvent], call: &str) -> usize {
    texts(events)
        .values()
        .map(|t| t.matches(call).count())
        .sum()
}
