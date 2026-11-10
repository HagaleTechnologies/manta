//! Live/replayed real-audio IQ source: coppa-audio AudioSource -> Hilbert
//! transformer -> Complex32, matching IqSource. ARCHITECTURE §3, design
//! doc §2.
//!
//! M1 scope: no automatic resampling. Sources must already run at exactly
//! TARGET_RATE_HZ (48000 Hz) natively; a rate mismatch is a hard error, not
//! a resample attempt (coppa-audio's ResamplingSource is unreachable -- no
//! `rubato` dependency and no `mod resampler;` declaration upstream).
//!
//! An RF reference is optional but supported: `with_center_freq_hz` accepts
//! an operator-supplied dial frequency (MAN-34), superseding design doc
//! §2's original "audio has no RF reference" scope decision. Without it,
//! `center_freq_hz()` still reports `0.0` and downstream frequencies are
//! bare baseband offsets.
//!
//! Live devices (`from_device`) sit behind a non-blocking callback ring
//! (MAN-530: manta's own capture-index-tagged ring in `audio_capture`),
//! where an empty read means "nothing captured yet", not end of
//! stream. `read` therefore waits for samples, and fails after
//! `DEVICE_STALL_TIMEOUT` with an error naming the device, the rate and
//! the likely cause, so the `IqSource` contract `0 = EOF` holds for every
//! source (MAN-231, MAN-131). File-backed sources keep returning 0 at end
//! of file.
//!
//! MAN-530: when the reader falls more than the ring's ~170 ms behind, the
//! callback drops the newest frames. Each dropped frame is counted in
//! `health_counters()` (`queue_dropped_samples`, one gap per contiguous
//! loss episode) the moment it is dropped, and the hole is reported
//! through the `IqSource` gap hooks on the first read that starts after
//! it: a hole shorter than `AUDIO_DISCONTINUITY_FRAMES` as
//! `take_missing_samples` plus `take_unknown_gap`, a longer one as
//! `take_discontinuity`. A read never returns frames from both sides of a
//! hole. The Hilbert state is kept across a short hole (its filter
//! transient is the same tolerance MAN-202 accepts for unknown gaps) and
//! restarted at a long one, which begins a fresh segment.

use crate::audio_capture::{CaptureRing, CAPTURE_RING_FRAMES};
use crate::devices::quoted_name;
<<<<<<< HEAD
use crate::overflow::OVERFLOW_WARN_INTERVAL;
use crate::stall::{StallEpisode, SOFT_STALL_AFTER};
use crate::{InputHealthCounters, IqSource};
=======
use crate::stall::{NoUsableSamples, StallEpisode, SOFT_STALL_AFTER};
use crate::IqSource;
>>>>>>> d0f42f14c270b7168d60521b4e2d04c6503cde81
use anyhow::{anyhow, Context, Result};
use coppa_audio::AudioSource;
use manta_dsp::hilbert::HilbertTransformer;
use num_complex::Complex32;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fixed target sample rate for M1 audio decode: 48000 / 93.75 = 512 (a
/// power of two), the constraint SingleChannelExtractor::new requires.
pub const TARGET_RATE_HZ: u32 = 48_000;

/// The rig-audio passband this source actually delivers, as offsets in Hz
/// ABOVE the dial frequency (`--dial-freq-hz`).
///
/// MAN-86 review: these are what `SKIMMER/SETT` advertises to Aggregator as
/// decodable coverage, and they are deliberately NOT derived from
/// `TARGET_RATE_HZ`. Two independent reasons: `read` Hilbert-transforms
/// real audio into an analytic signal, so all of its energy sits at
/// POSITIVE offsets from the dial -- the negative half of a
/// `+/- TARGET_RATE_HZ / 2` claim is on the wrong side of the dial
/// entirely; and a receiver's AF output is the ~3 kHz passband its IF
/// filter passes (ARCHITECTURE §3, "Degenerate ~3 kHz wideband mode"), not
/// the 24 kHz its sound card happens to sample.
///
/// The values are the nominal SSB/CW-wide AF passband a rig in its widest
/// setting delivers. A rig running a narrow CW filter passes less than
/// this, so the claim is a superset for that operator; making it exact
/// needs a config key for the rig's actual filter, which is out of MAN-86's
/// scope and is why these are named constants rather than literals.
pub const AUDIO_PASSBAND_LO_HZ: f64 = 300.0;
/// Upper edge of the rig-audio passband -- see `AUDIO_PASSBAND_LO_HZ`.
pub const AUDIO_PASSBAND_HI_HZ: f64 = 3_000.0;

/// How long a live device's `read` waits for a sample before failing
/// (MAN-231). Below the ~10 s network-source bound (hpsdr.rs/kiwi.rs):
/// local audio callbacks arrive every ~10 ms, and an operator whose device
/// delivers nothing should hear so quickly. A read in flight is not
/// interruptible, so this also bounds Ctrl-C latency during a stall; on a
/// delivering device each read returns within one callback period.
pub const DEVICE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Sleep between polls of an empty capture ring (matches `manta check`).
/// The poll only paces an empty ring: the ring holds 8192 samples, ~170 ms
/// at 48 kHz, and overflows whenever the decode thread itself falls that
/// far behind -- counted and reported as a gap since MAN-530.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Exact-zero samples from open before the digital-silence notice fires:
/// two seconds, the length of `listen`'s startup window.
const SILENCE_NOTICE_SAMPLES: u64 = 2 * TARGET_RATE_HZ as u64;

/// Called at most once per opened device, with its quoted name. See
/// `AudioIqSource::with_silence_notice`.
pub type SilenceNotice = Box<dyn FnOnce(&str) + Send>;

/// MAN-530: called with the device's quoted name when the capture ring has
/// dropped frames, at most once per `OVERFLOW_WARN_INTERVAL` per opened
/// source. See `AudioIqSource::with_overrun_notice`.
pub type OverrunNotice = Box<dyn Fn(&str) + Send + Sync>;

/// MAN-530: a capture hole at least this long (0.5 s at 48 kHz) is reported
/// as `take_discontinuity`, a fresh segment; a shorter one as
/// `take_missing_samples` plus `take_unknown_gap`, a clock advance and a
/// continuity reset. The half-second line HPSDR protocol 2 draws too.
pub const AUDIO_DISCONTINUITY_FRAMES: u64 = TARGET_RATE_HZ as u64 / 2;

/// What a live audio input most likely needs checked when it fails to open
/// or delivers nothing (MAN-131). macOS denies microphone access silently
/// -- the stream opens, then no callbacks arrive or every buffer is zero --
/// so permission is the likely cause there.
pub fn audio_input_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "on macOS this is usually microphone permission: allow the app \
         running manta in System Settings > Privacy & Security > Microphone, \
         quit and reopen that app, then confirm with `manta check`"
    } else {
        "check that the input is connected, unmuted and not held by another \
         program, then confirm with `manta check`"
    }
}

