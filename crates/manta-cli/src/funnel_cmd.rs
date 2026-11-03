//! MAN-198: `manta oracle --funnel PATH` -- the stage-attributed loss
//! funnel, written as JSON Lines: a header, one `lossFunnel` record per RBN
//! episode, then a `lossFunnelSummary`. Diagnostic only: the oracle's
//! stdout and `--jsonl` are unchanged, and no exit code reads it. See
//! docs/DECISIONS/2026-11-03-man198-loss-funnel.md.

use anyhow::{Context, Result};
use manta_engine::funnel::{self, Bypass, Opportunity, Stage, Tolerances, TruthLayer};
use manta_engine::PipelineConfig;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

/// What `manta oracle --funnel` needs beyond the oracle's own pipeline.
pub(crate) struct FunnelArgs<'a> {
    pub recording: &'a Path,
    pub rbn_csv: &'a Path,
    pub spotter: &'a str,
    pub capture_start_unix_s: i64,
    pub config: Option<&'a Path>,
    pub out: &'a Path,
    /// Oracle-bypass experiments, written after the production summary.
    pub bypasses: &'a [Bypass],
    /// The whole recording, for the bypasses.
    pub iq: &'a [num_complex::Complex32],
    pub tol: Tolerances,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Header<'a> {
    kind: &'static str,
    schema: u32,
    truth_layer: &'static str,
    spotter: &'a str,
    capture_start: String,
    recording_file: String,
    recording_sha256: String,
    sidecar_sha256: Option<String>,
    reference_file: String,
    reference_sha256: String,
    config_sha256: Option<String>,
    build: &'static str,
    engine: manta_decode::decoder::Engine,
    prior: &'static str,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    duration_s: f64,
    passband_hz: [f64; 2],
    channel_width_hz: f64,
    freq_correction_ppm: f64,
    tolerances: Tolerances,
    stages: Vec<&'static str>,
    delivered_means: &'static str,
    bypasses: Vec<&'static str>,
}

