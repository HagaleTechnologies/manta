//! MAN-19: "operators should be able to trust manta runs unattended for a
//! full day without crashing or leaking resources." ROADMAP.md's M2
//! accept gate calls for a 24h soak on a live 40m CW segment via real SDR
//! -- per MAN-20's decision (no real contest-weekend recording exists
//! yet, and other work must not block on that data gap), this harness
//! substitutes a synthetic 40m CW pileup scene (manta-testkit) looped
//! indefinitely through the same streaming pipeline `manta listen`/`manta
//! soak` run in production, driven for the requested wall-clock duration.
//!
//! Not part of the operator-facing `manta` binary (crates/manta-cli) --
//! this is measurement/bench tooling only, per MAN-19's scope.

use anyhow::{Context, Result};
use clap::Parser;
use manta_dsp::hilbert::HilbertTransformer;
use manta_engine::{soak_metrics_passed, soak_with_metrics, PipelineConfig, SoakMetricsSample};
use manta_input::IqSource;
use manta_testkit::keyer::Jitter;
use manta_testkit::scene::{render_scene, QsbSine, SignalSpec};
use num_complex::Complex32;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Fixed target rate the real `manta listen`/`manta soak` audio path
/// requires (manta-input::audio::TARGET_RATE_HZ) -- not re-exported, so
/// pinned here to the same literal value.
const FS_HZ: f64 = 48_000.0;

/// Deterministic so a re-run reproduces the same scene byte-for-byte
/// (manta-testkit's ChaCha8 fixture convention).
const DEFAULT_SEED: u64 = 0x4D41_4E31_395F_534F; // "MAN19_SO"...(ascii-ish)

#[derive(Parser)]
#[command(
    name = "man19-soak",
    about = "MAN-19: multi-hour unattended soak against a looped synthetic 40m CW pileup scene"
)]
struct Cli {
    /// Wall-clock soak duration, in hours. ROADMAP.md's M2 gate calls for 24.
    #[arg(long, default_value_t = 24.0)]
    duration_hours: f64,
    /// Length of the one base pileup scene that gets looped, in seconds.
    #[arg(long, default_value_t = 120.0)]
    scene_seconds: f64,
    /// How often to sample RSS + track/eviction counts, in seconds.
    #[arg(long, default_value_t = 30)]
    sample_interval_secs: u64,
    /// Output directory for scene.wav, scene_manifest.json, metrics.jsonl,
    /// report.json. Created if missing. Defaults to a timestamped dir
    /// under ./man19-soak-runs/.
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Scene RNG seed (noise + reproducibility).
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
}

/// A crowded 40m CW sub-band as heard in a receiver's audio passband:
/// ~20 simultaneous stations spread 400-2700 Hz, a couple of close pairs
/// (deliberately within a channel or two of each other) to exercise
/// adjacent-channel merge/eviction, varied speed/SNR/jitter. Loosely
/// modeled on manta-testkit's V8 pileup-scene precedent
/// (crates/manta-engine/benches/cpu_budget.rs), just audio-band offsets
/// instead of RF-band ones and far fewer signals (this needs to run for
/// hours, not profile a single 15s window).
fn pileup_signals() -> Vec<SignalSpec> {
    // Plausible-format, non-hardcoded-elsewhere fixture calls -- not
    // manta-testkit::callsigns::pileup_calls() (pub(crate), unreachable
    // from this crate).
    const CALLS: &[&str] = &[
        "W1AW", "K3LR", "N4ZZ", "VE3EJ", "W6YX", "K9CT", "W5AU", "N2IC", "K1TTT", "W3LPL", "VE7CC",
        "K5TR", "N6RO", "W4AN", "K8AZ", "W2GD", "N5DX", "K4XS", "W9RE", "VA3RJ",
    ];
    let offsets_hz: [f64; 20] = [
        420.0,
        560.0,
        560.0 + 70.0, // close pair around 560-630 Hz
        780.0,
        940.0,
        1120.0,
        1120.0 + 55.0, // close pair around 1120-1175 Hz
        1300.0,
        1470.0,
        1640.0,
        1810.0,
        1980.0,
        2150.0,
        2150.0 + 60.0, // close pair
        2320.0,
        2490.0,
        2660.0,
        700.0,
        1550.0,
        2000.0,
    ];
    CALLS
        .iter()
        .zip(offsets_hz.iter())
        .enumerate()
        .map(|(i, (call, &offset_hz))| {
            let wpm = 18.0 + (i as f32 * 7.0) % 18.0; // 18..36 WPM spread
            let snr_2500_db = 4.0 + (i as f32 * 5.0) % 20.0; // 4..24 dB spread
            SignalSpec {
                text: format!("CQ CQ DE {call} {call} K"),
                loop_text: true,
                wpm,
                offset_hz,
                snr_2500_db,
                jitter: (i % 2 == 0).then_some(Jitter {
                    sigma: 0.08,
                    seed: 0x1000 + i as u64,
                }),
                qsb: (i % 5 == 0).then_some(QsbSine {
                    rate_hz: 0.15 + i as f32 * 0.01,
                }),
                watterson: None,
                char_wpm: None,
            }
        })
        .collect()
}

