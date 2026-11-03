//! MAN-198: `manta oracle --funnel PATH` -- the stage-attributed loss
//! funnel, written as JSON Lines: a header, one `lossFunnel` record per RBN
//! episode, then a `lossFunnelSummary`. Diagnostic only: the oracle's
//! stdout and `--jsonl` are unchanged, and no exit code reads it. See
//! docs/DECISIONS/2026-11-03-man198-loss-funnel.md.

use anyhow::{Context, Result};
use manta_engine::funnel::{self, Opportunity, Stage, Tolerances, TruthLayer};
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
    pub tol: Tolerances,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Header<'a> {
    kind: &'static str,
    schema: u32,
    truth_layer: &'static str,
    spotter: &'a str,
    capture_start_unix_s: i64,
    recording_file: String,
    recording_sha256: String,
    reference_file: String,
    reference_sha256: String,
    config_sha256: Option<String>,
    build: &'static str,
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
    let report = funnel::run_funnel(src, pipeline, &opportunities, &args.tol, duration_s)?;
    let geo = report.geometry;
    let header = Header {
        kind: "lossFunnelHeader",
        schema: 1,
        truth_layer: TruthLayer::RbnSingleSpotter.label(),
        spotter: args.spotter,
        capture_start_unix_s: args.capture_start_unix_s,
        recording_file: file_name(args.recording),
        recording_sha256: sha256_file(args.recording)?,
        reference_file: file_name(args.rbn_csv),
        reference_sha256: sha256_file(args.rbn_csv)?,
        config_sha256: args.config.map(sha256_file).transpose()?,
        build: crate::build_info::DECODER_VERSION,
        sample_rate_hz: geo.sample_rate_hz,
        center_freq_hz: geo.center_freq_hz,
        duration_s: geo.duration_s,
        passband_hz: [geo.passband_hz.0, geo.passband_hz.1],
        channel_width_hz: geo.channel_width_hz,
        freq_correction_ppm: geo.freq_correction_ppm,
        tolerances: args.tol,
        stages: Stage::ALL.iter().map(|s| s.as_str()).collect(),
        delivered_means: "emitted (offline replay has no sink that drops)",
        bypasses: Vec::new(),
    };
    let file = std::fs::File::create(args.out)
        .with_context(|| format!("create {}", args.out.display()))?;
    let mut w = std::io::BufWriter::new(file);
    writeln!(w, "{}", serde_json::to_string(&header)?)?;
    for r in &report.records {
        writeln!(w, "{}", serde_json::to_string(r)?)?;
    }
    writeln!(w, "{}", serde_json::to_string(&report.summary)?)?;
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
