//! Real-signal decode oracle: channelize a window around each RBN-spotted
//! station, feed the strongest of the three channels around its kHz
//! straight into `TrackDecoder`, and score callsign recovery. Isolates
//! the decode core from the tracker. SPEC v2 §8.3.

use anyhow::{Context, Result};
use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::channelizer::Channelizer;
use num_complex::Complex32;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleSpot {
    pub call: String,
    pub khz: f64,
    pub t_sec: f64,
    pub snr_db: i32,
    pub wpm: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleResult {
    pub call: String,
    pub khz: f64,
    pub as_word: bool,
    pub framed: bool,
    pub substring: bool,
    pub wpm_ratio: Option<f32>,
    pub text: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OracleSummary {
    pub n: usize,
    pub as_word: usize,
    pub framed: usize,
    pub substring: usize,
    pub wpm_ratio_median: Option<f32>,
    pub by_snr: Vec<(String, usize, usize, usize)>,
}

/// First spot per (call, kHz) from an RBN daily-dump CSV, restricted to `spotter`.
pub fn parse_rbn_spots(csv_path: &Path, spotter: &str) -> Result<Vec<OracleSpot>> {
    let text = std::fs::read_to_string(csv_path)
        .with_context(|| format!("read {}", csv_path.display()))?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().context("empty csv")?.split(',').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .with_context(|| format!("missing column {name}"))
    };
    let (c_sp, c_freq, c_dx, c_db, c_date, c_speed) = (
        col("callsign")?,
        col("freq")?,
        col("dx")?,
        col("db")?,
        col("date")?,
        col("speed")?,
    );
    let mut seen = std::collections::BTreeMap::<(String, i64), OracleSpot>::new();
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() <= c_speed || f[c_sp] != spotter {
            continue;
        }
        let khz: f64 = f[c_freq].parse()?;
        let hms = &f[c_date][11..19];
        let t_sec = hms[3..5].parse::<f64>()? * 60.0 + hms[6..8].parse::<f64>()?;
        let spot = OracleSpot {
            call: f[c_dx].to_uppercase(),
            khz,
            t_sec,
            snr_db: f[c_db].parse().unwrap_or(0),
            wpm: f[c_speed].parse().unwrap_or(0),
        };
        let key = (spot.call.clone(), (khz * 10.0).round() as i64);
        match seen.get(&key) {
            Some(s) if s.t_sec <= t_sec => {}
            _ => {
                seen.insert(key, spot);
            }
        }
    }
    Ok(seen.into_values().collect())
}

pub fn score_text(call: &str, text: &str) -> (bool, bool, bool) {
    let words: Vec<&str> = text.split_whitespace().collect();
    let as_word = words.contains(&call);
    let framed = words
        .windows(2)
        .any(|w| matches!(w[0], "CQ" | "DE" | "TEST") && w[1] == call)
        || words
            .windows(3)
            .any(|w| w[0] == "CQ" && w[1] == "TEST" && w[2] == call);
    let substring = text.replace(' ', "").contains(call);
    (as_word, framed, substring)
}

pub fn run_oracle(
    iq: &[Complex32],
    fs: f64,
    center_hz: f64,
    spots: &[OracleSpot],
    window_s: f64,
    cfg: &DecodeConfig,
) -> Result<(Vec<OracleResult>, OracleSummary)> {
    let mut results = Vec::with_capacity(spots.len());
    for spot in spots {
        let mut ch = Channelizer::new(fs, center_hz).map_err(anyhow::Error::msg)?;
        let n = ch.n_channels();
        let delta = fs / n as f64;
        let k0 = ((((spot.khz * 1000.0 - center_hz) / delta).round() as i64).rem_euclid(n as i64))
            as usize;
        let t0 = (spot.t_sec - window_s / 2.0).max(0.0);
        let s0 = (t0 * fs) as usize;
        let s1 = ((t0 + window_s) * fs) as usize;
        if s0 >= iq.len() {
            results.push(OracleResult {
                call: spot.call.clone(),
                khz: spot.khz,
                as_word: false,
                framed: false,
                substring: false,
                wpm_ratio: None,
                text: String::new(),
            });
            continue;
        }
        let hops = ch.process(&iq[s0..s1.min(iq.len())]);
        let cands = [(k0 + n - 1) % n, k0, (k0 + 1) % n];
        let mean = |k: usize| {
            hops.iter().map(|h| h.power[k] as f64).sum::<f64>() / hops.len().max(1) as f64
        };
        let own = *cands
            .iter()
            .max_by(|&&a, &&b| mean(a).total_cmp(&mean(b)))
            .unwrap();
        let mut dec = TrackDecoder::new(1, cfg.clone());
        let mut events = Vec::new();
        for (i, h) in hops.iter().enumerate() {
            let p = h.power[own];
            events.extend(dec.push_hop(p.sqrt(), p, None, (s0 + i * ch.hop()) as u64));
        }
        events.extend(dec.finish());
        let text = events_to_text(&events);
        let wpm = events.iter().rev().find_map(|e| match e {
            DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
            _ => None,
        });
        let (as_word, framed, substring) = score_text(&spot.call, &text);
        let wpm_ratio = wpm.filter(|_| spot.wpm > 0).map(|w| w / spot.wpm as f32);
        results.push(OracleResult {
            call: spot.call.clone(),
            khz: spot.khz,
            as_word,
            framed,
            substring,
            wpm_ratio,
            text,
        });
    }
    let summary = summarize(spots, &results);
    Ok((results, summary))
}

fn summarize(spots: &[OracleSpot], results: &[OracleResult]) -> OracleSummary {
    let mut ratios: Vec<f32> = results.iter().filter_map(|r| r.wpm_ratio).collect();
    ratios.sort_by(f32::total_cmp);
    let median = if ratios.is_empty() {
        None
    } else {
        Some(ratios[ratios.len() / 2])
    };
    let bucket = |snr: i32| {
        if snr < 15 {
            "lt15"
        } else if snr < 25 {
            "15-25"
        } else {
            "ge25"
        }
    };
    let mut by = std::collections::BTreeMap::<&str, (usize, usize, usize)>::new();
    for (s, r) in spots.iter().zip(results) {
        let e = by.entry(bucket(s.snr_db)).or_default();
        e.0 += 1;
        e.1 += r.as_word as usize;
        e.2 += r.framed as usize;
    }
    OracleSummary {
        n: results.len(),
        as_word: results.iter().filter(|r| r.as_word).count(),
        framed: results.iter().filter(|r| r.framed).count(),
        substring: results.iter().filter(|r| r.substring).count(),
        wpm_ratio_median: median,
        by_snr: by
            .into_iter()
            .map(|(k, (n, a, f))| (k.to_string(), n, a, f))
            .collect(),
    }
}