/// Loops a real-valued mono buffer indefinitely through the same
/// real->analytic Hilbert conversion `manta_input::AudioIqSource` applies
/// to a live/replayed audio device (manta-input/src/audio.rs), but never
/// reports EOF -- `AudioIqSource` itself has no looping mode (confirmed:
/// its `read()` returns 0 past end-of-file/stream), so a many-hour soak
/// needs its own indefinite source rather than a giant WAV on disk.
struct LoopingAudioIqSource {
    real: Vec<f32>,
    cursor: usize,
    hilbert: HilbertTransformer,
}

impl LoopingAudioIqSource {
    fn new(real: Vec<f32>) -> Self {
        assert!(!real.is_empty(), "base scene must not be empty");
        LoopingAudioIqSource {
            real,
            cursor: 0,
            hilbert: HilbertTransformer::new(),
        }
    }
}

impl IqSource for LoopingAudioIqSource {
    fn sample_rate(&self) -> f64 {
        FS_HZ
    }

    fn center_freq_hz(&self) -> f64 {
        0.0 // audio has no RF reference, matches AudioIqSource
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let mut filled = 0;
        while filled < real.len() {
            let take = (self.real.len() - self.cursor).min(real.len() - filled);
            real[filled..filled + take]
                .copy_from_slice(&self.real[self.cursor..self.cursor + take]);
            self.cursor += take;
            filled += take;
            if self.cursor >= self.real.len() {
                self.cursor = 0;
            }
        }
        let analytic = self.hilbert.process(&real);
        buf.copy_from_slice(&analytic);
        Ok(buf.len())
    }
}

