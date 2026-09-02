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
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SoakReport {
    pub events_emitted: usize,
    pub rss_growth_bytes: u64,
    pub panicked: bool,
}

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

    let watchdog = std::thread::spawn(move || {
        while start.elapsed() < duration {
            let remaining = duration.saturating_sub(start.elapsed());
            std::thread::sleep(SAMPLE_INTERVAL.min(remaining.max(Duration::from_millis(1))));
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
