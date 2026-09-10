//! MAN-171 scratch diagnostic, kept as a reusable live-hardware tool: run
//! the real channelizer over a captured WAV and print average per-channel
//! power (mapped to absolute Hz), independent of the floor/gate/
//! track-manager -- separates "the channelizer isn't seeing the signal"
//! (an RF/antenna question) from "the channelizer sees it but the
//! detector doesn't act on it" (a manta code question).
//!
//! Usage: `cargo run -p manta-engine --release --example man171_power_map
//! -- <wav_path> [target_hz ...] [-- profile <channel_k>]`

use manta_dsp::channelizer::{power_db, Channelizer};
use manta_input::{read_all, IqSource, WavIqSource};

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
    let mut sum_power_db = vec![0.0f64; n];
    let mut count = 0u64;
    for chunk in iq.chunks(65536) {
        let hops = ch.process(chunk);
        for hop in &hops {
            for (sum, &p) in sum_power_db.iter_mut().zip(&hop.power) {
                *sum += power_db(p);
            }
            count += 1;
        }
    }
    let avg_power_db: Vec<f64> = sum_power_db.iter().map(|&s| s / count as f64).collect();

    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| avg_power_db[b].partial_cmp(&avg_power_db[a]).unwrap());

    println!("top 30 channels by average power:");
    for &k in idx.iter().take(30) {
        println!(
            "  k={k} freq={:.1} avg_power_db={:.2}",
            ch.channel_freq_hz(k),
            avg_power_db[k]
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
            "target {target}: nearest channel k={k} freq={:.1} avg_power_db={:.2}",
            ch.channel_freq_hz(k),
            avg_power_db[k]
        );
    }

    if let Some(center_k_str) = rest.next() {
        let center_k: i64 = center_k_str.parse()?;
        let half: i64 = 40;
        println!("channel profile around k={center_k}:");
        for dk in -half..=half {
            let k = ((center_k + dk).rem_euclid(n as i64)) as usize;
            println!(
                "  k={k} freq={:.1} avg_power_db={:.2}",
                ch.channel_freq_hz(k),
                avg_power_db[k]
            );
        }
    }

    Ok(())
}