/// `--device NAME` matched no input.
fn no_matching_input(requested: &str) -> anyhow::Error {
    anyhow!(
        "no audio input matching {} (manta needs a {TARGET_RATE_HZ} Hz input) -- \
         run `manta devices` to list the inputs --device can select",
        quoted_name(requested)
    )
}

/// No `--device` and the host has no default input.
fn no_default_input() -> anyhow::Error {
    anyhow!(
        "no default audio input (manta needs a {TARGET_RATE_HZ} Hz input) -- \
         run `manta devices` to list the inputs --device can select"
    )
}

/// The device exists but its config query, stream build or start failed.
fn did_not_open(label: &str, cause: &anyhow::Error) -> anyhow::Error {
    anyhow!(
        "audio input {label} did not open at {TARGET_RATE_HZ} Hz: {cause:#} -- {}",
        audio_input_hint()
    )
}

/// The device's native rate is not `TARGET_RATE_HZ`; M1 does not resample.
fn wrong_rate(label: &str, native: u32) -> anyhow::Error {
    let where_to_set = if cfg!(target_os = "macos") {
        "the system's audio settings (Audio MIDI Setup on macOS)"
    } else {
        "the system's audio settings"
    };
    anyhow!(
        "audio input {label} runs at {native} Hz, but manta needs {TARGET_RATE_HZ} Hz \
         and does not resample -- set that input to {TARGET_RATE_HZ} Hz in \
         {where_to_set}, or choose another with --device"
    )
}

/// A live device delivered nothing for `waited`. Typed as
/// [`NoUsableSamples`] (MAN-533 D4) so the reconnect layer reports it as
/// `no usable samples`; the text is unchanged.
fn no_samples(label: &str, waited: Duration) -> anyhow::Error {
    NoUsableSamples::new(format!(
        "audio input {label} delivered no samples in {} s at {TARGET_RATE_HZ} Hz -- {}",
        waited.as_secs_f64(),
        audio_input_hint()
    ))
    .into()
}

/// A capture device behind a non-blocking callback ring (MAN-530's tagged
/// `audio_capture` ring): `read` must wait for samples, because an empty
/// ring is not end of stream.
struct LiveDevice {
    /// The device's name as `manta devices` spells it (quoted).
    label: String,
    /// Where samples come from: the tagged capture ring in production.
    ring: Box<dyn CaptureRing>,
    /// Keeps the cpal stream's owner thread alive; `None` in tests.
    _stream: Option<crate::audio_capture::CaptureStream>,
    /// MAN-530: capture-ring loss totals, shared with the callback.
    health: Option<Arc<InputHealthCounters>>,
    /// MAN-530: per-read gap reports for the hole before the last read.
    gap: PendingGap,
    /// MAN-530: rate-limited operator notice on capture-ring loss.
    overrun_notice: Option<OverrunNotice>,
    overrun_watch: OverrunWatch,
    stall_timeout: Duration,
    /// Armed until it fires or a non-zero sample arrives.
    silence_notice: Option<SilenceNotice>,
    /// Exact-zero samples read since open, while the notice is armed.
    zero_samples: u64,
    /// MAN-489: the one-shot soft-stall episode behind `take_stalled`.
    /// `stall_timeout` stays the hard device-loss bound.
    stall: StallEpisode,
    /// MAN-489: when the current empty-ring wait began, carried across a
    /// soft report's return so the hard bound cannot restart mid-outage.
    /// Cleared by delivery.
    waiting_since: Option<Instant>,
    /// MAN-489: time of the last delivered samples (the soft clock).
    last_delivery: Instant,
}

/// MAN-530: the gap reports one read left pending, each drained once.
#[derive(Debug, Default)]
struct PendingGap {
    discontinuity: Option<u64>,
    missing: Option<u64>,
    unknown: bool,
    /// The current read starts a fresh segment: restart the Hilbert
    /// filter so no pre-hole audio leaks into it. Drained by `read`.
    restart_filter: bool,
}

impl PendingGap {
    /// Record a hole of `missing` frames immediately before the samples
    /// the current read returns. Below the threshold: known count plus an
    /// unknown-gap continuity reset (MAN-202 decision 7); at or above it: a
    /// discontinuity only. A hole an earlier read left untaken is added in,
    /// so a caller that skips a take never loses clock time.
    fn note_hole(&mut self, missing: u64) {
        if missing == 0 {
            return;
        }
        let total = missing
            .saturating_add(self.missing.take().unwrap_or(0))
            .saturating_add(self.discontinuity.take().unwrap_or(0));
        if total >= AUDIO_DISCONTINUITY_FRAMES {
            self.discontinuity = Some(total);
            self.unknown = false;
            self.restart_filter = true;
        } else {
            self.missing = Some(total);
            self.unknown = true;
        }
    }
}

/// MAN-530: decides when the overrun notice fires. Separate from gap
/// delivery: counters can grow while a read still returns pre-hole audio.
#[derive(Debug, Default)]
struct OverrunWatch {
    /// `queue_dropped_samples` when last observed.
    seen: u64,
    last_warn: Option<Instant>,
}

impl OverrunWatch {
    /// `true` when the dropped total grew since the last observation and
    /// the once-per-`OVERFLOW_WARN_INTERVAL` gate is open.
    fn due(&mut self, dropped: u64, now: Instant) -> bool {
        if dropped <= self.seen {
            return false;
        }
        self.seen = dropped;
        let open = self
            .last_warn
            .is_none_or(|last| now.duration_since(last) >= OVERFLOW_WARN_INTERVAL);
        if open {
            self.last_warn = Some(now);
        }
        open
    }
}

impl LiveDevice {
    /// MAN-530: fire the overrun notice if the ring dropped frames since
    /// the last look and the rate limit allows.
    fn watch_for_overrun(&mut self) {
        let (Some(health), Some(notice)) = (&self.health, &self.overrun_notice) else {
            return;
        };
        if self
            .overrun_watch
            .due(health.queue_dropped_samples(), Instant::now())
        {
            notice(&self.label);
        }
    }

    /// Fire the silence notice once `SILENCE_NOTICE_SAMPLES` exact zeros
    /// have arrived since open; disarm it on the first non-zero sample.
    fn watch_for_silence(&mut self, samples: &[f32]) {
        if self.silence_notice.is_none() {
            return;
        }
        if samples.iter().any(|&s| s != 0.0) {
            self.silence_notice = None;
            return;
        }
        self.zero_samples += samples.len() as u64;
        if self.zero_samples >= SILENCE_NOTICE_SAMPLES {
            if let Some(notice) = self.silence_notice.take() {
                notice(&self.label);
            }
        }
    }
}

