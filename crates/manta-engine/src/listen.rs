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

/// One continuous decode segment: its own channelizer and track manager,
/// spanning the sample clock until the next discontinuity (MAN-73). The
/// `Validator` spans every segment of a `listen()` call; only the segment
/// itself (and its sample clock) restarts.
struct Segment {
    ch: manta_dsp::channelizer::Channelizer,
    tm: crate::track::TrackManager,
    hop: u64,
    pad_hops: u64,
}

fn new_segment(
    fs: f64,
    center_freq_hz: f64,
    cfg: &PipelineConfig,
    next_id: u32,
) -> Result<Segment> {
    let ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    tm.resume_track_ids_from(next_id);
    Ok(Segment {
        ch,
        tm,
        hop,
        pad_hops,
    })
}

fn emit(
    events: Vec<DecoderEvent>,
    validator: &mut Validator,
    calibration_factor: f64,
    on_event: &mut impl FnMut(&DecoderEvent),
    on_spot: &mut impl FnMut(&crate::Spot),
) {
    for ev in events {
        on_event(&crate::calibrate_track_meta(&ev, calibration_factor));
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
}

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
///
/// MAN-73: a live source may report a discontinuity via
/// `IqSource::take_discontinuity()` (e.g. after a reconnect). When it does,
/// the current segment (channelizer + track manager) is closed -- every
/// open track gets its `TrackClosed` -- and a fresh segment starts, with
/// its sample clock advanced by the reported gap and its track ids
/// continuing from the closed segment's. This keeps spot timestamps
/// wall-clock-true across the outage without splicing pre-outage audio
/// onto post-outage audio or zero-filling the gap (zero-fill pins the
/// noise floor and floods false tracks on resume -- see
/// `IqSource::take_discontinuity`'s doc comment). File replay never
/// reports a discontinuity, so this is a no-op there.
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

    let mut validator = Validator::bundled(fs)
        .with_freq_correction_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?
        .with_blocklist(cfg.blocklist.clone())
        .with_notch(cfg.notch.clone());
    for call in &cfg.allowlist {
        validator.allowlist(call);
    }

    let mut seg = new_segment(fs, center_freq_hz, cfg, 1)?;
    let mut seg_base: u64 = 0;

    let padding = vec![Complex32::new(0.0, 0.0); seg.ch.filter_len()];
    let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );

    let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
    let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
    let mut filled = 0;
    while filled < calib_n {
        let n = src.read(&mut calib[filled..])?;
        if n == 0 {
            anyhow::bail!("audio source ended during startup calibration");
        }
        if let Some(gap) = src.take_discontinuity() {
            // A discontinuity during calibration: the pre-gap partial
            // buffer was never processed through the channelizer, so it's
            // simply discarded (not spliced) -- keep the post-gap samples
            // just read, advance the sample clock by the discarded samples
            // plus the gap, and keep filling from there.
            calib.copy_within(filled..filled + n, 0);
            seg_base += filled as u64 + gap;
            filled = n;
            continue;
        }
        filled += n;
    }
    let events = seg.tm.process_hops(&seg.ch.process(&calib), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );
    let mut seg_consumed: u64 = calib_n as u64;

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(gap) = src.take_discontinuity() {
            let finish_events = seg.tm.finish();
            emit(
                finish_events,
                &mut validator,
                calibration_factor,
                &mut on_event,
                &mut on_spot,
            );
            let next_id = seg.tm.next_track_id();
            seg_base += seg_consumed + gap;
            seg_consumed = 0;
            seg = new_segment(fs, center_freq_hz, cfg, next_id)?;
            let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
                seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
            });
            emit(
                events,
                &mut validator,
                calibration_factor,
                &mut on_event,
                &mut on_spot,
            );
        }
        let events = seg.tm.process_hops(&seg.ch.process(&chunk[..n]), |m| {
            seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
        });
        emit(
            events,
            &mut validator,
            calibration_factor,
            &mut on_event,
            &mut on_spot,
        );
        seg_consumed += n as u64;
    }
    let events = seg.tm.finish();
    emit(
        events,
        &mut validator,
        calibration_factor,
        &mut on_event,
        &mut on_spot,
    );
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

    /// MAN-73 test double: serves `samples` on a loop. Once the cumulative
    /// number of samples served reaches `arm_at` (default: the full
    /// vector length, i.e. "after exhaustion"), the *next* `read()` call
    /// rewinds to the start of `samples` and arms a one-shot
    /// `take_discontinuity()` of `gap` samples -- simulating a
    /// `ReconnectingSource` that lost the connection, waited out `gap`
    /// worth of wall-clock time, and resumed producing real samples.
    /// `chunk_cap` bounds how many samples a single `read()` call serves,
    /// so a caller reading into a buffer much larger than `chunk_cap`
    /// (e.g. the startup calibration buffer) still sees the gap arm
    /// partway through, not only at a whole-buffer boundary.
    struct GapSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        gap: u64,
        arm_at: usize,
        chunk_cap: usize,
        served: usize,
        armed: bool,
        pending_gap: Option<u64>,
    }

    impl GapSource {
        fn new(samples: Vec<Complex32>, fs: f64, center_freq_hz: f64, gap_samples: u64) -> Self {
            let arm_at = samples.len();
            GapSource {
                chunk_cap: samples.len().max(1),
                samples,
                cursor: 0,
                fs,
                center_freq_hz,
                gap: gap_samples,
                arm_at,
                served: 0,
                armed: false,
                pending_gap: None,
            }
        }

        /// Arm the gap after `k` cumulative samples served instead of at
        /// exhaustion -- used to simulate a discontinuity arriving
        /// partway through startup calibration.
        fn arm_after_samples(mut self, k: usize) -> Self {
            self.arm_at = k;
            self.chunk_cap = (k / 4).max(64);
            self
        }
    }

    impl manta_input::IqSource for GapSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if !self.armed && self.served >= self.arm_at {
                self.cursor = 0;
                self.armed = true;
                self.pending_gap = Some(self.gap);
            }
            let avail = self.samples.len() - self.cursor;
            let n = buf.len().min(avail).min(self.chunk_cap);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            self.served += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.pending_gap.take()
        }
    }

    /// MAN-73: a discontinuity reported mid-stream closes the current
    /// segment (every open track gets `TrackClosed`), starts a fresh one
    /// whose sample clock is advanced by the gap, and never re-reads any
    /// pre-gap data -- so no false spot is produced by splicing pre- and
    /// post-gap audio together, track ids are never reused, and spot
    /// timestamps land on the correct (post-gap) side of the clock.
    #[test]
    fn listen_restarts_the_segment_on_a_discontinuity_and_advances_sample_ts() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let len = rendered.samples.len() as u64;
        let gap = (spec.fs as u64) * 3600; // 1 hour
        let src: Box<dyn manta_input::IqSource> = Box::new(GapSource::new(
            rendered.samples.clone(),
            spec.fs,
            spec.center_freq_hz,
            gap,
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut track_meta_ids: Vec<(u64, u32)> = Vec::new(); // (approx order, track_id)
        let mut closed_before_first_post_gap_char = true;
        let mut seen_post_gap_char = false;
        let mut closed_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut order = 0u64;
        manta_engine_listen_with_discontinuity_probe(src, &stop, &mut spots, |ev| {
            order += 1;
            match ev {
                DecoderEvent::TrackMeta { track_id, .. } => {
                    track_meta_ids.push((order, *track_id));
                }
                DecoderEvent::TrackClosed { track_id } => {
                    closed_ids.insert(*track_id);
                }
                DecoderEvent::CharDecoded { track_id, .. } => {
                    if *track_id > 1 && !closed_ids.contains(&1) {
                        // A post-gap track emitted a char before the
                        // pre-gap track (id 1) was ever closed.
                        closed_before_first_post_gap_char = false;
                    }
                    if *track_id > 1 {
                        seen_post_gap_char = true;
                    }
                }
                _ => {}
            }
        })
        .unwrap();
        let _ = stop;

        assert!(!spots.is_empty(), "expected at least one spot");
        assert!(
            spots.iter().all(|s: &crate::Spot| s.callsign == "W1AW"),
            "every spot must be W1AW (no false spot from splicing pre/post-gap audio): {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts < len),
            "expected at least one pre-gap spot, got {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts >= len + gap),
            "expected at least one post-gap spot with sample_ts >= {}, got {spots:?}",
            len + gap
        );

        let pre_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .filter(|(_, id)| *id == 1)
            .map(|(_, id)| *id)
            .collect();
        let post_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .map(|(_, id)| *id)
            .filter(|id| !pre_gap_ids.contains(id))
            .collect();
        assert!(
            !post_gap_ids.is_empty(),
            "expected at least one post-gap track id, got {track_meta_ids:?}"
        );
        assert!(
            post_gap_ids.iter().all(|id| pre_gap_ids.iter().all(|p| id > p)),
            "every post-gap track id must be greater than every pre-gap id: pre={pre_gap_ids:?} post={post_gap_ids:?}"
        );
        assert!(seen_post_gap_char, "expected a post-gap CharDecoded event");
        assert!(
            closed_before_first_post_gap_char,
            "the pre-gap track's TrackClosed must arrive before any post-gap CharDecoded"
        );
    }

    /// Thin wrapper around `listen()` used only by the discontinuity tests
    /// above/below, so the on_event closure can observe every event
    /// (including TrackClosed) while still routing spots to a Vec the way
    /// the other tests in this module do.
    fn manta_engine_listen_with_discontinuity_probe(
        src: Box<dyn manta_input::IqSource>,
        stop: &Arc<AtomicBool>,
        spots: &mut Vec<crate::Spot>,
        mut on_event: impl FnMut(&DecoderEvent),
    ) -> Result<()> {
        listen(
            src,
            &PipelineConfig::default(),
            stop.clone(),
            |ev| on_event(ev),
            |spot| spots.push(spot.clone()),
        )
    }

    /// MAN-73: a discontinuity reported *during* startup calibration
    /// discards the pre-gap partial calibration buffer (it's never
    /// processed through the channelizer) rather than splicing it onto
    /// post-gap data, and still advances the sample clock correctly.
    #[test]
    fn listen_handles_a_discontinuity_during_startup_calibration() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gap = (spec.fs as u64) * 5; // 5 s
        let arm_after = (spec.fs * 0.5).round() as usize; // 0.5 s into the 2 s calibration fill
        let src: Box<dyn manta_input::IqSource> = Box::new(
            GapSource::new(rendered.samples.clone(), spec.fs, spec.center_freq_hz, gap)
                .arm_after_samples(arm_after),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "expected at least one spot");
        let min_expected_ts = arm_after as u64 + gap;
        assert!(
            spots.iter().all(|s| s.sample_ts >= min_expected_ts),
            "every spot's sample_ts must be >= {min_expected_ts} (discarded pre-gap calibration \
             samples + the gap), got {spots:?}"
        );
    }

    /// MAN-73: a source that never reports a discontinuity (every source
    /// today, and file replay always) must decode identically across runs
    /// -- this is the regression the golden/determinism suites already
    /// pin globally; this test documents the contract locally for this
    /// module.
    #[test]
    fn listen_without_discontinuity_is_unchanged() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();

        let run = || {
            let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut spots = Vec::new();
            listen(
                src,
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                |_ev| {},
                |spot| spots.push((spot.callsign.clone(), spot.sample_ts, spot.track_id)),
            )
            .unwrap();
            spots
        };

        assert_eq!(run(), run(), "identical input must decode identically");
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
}
