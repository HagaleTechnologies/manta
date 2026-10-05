//! Real-signal decode oracle: channelize a window around each RBN-spotted
//! station, feed the strongest of the three channels around its kHz
//! straight into `TrackDecoder`, and score callsign recovery. Isolates
//! the decode core from the tracker. SPEC v2 §8.3.

use anyhow::{bail, Context, Result};
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

/// Parses an RBN CSV `date` column value ("YYYY-MM-DD HH:MM:SS", UTC) or an
/// ISO-8601 timestamp ("YYYY-MM-DDTHH:MM:SSZ") into Unix epoch seconds.
/// Hand-rolled (no chrono/time dependency in this workspace) via Howard
/// Hinnant's days-from-civil algorithm; UTC, proleptic Gregorian, no leap
/// seconds -- matches `scripts/score-against-rbn.py`'s own handling of both
/// timestamp shapes. Validates every component's range (local review gate,
/// PR #161: the original version parsed numeric fields but never checked
/// them, so e.g. "2025-13-01T00:00:00Z" or "2025-02-29T00:00:00Z" [2025 is
/// not a leap year] silently normalized into a wrong epoch via
/// `days_from_civil`'s unchecked arithmetic instead of erroring) and that
/// no trailing garbage follows the expected component count.
pub fn parse_utc_timestamp(s: &str) -> Result<i64> {
    let sep = s
        .find(['T', ' '])
        .with_context(|| format!("no date/time separator in {s:?}"))?;
    let (date, time) = (&s[..sep], &s[sep + 1..]);
    let time = time.trim_end_matches('Z');
    // Accept an explicit +00:00/-00:00 (or +0000/-0000) UTC offset -- the
    // form `scripts/score-against-rbn.py`'s own `parse_iso` produces after
    // normalizing "Z" to "+00:00", and a form CLI users may reasonably
    // type directly -- as equivalent to "Z". Reject any NON-zero offset
    // explicitly rather than silently misparsing it as UTC (local review
    // gate, PR #161 round 2): this parser does no offset arithmetic, so a
    // real non-zero offset must be a hard error, not a wrong answer.
    let time = match time.rfind(['+', '-']) {
        Some(off_pos) => {
            let (clock, offset) = time.split_at(off_pos);
            let normalized = offset.replace(':', "");
            if normalized == "+0000" || normalized == "-0000" {
                clock
            } else {
                bail!(
                    "unsupported non-UTC offset {offset:?} in {s:?} -- only Z or a zero UTC \
                     offset (+00:00/-00:00) is supported, since this parser does no offset \
                     arithmetic"
                );
            }
        }
        None => time,
    };
    let mut d = date.split('-');
    let y: i64 = d
        .next()
        .context("year")?
        .parse()
        .with_context(|| format!("year in {s:?}"))?;
    let mo: i64 = d
        .next()
        .context("month")?
        .parse()
        .with_context(|| format!("month in {s:?}"))?;
    let da: i64 = d
        .next()
        .context("day")?
        .parse()
        .with_context(|| format!("day in {s:?}"))?;
    if d.next().is_some() {
        bail!("unexpected extra date component in {s:?}");
    }
    let mut t = time.split(':');
    let h: i64 = t
        .next()
        .context("hour")?
        .parse()
        .with_context(|| format!("hour in {s:?}"))?;
    let mi: i64 = t
        .next()
        .context("minute")?
        .parse()
        .with_context(|| format!("minute in {s:?}"))?;
    let se: i64 = t
        .next()
        .context("second")?
        .parse()
        .with_context(|| format!("second in {s:?}"))?;
    if t.next().is_some() {
        bail!("unexpected extra time component in {s:?}");
    }
    // Codex review, PR #161 round 2: an extreme but syntactically valid
    // year (e.g. i64::MAX) reaches `days_from_civil`'s unchecked arithmetic
    // and panics with an overflow instead of returning the intended parse
    // error. No real capture date needs a year outside this range.
    if !(1..=9999).contains(&y) {
        bail!("year out of range 1-9999 in {s:?}: {y}");
    }
    if !(1..=12).contains(&mo) {
        bail!("month out of range 1-12 in {s:?}: {mo}");
    }
    let dim = days_in_month(y, mo);
    if da < 1 || da > dim {
        bail!("day out of range 1-{dim} for {y:04}-{mo:02} in {s:?}: {da}");
    }
    if !(0..=23).contains(&h) {
        bail!("hour out of range 0-23 in {s:?}: {h}");
    }
    if !(0..=59).contains(&mi) {
        bail!("minute out of range 0-59 in {s:?}: {mi}");
    }
    if !(0..=59).contains(&se) {
        bail!("second out of range 0-59 in {s:?}: {se}");
    }
    Ok(days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + se)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// `m` must already be validated as 1-12 (checked in `parse_utc_timestamp`
/// before this is called).
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => unreachable!("month {m} must already be validated as 1-12"),
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// First spot per (call, kHz) from an RBN daily-dump CSV, restricted to
/// `spotter`. `capture_start_epoch_s` (Unix epoch seconds, UTC -- see
/// `parse_utc_timestamp`) anchors `OracleSpot::t_sec` to the actual
/// recording start (Codex review, PR #161: extracting only `MM:SS` from
/// the RBN row and treating it as an offset from the file start silently
/// assumes the capture starts exactly on the hour and never crosses an
/// hour boundary -- wrong for any other capture).
pub fn parse_rbn_spots(
    csv_path: &Path,
    spotter: &str,
    capture_start_epoch_s: i64,
) -> Result<Vec<OracleSpot>> {
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
        let t_sec = (parse_utc_timestamp(f[c_date])? - capture_start_epoch_s) as f64;
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
    // Codex review, PR #161 round 2: a negative window_s makes s1 < s0 below,
    // panicking on the iq[s0..s1] slice; zero or NaN silently produces an
    // empty/undefined window per spot instead of a load-time error; infinity
    // decodes the entire capture for every spot. Validate here, at the
    // public oracle boundary, rather than only at the CLI, since this
    // function is directly callable from other code (tests, future
    // callers) that never goes through clap's value_parser.
    anyhow::ensure!(
        window_s.is_finite() && window_s > 0.0,
        "window_s must be finite and positive (got {window_s})"
    );
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
    // Codex review, PR #161 round 14: `ratios[len/2]` picks the UPPER
    // middle value for an even-length set instead of averaging the two
    // central values -- e.g. [0.9, 1.1] reported 1.1, not the
    // conventional median 1.0. The stage-1 acceptance gate compares this
    // against [0.95, 1.05], so an even-sized result set could incorrectly
    // pass or fail purely from this rounding-direction bug.
    let median = if ratios.is_empty() {
        None
    } else if ratios.len() % 2 == 0 {
        let hi = ratios.len() / 2;
        Some((ratios[hi - 1] + ratios[hi]) / 2.0)
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

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    #[test]
    fn parses_rbn_space_separated_and_iso8601_z_forms() {
        // Same instant, two shapes this function must accept (RBN's own
        // "date" column, and an ISO-8601 --capture-start).
        assert_eq!(
            parse_utc_timestamp("2025-11-29 00:00:00").unwrap(),
            parse_utc_timestamp("2025-11-29T00:00:00Z").unwrap()
        );
    }

    #[test]
    fn epoch_zero_round_trips() {
        assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
    }

    #[test]
    fn known_epoch_value_matches() {
        // 2025-11-29T00:00:00Z, cross-checked against `date -u -d@<epoch>`.
        assert_eq!(
            parse_utc_timestamp("2025-11-29T00:00:00Z").unwrap(),
            1_764_374_400
        );
    }

    #[test]
    fn capture_start_anchoring_survives_an_hour_boundary() {
        // Codex review, PR #161: the old `MM:SS`-only parser treated wall-clock
        // time as an offset from the top of an hour. A spot at 00:59:50 seen
        // shortly after a 00:59:30 capture start must land ~20s in, not wrap
        // toward the start of the *next* hour's MM:SS reading (only 20s either
        // way here, but the old bug would have silently produced a huge wrong
        // offset for a spot just past the hour, e.g. 01:00:05).
        let capture_start = parse_utc_timestamp("2025-11-29T00:59:30Z").unwrap();
        let spot_row_epoch = parse_utc_timestamp("2025-11-29T01:00:05Z").unwrap();
        assert_eq!(spot_row_epoch - capture_start, 35);
    }

    #[test]
    fn rejects_out_of_range_month() {
        assert!(parse_utc_timestamp("2025-13-01T00:00:00Z").is_err());
        assert!(parse_utc_timestamp("2025-00-01T00:00:00Z").is_err());
    }

    #[test]
    fn rejects_extreme_years_instead_of_overflowing() {
        // Codex review, PR #161 round 2: this exact input panicked with
        // "attempt to multiply with overflow" in days_from_civil before the
        // year-range check was added.
        assert!(parse_utc_timestamp("9223372036854775807-01-01T00:00:00Z").is_err());
        assert!(parse_utc_timestamp("0-01-01T00:00:00Z").is_err());
        assert!(parse_utc_timestamp("10000-01-01T00:00:00Z").is_err());
        assert!(parse_utc_timestamp("1-01-01T00:00:00Z").is_ok());
        assert!(parse_utc_timestamp("9999-01-01T00:00:00Z").is_ok());
    }

    #[test]
    fn rejects_feb_29_in_a_non_leap_year() {
        assert!(parse_utc_timestamp("2025-02-29T00:00:00Z").is_err());
    }

    #[test]
    fn accepts_feb_29_in_a_leap_year() {
        assert!(parse_utc_timestamp("2024-02-29T00:00:00Z").is_ok());
    }

    #[test]
    fn rejects_out_of_range_hour_minute_second() {
        assert!(parse_utc_timestamp("2025-11-29T99:00:00Z").is_err());
        assert!(parse_utc_timestamp("2025-11-29T00:60:00Z").is_err());
        assert!(parse_utc_timestamp("2025-11-29T00:00:60Z").is_err());
    }

    #[test]
    fn rejects_day_out_of_range_for_its_month() {
        assert!(parse_utc_timestamp("2025-04-31T00:00:00Z").is_err()); // April has 30 days
        assert!(parse_utc_timestamp("2025-11-29T00:00:00Z").is_ok()); // sanity: valid date still parses
    }

    #[test]
    fn rejects_trailing_garbage_components() {
        assert!(parse_utc_timestamp("2025-11-29-01T00:00:00Z").is_err());
        assert!(parse_utc_timestamp("2025-11-29T00:00:00:00Z").is_err());
    }

    #[test]
    fn accepts_explicit_zero_utc_offset_forms() {
        // Local review gate, PR #161 round 2: "+00:00" is a valid ISO-8601
        // UTC timestamp (it's what score-against-rbn.py's own parse_iso
        // produces after normalizing "Z"), and must parse identically to "Z".
        let z = parse_utc_timestamp("2025-11-29T00:00:00Z").unwrap();
        assert_eq!(parse_utc_timestamp("2025-11-29T00:00:00+00:00").unwrap(), z);
        assert_eq!(parse_utc_timestamp("2025-11-29T00:00:00-00:00").unwrap(), z);
        assert_eq!(parse_utc_timestamp("2025-11-29T00:00:00+0000").unwrap(), z);
    }

    #[test]
    fn rejects_non_zero_utc_offsets_rather_than_silently_misparsing() {
        // This parser does no offset arithmetic -- a real non-zero offset
        // must be a hard error, never silently treated as UTC.
        assert!(parse_utc_timestamp("2025-11-29T00:00:00+05:00").is_err());
        assert!(parse_utc_timestamp("2025-11-29T00:00:00-08:00").is_err());
    }
}

#[cfg(test)]
mod summarize_tests {
    use super::*;

    fn result_with_ratio(wpm_ratio: f32) -> OracleResult {
        OracleResult {
            call: "W5AU".to_string(),
            khz: 14000.0,
            as_word: false,
            framed: false,
            substring: false,
            wpm_ratio: Some(wpm_ratio),
            text: String::new(),
        }
    }

    fn spot() -> OracleSpot {
        OracleSpot {
            call: "W5AU".to_string(),
            khz: 14000.0,
            t_sec: 0.0,
            snr_db: 20,
            wpm: 20,
        }
    }

    #[test]
    fn median_averages_the_two_central_values_for_an_even_sized_set() {
        // Codex review, PR #161 round 14: ratios[len/2] picked the upper
        // middle value for an even-length set (e.g. [0.9, 1.1] reported
        // 1.1, not the conventional median 1.0), which could flip the
        // stage-1 acceptance gate's [0.95, 1.05] pass/fail comparison.
        let results = vec![result_with_ratio(0.9), result_with_ratio(1.1)];
        let spots = vec![spot(), spot()];
        let summary = summarize(&spots, &results);
        assert_eq!(summary.wpm_ratio_median, Some(1.0));
    }

    #[test]
    fn median_is_the_middle_value_for_an_odd_sized_set() {
        let results = vec![
            result_with_ratio(0.8),
            result_with_ratio(1.0),
            result_with_ratio(1.4),
        ];
        let spots = vec![spot(), spot(), spot()];
        let summary = summarize(&spots, &results);
        assert_eq!(summary.wpm_ratio_median, Some(1.0));
    }

    #[test]
    fn median_is_none_when_no_result_has_a_wpm_ratio() {
        let results = vec![OracleResult {
            call: "W5AU".to_string(),
            khz: 14000.0,
            as_word: false,
            framed: false,
            substring: false,
            wpm_ratio: None,
            text: String::new(),
        }];
        let spots = vec![spot()];
        let summary = summarize(&spots, &results);
        assert_eq!(summary.wpm_ratio_median, None);
    }
}
