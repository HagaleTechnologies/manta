//! File-replay decorators (MAN-269): `PacedSource` plays a recording at its
//! own pace (`run --realtime`) and `LoopingSource` starts it again at each
//! end (`run --loop`). Both decide only *when* samples arrive, never *which*:
//! they hand back exactly what the inner source returned, in the same order,
//! so a paced replay's decoded output is byte-identical to an unpaced one.
//! Neither touches `manta_engine::listen`; they compose outside it through
//! `IqSource::read`. Rationale and measurements:
//! docs/DECISIONS/2026-10-10-man269-paced-looping-replay.md.

use crate::{InputHealthCounters, IqSource};
use anyhow::{bail, Result};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Opens the next pass of a looped recording. The same shape as
/// `manta-cli`'s `reconnect::Opener`.
pub type Reopen = Box<dyn FnMut() -> Result<Box<dyn IqSource>>>;

/// Paces `inner` to wall-clock time at its own sample rate.
///
/// Sleeps are computed from the cumulative delivered count, including the
/// buffer about to be returned, against one start instant, so a late chunk
/// is absorbed rather than compounded. Once the consumer is behind the
/// recording it never sleeps: pacing degrades to unpaced, never stalls.
pub struct PacedSource {
    inner: Box<dyn IqSource>,
    fs: f64,
    delivered: u64,
    /// Started on the first `read`, not at construction: the CLI binds its
    /// listeners and hashes the file for the session nonce in between, and
    /// charging that to the recording would shorten the window a client has
    /// to connect in (MAN-121 round 2).
    start: Option<Instant>,
}

impl PacedSource {
    /// Fails on a non-finite or non-positive `inner.sample_rate()`, which
    /// would make `Duration::from_secs_f64` panic.
    pub fn new(inner: Box<dyn IqSource>) -> Result<Self> {
        let fs = inner.sample_rate();
        if !fs.is_finite() || fs <= 0.0 {
            bail!("cannot pace a replay whose sample rate is {fs} Hz");
        }
        Ok(PacedSource {
            inner,
            fs,
            delivered: 0,
            start: None,
        })
    }

    /// Wall-clock since the first `read`, or zero before it.
    fn elapsed(&self) -> Duration {
        self.start.map_or(Duration::ZERO, |start| start.elapsed())
    }
}

/// How much longer `read` must wait before returning a buffer that brings
/// the cumulative count to `delivered`, given `elapsed` since the first read.
/// `None` means return now. A pure function so the pacing decision can be
/// asserted exactly instead of through a wall-clock upper bound.
fn pacing_delay(delivered: u64, fs: f64, elapsed: Duration) -> Option<Duration> {
    Duration::from_secs_f64(delivered as f64 / fs)
        .checked_sub(elapsed)
        .filter(|d| !d.is_zero())
}

