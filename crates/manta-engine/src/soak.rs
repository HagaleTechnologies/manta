//! Soak harness: run the listen pipeline for a fixed duration, asserting no
//! panic and bounded memory growth. ROADMAP M1 accept criterion; reused by
//! M2/M3's longer soaks (design doc §7).
//!
//! Deviation from the design doc: input-overrun tracking is NOT
//! implemented. coppa-audio's CpalSource doesn't expose its internal
//! ring's overflow_count() publicly, and file-replay sources (what this
//! harness runs against in CI) have no ring and cannot overrun by
//! construction. Live-hardware overrun observability needs a coppa-audio
//! API addition -- a real upstream ask, not made unilaterally here.

use crate::{listen, PipelineConfig};
use anyhow::Result;
use manta_input::IqSource;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Growth in peak RSS beyond this, after the warm-up window, fails the soak.
const RSS_GROWTH_LIMIT_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB
const WARMUP: Duration = Duration::from_secs(10);
/// How often the watchdog wakes to check the deadline. Short enough that it
/// also notices `listen` returning early (file EOF) promptly, rather than
/// holding `soak()` open until the requested deadline with nothing running.
const WATCHDOG_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct SoakReport {
    pub events_emitted: usize,
    pub rss_growth_bytes: u64,
    pub panicked: bool,
    /// How long `listen` was ACTUALLY running -- not the requested
    /// duration. A file source returns at EOF, so a one-minute fixture
    /// asked for a 24 h soak exercises the pipeline for one minute; a
    /// caller that reported the request instead would record that as a
    /// successful 24 h soak (MAN-130 remediation).
    pub ran_for: Duration,
}

// Windows has no libc::rusage/getrusage (POSIX-only) -- see this crate's
// Cargo.toml for why windows-sys is target-gated in as the equivalent.
#[cfg(unix)]
fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            raw // macOS reports ru_maxrss in bytes
        } else {
            raw * 1024 // Linux (and most others) report it in KB
        }
    }
}

#[cfg(windows)]
fn peak_rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let ok = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
        if ok != 0 {
            counters.PeakWorkingSetSize as u64
        } else {
            0
        }
    }
}

/// Run `listen` against `src` for `duration`, tracking panics and peak-RSS
/// growth. See module doc for the overrun-tracking deviation. Returns an
/// error (not a panic report) if `listen()` itself returns `Err` -- e.g. no
/// signal found during startup calibration.
pub fn soak(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    duration: Duration,
) -> Result<SoakReport> {
    // Validated before the watchdog thread is spawned -- otherwise an
    // invalid config only surfaces once `duration` elapses (`listen()`
    // itself already validates and returns fast, but the watchdog below
    // doesn't know that and sleeps for the full `duration` regardless;
    // MAN-29 review round 2).
    manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watchdog = stop.clone();
    let start = Instant::now();
    let baseline_rss = peak_rss_bytes();
    let mut worst_growth = 0u64;
    let mut event_count = 0usize;

    let finished = Arc::new(AtomicBool::new(false));
    let finished_watchdog = finished.clone();
    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration && !finished_watchdog.load(Ordering::Relaxed) {
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(WATCHDOG_POLL.min(remaining.max(Duration::from_millis(1))));
        }
        stop_watchdog.store(true, Ordering::Relaxed);
    });

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        listen(
            src,
            cfg,
            stop.clone(),
            |_ev| {
                event_count += 1;
                if start.elapsed() >= WARMUP {
                    let rss = peak_rss_bytes();
                    worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
                }
            },
            |_spot| {},
        )
    }));
    // Sampled BEFORE joining the watchdog: the watchdog's own wait is not
    // time the pipeline was being exercised.
    let ran_for = start.elapsed();
    finished.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    let panicked = match result {
        Ok(Ok(())) => false,
        Ok(Err(e)) => anyhow::bail!("listen() returned an error (not a panic): {e}"),
        Err(_) => true,
    };

    Ok(SoakReport {
        events_emitted: event_count,
        rss_growth_bytes: worst_growth,
        panicked,
        ran_for,
    })
}

/// Pass/fail per ROADMAP's M1 gate (panic, unbounded memory).
pub fn soak_passed(report: &SoakReport) -> bool {
    !report.panicked && report.rss_growth_bytes < RSS_GROWTH_LIMIT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_input::AudioIqSource;

    #[test]
    fn soak_reports_no_panic_on_a_clean_short_signal() {
        let fs = manta_input::TARGET_RATE_HZ;
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        let (env, _) =
            manta_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs as f64, 8.0)
                .unwrap();
        let mut real = vec![0.0f32; env.len()];
        let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
        let mut phi = 0.0f64;
        for (i, r) in real.iter_mut().enumerate() {
            *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
            phi += dphi;
        }
        let src: Box<dyn manta_input::IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap(),
        );
        let report = soak(src, &PipelineConfig::default(), Duration::from_secs(1)).unwrap();
        assert!(!report.panicked);
        assert!(soak_passed(&report));
    }

    /// MAN-130 remediation: a file source returns at EOF, so asking for a
    /// longer soak than the fixture can sustain exercises the pipeline only
    /// until EOF. `ran_for` must report that measured interval, never the
    /// request -- otherwise automation records a short fixture as a
    /// successful long soak. The requested 60 s here is deliberately far
    /// larger than both assertions' thresholds, so a regression back to
    /// reporting/waiting out the request fails rather than passing slowly.
    #[test]
    fn soak_reports_the_interval_listen_actually_ran_not_the_request() {
        let fs = manta_input::TARGET_RATE_HZ;
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        // The same fixture length as the test above: a shorter one hits EOF
        // during `listen`'s startup calibration, which fails the soak with
        // an error instead of running to EOF.
        let (env, _) =
            manta_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs as f64, 8.0)
                .unwrap();
        let mut real = vec![0.0f32; env.len()];
        let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
        let mut phi = 0.0f64;
        for (i, r) in real.iter_mut().enumerate() {
            *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
            phi += dphi;
        }
        let src: Box<dyn manta_input::IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap(),
        );
        let start = Instant::now();
        let report = soak(src, &PipelineConfig::default(), Duration::from_secs(60)).unwrap();
        assert!(
            report.ran_for < Duration::from_secs(45),
            "ran_for {:?} looks like the requested 60 s, not the measured run",
            report.ran_for
        );
        assert!(
            start.elapsed() < Duration::from_secs(50),
            "soak() waited out its watchdog after EOF ({:?}) instead of returning",
            start.elapsed()
        );
    }

    /// MAN-29 review round 2: an invalid `freq_correction_ppm` must fail
    /// `soak()` before it joins the watchdog thread -- otherwise the error
    /// doesn't surface until the full `duration` elapses (potentially 24h
    /// on a real hardware soak). `duration` here (20s) is deliberately
    /// larger than the assertion's threshold (5s), so a regression back to
    /// the duration-gated bug fails this test quickly rather than hanging
    /// for a real 24h soak's worth of wall-clock time.
    #[test]
    fn soak_rejects_an_invalid_freq_correction_ppm_before_the_watchdog_duration() {
        let fs = manta_input::TARGET_RATE_HZ;
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        let (env, _) =
            manta_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs as f64, 8.0)
                .unwrap();
        let real: Vec<f32> = env.to_vec();
        let src: Box<dyn manta_input::IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap(),
        );
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let start = Instant::now();
        let result = soak(src, &cfg, Duration::from_secs(20));
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "soak() with an invalid freq_correction_ppm took {:?} -- it must fail before \
             joining the watchdog, not wait out the requested duration",
            start.elapsed()
        );
    }
}