/// A real audio source (device or file) converted to analytic Complex32,
/// implementing IqSource. ARCHITECTURE §3 "Audio passband" input.
pub struct AudioIqSource {
    /// The file-backed source; `None` for a live device, which reads
    /// `live.ring` instead.
    src: Option<Box<dyn AudioSource>>,
    hilbert: HilbertTransformer,
    /// Operator-supplied RF dial frequency this audio passband sits on, Hz.
    /// `0.0` = no RF reference supplied: reported frequencies are baseband
    /// offsets. See `with_center_freq_hz` (MAN-34).
    center_freq_hz: f64,
    /// `Some` only for `from_device`; file sources keep `0 = EOF`.
    live: Option<LiveDevice>,
    /// MAN-259: `from_device`'s runtime up to `cpal`'s `start`; `ZERO`
    /// for a file. Surfaced by `acquisition_start_offset()`.
    acquisition_start_offset: Duration,
}

impl AudioIqSource {
    /// Wrap an already-started AudioSource at TARGET_RATE_HZ.
    pub fn new(src: Box<dyn AudioSource>) -> Result<Self> {
        if src.sample_rate() != TARGET_RATE_HZ {
            return Err(anyhow!(
                "AudioIqSource requires {TARGET_RATE_HZ} Hz, got {}",
                src.sample_rate()
            ));
        }
        Ok(AudioIqSource {
            src: Some(src),
            hilbert: HilbertTransformer::new(),
            center_freq_hz: 0.0,
            live: None,
            acquisition_start_offset: Duration::ZERO,
        })
    }

    /// Wrap a started live device: `read` waits up to `stall_timeout` for
    /// samples instead of reporting an empty ring as end of stream.
    /// `acquisition_start_offset` is how long the open took before the
    /// device was started (MAN-259).
    fn live(
        ring: Box<dyn CaptureRing>,
        label: String,
        stall_timeout: Duration,
        acquisition_start_offset: Duration,
        health: Option<Arc<InputHealthCounters>>,
    ) -> Self {
        AudioIqSource {
            src: None,
            hilbert: HilbertTransformer::new(),
            center_freq_hz: 0.0,
            acquisition_start_offset,
            live: Some(LiveDevice {
                label,
                ring,
                _stream: None,
                health,
                gap: PendingGap::default(),
                overrun_notice: None,
                overrun_watch: OverrunWatch::default(),
                stall_timeout,
                silence_notice: None,
                zero_samples: 0,
                stall: StallEpisode::new(SOFT_STALL_AFTER),
                waiting_since: None,
                last_delivery: Instant::now(),
            }),
        }
    }

    /// Call `notice` once, with the device's quoted name, if the first two
    /// seconds a live device delivers are all exact zeros -- on macOS the
    /// usual sign of a denied microphone permission (MAN-131). A no-op for
    /// file-backed sources: silent WAV fixtures are routine.
    pub fn with_silence_notice(mut self, notice: SilenceNotice) -> Self {
        if let Some(live) = &mut self.live {
            live.silence_notice = Some(notice);
        }
        self
    }

    /// MAN-530: call `notice`, with the device's safely quoted name, when
    /// the capture ring has dropped frames because the decode thread fell
    /// behind: on the first observed loss, then at most once per 60 s. It
    /// runs on the reading thread, possibly while that read still returns
    /// audio from before the hole. A no-op for file-backed sources.
    pub fn with_overrun_notice(mut self, notice: OverrunNotice) -> Self {
        if let Some(live) = &mut self.live {
            live.overrun_notice = Some(notice);
        }
        self
    }

    /// Attach the operator-supplied RF dial frequency the rig is tuned to
    /// (MAN-34). An audio passband carries no RF reference of its own, so
    /// without this a track's reported frequency is a bare audio-tone
    /// offset (e.g. 700 Hz) rather than an absolute RBN frequency. This is
    /// the manual equivalent of entering the dial frequency once -- manta
    /// does not poll the rig (no CAT; README non-goals).
    ///
    /// Sideband convention: `center_freq_hz()` is added to the decoded
    /// audio-tone offset as-is, so pass the suppressed-carrier/USB dial
    /// reading. On a CW-mode dial display, subtract your sidetone pitch
    /// first, or the reported frequency reads high by the pitch amount.
    ///
    /// Rejects non-finite and non-positive values, matching the CLI's
    /// `--dial-freq-hz` parser: "no reference" is expressed by not calling
    /// this, not by passing 0.0.
    pub fn with_center_freq_hz(mut self, center_freq_hz: f64) -> Result<Self> {
        if !center_freq_hz.is_finite() || center_freq_hz <= 0.0 {
            return Err(anyhow!(
                "center frequency must be a finite, positive number of Hz, \
                 got {center_freq_hz}"
            ));
        }
        self.center_freq_hz = center_freq_hz;
        Ok(self)
    }

    /// Open the named input device (default device if `None`). Requires the
    /// device's native rate to be exactly TARGET_RATE_HZ (48000) -- M1 does
    /// not resample, and the rate is checked before the stream starts.
    ///
    /// Every failure names the device (as `manta devices` spells it, or
    /// what was requested), states the 48000 Hz requirement and says what
    /// to check next (MAN-131). The returned source is live: `read` waits
    /// for samples and fails after `DEVICE_STALL_TIMEOUT` of none (MAN-231).
    pub fn from_device(name: Option<&str>) -> Result<Self> {
        let entered = Instant::now();
        let health = Arc::new(InputHealthCounters::new());
        let name = name.map(str::to_owned);
        // MAN-530: device lookup, stream build/play and stream teardown all
        // run on the capture owner thread; only the name, the health Arc,
        // the ring consumer and the startup result cross threads.
        let opened = crate::audio_capture::spawn_owner(
            CAPTURE_RING_FRAMES,
            Arc::clone(&health),
            entered,
            move |producer, entered| open_capture_stream(name.as_deref(), producer, entered),
        )?;
        let mut source = AudioIqSource::live(
            Box::new(opened.consumer),
            opened.label,
            DEVICE_STALL_TIMEOUT,
            opened.acquisition_start_offset,
            Some(health),
        );
        if let Some(live) = &mut source.live {
            live._stream = Some(opened.stream);
        }
        Ok(source)
    }

    /// Open a WAV file, replayed as an audio source (soak harness / `listen
    /// --source`). Requires the file's rate to be exactly TARGET_RATE_HZ.
    pub fn from_wav_file(path: &Path) -> Result<Self> {
        let wav_src = coppa_audio::WavSource::open(path)?;
        AudioIqSource::new(Box::new(wav_src))
    }
}

