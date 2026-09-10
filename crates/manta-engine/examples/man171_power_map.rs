//! MAN-171 scratch diagnostic, kept as a reusable live-hardware tool: run
//! the real channelizer over a captured WAV and print per-channel power
//! statistics (mapped to absolute Hz), independent of the floor/gate/
//! track-manager -- separates "the channelizer isn't seeing the signal"
//! (an RF/antenna question) from "the channelizer sees it but the
//! detector doesn't act on it" (a manta code question).
//!
//! Reports mean of LINEAR power (not a dB average, which computes a
//! geometric mean and can hide a signal present only part of the time --
//! e.g. keyed CW at ~50% duty cycle) alongside the max and 99th-percentile
//! per-hop power, so an intermittent signal shows up even when its
//! time-averaged level doesn't.
//!
//! Usage: `cargo run -p manta-engine --release --example man171_power_map
//! -- <wav_path> [target_hz ...] [profile <channel_k>]`

use manta_dsp::channelizer::{power_db, Channelizer};
use manta_input::{read_all, IqSource, WavIqSource};

fn percentile(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p * (sorted.len() - 1) as f64).round() as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = std::path::Path::new(&args[0]);
    let mut src = WavIqSource::open(path)?;
    let fs = src.sample_rate();
    let center = src.center_freq_hz();
    let iq = read_all(&mut src)?;
    eprintln!("loaded {} samples, fs={fs}, center={center}", iq.len());

    let mut ch = Channelizer::new(fs, center).map_err(anyhow::Error::msg)?;
    let n = ch.n_channels();
    // Per-channel history of every hop's linear power, so mean/max/
    // percentile are all computed correctly (a dB average of per-hop dB
    // values is a geometric mean of linear power, not an arithmetic one --
    // it systematically understates a signal present less than 100% of
    // the time).
    let mut history: Vec<Vec<f32>> = vec![Vec::new(); n];
    for chunk in iq.chunks(65536) {
        let hops = ch.process(chunk);
        for hop in &hops {
            for (hist, &p) in history.iter_mut().zip(&hop.power) {
                hist.push(p);
            }
        }
    }
    let count = history.first().map(|h| h.len()).unwrap_or(0);

    let mut mean_power_db = vec![0.0f64; n];
    let mut max_power_db = vec![0.0f64; n];
    let mut p99_power_db = vec![0.0f64; n];
    for k in 0..n {
        let mean_linear: f64 = history[k].iter().map(|&p| p as f64).sum::<f64>() / count as f64;
        mean_power_db[k] = power_db(mean_linear as f32);
        let mut sorted = history[k].clone();
        sorted.sort_by(f32::total_cmp);
        max_power_db[k] = power_db(*sorted.last().unwrap_or(&0.0));
        p99_power_db[k] = power_db(percentile(&sorted, 0.99));
    }

    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| mean_power_db[b].partial_cmp(&mean_power_db[a]).unwrap());

    println!("top 30 channels by mean linear power:");
    for &k in idx.iter().take(30) {
        println!(
            "  k={k} freq={:.1} mean_db={:.2} p99_db={:.2} max_db={:.2}",
            ch.channel_freq_hz(k),
            mean_power_db[k],
            p99_power_db[k],
            max_power_db[k]
        );
    }

    let channel_for_freq = |target: f64| -> usize {
        let delta = fs / n as f64;
        let signed = ((target - center) / delta).round() as i64;
        signed.rem_euclid(n as i64) as usize
    };

    let mut rest = args[1..].iter();
    for arg in rest.by_ref() {
        if arg == "profile" {
            break;
        }
        let target: f64 = arg.parse()?;
        let k = channel_for_freq(target);
        println!(
            "target {target}: nearest channel k={k} freq={:.1} mean_db={:.2} p99_db={:.2} max_db={:.2}",
            ch.channel_freq_hz(k),
            mean_power_db[k],
            p99_power_db[k],
            max_power_db[k]
        );
    }

    if let Some(center_k_str) = rest.next() {
        let center_k: i64 = center_k_str.parse()?;
        let half: i64 = 40;
        println!("channel profile around k={center_k}:");
        for dk in -half..=half {
            let k = ((center_k + dk).rem_euclid(n as i64)) as usize;
            println!(
                "  k={k} freq={:.1} mean_db={:.2} p99_db={:.2} max_db={:.2}",
                ch.channel_freq_hz(k),
                mean_power_db[k],
                p99_power_db[k],
                max_power_db[k]
            );
        }
    }

    Ok(())
}
