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
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// ROADMAP.md's M2 accept gate: 24h. Below this, a run may be healthy but
/// is not itself a MAN-19 acceptance result (round 1 review).
const MAN19_ACCEPT_HOURS: f64 = 24.0;

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
    /// Wall-clock soak duration, in hours. ROADMAP.md's M2 gate calls for
    /// 24. Must be finite and positive -- NaN reaches
    /// `Duration::from_secs_f64` and panics.
    #[arg(long, default_value_t = 24.0, value_parser = parse_positive_finite_hours)]
    duration_hours: f64,
    /// Length of the one base pileup scene that gets looped, in seconds.
    /// Must be finite and positive -- NaN makes `key_text_loop`'s
    /// budget comparison permanently false (scene construction grows
    /// unbounded until resource exhaustion); very small/negative values
    /// produce an empty buffer and trip `LoopingAudioIqSource::new`'s
    /// assertion instead of a normal CLI error.
    #[arg(long, default_value_t = 120.0, value_parser = parse_positive_finite_scene_seconds)]
    scene_seconds: f64,
    /// How often to sample RSS + track/eviction counts, in seconds. Must
    /// be positive -- at 0, every processed chunk would trigger a sample
    /// (this replays in memory with no real-time pacing), serializing and
    /// flushing a JSON record far faster than any sampling interval is
    /// meant to, and could exhaust the output disk over a long soak.
    #[arg(long, default_value_t = 30, value_parser = parse_positive_secs)]
    sample_interval_secs: u64,
    /// Output directory for scene.wav, scene_manifest.json, metrics.jsonl,
    /// report.json. Created if missing. Defaults to a collision-resistant
    /// per-run dir under the OS temp dir (AGENTS.md: "use per-session
    /// scratch dirs") -- never inside the repo/worktree by default, and
    /// never colliding with a concurrent run started in the same second
    /// (round 3 review: the old default was a bare UNIX-seconds
    /// timestamp under the current directory, so two runs launched
    /// within the same second silently overwrote each other's files, and
    /// a run left large untracked artifacts inside the worktree).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Scene RNG seed (noise + reproducibility).
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
}

/// Clap value parser for `--sample-interval-secs`: rejects 0 at CLI-parse
/// time, before opening any output file (round 3 review).
fn parse_positive_secs(s: &str) -> std::result::Result<u64, String> {
    let secs: u64 = s
        .parse()
        .map_err(|e| format!("invalid --sample-interval-secs {s:?}: {e}"))?;
    if secs == 0 {
        return Err("--sample-interval-secs must be positive (0 samples every processed chunk, far faster than intended)".to_string());
    }
    Ok(secs)
}

/// Clap value parser for `--duration-hours`: rejects non-finite/
/// non-positive values before they reach `Duration::from_secs_f64` (which
/// panics on NaN) (round 4 review), a value so large that `hours *
/// 3600.0` (the exact conversion `main()` performs) itself overflows to
/// infinity (round 6 review: `1e308 * 3600.0` overflows f64's ~1.8e308
/// max), AND a value whose seconds, while still finite, exceed what
/// `Duration` itself can represent -- e.g. `1e16` hours is `3.6e19`
/// seconds, a perfectly finite f64, but past `Duration::MAX`'s ~1.8e19
/// second ceiling (`u64` seconds internally), so `Duration::from_secs_f64`
/// panics anyway (round 7 review).
fn parse_positive_finite_hours(s: &str) -> std::result::Result<f64, String> {
    let hours: f64 = s
        .parse()
        .map_err(|e| format!("invalid --duration-hours {s:?}: {e}"))?;
    if !hours.is_finite() || hours <= 0.0 {
        return Err(format!(
            "--duration-hours must be finite and positive, got {hours}"
        ));
    }
    let secs = hours * 3600.0;
    // Round 8 review: `Duration::MAX.as_secs_f64()` itself rounds UP to
    // exactly 2^64 (u64::MAX seconds isn't exactly representable in f64,
    // and the nearest representable value above it is 2^64) -- a strict
    // `>` bound let a `secs` that rounds to exactly that boundary value
    // through, which still panics in `Duration::from_secs_f64` (its true
    // max is u64::MAX seconds, one less than 2^64). `>=` closes that.
    if !secs.is_finite() || secs >= Duration::MAX.as_secs_f64() {
        return Err(format!(
            "--duration-hours {hours} is too large -- {secs} seconds exceeds what Duration can represent"
        ));
    }
    Ok(hours)
}