fn write_mono_wav(path: &std::path::Path, real: &[f32]) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: FS_HZ as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in real {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let out_dir = cli.out_dir.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        PathBuf::from(format!("man19-soak-runs/{ts}"))
    });
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating out-dir {}", out_dir.display()))?;

    eprintln!(
        "MAN-19 soak: duration={}h scene={}s sample_interval={}s seed={:#x} out_dir={}",
        cli.duration_hours,
        cli.scene_seconds,
        cli.sample_interval_secs,
        cli.seed,
        out_dir.display()
    );

    let signals = pileup_signals();
    let (complex, texts) = render_scene(&signals, FS_HZ, cli.scene_seconds, Some(cli.seed))
        .context("rendering base pileup scene")?;
    let real: Vec<f32> = complex.iter().map(|c| c.re).collect();

    write_mono_wav(&out_dir.join("scene.wav"), &real).context("writing scene.wav")?;
    std::fs::write(
        out_dir.join("scene_manifest.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "fs_hz": FS_HZ,
            "scene_seconds": cli.scene_seconds,
            "seed": cli.seed,
            "n_signals": signals.len(),
            "offsets_hz": signals.iter().map(|s| s.offset_hz).collect::<Vec<_>>(),
            "keyed_texts": texts,
        }))?,
    )
    .context("writing scene_manifest.json")?;
    eprintln!(
        "base scene: {} signals, {:.1}s ({} samples) -- {}",
        signals.len(),
        cli.scene_seconds,
        real.len(),
        out_dir.join("scene.wav").display()
    );

    let src: Box<dyn IqSource> = Box::new(LoopingAudioIqSource::new(real));
    let cfg = PipelineConfig::default();

    let metrics_path = out_dir.join("metrics.jsonl");
    let metrics_file = std::fs::File::create(&metrics_path)
        .with_context(|| format!("creating {}", metrics_path.display()))?;
    let mut metrics_w = std::io::BufWriter::new(metrics_file);

    let on_sample = |s: &SoakMetricsSample| {
        let line = serde_json::json!({
            "elapsed_s": s.elapsed_s,
            "rss_bytes": s.rss_bytes,
            "rss_mib": s.rss_bytes as f64 / (1024.0 * 1024.0),
            "events_emitted": s.events_emitted,
            "spots_emitted": s.spots_emitted,
            "active_tracks": s.active_tracks,
            "close_counts": {
                "unconfirmed": s.close_counts.unconfirmed,
                "hang_expired": s.close_counts.hang_expired,
                "silent": s.close_counts.silent,
                "merged": s.close_counts.merged,
                "evicted": s.close_counts.evicted,
            },
        });
        if let Ok(text) = serde_json::to_string(&line) {
            let _ = writeln!(metrics_w, "{text}");
            let _ = metrics_w.flush(); // survive a kill mid-run
        }
        eprintln!(
            "t={:>8.0}s rss={:>7.1}MiB active_tracks={:>4} evicted={:>5} merged={:>5} events={} spots={}",
            s.elapsed_s,
            s.rss_bytes as f64 / (1024.0 * 1024.0),
            s.active_tracks,
            s.close_counts.evicted,
            s.close_counts.merged,
            s.events_emitted,
            s.spots_emitted,
        );
    };

    let report = soak_with_metrics(
        src,
        &cfg,
        Duration::from_secs_f64(cli.duration_hours * 3600.0),
        Duration::from_secs(cli.sample_interval_secs),
        on_sample,
    )?;

    let rss_growth_mib = report.rss_growth_bytes as f64 / (1024.0 * 1024.0);
    let no_crash = !report.panicked;
    // File/looping-in-memory replay has no ring buffer to overrun by
    // construction (matches soak.rs's own documented deviation for
    // file-replay sources) -- always satisfied here, not a live-hardware
    // measurement.
    let no_input_overrun = true;
    let no_unbounded_growth = soak_metrics_passed(&report);
    // Satisfied structurally: every metrics.jsonl line carries
    // active_tracks + close_counts throughout the run.
    let metrics_visible = true;
    let overall_pass = no_crash && no_input_overrun && no_unbounded_growth && metrics_visible;

    let summary = serde_json::json!({
        "duration_requested_hours": cli.duration_hours,
        "duration_actual_secs": report.duration_actual.as_secs_f64(),
        "events_emitted": report.events_emitted,
        "spots_emitted": report.spots_emitted,
        "rss_growth_bytes": report.rss_growth_bytes,
        "rss_growth_mib": rss_growth_mib,
        "panicked": report.panicked,
        "peak_active_tracks": report.peak_active_tracks,
        "final_close_counts": {
            "unconfirmed": report.final_close_counts.unconfirmed,
            "hang_expired": report.final_close_counts.hang_expired,
            "silent": report.final_close_counts.silent,
            "merged": report.final_close_counts.merged,
            "evicted": report.final_close_counts.evicted,
        },
        "man19_criteria": {
            "no_crash": no_crash,
            "no_input_overrun": no_input_overrun,
            "no_unbounded_memory_growth": no_unbounded_growth,
            "track_count_and_evictions_visible_in_metrics": metrics_visible,
        },
        "overall_pass": overall_pass,
    });
    std::fs::write(
        out_dir.join("report.json"),
        serde_json::to_string_pretty(&summary)?,
    )
    .context("writing report.json")?;

    println!("{}", serde_json::to_string_pretty(&summary)?);
    eprintln!(
        "MAN-19 soak {}: {}",
        if overall_pass { "PASSED" } else { "FAILED" },
        out_dir.join("report.json").display()
    );

    if !overall_pass {
        std::process::exit(1);
    }
    Ok(())
}
