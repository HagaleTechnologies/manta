//! Proves the channelizer + placeholder-detector pairing (Tasks 1-6) is
//! chunk-size-invariant, the same property M1's chunking_determinism.rs
//! proved for the deprecated SingleChannelExtractor path.

use skimmer_decode::decoder::{DecodeConfig, TrackDecoder};
use skimmer_decode::events::DecoderEvent;
use skimmer_dsp::channelizer::Channelizer;
use skimmer_testkit::vectors::{render, v1};

fn decode_all_at_once(iq: &[num_complex::Complex32], fs: f64, k0: usize) -> Vec<DecoderEvent> {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for hop_out in ch.process(iq) {
        events.extend(decoder.push_envelope(hop_out.power[k0].sqrt(), hop_out.m * ch.hop() as u64));
    }
    events.extend(decoder.finish());
    events
}

fn decode_in_chunks(
    iq: &[num_complex::Complex32],
    fs: f64,
    k0: usize,
    chunk_size: usize,
) -> Vec<DecoderEvent> {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let hop = ch.hop() as u64;
    let mut decoder = TrackDecoder::new(1, DecodeConfig::default());
    let mut events = Vec::new();
    for chunk in iq.chunks(chunk_size) {
        for hop_out in ch.process(chunk) {
            events.extend(decoder.push_envelope(hop_out.power[k0].sqrt(), hop_out.m * hop));
        }
    }
    events.extend(decoder.finish());
    events
}

#[test]
fn chunked_channelizer_feeding_matches_whole_buffer_feeding() {
    let spec = v1();
    let rendered = render(&spec).unwrap();

    let mut calib_ch = Channelizer::new(spec.fs, 0.0).unwrap();
    let calib_hops = calib_ch.process(&rendered.samples);
    let n = calib_ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in &calib_hops {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    let mut k0 = 0;
    for (k, &p) in avg_power.iter().enumerate() {
        if p > avg_power[k0] {
            k0 = k;
        }
    }

    let whole = decode_all_at_once(&rendered.samples, spec.fs, k0);
    for &chunk_size in &[97usize, 1_024, 8_192, 100_000] {
        let chunked = decode_in_chunks(&rendered.samples, spec.fs, k0, chunk_size);
        assert_eq!(
            whole, chunked,
            "chunk_size={chunk_size} produced different events than whole-buffer decode"
        );
    }
}