impl IqSource for PacedSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // The clock starts before the inner read: the time the inner source
        // spends producing this buffer is part of the recording's interval.
        self.start.get_or_insert_with(Instant::now);
        let n = self.inner.read(buf)?;
        self.delivered += n as u64;
        // Sleep after the read, against the count that includes this buffer,
        // so even `listen`'s first read (the whole 2 s calibration buffer)
        // waits for its own recording interval.
        if let Some(delay) = pacing_delay(self.delivered, self.fs, self.elapsed()) {
            std::thread::sleep(delay);
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

/// Reopens the recording each time it ends, so replay continues until the
/// consumer stops reading.
///
/// Reopening rather than rewinding works for both replay kinds: neither
/// `WavIqSource` nor `AudioIqSource` can rewind. The wrap is a hard splice
/// (last sample straight to first) that the channelizer hears as a click.
/// It deliberately does NOT report a discontinuity: `listen` discards a
/// partial calibration fill on one, so a recording shorter than the 2 s
/// calibration window would never finish calibrating.
pub struct LoopingSource {
    label: String,
    inner: Box<dyn IqSource>,
    reopen: Reopen,
    fs: f64,
    center_freq_hz: f64,
}

impl LoopingSource {
    /// `label` names the recording in errors; `first` is the already-open
    /// first pass and `reopen` opens every later one.
    pub fn new(label: impl Into<String>, first: Box<dyn IqSource>, reopen: Reopen) -> Self {
        let fs = first.sample_rate();
        let center_freq_hz = first.center_freq_hz();
        LoopingSource {
            label: label.into(),
            inner: first,
            reopen,
            fs,
            center_freq_hz,
        }
    }
}

impl IqSource for LoopingSource {
    /// The first pass's rate, which every later pass must match.
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        // One reopen per call, so an empty recording ends (returns 0)
        // instead of spinning.
        let next = (self.reopen)()?;
        if next.sample_rate() != self.fs {
            bail!(
                "replay {} changed between loop passes: sample rate {} Hz, expected {} Hz",
                self.label,
                next.sample_rate(),
                self.fs
            );
        }
        if next.center_freq_hz() != self.center_freq_hz {
            bail!(
                "replay {} changed between loop passes: center frequency {} Hz, expected {} Hz",
                self.label,
                next.center_freq_hz(),
                self.center_freq_hz
            );
        }
        self.inner = next;
        self.inner.read(buf)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct VecSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    fn vec_source(samples: Vec<Complex32>, fs: f64) -> VecSource {
        VecSource {
            samples,
            cursor: 0,
            fs,
            center_freq_hz: 0.0,
        }
    }

    impl IqSource for VecSource {
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

    /// Every optional `IqSource` method returns something distinctive.
    struct Distinctive {
        live: Arc<AtomicBool>,
        counters: Arc<InputHealthCounters>,
        gap: Option<u64>,
    }

    impl Distinctive {
        fn new() -> Self {
            Distinctive {
                live: Arc::new(AtomicBool::new(true)),
                counters: Arc::new(InputHealthCounters::new()),
                gap: Some(7),
            }
        }
    }

    impl IqSource for Distinctive {
        fn sample_rate(&self) -> f64 {
            1_000_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            (-1234.0, 5678.0)
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            Ok(buf.len())
        }
        fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
            Some(self.live.clone())
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
        fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
            Some(self.counters.clone())
        }
    }

    fn assert_forwards(
        src: &mut dyn IqSource,
        inner: (&Arc<AtomicBool>, &Arc<InputHealthCounters>),
    ) {
        assert_eq!(src.sample_rate(), 1_000_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        assert_eq!(src.rf_passband_hz(), (-1234.0, 5678.0));
        assert!(Arc::ptr_eq(&src.confirmed_live_handle().unwrap(), inner.0));
        assert!(Arc::ptr_eq(&src.health_counters().unwrap(), inner.1));
        assert_eq!(src.take_discontinuity(), Some(7));
        assert_eq!(src.take_discontinuity(), None);
    }

    fn pattern(n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| Complex32::new(i as f32, -(i as f32)))
            .collect()
    }

    fn drain(src: &mut dyn IqSource, chunk: usize) -> Vec<Complex32> {
        let mut all = Vec::new();
        let mut buf = vec![Complex32::new(0.0, 0.0); chunk];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..n]);
        }
    }

    /// Fill `want` samples the way `listen`'s calibration read does:
    /// `read(&mut buf[filled..])` until full.
    fn fill(src: &mut dyn IqSource, want: usize) -> Vec<Complex32> {
        let mut buf = vec![Complex32::new(0.0, 0.0); want];
        let mut filled = 0;
        while filled < want {
            let n = src.read(&mut buf[filled..]).unwrap();
            assert!(n > 0, "source ended after {filled} of {want} samples");
            assert_eq!(
                src.take_discontinuity(),
                None,
                "the loop wrap must not signal a gap"
            );
            filled += n;
        }
        buf
    }

    /// A reopen that hands out fresh copies of `samples` and counts calls.
    fn reopen_of(samples: Vec<Complex32>, fs: f64, calls: Rc<Cell<usize>>) -> Reopen {
        Box::new(move || {
            calls.set(calls.get() + 1);
            Ok(Box::new(vec_source(samples.clone(), fs)) as Box<dyn IqSource>)
        })
    }

    // ---- PacedSource

    #[test]
    fn paced_source_delivers_the_same_samples_in_the_same_order() {
        // A high fake rate keeps this fast; the pacing math is rate-agnostic.
        let samples = pattern(500);
        let mut paced =
            PacedSource::new(Box::new(vec_source(samples.clone(), 1_000_000.0))).unwrap();
        assert_eq!(drain(&mut paced, 64), samples);
    }

    #[test]
    fn paced_source_paces_the_very_first_read() {
        // `listen`'s first read is the whole calibration buffer; it must wait
        // for its own 800 / 8000 = 100 ms. Lower bound only.
        let mut paced = PacedSource::new(Box::new(vec_source(pattern(800), 8000.0))).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 800];
        let start = Instant::now();
        assert_eq!(paced.read(&mut buf).unwrap(), 800);
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "the first read returned after {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn paced_source_clock_starts_at_the_first_read_not_at_construction() {
        let mut paced = PacedSource::new(Box::new(vec_source(pattern(800), 8000.0))).unwrap();
        // Stands in for the CLI's setup between construction and first read.
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(paced.elapsed(), Duration::ZERO);
        let mut buf = vec![Complex32::new(0.0, 0.0); 800];
        let start = Instant::now();
        assert_eq!(paced.read(&mut buf).unwrap(), 800);
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "the first read returned after {:?}, so setup time was charged to the recording",
            start.elapsed()
        );
    }

    #[test]
    fn paced_source_takes_at_least_the_recording_duration() {
        // 2400 / 8000 = 300 ms. Lower bound only: an upper bound measures the
        // machine as much as the code.
        let mut paced = PacedSource::new(Box::new(vec_source(pattern(2400), 8000.0))).unwrap();
        let start = Instant::now();
        drain(&mut paced, 256);
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "a 300 ms recording took only {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn pacing_delay_is_the_remaining_debt_and_none_once_behind() {
        assert_eq!(pacing_delay(800, 8000.0, Duration::from_millis(200)), None);
        assert_eq!(
            pacing_delay(800, 8000.0, Duration::from_millis(40)),
            Some(Duration::from_millis(60))
        );
        assert_eq!(pacing_delay(0, 8000.0, Duration::ZERO), None);
    }

    #[test]
    fn paced_source_passes_eof_through_without_accruing_debt() {
        let mut paced = PacedSource::new(Box::new(vec_source(pattern(5), 8000.0))).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(paced.read(&mut buf).unwrap(), 5);
        assert_eq!(paced.read(&mut buf).unwrap(), 0);
        assert_eq!(paced.read(&mut buf).unwrap(), 0);
        assert_eq!(paced.delivered, 5, "EOF must not advance `delivered`");
        assert_eq!(
            pacing_delay(paced.delivered, paced.fs, paced.elapsed()),
            None
        );
    }

    #[test]
    fn paced_source_rejects_a_non_positive_or_non_finite_rate() {
        for fs in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let src = Box::new(vec_source(pattern(1), fs));
            assert!(PacedSource::new(src).is_err(), "fs = {fs} was accepted");
        }
    }

    #[test]
    fn paced_source_forwards_every_iq_source_method() {
        let inner = Distinctive::new();
        let (live, counters) = (inner.live.clone(), inner.counters.clone());
        let mut paced = PacedSource::new(Box::new(inner)).unwrap();
        assert_forwards(&mut paced, (&live, &counters));
    }

    // ---- LoopingSource

    #[test]
    fn looping_source_replays_the_same_samples_after_eof() {
        let samples = pattern(100);
        let calls = Rc::new(Cell::new(0));
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(vec_source(samples.clone(), 8000.0)),
            reopen_of(samples.clone(), 8000.0, calls.clone()),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 100];
        for _ in 0..3 {
            assert_eq!(looped.read(&mut buf).unwrap(), 100);
            assert_eq!(buf, samples);
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn looping_source_fills_a_read_longer_than_the_recording_across_the_wrap() {
        let samples = pattern(100);
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(vec_source(samples.clone(), 8000.0)),
            reopen_of(samples.clone(), 8000.0, Rc::new(Cell::new(0))),
        );
        let got = fill(&mut looped, 250);
        let want: Vec<Complex32> = samples.iter().cycle().take(250).copied().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn looping_source_stops_at_an_empty_recording_instead_of_spinning() {
        let calls = Rc::new(Cell::new(0));
        let mut looped = LoopingSource::new(
            "empty.wav",
            Box::new(vec_source(Vec::new(), 8000.0)),
            reopen_of(Vec::new(), 8000.0, calls.clone()),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 16];
        assert_eq!(looped.read(&mut buf).unwrap(), 0);
        assert_eq!(calls.get(), 1, "one reopen per read call");
    }

    #[test]
    fn looping_source_rejects_a_reopen_with_a_different_sample_rate() {
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(vec_source(Vec::new(), 96_000.0)),
            reopen_of(pattern(10), 48_000.0, Rc::new(Cell::new(0))),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 16];
        let err = looped.read(&mut buf).unwrap_err().to_string();
        assert_eq!(
            err,
            "replay rec.wav changed between loop passes: sample rate 48000 Hz, expected 96000 Hz"
        );
    }

    #[test]
    fn looping_source_rejects_a_reopen_with_a_different_center_frequency() {
        let first = VecSource {
            center_freq_hz: 14_000_000.0,
            ..vec_source(Vec::new(), 8000.0)
        };
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(first),
            reopen_of(pattern(10), 8000.0, Rc::new(Cell::new(0))),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 16];
        let err = looped.read(&mut buf).unwrap_err().to_string();
        assert_eq!(
            err,
            "replay rec.wav changed between loop passes: center frequency 0 Hz, expected 14000000 Hz"
        );
    }

    #[test]
    fn looping_source_propagates_a_reopen_error() {
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(vec_source(Vec::new(), 8000.0)),
            Box::new(|| bail!("open WAV rec.wav: gone")),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 16];
        let err = looped.read(&mut buf).unwrap_err().to_string();
        assert_eq!(err, "open WAV rec.wav: gone");
    }

    #[test]
    fn looping_source_forwards_every_iq_source_method() {
        let inner = Distinctive::new();
        let (live, counters) = (inner.live.clone(), inner.counters.clone());
        let mut looped = LoopingSource::new(
            "rec.wav",
            Box::new(inner),
            Box::new(|| bail!("not reached")),
        );
        assert_forwards(&mut looped, (&live, &counters));
    }

    // ---- composition

    #[test]
    fn paced_looping_source_keeps_one_clock_across_passes() {
        // Three passes of a 400-sample, 8 kS/s (50 ms) recording: one clock
        // spans all of them, so 1200 samples take >= 150 ms. Lower bound only.
        let samples = pattern(400);
        let looped = LoopingSource::new(
            "rec.wav",
            Box::new(vec_source(samples.clone(), 8000.0)),
            reopen_of(samples, 8000.0, Rc::new(Cell::new(0))),
        );
        let mut paced = PacedSource::new(Box::new(looped)).unwrap();
        let start = Instant::now();
        let got = fill(&mut paced, 1200);
        assert_eq!(got.len(), 1200);
        assert!(
            start.elapsed() >= Duration::from_millis(140),
            "three 50 ms passes took only {:?}",
            start.elapsed()
        );
    }
}
