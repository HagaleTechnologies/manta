//! Streaming pipeline: live/replayed audio -> PFB channelizer ->
//! `TrackManager` (SPEC §2), run continuously until Ctrl-C or EOF, emitting
//! the merged multi-track decode event stream as it's produced. No actor/
//! ring-thread split; see design doc §4.

use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::Validator;
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// MAN-4/D6: the operator may widen the detector's DC/Nyquist guard but
/// never narrow it below what the source's front end physically requires
/// (`configured_hz.max(source_hz)`). Also used by `soak_with_metrics`
/// (`soak_metrics.rs`), which duplicates this loop's construction rather
/// than calling `listen()` directly (see that module's doc comment).
pub(crate) fn effective_guard_hz(configured_hz: f64, source_hz: f64) -> f64 {
    configured_hz.max(source_hz)
}

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
    // Validated up front so a bad config value fails fast, before spending
    // CALIBRATION_SECONDS reading from a live device (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();

    let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
    let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
    let mut filled = 0;
    while filled < calib_n {
        let n = src.read(&mut calib[filled..])?;
        if n == 0 {
            anyhow::bail!("audio source ended during startup calibration");
        }
        filled += n;
    }
    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let mut detector = cfg.detector;
    detector.guard_hz = effective_guard_hz(detector.guard_hz, src.analytic_guard_hz());
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        detector,
        cfg.decode.clone(),
    );
    let mut validator = Validator::bundled(fs)
        .with_freq_correction_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?
        .with_blocklist(cfg.blocklist.clone())
        .with_notch(cfg.notch.clone());
    for call in &cfg.allowlist {
        validator.allowlist(call);
    }

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_track_meta(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    for ev in tm.process_hops(&ch.process(&calib), |m| m.saturating_sub(pad_hops) * hop) {
        on_event(&crate::calibrate_track_meta(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        for ev in tm.process_hops(&ch.process(&chunk[..n]), |m| {
            m.saturating_sub(pad_hops) * hop
        }) {
            on_event(&crate::calibrate_track_meta(&ev, calibration_factor));
            for spot in validator.ingest(&ev) {
                on_spot(&spot);
            }
        }
    }
    for ev in tm.finish() {
        on_event(&crate::calibrate_track_meta(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory IqSource for testing, reporting a fixed,
    /// caller-chosen `center_freq_hz` (unlike `AudioIqSource`, which always
    /// reports 0.0) -- this is what proves `listen()` actually reads
    /// `src.center_freq_hz()` instead of hardcoding 0.0.
    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FixedFreqSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    #[test]
    fn listen_raises_the_configured_guard_to_the_sources_floor() {
        // An operator config of 0.0 with an AudioIqSource must end up at
        // HILBERT_GUARD_HZ; an operator config of 1000.0 must stay at
        // 1000.0; a configured value below the source's floor is raised,
        // never lowered (MAN-4/D6).
        assert_eq!(effective_guard_hz(0.0, 300.0), 300.0);
        assert_eq!(effective_guard_hz(1000.0, 300.0), 1000.0);
        assert_eq!(effective_guard_hz(100.0, 300.0), 300.0);
    }

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        // A real V1-style golden signal (clean +20 dB tone), but fed through
        // a source that reports a nonzero center_freq_hz -- if listen() were
        // still hardcoding 0.0, every TrackMeta.freq_hz would come back as
        // just the +12.34 kHz baseband offset, not centered on 14 MHz.
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340 \
             (which is what a hardcoded center_freq_hz=0.0 would produce)",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-29: `PipelineConfig::freq_correction_ppm` reaches the emitted
    /// spot's `freq_hz`, corrected by the configured ppm -- end-to-end
    /// through `listen()`, not just the `manta-spot::Validator` unit.
    #[test]
    fn listen_applies_freq_correction_ppm_to_emitted_spot_freq_hz() {
        const PPM: f64 = 10.0; // ~140 Hz at 14 MHz.
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(src, &cfg, stop, |_ev| {}, |spot| spots.push(spot.clone())).unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
        for spot in &spots {
            let uncorrected = spot.freq_hz / factor;
            assert!(
                (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
                "spot.freq_hz {} divided back by the calibration factor should land near the \
                 raw decoded frequency {}, proving the correction was applied once, multiplicatively",
                spot.freq_hz,
                spec.center_freq_hz + 12_340.0
            );
        }
    }

    /// MAN-29 review round 3: the `TrackMeta` events `listen()` passes to
    /// `on_event` (consumed directly by `listen --json`) must be
    /// calibrated too, not just the emitted spots.
    #[test]
    fn listen_calibrates_track_meta_events_passed_to_on_event() {
        const PPM: f64 = 10.0;
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &cfg,
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        let uncorrected = freq_hz / factor;
        assert!(
            (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "on_event's TrackMeta.freq_hz {freq_hz} divided back by the calibration factor \
             should land near the raw decoded frequency {}",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-29 review: an invalid `freq_correction_ppm` must fail `listen()`
    /// up front rather than silently poisoning spot output.
    #[test]
    fn listen_rejects_an_invalid_freq_correction_ppm() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        assert!(listen(src, &cfg, stop, |_ev| {}, |_spot| {}).is_err());
    }

    /// MAN-31: `listen()` is the other production call site that must
    /// apply an operator-supplied suppression list.
    #[test]
    fn listen_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let mut spots = Vec::new();
        listen(
            src,
            &cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(
            spots.is_empty(),
            "blocklisted callsign must never be spotted, got {spots:?}"
        );
    }

    /// MAN-4's Gherkin, at the level where it can actually be measured.
    /// `listen()`'s callback API cannot reach `TrackManager` internals, so
    /// this drives `Channelizer` + `TrackManager` directly over
    /// `AudioIqSource`-produced IQ, which is exactly the pipeline
    /// `listen()` runs (including this same `effective_guard_hz` raise).
    #[test]
    fn a_clean_audio_tone_spawns_one_track_and_no_churn() {
        let fs = 48_000.0;
        let tone_hz = 750.0; // 750 Hz = 8 * 93.75 Hz -- an exact channel-8 center.
        let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
        let (env, _keyed_text) =
            manta_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();

        // MAN-4 Phase 4 contingency ladder branch 2: a strictly noiseless
        // fixture gives the 25th-percentile floor estimator nothing but the
        // f32 numerical floor to lock onto, so ordinary filter sidelobes
        // ~100 dB down still clear a +12 dB gate -- a fixture artifact, not
        // a filter defect (confirmed: without noise, spurious spawns were
        // spread broadly across channels 4..44 and 504, not clustered near
        // channel 8/504 the way a residual image leak would be). A
        // realistic noise floor at +20 dB SNR-in-2500, matching
        // `scene::render_scene`'s convention, fixes it.
        let amp = manta_testkit::noise::amplitude_for_snr_2500(20.0, fs);
        let mut real = vec![0.0f32; env.len()];
        let dphi = std::f64::consts::TAU * tone_hz / fs;
        let mut phi = 0.0f64;
        for (i, r) in real.iter_mut().enumerate() {
            *r = env.get(i).copied().unwrap_or(0.0) * amp * phi.cos() as f32;
            phi += dphi;
        }
        manta_testkit::noise::add_real_unit_awgn(&mut real, 4);

        let mut src = manta_input::AudioIqSource::new(Box::new(
            coppa_audio::WavSource::from_samples(real, 48_000),
        ))
        .unwrap();
        let all = manta_input::read_all(&mut src).unwrap();

        let mut ch = manta_dsp::channelizer::Channelizer::new(fs, 0.0).unwrap();
        let hop_samples = ch.hop() as u64;
        let detector = crate::track::DetectorConfig {
            guard_hz: effective_guard_hz(0.0, src.analytic_guard_hz()),
            ..crate::track::DetectorConfig::default()
        };
        let mut tm = crate::track::TrackManager::new(
            ch.n_channels(),
            fs,
            0.0,
            detector,
            manta_decode::decoder::DecodeConfig::default(),
        );
        for chunk in all.chunks(4096) {
            let hops = ch.process(chunk);
            tm.process_hops(&hops, |m| m * hop_samples);
        }
        tm.finish();

        let census = tm.spawns_by_channel();
        let spawned: Vec<(usize, u32)> = census
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(k, c)| (k, *c))
            .collect();
        // The tone is at 750 Hz = channel 8 exactly; +/-1 is the ownership window.
        assert!(
            spawned.iter().all(|(k, _)| (7..=9).contains(k)),
            "tracks spawned away from channel 8: {spawned:?}"
        );
        assert_eq!(
            tm.total_spawns(),
            1,
            "spurious respawns; census {spawned:?}"
        );
        assert_eq!(
            tm.close_counts().unconfirmed,
            0,
            "CANDIDATE churn (the 'churning track IDs' symptom): {:?}",
            tm.close_counts()
        );
    }
}