fn sha256_file(p: &Path) -> Result<String> {
    let bytes = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The recording's sidecar: `PATH.meta.jsonl` for a manta IQ recording,
/// else the `<name>.json` next to a WAV; `None` when there is none.
fn sidecar_sha256(recording: &Path) -> Result<Option<String>> {
    let sidecar = if manta_input::is_native_recording(recording) {
        manta_input::metadata_path(recording)
    } else {
        manta_input::sidecar_path(recording)
    };
    if sidecar.is_file() {
        sha256_file(&sidecar).map(Some)
    } else {
        Ok(None)
    }
}

/// Unix seconds as ISO-8601 UTC (`2025-11-29T00:00:00Z`), the form
/// `--capture-start` takes. Howard Hinnant's `civil_from_days`.
fn utc_iso(unix_s: i64) -> String {
    let days = unix_s.div_euclid(86_400);
    let rem = unix_s.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Runs the recording once more through the production pipeline, writes
/// the funnel file, and prints one `note:` line to stderr.
pub(crate) fn run(args: &FunnelArgs, pipeline: &PipelineConfig, duration_s: f64) -> Result<()> {
    let episodes = manta_testkit::oracle::parse_rbn_episodes(
        args.rbn_csv,
        args.spotter,
        args.capture_start_unix_s,
        args.tol.episode_gap_s,
    )?;
    let opportunities: Vec<Opportunity> = episodes
        .iter()
        .map(|e| Opportunity {
            call: e.call.clone(),
            freq_hz: e.khz * 1000.0,
            first_s: e.first_s,
            last_s: e.last_s,
            rows: e.rows.len(),
            snr_db: e.rows.iter().map(|r| r.snr_db).max().unwrap_or(0),
            wpm: e.rows.iter().find_map(|r| r.wpm),
        })
        .collect();
    let src = manta_input::open_iq_file(args.recording, manta_input::IqFileOptions::default())?;
    let center_hz = src.center_freq_hz();
    let report = funnel::run_funnel(src, pipeline, &opportunities, &args.tol, duration_s)?;
    let geo = report.geometry;
    // Each named once, in the order first given.
    let mut bypasses: Vec<Bypass> = Vec::new();
    for b in args.bypasses {
        if !bypasses.contains(b) {
            bypasses.push(*b);
        }
    }
    let header = Header {
        kind: "lossFunnelHeader",
        schema: 1,
        truth_layer: TruthLayer::RbnSingleSpotter.label(),
        spotter: args.spotter,
        capture_start: utc_iso(args.capture_start_unix_s),
        recording_file: file_name(args.recording),
        recording_sha256: sha256_file(args.recording)?,
        sidecar_sha256: sidecar_sha256(args.recording)?,
        reference_file: file_name(args.rbn_csv),
        reference_sha256: sha256_file(args.rbn_csv)?,
        config_sha256: args.config.map(sha256_file).transpose()?,
        build: crate::build_info::DECODER_VERSION,
        engine: pipeline.decode.engine,
        prior: if pipeline.decode.prior_enabled {
            "on"
        } else {
            "off"
        },
        sample_rate_hz: geo.sample_rate_hz,
        center_freq_hz: geo.center_freq_hz,
        duration_s: geo.duration_s,
        passband_hz: [geo.passband_hz.0, geo.passband_hz.1],
        channel_width_hz: geo.channel_width_hz,
        freq_correction_ppm: geo.freq_correction_ppm,
        tolerances: args.tol,
        stages: Stage::ALL.iter().map(|s| s.as_str()).collect(),
        delivered_means: "emitted (offline replay has no sink that drops)",
        bypasses: bypasses.iter().map(|b| b.label()).collect(),
    };
    // Run before the file is created, so a failed bypass leaves no file
    // whose header names lines that never follow.
    let bypassed = bypasses
        .iter()
        .map(|b| {
            funnel::run_bypass(
                *b,
                args.iq,
                center_hz,
                pipeline,
                &report.records,
                &geo,
                &args.tol,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let file = std::fs::File::create(args.out)
        .with_context(|| format!("create {}", args.out.display()))?;
    let mut w = std::io::BufWriter::new(file);
    writeln!(w, "{}", serde_json::to_string(&header)?)?;
    for r in &report.records {
        writeln!(w, "{}", serde_json::to_string(r)?)?;
    }
    writeln!(w, "{}", serde_json::to_string(&report.summary)?)?;
    // Oracle-assisted lines only after the production summary: every line
    // after the header (which names the bypasses) is the same bytes with or
    // without them.
    for (lines, summary) in &bypassed {
        for l in lines {
            writeln!(w, "{}", serde_json::to_string(l)?)?;
        }
        writeln!(w, "{}", serde_json::to_string(summary)?)?;
    }
    w.flush()?;
    let s = &report.summary;
    let lost: Vec<String> = Stage::ALL
        .iter()
        .filter(|st| s.lost_at.get(**st) > 0)
        .map(|st| format!("{} {}", st.as_str(), s.lost_at.get(*st)))
        .collect();
    crate::logging::note(&format!(
        "note: loss funnel: {} opportunities ({} truth), {} delivered; lost at {}; written to {}",
        s.opportunities,
        TruthLayer::RbnSingleSpotter.label(),
        s.delivered,
        if lost.is_empty() {
            "none".to_string()
        } else {
            lost.join(", ")
        },
        args.out.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::utc_iso;

    #[test]
    fn utc_iso_round_trips_the_capture_start_parser() {
        for iso in [
            "1970-01-01T00:00:00Z",
            "2000-02-29T12:34:56Z",
            "2024-02-29T23:59:59Z",
            "2025-11-29T00:00:00Z",
            "2026-12-31T00:00:01Z",
            "2100-03-01T00:00:00Z",
        ] {
            let unix_s = manta_testkit::oracle::parse_utc_timestamp(iso).unwrap();
            assert_eq!(utc_iso(unix_s), iso);
        }
    }
}
