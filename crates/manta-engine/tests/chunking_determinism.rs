//! Proves the M1 streaming design's core claim (design doc §4): feeding
//! SingleChannelExtractor/TrackDecoder in small chunks produces
//! byte-identical output to M0's single whole-buffer call. This is what
//! lets `listen`'s per-chunk loop reuse the M0 decode chain unchanged.

use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::freqest::estimate_peak_hz;
use manta_dsp::single::SingleChannelExtractor;
use manta_testkit::vectors::{render, v1};
use num_complex::Complex32;

fn decode_all_at_once(iq: &[Complex32], fs: f64, offset_hz: f64) -> Vec<DecoderEvent> {
    let mut extractor = SingleChannelExtractor::new(fs, offset_hz).unwrap();
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for (m, y) in extractor.process(iq).into_iter().enumerate() {
        events.extend(decoder.push_envelope(y.norm(), m as u64 * extractor.hop() as u64));
    }
    events.extend(decoder.finish());
    events
}

fn decode_in_chunks(
    iq: &[Complex32],
    fs: f64,
    offset_hz: f64,
    chunk_size: usize,
) -> Vec<DecoderEvent> {
    let mut extractor = SingleChannelExtractor::new(fs, offset_hz).unwrap();
    let hop = extractor.hop() as u64;
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    let mut m: u64 = 0;
    for chunk in iq.chunks(chunk_size) {
        for y in extractor.process(chunk) {
            events.extend(decoder.push_envelope(y.norm(), m * hop));
            m += 1;
        }
    }
    events.extend(decoder.finish());
    events
}

#[test]
fn chunked_feeding_matches_whole_buffer_feeding() {
    let spec = v1();
    let rendered = render(&spec).unwrap();
    let offset_hz = estimate_peak_hz(&rendered.samples, spec.fs).unwrap();

    let whole = decode_all_at_once(&rendered.samples, spec.fs, offset_hz);
    for &chunk_size in &[97usize, 1_024, 8_192, 100_000] {
        let chunked = decode_in_chunks(&rendered.samples, spec.fs, offset_hz, chunk_size);
        assert_eq!(
            whole, chunked,
            "chunk_size={chunk_size} produced different events than whole-buffer decode"
        );
    }
}
