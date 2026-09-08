//! Wall-clock pacing for file replay (MAN-121). A pure sleep wrapper: it
//! delivers exactly the samples its inner source delivers, in the same
//! order, and only decides *when*. The decode path is untouched, which is
//! what keeps `--realtime` output byte-identical to unpaced output.

use crate::IqSource;
use anyhow::Result;
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Paces `inner` to wall-clock realtime at its own sample rate.
///
/// Drift-free by construction: sleeps are computed from CUMULATIVE
/// delivered samples (including the chunk being returned) against a single
/// start instant (taken on the first read), not per-chunk, so a chunk
/// that arrives late is absorbed rather than compounded. If the consumer
/// falls behind the recording, `due <= elapsed` and this never sleeps at
/// all -- pacing degrades to unpaced instead of ever stalling the
/// pipeline.
pub struct PacedSource {
    inner: Box<dyn IqSource>,
    fs: f64,
    delivered: u64,
    /// The recording clock's origin, started LAZILY on the first `read()`
    /// rather than at construction. `manta-cli` builds the paced source
    /// before it resolves the replay epoch, hashes the whole recording for
    /// the session nonce, and binds the telnet/JSON listeners -- all of
    /// which happen between `new()` and the first read. Anchoring the
    /// clock at construction credited that setup time against the
    /// recording's own timeline, so the first buffer's pacing debt was
    /// already partly spent before the servers were even listening and a
    /// client had that much less of the window to connect in.
    start: Option<Instant>,
}

impl PacedSource {
    pub fn new(inner: Box<dyn IqSource>) -> Self {
        let fs = inner.sample_rate();
        PacedSource {
            inner,
            fs,
            delivered: 0,
            start: None,
        }
    }

    /// Wall-clock since the recording clock started, or zero before the
    /// first `read()` has started it.
    fn elapsed(&self) -> Duration {
        self.start.map_or(Duration::ZERO, |start| start.elapsed())
    }
}

/// How long `read()` must still wait before it may RETURN a buffer that
/// brings the cumulative delivered count to `delivered`, given `elapsed`
/// wall-clock since the paced source started.
///
/// The count is the one INCLUDING the buffer about to be returned, not the
/// one before it (round-2 review): pacing on the previous count would hand
/// every buffer to the consumer a full chunk before its own recording
/// interval had elapsed, and the very first chunk -- which for
/// `manta_engine::listen` is the entire two-second calibration buffer --
/// would be delivered instantly, so a spot inside it could be published
/// before any client had time to connect.
///
/// `None` means "return now": the consumer is already at or past the
/// recording's own clock, so pacing degrades to unpaced rather than ever
/// stalling the pipeline. Extracted as a pure function so the pacing
/// decision can be asserted exactly, instead of inferred from a wall-clock
/// upper bound on a whole `read()`. Such bounds measure the machine as
/// much as the code and are flake-prone on a loaded CI runner -- the
/// `--features hpsdr` job in particular runs this module's tests inside a
/// far heavier `manta-input` test binary (every UDP-loopback HPSDR test
/// too) than the default job does.
fn pacing_delay(delivered: u64, fs: f64, elapsed: Duration) -> Option<Duration> {
    let due = Duration::from_secs_f64(delivered as f64 / fs);
    if due > elapsed {
        Some(due - elapsed)
    } else {
        None
    }
}