/// MAN-530: runs on the capture owner thread. Find the input, check its
/// rate, then build and play a 48 kHz mono f32 stream whose callback feeds
/// `producer`. Every failure names the device, states the 48000 Hz
/// requirement and says what to check next (MAN-131). Returns the playing
/// stream (destroyed later on this same thread), the device label and the
/// open time up to `play` (MAN-259).
fn open_capture_stream(
    name: Option<&str>,
    mut producer: crate::audio_capture::CaptureProducer,
    entered: Instant,
) -> Result<(cpal::Stream, String, Duration)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let device = match name {
        Some(n) => coppa_audio::find_input_device_by_name(n).ok_or_else(|| no_matching_input(n))?,
        None => cpal::default_host()
            .default_input_device()
            .ok_or_else(no_default_input)?,
    };
    let label = match device.description() {
        Ok(description) => quoted_name(description.name()),
        Err(_) => match name {
            Some(n) => format!("matching {}", quoted_name(n)),
            None => "(system default)".to_string(),
        },
    };
    let native_rate = device
        .default_input_config()
        .context("query its default input config")
        .map_err(|e| did_not_open(&label, &e))?
        .sample_rate();
    if native_rate != TARGET_RATE_HZ {
        return Err(wrong_rate(&label, native_rate));
    }
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: native_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| producer.write(data),
            |err| eprintln!("CPAL input error: {err}"),
            None,
        )
        .map_err(|e| anyhow!("Failed to build input stream: {e}"))
        .map_err(|e| did_not_open(&label, &e))?;
    // MAN-259: the capture ring fills from `play` on.
    let acquisition_start_offset = entered.elapsed();
    stream
        .play()
        .map_err(|e| anyhow!("Failed to start input stream: {e}"))
        .map_err(|e| did_not_open(&label, &e))?;
    Ok((stream, label, acquisition_start_offset))
}

impl IqSource for AudioIqSource {
    fn sample_rate(&self) -> f64 {
        TARGET_RATE_HZ as f64
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz // 0.0 when no dial frequency was supplied -- MAN-34.
    }

    /// The rig's AF passband, ABOVE the dial frequency only -- never
    /// `+/- TARGET_RATE_HZ / 2`. See `AUDIO_PASSBAND_LO_HZ`.
    fn rf_passband_hz(&self) -> (f64, f64) {
        (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
    }

    fn acquisition_start_offset(&self) -> Duration {
        self.acquisition_start_offset
    }

    /// MAN-273: the Hilbert front end's DC/Nyquist guard at this source's
    /// pinned rate, the band its image-rejection contract excludes.
    fn analytic_guard_hz(&self) -> f64 {
        manta_dsp::hilbert::hilbert_guard_hz(TARGET_RATE_HZ as f64)
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut real = vec![0.0f32; buf.len()];
        let got = self.read_real(&mut real)?;
        if got == 0 {
            return Ok(0);
        }
        if let Some(live) = &mut self.live {
            // MAN-530: a long hole starts a fresh segment; its first
            // analytic samples must not mix in audio from before it. A
            // short hole keeps the filter (MAN-202's transient tolerance).
            if std::mem::take(&mut live.gap.restart_filter) {
                self.hilbert = HilbertTransformer::new();
            }
            live.watch_for_silence(&real[..got]);
        }
        let analytic = self.hilbert.process(&real[..got]);
        buf[..got].copy_from_slice(&analytic);
        Ok(got)
    }

    /// MAN-489: consumed once per sample outage; see the private
    /// `StallEpisode` episode state.
    /// Only the live path reports: a file's `Ok(0)` stays ordinary EOF.
    fn take_stalled(&mut self) -> bool {
        self.live.as_mut().is_some_and(|live| live.stall.take())
    }

    /// MAN-530: a capture hole of `AUDIO_DISCONTINUITY_FRAMES` or more
    /// immediately before the last read's samples. Live devices only.
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.live
            .as_mut()
            .and_then(|live| live.gap.discontinuity.take())
    }

    /// MAN-530: the exact length of a shorter capture hole immediately
    /// before the last read's samples, paired with `take_unknown_gap`.
    fn take_missing_samples(&mut self) -> Option<u64> {
        self.live.as_mut().and_then(|live| live.gap.missing.take())
    }

    /// MAN-530: `true` with a short capture hole's `take_missing_samples`,
    /// so the engine resets continuity-dependent decode state at the hole
    /// as well as advancing its clock.
    fn take_unknown_gap(&mut self) -> bool {
        self.live
            .as_mut()
            .is_some_and(|live| std::mem::take(&mut live.gap.unknown))
    }

    /// MAN-530: live capture-ring loss totals. `None` for files.
    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.live.as_ref().and_then(|live| live.health.clone())
    }
}

