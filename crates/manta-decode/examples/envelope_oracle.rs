//! Throwaway diagnostic (MAN-166, design doc §1.2): feed a raw per-hop
//! channel *power* stream (little-endian f32, 375 Hz, one file per signal,
//! as written by `scripts/diagnostics/man166/probe2.py`) straight into
//! `TrackDecoder`, bypassing the detector/track manager, and print what the
//! decode core alone makes of it. Superseded by the real oracle harness in
//! `docs/superpowers/plans/2026-09-09-decode-core-v2.md` Task 6.
//!
//! Usage: cargo run -p manta-decode --example envelope_oracle -- <file.f32>...
use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use std::fs;

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = fs::read(&path).expect("read");
        let power: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let mut dec = TrackDecoder::new(1, DecodeConfig::default());
        let mut events = Vec::new();
        for (i, p) in power.iter().enumerate() {
            events.extend(dec.push_envelope(p.max(0.0).sqrt(), i as u64 * 512));
        }
        events.extend(dec.finish());
        let text = events_to_text(&events);
        let nchar = events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::CharDecoded { .. }))
            .count();
        let wpm = events.iter().rev().find_map(|e| match e {
            DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
            _ => None,
        });
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        println!(
            "{name}\tchars={nchar}\tgarble={}\twpm={wpm:?}\t{text}",
            dec.garble_count
        );
    }
}