impl IqSource for PacedSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        // Do not swallow the inner source's own liveness signal (MAN-55).
        self.inner.confirmed_live_handle()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        // Start the recording clock here, on the first read, and BEFORE
        // the inner read runs -- the time the inner source itself spends
        // producing this buffer is part of the recording's own interval,
        // not something to sleep on top of.
        self.start.get_or_insert_with(Instant::now);
        let n = self.inner.read(buf)?;
        self.delivered += n as u64;
        // Sleep AFTER the read, against the count that INCLUDES this
        // buffer: the samples this call is about to hand back must have
        // had their own recording interval elapse first. Sleeping before
        // the read instead paced against the PREVIOUS call's samples, so
        // `listen`'s first read -- the whole two-second calibration
        // buffer -- returned at once and anything decoded from it could
        // reach the servers before a client could connect.
        if let Some(delay) = pacing_delay(self.delivered, self.fs, self.elapsed()) {
            std::thread::sleep(delay);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
    }

    impl IqSource for VecSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
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

    #[test]
    fn paced_source_delivers_the_same_samples_in_the_same_order() {
        // A high fake fs keeps this test fast -- pacing math is the same
        // regardless of rate.
        let samples: Vec<Complex32> = (0..500)
            .map(|i| Complex32::new(i as f32, -(i as f32)))
            .collect();
        let src = VecSource {
            samples: samples.clone(),
            cursor: 0,
            fs: 1_000_000.0,
        };
        let mut paced = PacedSource::new(Box::new(src));
        let drained = drain(&mut paced, 64);
        assert_eq!(drained, samples);
    }

    #[test]
    fn paced_source_takes_at_least_the_recording_duration() {
        let samples = vec![Complex32::new(0.0, 0.0); 2400];
        let src = VecSource {
            samples,
            cursor: 0,
            fs: 8000.0,
        };
        let mut paced = PacedSource::new(Box::new(src));
        let start = Instant::now();
        drain(&mut paced, 256);
        // 2400 samples at 8000 S/s = 0.3s. Generous lower bound only --
        // never assert an upper bound, that is CI-flaky.
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "elapsed {:?} was too short for a 0.3s recording",
            start.elapsed()
        );
    }

    #[test]
    fn paced_source_does_not_sleep_when_the_consumer_is_already_behind() {
        struct SlowSource {
            inner: VecSource,
        }
        impl IqSource for SlowSource {
            fn sample_rate(&self) -> f64 {
                self.inner.sample_rate()
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                std::thread::sleep(Duration::from_millis(200));
                self.inner.read(buf)
            }
        }
        // 800 samples at 8000 S/s = 0.1s of "recording" pacing, but the
        // single read() call already takes 0.2s -- pacing must add ~nothing
        // on top of that one call.
        let samples = vec![Complex32::new(0.0, 0.0); 800];
        let slow = SlowSource {
            inner: VecSource {
                samples,
                cursor: 0,
                fs: 8000.0,
            },
        };
        let mut paced = PacedSource::new(Box::new(slow));
        let mut buf = vec![Complex32::new(0.0, 0.0); 800];
        assert_eq!(paced.read(&mut buf).unwrap(), 800);
        // Asserted on the pacing DECISION, not on a wall-clock upper bound
        // for the whole call: `elapsed` is already >= the inner source's
        // own 200ms by the time `read()` returns, while the recording is
        // only worth 800/8000 = 100ms, so the next read's delay is
        // guaranteed to be None on any machine at any load. A `<280ms`
        // bound on the whole call asserted the same thing but could be
        // broken by an oversubscribed CI runner overshooting the 200ms
        // sleep rather than by pacing compounding.
        assert_eq!(
            pacing_delay(paced.delivered, paced.fs, paced.elapsed()),
            None,
            "pacing must ask for no delay once the consumer has fallen behind"
        );
        // The pure decision itself, pinned exactly: 100ms of recording
        // delivered, 200ms of wall-clock spent -- no delay, and no
        // compounding of the two.
        assert_eq!(pacing_delay(800, 8000.0, Duration::from_millis(200)), None);
        assert_eq!(
            pacing_delay(800, 8000.0, Duration::from_millis(40)),
            Some(Duration::from_millis(60)),
            "when the consumer is AHEAD, the delay is the remaining debt only"
        );
    }

    // Round-2 review: the FIRST read must be paced too. `listen`'s first
    // read asks for the whole two-second calibration buffer, and pacing
    // against the count BEFORE that buffer made it return instantly --
    // every sample of the first chunk was handed to the decoder at
    // startup, so a spot inside it could reach the servers before a client
    // could connect. Asserted on the single first call, not on a drain.
    #[test]
    fn paced_source_paces_the_very_first_read() {
        let samples = vec![Complex32::new(0.0, 0.0); 800];
        let src = VecSource {
            samples,
            cursor: 0,
            fs: 8000.0,
        };
        let mut paced = PacedSource::new(Box::new(src));
        let mut buf = vec![Complex32::new(0.0, 0.0); 800];
        let start = Instant::now();
        assert_eq!(paced.read(&mut buf).unwrap(), 800);
        // 800 samples at 8000 S/s = 0.1s of recording. Generous lower
        // bound only -- never an upper bound, that is CI-flaky.
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "the first read returned after {:?}, before its own 0.1s of \
             recording had elapsed",
            start.elapsed()
        );
    }

    // The recording clock must start at the FIRST READ, not at
    // construction. `manta-cli` builds the paced source, then resolves the
    // replay epoch, hashes the whole recording for the session nonce, and
    // binds the telnet/JSON listeners before `listen()` ever reads a
    // sample. Charging that setup time to the recording meant the first
    // buffer -- `listen`'s whole two-second calibration read -- came due
    // that much sooner, eating into the window a client has to connect
    // after the servers are actually up.
    #[test]
    fn paced_source_clock_starts_at_the_first_read_not_at_construction() {
        let samples = vec![Complex32::new(0.0, 0.0); 800];
        let src = VecSource {
            samples,
            cursor: 0,
            fs: 8000.0,
        };
        let mut paced = PacedSource::new(Box::new(src));
        // Stand in for the CLI's own between-construction-and-first-read
        // setup: epoch resolution, whole-file hashing, server bind.
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            paced.elapsed(),
            Duration::ZERO,
            "the recording clock must not run before the first read"
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 800];
        let start = Instant::now();
        assert_eq!(paced.read(&mut buf).unwrap(), 800);
        // 800 samples at 8000 S/s = 0.1s of recording, owed in FULL from
        // this point -- not reduced by the 150ms of setup above. Generous
        // lower bound only; never an upper bound, that is CI-flaky.
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "the first read returned after {:?}, so the 150ms of setup \
             before it was credited against the recording's own clock",
            start.elapsed()
        );
    }

    #[test]
    fn paced_source_passes_eof_through() {
        let samples = vec![Complex32::new(0.0, 0.0); 5];
        let src = VecSource {
            samples,
            cursor: 0,
            fs: 8000.0,
        };
        let mut paced = PacedSource::new(Box::new(src));
        let mut buf = vec![Complex32::new(0.0, 0.0); 5];
        assert_eq!(paced.read(&mut buf).unwrap(), 5);
        assert_eq!(paced.read(&mut buf).unwrap(), 0);
        // EOF must not advance the delivered counter, or every further
        // read at EOF would accrue a larger and larger sleep debt against
        // a source that has nothing left to give. Asserted on the counter
        // and on the pacing decision rather than on a wall-clock upper
        // bound for the EOF read, which a loaded CI runner can break for
        // reasons that have nothing to do with pacing.
        assert_eq!(paced.delivered, 5, "EOF must not advance `delivered`");
        // The whole debt a 5-sample read at 8 kS/s can ever ask for is
        // 5/8000 s, and it is already spent by the time EOF is reached.
        assert_eq!(
            pacing_delay(5, 8000.0, Duration::ZERO),
            Some(Duration::from_secs_f64(5.0 / 8000.0))
        );
        assert_eq!(
            pacing_delay(paced.delivered, paced.fs, paced.elapsed()),
            None,
            "the 625us debt is long spent -- EOF reads must not sleep"
        );
    }
}