impl AudioIqSource {
    /// Real samples from the underlying source. A file returns 0 at end of
    /// file; a live device polls its ring until at least one sample
    /// arrives, reports a resumable soft stall once the ring has been
    /// empty past the soft deadline after delivery (MAN-489), and fails
    /// after its stall timeout. An empty request never waits.
    fn read_real(&mut self, real: &mut [f32]) -> Result<usize> {
        let Some(live) = &mut self.live else {
            return match &mut self.src {
                Some(src) => src.read(real),
                None => Ok(0),
            };
        };
        if real.is_empty() {
            return Ok(0);
        }
        loop {
            let run = live.ring.read_run(real)?;
            live.watch_for_overrun();
            let got = run.got;
            if got > 0 {
                // MAN-530: the run starts after `run.missing` lost frames;
                // it never extends past a later hole.
                live.gap.note_hole(run.missing);
                // Delivery -- zero-valued samples included -- re-arms the
                // episode and ends any wait episode, so the hard bound
                // measures only genuine silence.
                live.waiting_since = None;
                live.last_delivery = Instant::now();
                live.stall.note_delivery();
                return Ok(got);
            }
            // The wait episode survives a soft report's return, so the
            // hard device-loss bound below cannot restart mid-outage.
            let since = *live.waiting_since.get_or_insert_with(Instant::now);
            if live
                .stall
                .due(live.last_delivery, Instant::now(), live.stall_timeout)
            {
                live.stall.report();
                return Ok(0);
            }
            if since.elapsed() >= live.stall_timeout {
                return Err(no_samples(&live.label, live.stall_timeout));
            }
            std::thread::sleep(DEVICE_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::audio_capture::{capture_ring, CaptureProducer, CaptureRead};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A fake capture ring with the semantics of coppa's `CpalSource`: each
    /// read pops the next `(count, value)` (count 0 = empty ring) and fills
    /// that many samples of `value`; an exhausted script reads as an empty
    /// ring forever, like a callback that never fires.
    /// The handle a test resumes a scripted ring through.
    type ScriptFeeder = Arc<Mutex<VecDeque<(usize, f32)>>>;

    struct ScriptedRing {
        script: VecDeque<(usize, f32)>,
        /// MAN-489: externally controlled resume -- entries pushed here
        /// after the script runs out, like a capture callback that starts
        /// firing again.
        more: ScriptFeeder,
    }

    impl AudioSource for ScriptedRing {
        fn read(&mut self, buf: &mut [f32]) -> Result<usize> {
            let (count, value) = self
                .script
                .pop_front()
                .or_else(|| self.more.lock().unwrap().pop_front())
                .unwrap_or((0, 0.0));
            let n = count.min(buf.len());
            buf[..n].fill(value);
            Ok(n)
        }
        fn sample_rate(&self) -> u32 {
            TARGET_RATE_HZ
        }
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl CaptureRing for ScriptedRing {
        fn read_run(&mut self, out: &mut [f32]) -> Result<CaptureRead> {
            Ok(CaptureRead {
                got: AudioSource::read(self, out)?,
                missing: 0,
            })
        }
    }

    fn scripted(script: &[(usize, f32)], stall: Duration) -> AudioIqSource {
        let (aiq, _feeder) = scripted_ring(script, stall);
        aiq
    }

    /// MAN-489: the source plus the handle its ring can be resumed
    /// through -- an externally controlled pause/resume, not a fixed
    /// count of empty polls.
    fn scripted_ring(script: &[(usize, f32)], stall: Duration) -> (AudioIqSource, ScriptFeeder) {
        let ring = ScriptedRing {
            script: script.iter().copied().collect(),
            more: Arc::new(Mutex::new(VecDeque::new())),
        };
        let feeder = Arc::clone(&ring.more);
        (
            AudioIqSource::live(
                Box::new(ring),
                "\"Test Mic\"".to_string(),
                stall,
                Duration::ZERO,
                None,
            ),
            feeder,
        )
    }

    /// A notice that records each name it is called with.
    fn recording_notice() -> (SilenceNotice, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Box::new(move |name: &str| sink.lock().unwrap().push(name.to_string())),
            seen,
        )
    }

    #[test]
    fn a_live_device_waits_out_an_empty_capture_ring() {
        let mut aiq = scripted(
            &[(0, 0.0), (0, 0.0), (0, 0.0), (480, 0.5)],
            Duration::from_secs(1),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 480);
    }

    /// MAN-489: a delivering device whose ring goes empty reports a
    /// resumable soft stall (`Ok(0)` + `take_stalled` once) before its
    /// hard device-loss error, resumes on the same ring, and reports a
    /// second outage the same way. Zero-valued samples count as delivery.
    #[test]
    fn a_delivering_device_reports_stalls_and_resumes_on_the_same_ring() {
        const HARD: Duration = Duration::from_secs(5);
        let (mut aiq, feeder) = scripted_ring(&[(256, 0.25)], HARD);
        // Shorten only the soft deadline; the hard bound stays, so the
        // guard (`hard > soft`) keeps the report ahead of the error.
        aiq.live.as_mut().unwrap().stall =
            crate::stall::StallEpisode::new(Duration::from_millis(80));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 256);
        // The ring goes empty; no device reopen, no error -- one report.
        let started = Instant::now();
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
        assert!(
            started.elapsed() < HARD,
            "report must precede the hard error"
        );
        assert!(aiq.take_stalled());
        assert!(!aiq.take_stalled());
        // Same ring resumes (all-zero samples are delivery too).
        feeder.lock().unwrap().push_back((128, 0.0));
        assert_eq!(aiq.read(&mut buf).unwrap(), 128);
        assert!(!aiq.take_stalled());
        // A second outage reports again.
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
        assert!(aiq.take_stalled());
    }

    /// MAN-489 phase 5: at production durations (the real
    /// `SOFT_STALL_AFTER` and `DEVICE_STALL_TIMEOUT`, no shortened knob) a
    /// delivering device whose ring goes empty reports inside the ticket's
    /// 5 s bound, before its hard device-loss error.
    #[test]
    fn an_emptied_ring_reports_within_the_production_five_second_bound() {
        let (mut aiq, _feeder) = scripted_ring(&[(256, 0.25)], DEVICE_STALL_TIMEOUT);
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 256);
        let started = Instant::now();
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
        let reported = started.elapsed();
        assert!(reported < Duration::from_secs(5), "{reported:?}");
        assert!(aiq.take_stalled());
    }

    /// MAN-489: the hard device-loss bound survives the soft report -- a
    /// stream that never resumes still errors within its stall timeout,
    /// measured from the wait episode the report did not reset.
    #[test]
    fn after_a_soft_stall_the_hard_device_loss_bound_still_fires() {
        let (mut aiq, _feeder) = scripted_ring(&[(256, 0.25)], Duration::from_millis(300));
        aiq.live.as_mut().unwrap().stall =
            crate::stall::StallEpisode::new(Duration::from_millis(60));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        assert_eq!(aiq.read(&mut buf).unwrap(), 256);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
        assert!(aiq.take_stalled());
        let err = aiq.read(&mut buf).unwrap_err();
        // MAN-533 D4: the device-loss bound carries the shared type.
        assert!(crate::is_no_usable_samples(&err), "{err:#}");
        let err = err.to_string();
        assert!(err.contains("delivered no samples in 0.3 s"), "{err}");
    }

    #[test]
    fn a_live_device_that_never_delivers_names_itself_the_rate_and_the_hint() {
        let mut aiq = scripted(&[], Duration::from_millis(30));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        let err = aiq.read(&mut buf).unwrap_err().to_string();
        assert!(
            err.contains("audio input \"Test Mic\" delivered no samples in 0.03 s at 48000 Hz -- "),
            "{err}"
        );
        assert!(err.ends_with(audio_input_hint()), "{err}");
    }

    #[test]
    fn a_live_device_that_never_delivers_fails_within_its_stall_timeout() {
        let stall = Duration::from_millis(50);
        let mut aiq = scripted(&[], stall);
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        let started = Instant::now();
        let err = aiq.read(&mut buf).unwrap_err();
        let elapsed = started.elapsed();
        // MAN-533 scenario 2 (audio): reads succeed with nothing, and the
        // bound ends in the shared type the reconnect layer reopens on.
        assert!(crate::is_no_usable_samples(&err), "{err:#}");
        let err = err.to_string();
        assert!(
            err.contains("delivered no samples in 0.05 s at 48000 Hz"),
            "{err}"
        );
        assert!(
            elapsed >= stall && elapsed < Duration::from_secs(1),
            "elapsed={elapsed:?}"
        );
    }

    #[test]
    fn an_empty_request_does_not_block_a_live_device() {
        let mut aiq = scripted(&[], Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(aiq.read(&mut []).unwrap(), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn digital_silence_notice_fires_once_after_two_seconds_of_exact_zeros() {
        let (notice, seen) = recording_notice();
        let mut aiq =
            scripted(&[(4800, 0.0); 50], Duration::from_secs(1)).with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        for read in 1..=50u64 {
            assert_eq!(aiq.read(&mut buf).unwrap(), 4800);
            let fired = seen.lock().unwrap().len();
            // 20 reads x 4800 = 96 000 samples = 2 s at 48 kHz.
            let expected = usize::from(read * 4800 >= 2 * TARGET_RATE_HZ as u64);
            assert_eq!(fired, expected, "after read {read}");
        }
        assert_eq!(*seen.lock().unwrap(), ["\"Test Mic\""]);
    }

    #[test]
    fn digital_silence_notice_never_fires_after_a_nonzero_sample() {
        let (notice, seen) = recording_notice();
        let mut script = vec![(4800, 1e-4)];
        script.extend([(4800, 0.0); 50]);
        let mut aiq = scripted(&script, Duration::from_secs(1)).with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        for _ in 0..script.len() {
            assert_eq!(aiq.read(&mut buf).unwrap(), 4800);
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_file_backed_source_ignores_a_silence_notice() {
        let (notice, seen) = recording_notice();
        let samples = vec![0.0; 5 * TARGET_RATE_HZ as usize];
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            samples,
            TARGET_RATE_HZ,
        ));
        let mut aiq = AudioIqSource::new(src).unwrap().with_silence_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4800];
        while aiq.read(&mut buf).unwrap() > 0 {}
        assert!(seen.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_hint_names_the_macos_privacy_setting() {
        let hint = audio_input_hint();
        assert!(
            hint.contains("System Settings > Privacy & Security > Microphone"),
            "{hint}"
        );
        assert!(hint.contains("manta check"), "{hint}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_hint_is_generic_off_macos() {
        let hint = audio_input_hint();
        assert!(hint.contains("manta check"), "{hint}");
        assert!(!hint.contains("Privacy"), "{hint}");
    }

    #[test]
    fn open_failure_messages_name_the_device_and_the_rate() {
        assert_eq!(
            no_matching_input("NoSuchCard").to_string(),
            "no audio input matching \"NoSuchCard\" (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        assert_eq!(
            no_default_input().to_string(),
            "no default audio input (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        let rate = wrong_rate("\"Mic\"", 44_100).to_string();
        assert!(
            rate.starts_with(
                "audio input \"Mic\" runs at 44100 Hz, but manta needs 48000 Hz \
                 and does not resample -- set that input to 48000 Hz in "
            ),
            "{rate}"
        );
        assert!(
            rate.ends_with(", or choose another with --device"),
            "{rate}"
        );
        assert_eq!(
            rate.contains("Audio MIDI Setup"),
            cfg!(target_os = "macos"),
            "{rate}"
        );
        assert_eq!(
            did_not_open("\"Mic\"", &anyhow!("boom")).to_string(),
            format!(
                "audio input \"Mic\" did not open at 48000 Hz: boom -- {}",
                audio_input_hint()
            )
        );
        // A context chain is flattened onto the one line.
        let chained = anyhow!("not available").context("query its default input config");
        assert!(did_not_open("\"Mic\"", &chained).to_string().contains(
            "did not open at 48000 Hz: query its default input config: not available -- "
        ));
    }

    #[test]
    fn converts_real_samples_to_analytic_iq() {
        let fs = TARGET_RATE_HZ;
        let f = 1_000.0;
        let samples: Vec<f32> = (0..4000)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs as f64).cos() as f32)
            .collect();
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(samples, fs));
        let mut aiq = AudioIqSource::new(src).unwrap();
        assert_eq!(aiq.sample_rate(), fs as f64);
        assert_eq!(aiq.center_freq_hz(), 0.0);
        // MAN-86 review: coverage is the rig's AF passband above the dial,
        // NOT the analytic stream's +/- 24 kHz Nyquist span.
        assert_eq!(
            aiq.rf_passband_hz(),
            (AUDIO_PASSBAND_LO_HZ, AUDIO_PASSBAND_HI_HZ)
        );
        assert!(
            aiq.rf_passband_hz().0 > 0.0,
            "analytic audio carries no spectrum below the dial frequency"
        );
        assert!(aiq.rf_passband_hz().1 < aiq.sample_rate() / 2.0);
        let mut buf = vec![Complex32::new(0.0, 0.0); 4000];
        let n = aiq.read(&mut buf).unwrap();
        assert!(n > 0);
        // Well past the Hilbert filter's transient, magnitude should be ~unit.
        assert!(
            (buf[2000].norm() - 1.0).abs() < 0.1,
            "norm={}",
            buf[2000].norm()
        );
    }

    #[test]
    fn rejects_mismatched_sample_rate() {
        let src: Box<dyn AudioSource> =
            Box::new(coppa_audio::WavSource::from_samples(vec![0.0; 10], 44_100));
        assert!(AudioIqSource::new(src).is_err());
    }

    #[test]
    fn reports_eof_as_zero_read() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 5],
            TARGET_RATE_HZ,
        ));
        let mut aiq = AudioIqSource::new(src).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(aiq.read(&mut buf).unwrap(), 5);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn center_freq_hz_defaults_to_zero_without_an_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        assert_eq!(AudioIqSource::new(src).unwrap().center_freq_hz(), 0.0);
    }

    #[test]
    fn with_center_freq_hz_is_reported_as_the_sources_rf_reference() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 10],
            TARGET_RATE_HZ,
        ));
        let aiq = AudioIqSource::new(src)
            .unwrap()
            .with_center_freq_hz(14_030_000.0)
            .unwrap();
        assert_eq!(aiq.center_freq_hz(), 14_030_000.0);
    }

    #[test]
    fn with_center_freq_hz_rejects_non_finite_and_non_positive_values() {
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -14_030_000.0,
        ] {
            let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
                vec![0.0; 10],
                TARGET_RATE_HZ,
            ));
            assert!(
                AudioIqSource::new(src)
                    .unwrap()
                    .with_center_freq_hz(bad)
                    .is_err(),
                "with_center_freq_hz({bad}) should have been rejected"
            );
        }
    }

    #[test]
    fn audio_source_declares_the_rate_derived_hilbert_guard() {
        let src: Box<dyn AudioSource> = Box::new(coppa_audio::WavSource::from_samples(
            vec![0.0; 64],
            TARGET_RATE_HZ,
        ));
        let aiq = AudioIqSource::new(src).unwrap();
        assert_eq!(aiq.analytic_guard_hz(), 600.0);
        assert_eq!(
            aiq.analytic_guard_hz(),
            manta_dsp::hilbert::hilbert_guard_hz(48_000.0)
        );
    }

    // ---- MAN-530: capture-ring overruns ----

    /// A live source over the production tagged ring, plus the producer a
    /// test drives as the capture callback and the shared counters.
    fn tagged_live(stall: Duration) -> (AudioIqSource, CaptureProducer, Arc<InputHealthCounters>) {
        tagged_live_with(CAPTURE_RING_FRAMES, stall)
    }

    fn tagged_live_with(
        capacity: usize,
        stall: Duration,
    ) -> (AudioIqSource, CaptureProducer, Arc<InputHealthCounters>) {
        let health = Arc::new(InputHealthCounters::new());
        let (producer, consumer) = capture_ring(capacity, 0, Arc::clone(&health));
        let aiq = AudioIqSource::live(
            Box::new(consumer),
            "\"USB \\\"Audio\\\"\"".to_string(),
            stall,
            Duration::ZERO,
            Some(Arc::clone(&health)),
        );
        (aiq, producer, health)
    }

    /// Capture frames `from..from + n` in 480-frame (10 ms) callbacks;
    /// each frame's value encodes its capture index.
    fn capture(p: &mut CaptureProducer, from: usize, n: usize) {
        let frames: Vec<f32> = (from..from + n)
            .map(|i| (i % 1000) as f32 / 1000.0 + 0.001)
            .collect();
        for chunk in frames.chunks(480) {
            p.write(chunk);
        }
    }

    /// One read's length and the three hooks it left pending.
    fn read_with_hooks(
        aiq: &mut AudioIqSource,
        want: usize,
    ) -> (usize, Option<u64>, Option<u64>, bool) {
        let mut buf = vec![Complex32::new(0.0, 0.0); want];
        let got = aiq.read(&mut buf).unwrap();
        (
            got,
            aiq.take_discontinuity(),
            aiq.take_missing_samples(),
            aiq.take_unknown_gap(),
        )
    }

    /// Discontinuity, missing count and unknown-gap flag one read left.
    type Hooks = (Option<u64>, Option<u64>, bool);

    /// Park the reader while `parked_total` frames are captured (all but
    /// the first ring's worth are lost), then read with the cycling
    /// `request` sizes, resuming capture once the first read freed room.
    /// Returns the frames read before the hole, the hooks the first
    /// post-hole read left, and the counters.
    fn park_and_resume(
        parked_total: usize,
        request: &[usize],
    ) -> (usize, Hooks, Arc<InputHealthCounters>) {
        let (mut aiq, mut p, health) = tagged_live(Duration::from_secs(1));
        capture(&mut p, 0, parked_total);
        let mut pre = 0;
        let mut i = 0;
        loop {
            let want = request[i % request.len()];
            i += 1;
            let (got, disc, miss, unknown) = read_with_hooks(&mut aiq, want);
            assert!(got <= want);
            if i == 1 {
                // Capture resumes into the room the first read freed, so
                // the hole is exactly `parked_total - CAPTURE_RING_FRAMES`.
                capture(&mut p, parked_total, got.min(480));
            }
            if pre < CAPTURE_RING_FRAMES {
                // Pre-hole reads never carry a hook, whatever the counters say.
                assert_eq!(
                    (disc, miss, unknown),
                    (None, None, false),
                    "read {i} before the hole"
                );
                pre += got;
                assert!(pre <= CAPTURE_RING_FRAMES, "a read straddled the hole");
                continue;
            }
            return (pre, (disc, miss, unknown), health);
        }
    }

    #[test]
    fn a_500_ms_park_reports_15808_missing_at_the_first_post_hole_sample() {
        for request in [
            &[1usize, 4096, 4096][..],
            &[2048],
            &[4096],
            &[1000, 3, 4096, 517],
        ] {
            let (pre, hooks, health) = park_and_resume(24_000, request);
            assert_eq!(pre, CAPTURE_RING_FRAMES, "request {request:?}");
            assert_eq!(hooks, (None, Some(15_808), true), "request {request:?}");
            assert_eq!(health.queue_dropped_samples(), 15_808);
            assert_eq!(health.gaps_detected(), 1);
            assert_eq!(health.dropped_packets(), 0);
            assert_eq!(health.malformed_packets(), 0);
        }
    }

    #[test]
    fn single_frame_reads_still_stop_at_the_hole() {
        let (mut aiq, mut p, _health) = tagged_live(Duration::from_secs(1));
        capture(&mut p, 0, CAPTURE_RING_FRAMES + 10);
        assert_eq!(read_with_hooks(&mut aiq, 1), (1, None, None, false));
        capture(&mut p, CAPTURE_RING_FRAMES + 10, 1);
        for n in 1..CAPTURE_RING_FRAMES {
            assert_eq!(
                read_with_hooks(&mut aiq, 1),
                (1, None, None, false),
                "frame {n}"
            );
        }
        assert_eq!(read_with_hooks(&mut aiq, 1), (1, None, Some(10), true));
        assert_eq!(aiq.take_missing_samples(), None);
    }

    #[test]
    fn the_discontinuity_threshold_is_half_a_second() {
        for (hole, expected) in [
            (23_999u64, (None, Some(23_999), true)),
            (24_000, (Some(24_000), None, false)),
            (24_001, (Some(24_001), None, false)),
        ] {
            let parked = CAPTURE_RING_FRAMES + hole as usize;
            let (pre, hooks, health) = park_and_resume(parked, &[4096]);
            assert_eq!(pre, CAPTURE_RING_FRAMES);
            assert_eq!(hooks, expected, "hole {hole}");
            assert_eq!(health.queue_dropped_samples(), hole);
        }
    }

    #[test]
    fn hooks_drain_once_and_holes_never_share_a_buffer() {
        let (mut aiq, mut p, health) = tagged_live(Duration::from_secs(1));
        capture(&mut p, 0, CAPTURE_RING_FRAMES + 100);
        assert_eq!(read_with_hooks(&mut aiq, 8192).0, 8192);
        capture(&mut p, 8292, 300); // accepted
        capture(&mut p, 8592, CAPTURE_RING_FRAMES); // fills, then partly lost
                                                    // 300 + 7892 contiguous frames after a 100-frame hole; the other
                                                    // 300 offered frames were lost, a second episode.
        let (got, disc, miss, unknown) = read_with_hooks(&mut aiq, 100_000);
        assert_eq!(
            (got, disc, miss, unknown),
            (CAPTURE_RING_FRAMES, None, Some(100), true)
        );
        // Drained: a second take reports nothing.
        assert_eq!(aiq.take_missing_samples(), None);
        assert!(!aiq.take_unknown_gap());
        assert_eq!(aiq.take_discontinuity(), None);
        assert_eq!(health.gaps_detected(), 2);
        assert_eq!(health.queue_dropped_samples(), 400);
        capture(&mut p, 8592 + CAPTURE_RING_FRAMES, 10);
        let (got, disc, miss, unknown) = read_with_hooks(&mut aiq, 100_000);
        assert_eq!((got, disc, miss, unknown), (10, None, Some(300), true));
    }

    #[test]
    fn an_empty_request_consumes_no_pending_marker() {
        let (mut aiq, mut p, _health) = tagged_live(Duration::from_secs(1));
        capture(&mut p, 0, CAPTURE_RING_FRAMES + 50);
        assert_eq!(read_with_hooks(&mut aiq, 8192).0, 8192);
        capture(&mut p, CAPTURE_RING_FRAMES + 50, 20);
        assert_eq!(read_with_hooks(&mut aiq, 0), (0, None, None, false));
        assert_eq!(read_with_hooks(&mut aiq, 64), (20, None, Some(50), true));
    }

    #[test]
    fn an_overrun_live_ring_still_soft_stalls_and_hard_fails() {
        let (mut aiq, mut p, health) = tagged_live(Duration::from_millis(300));
        capture(&mut p, 0, CAPTURE_RING_FRAMES + 480);
        assert_eq!(read_with_hooks(&mut aiq, 8192).0, 8192);
        assert_eq!(health.queue_dropped_samples(), 480);
        // Capture stops: the empty ring is a stall, never EOF-with-a-hook.
        let mut buf = vec![Complex32::new(0.0, 0.0); 64];
        let started = Instant::now();
        loop {
            match aiq.read(&mut buf) {
                Ok(0) => {
                    assert!(aiq.take_stalled() || started.elapsed() < Duration::from_secs(2));
                    assert_eq!(aiq.take_missing_samples(), None);
                }
                Ok(n) => panic!("read {n} from an empty ring"),
                Err(e) => {
                    assert!(e.to_string().contains("delivered no samples"), "{e:#}");
                    break;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn health_counters_exist_only_for_live_audio() {
        let (aiq, _p, health) = tagged_live(Duration::from_secs(1));
        let shared = aiq.health_counters().expect("live audio has counters");
        assert!(Arc::ptr_eq(&shared, &health));
        assert_eq!(shared.queue_dropped_samples(), 0);
        let file = AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(
            vec![0.1; 16],
            TARGET_RATE_HZ,
        )))
        .unwrap();
        assert!(file.health_counters().is_none());
    }

    fn recording_overrun() -> (OverrunNotice, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (
            Box::new(move |label: &str| sink.lock().unwrap().push(label.to_string())),
            seen,
        )
    }

    #[test]
    fn the_overrun_notice_fires_once_with_the_quoted_label() {
        let (aiq, mut p, _health) = tagged_live(Duration::from_secs(1));
        let (notice, seen) = recording_overrun();
        let mut aiq = aiq.with_overrun_notice(notice);
        capture(&mut p, 0, 4096);
        let _ = read_with_hooks(&mut aiq, 1024);
        assert!(seen.lock().unwrap().is_empty(), "no loss, no notice");
        capture(&mut p, 4096, 24_000);
        // Warns while still returning pre-hole audio.
        let (got, disc, miss, unknown) = read_with_hooks(&mut aiq, 1024);
        assert_eq!((got, disc, miss, unknown), (1024, None, None, false));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["\"USB \\\"Audio\\\"\"".to_string()]
        );
        let _ = read_with_hooks(&mut aiq, 1024);
        capture(&mut p, 28_096, 24_000);
        let _ = read_with_hooks(&mut aiq, 1024);
        assert_eq!(seen.lock().unwrap().len(), 1, "rate-limited");
    }

    #[test]
    fn the_overrun_gate_opens_once_per_60_seconds_on_growth_only() {
        let mut watch = OverrunWatch::default();
        let t0 = Instant::now();
        assert!(!watch.due(0, t0), "a clean input never warns");
        assert!(watch.due(10, t0));
        assert!(
            !watch.due(10, t0 + Duration::from_secs(120)),
            "no growth, no warning"
        );
        assert!(!watch.due(20, t0 + Duration::from_secs(59)));
        assert!(watch.due(30, t0 + Duration::from_secs(60)));
        assert!(!watch.due(40, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn a_file_backed_source_ignores_an_overrun_notice() {
        let (notice, seen) = recording_overrun();
        let mut aiq = AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(
            vec![0.1; 64],
            TARGET_RATE_HZ,
        )))
        .unwrap()
        .with_overrun_notice(notice);
        let mut buf = vec![Complex32::new(0.0, 0.0); 128];
        assert_eq!(aiq.read(&mut buf).unwrap(), 64);
        assert_eq!(aiq.read(&mut buf).unwrap(), 0);
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(aiq.take_missing_samples(), None);
        assert!(!aiq.take_unknown_gap());
        assert_eq!(aiq.take_discontinuity(), None);
    }

    #[test]
    fn untaken_holes_accumulate_instead_of_being_overwritten() {
        let (mut aiq, mut p, _health) = tagged_live_with(4, Duration::from_secs(1));
        let mut buf = vec![Complex32::new(0.0, 0.0); 8];
        capture(&mut p, 0, 4 + 10); // 10 lost
        assert_eq!(aiq.read(&mut buf).unwrap(), 4);
        capture(&mut p, 14, 4 + 20); // 4 accepted, 20 lost
        assert_eq!(aiq.read(&mut buf).unwrap(), 4); // after the 10-frame hole, not taken
        capture(&mut p, 38, 1);
        assert_eq!(aiq.read(&mut buf).unwrap(), 1); // after the 20-frame hole
        assert_eq!(aiq.take_missing_samples(), Some(30));
        assert!(aiq.take_unknown_gap());
        assert_eq!(aiq.take_discontinuity(), None);
    }

    #[test]
    fn a_long_hole_restarts_the_hilbert_filter_but_a_short_one_does_not() {
        let value = |i: usize| (i % 1000) as f32 / 1000.0 + 0.001;
        for (hole, restarted) in [(24_000usize, true), (100, false)] {
            let (mut aiq, mut p, _health) = tagged_live(Duration::from_secs(1));
            capture(&mut p, 0, CAPTURE_RING_FRAMES + hole);
            let mut buf = vec![Complex32::new(0.0, 0.0); CAPTURE_RING_FRAMES];
            assert_eq!(aiq.read(&mut buf).unwrap(), CAPTURE_RING_FRAMES);
            let resume = CAPTURE_RING_FRAMES + hole;
            capture(&mut p, resume, 480);
            let mut post = vec![Complex32::new(0.0, 0.0); 480];
            assert_eq!(aiq.read(&mut post).unwrap(), 480);
            let real: Vec<f32> = (resume..resume + 480).map(value).collect();
            let fresh = HilbertTransformer::new().process(&real);
            let same = post.iter().zip(&fresh).all(|(a, b)| (a - b).norm() < 1e-6);
            assert_eq!(same, restarted, "hole {hole}");
        }
    }
}