/// Safety budget for the primary `Vec<Complex32>` scene buffer alone --
/// round 6's 2-billion-sample cap (~16 GiB) still permits an allocation
/// large enough to abort the process on an ordinary machine before Clap
/// ever gets to return the intended error (Rust's default allocator
/// aborts on OOM; that isn't a catchable `Result`/panic). 256 MiB is
/// generous next to `--scene-seconds`'s documented 120s default and
/// covers any realistic "long base scene" use case (round 7 review's
/// per-hop peak-tracking discussion alone implies scenes are minutes,
/// not hours) while staying well inside what even a small CI runner can
/// allocate without risk.
const MAX_SCENE_BYTES: usize = 256 * 1024 * 1024;
/// `MAX_SCENE_BYTES` in `Complex32` samples (8 bytes each) -- ~699s
/// (~11.6 min) at `FS_HZ` (round 9 review).
const MAX_SCENE_SAMPLES: f64 = (MAX_SCENE_BYTES / std::mem::size_of::<Complex32>()) as f64;

/// Clap value parser for `--scene-seconds`: rejects non-finite/
/// non-positive values before they reach `render_scene`/`key_text_loop`
/// (round 4 review: NaN makes `key_text_loop`'s budget comparison
/// permanently false, growing the scene buffer unbounded), rejects any
/// positive value too small to round to at least one sample at `FS_HZ`
/// (round 5 review: `secs <= 0.0` alone still let e.g. `0.000001`
/// through, which `render_scene`'s own `(duration_s * fs).round() as
/// usize` computation -- mirrored here -- turns into an empty buffer,
/// tripping `LoopingAudioIqSource::new`'s assertion instead of a normal
/// CLI error), AND rejects a value so large the sample count either
/// overflows to infinity or exceeds `MAX_SCENE_SAMPLES` -- e.g. `1e300`
/// passes the finite/positive check but `secs * FS_HZ` is itself
/// `usize::MAX`-saturating or an outright multi-exabyte allocation
/// attempt, which aborts the process rather than returning a normal CLI
/// error (round 6 review).
fn parse_positive_finite_scene_seconds(s: &str) -> std::result::Result<f64, String> {
    let secs: f64 = s
        .parse()
        .map_err(|e| format!("invalid --scene-seconds {s:?}: {e}"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!(
            "--scene-seconds must be finite and positive, got {secs}"
        ));
    }
    let n_samples = secs * FS_HZ;
    if n_samples.round() < 1.0 {
        return Err(format!(
            "--scene-seconds {secs} is too small to produce even one sample at {FS_HZ} Hz"
        ));
    }
    if !n_samples.is_finite() || n_samples >= MAX_SCENE_SAMPLES {
        return Err(format!(
            "--scene-seconds {secs} would need {n_samples:.0} samples at {FS_HZ} Hz, over the \
             {MAX_SCENE_SAMPLES:.0}-sample cap -- this is meant to be the one base loop unit the \
             harness repeats for the whole --duration-hours run, not the run's own length"
        ));
    }
    Ok(secs)
}

/// A crowded 40m CW sub-band as heard in a receiver's audio passband:
/// ~20 simultaneous stations spread 400-2700 Hz, a couple of close pairs
/// (deliberately within a channel or two of each other) to exercise
/// adjacent-channel merge/eviction, varied speed/SNR/jitter, plus one
/// deliberately clean, isolated, high-SNR caller (matching soak.rs's own
/// proven-working single-signal test) so at least one track reliably
/// decodes cleanly enough to pass grammar/CTY validation and reach
/// `RepetitionGate::record` -- without it, `gate_records_total` measured
/// 0 across every run tried (round 3 review's `workload_exercised`
/// addition would otherwise make this harness permanently unable to
/// pass: the rest of the pileup is deliberately dense/colliding enough
/// that nothing else here ever clears grammar+cty). Loosely modeled on
/// manta-testkit's V8 pileup-scene precedent
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
    let mut signals: Vec<SignalSpec> = CALLS
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
        .collect();
    // The deliberately clean, isolated caller -- see the function doc.
    // Offset clear of every other signal above (max is 2660 Hz); 24 dB
    // SNR, no jitter/QSB, moderate 20 WPM -- same shape as soak.rs's own
    // unit test (`soak_reports_no_panic_on_a_clean_short_signal`), which
    // is proven to decode "CQ CQ DE W1AW W1AW K" end to end.
    signals.push(SignalSpec {
        text: "CQ CQ DE N1CLR N1CLR K".to_string(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 3400.0,
        snr_2500_db: 24.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    });
    signals
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
        // Round 3 review: a bare UNIX-seconds timestamp under the current
        // directory both collides (two runs launched within the same
        // second silently overwrote each other's scene/metrics/report
        // files) and, when invoked from inside the worktree with no
        // --out-dir, left large untracked artifacts inside the repo.
        // Seconds+nanos+pid under the OS temp dir is collision-resistant
        // and matches AGENTS.md's "use per-session scratch dirs".
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        std::env::temp_dir().join(format!(
            "man19-soak-runs/{}-{:09}-{}",
            now.as_secs(),
            now.subsec_nanos(),
            std::process::id()
        ))
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

    // MAN-19 review round 1: a write/flush failure (disk full, unmounted
    // volume) during an unattended run must not be silently swallowed --
    // that would leave `metrics.jsonl` incomplete or empty while
    // `overall_pass` hardcodes `metrics_visible = true` below, reporting
    // success for a run whose actual observability failed. Recorded here
    // (not returned from the closure -- `soak_with_metrics`'s `on_sample`
    // is `FnMut(&SoakMetricsSample)`, not fallible) and checked once the
    // run completes.
    let io_error: RefCell<Option<std::io::Error>> = RefCell::new(None);
    // MAN-19 review round 2: catches the write-side failure the counter
    // above can't -- `on_sample` never being called at all (e.g.
    // `--sample-interval-secs` >= the requested duration). Without this,
    // `metrics_visible` stayed hardcoded true even for an empty
    // metrics.jsonl on an otherwise-healthy run.
    let sample_count: RefCell<usize> = RefCell::new(0);

    let on_sample = |s: &SoakMetricsSample| {
        if io_error.borrow().is_some() {
            return; // already failed -- stop trying, first error wins
        }
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
        let text = match serde_json::to_string(&line) {
            Ok(t) => t,
            Err(e) => {
                *io_error.borrow_mut() = Some(std::io::Error::other(e));
                return;
            }
        };
        if let Err(e) = writeln!(metrics_w, "{text}") {
            *io_error.borrow_mut() = Some(e);
            return;
        }
        if let Err(e) = metrics_w.flush() {
            // survive a kill mid-run -- flushed per sample so a partial
            // run's data isn't lost, but a flush failure itself is real
            *io_error.borrow_mut() = Some(e);
            return;
        }
        *sample_count.borrow_mut() += 1;
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
    if let Some(e) = io_error.into_inner() {
        anyhow::bail!("failed to persist metrics.jsonl mid-run: {e}");
    }

    let rss_growth_mib = report.rss_growth_bytes as f64 / (1024.0 * 1024.0);
    let no_crash = !report.panicked;
    // File/looping-in-memory replay has no ring buffer to overrun by
    // construction (matches soak.rs's own documented deviation for
    // file-replay sources) -- always satisfied here, not a live-hardware
    // measurement.
    let no_input_overrun = true;
    let no_unbounded_growth = soak_metrics_passed(&report);
    // MAN-19 review round 2: `on_sample` never firing at all (e.g.
    // `--sample-interval-secs` >= the requested duration) previously left
    // this hardcoded true against an empty metrics.jsonl.
    let metrics_visible = sample_count.into_inner() > 0;
    // MAN-19 review round 2: round 1's version only required *some*
    // events and *some* closes to exist, not that they came from the
    // SAME track -- one stable long-lived track can supply
    // `events_emitted`/`peak_active_tracks` while an unrelated flood of
    // never-promoted CANDIDATEs (which never touch `Validator`/
    // `RepetitionGate` at all -- see track.rs's `has_emitted` gate)
    // supplies `final_close_counts`, satisfying the old check with the
    // actual teardown path never exercised. `track_closed_events` is
    // exactly "how many tracks were promoted, emitted real output, AND
    // then closed" -- the only population that could have created
    // Validator/RepetitionGate per-track_id state, so it's the only
    // meaningful evidence the leak-motivated path ran at all.
    // MAN-19 review round 3: `track_closed_events > 0` alone still isn't
    // enough -- a track can promote, emit only TrackMeta/SpeedUpdate (no
    // CharDecoded/word ever reaching a repetition-gate candidate), and
    // close, satisfying that bar while RepetitionGate::record was never
    // called -- leaving forget_track's half of the leak fix untested.
    // gate_records_total is direct evidence the gate itself was touched.
    let workload_exercised = report.events_emitted > 0
        && report.peak_active_tracks > 0
        && report.track_closed_events > 0
        && report.gate_records_total > 0;
    // MAN-19 review round 1: a smoke invocation (e.g. `--duration-hours
    // 0.001` for local iteration) must not report the same "PASSED" this
    // harness uses for a genuine acceptance run -- and a run whose
    // *processing* ended well short of what was requested (source EOF,
    // an error, a panic) is not a completed run at all, regardless of
    // how long `--duration-hours` asked for. 1% tolerance for scheduling
    // jitter around the watchdog's 1s poll granularity.
    let requested_duration_reached =
        report.duration_actual.as_secs_f64() >= cli.duration_hours * 3600.0 * 0.99;
    let is_full_24h_run = cli.duration_hours >= MAN19_ACCEPT_HOURS;
    let overall_pass = no_crash
        && no_input_overrun
        && no_unbounded_growth
        && metrics_visible
        && workload_exercised
        && requested_duration_reached;
    // MAN-19 review round 2: this binary always replays a synthetic,
    // looped, in-memory scene through `LoopingAudioIqSource` -- it can
    // never be the live-SDR run ROADMAP.md's M2 gate literally specifies,
    // and `no_input_overrun` is `true` only because a file/loop replay
    // has no ring buffer to overrun *by construction*, not because
    // overrun was actually observed against real hardware. Naming this
    // "the MAN-19 acceptance gate" would overclaim what a healthy result
    // here actually proves. Per MAN-20's decision, a synthetic run is the
    // accepted INTERIM substitute while no real recording/hardware
    // exists -- so this is reported as exactly that: a synthetic-soak
    // result, never as literally satisfying the ROADMAP text.
    let synthetic_soak_passed = overall_pass && is_full_24h_run;

    let summary = serde_json::json!({
        "duration_requested_hours": cli.duration_hours,
        "duration_actual_secs": report.duration_actual.as_secs_f64(),
        "events_emitted": report.events_emitted,
        "spots_emitted": report.spots_emitted,
        "track_closed_events": report.track_closed_events,
        "gate_records_total": report.gate_records_total,
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
            "no_input_overrun_by_construction": no_input_overrun,
            "no_unbounded_memory_growth": no_unbounded_growth,
            "track_count_and_evictions_visible_in_metrics": metrics_visible,
            "workload_exercised": workload_exercised,
            "requested_duration_reached": requested_duration_reached,
        },
        "overall_pass": overall_pass,
        "is_full_24h_run": is_full_24h_run,
        "synthetic_soak_passed": synthetic_soak_passed,
        "note": "This is a synthetic, looped-scene proxy result (MAN-20's accepted interim substitute), not a literal live-SDR run of ROADMAP.md's M2 24h soak gate -- no_input_overrun holds only by construction (a file/loop replay has no ring buffer to overrun), not from real-hardware observation.",
    });
    std::fs::write(
        out_dir.join("report.json"),
        serde_json::to_string_pretty(&summary)?,
    )
    .context("writing report.json")?;

    println!("{}", serde_json::to_string_pretty(&summary)?);
    eprintln!(
        "MAN-19 synthetic soak {}: {}",
        if synthetic_soak_passed {
            "PASSED (synthetic proxy per MAN-20 -- not a live-SDR run)"
        } else if overall_pass {
            "OK (healthy, but not a full 24h run -- see is_full_24h_run)"
        } else {
            "FAILED"
        },
        out_dir.join("report.json").display()
    );

    if !overall_pass {
        std::process::exit(1);
    }
    Ok(())
}
