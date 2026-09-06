//! KiwiSDR network IQ source: connects to a public/private KiwiSDR receiver
//! over its WebSocket protocol, requests raw complex-IQ mode (`mod=iq`), and
//! rational-resamples the receiver's native (device-specific, non-round)
//! sample rate up to 96000 Hz before handing samples to the rest of the
//! pipeline. ARCHITECTURE §3, docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md.
//!
//! Protocol notes (real, live-verified against several public receivers
//! during implementation -- see docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md
//! for the full findings and the design spec for the original brainstorming):
//!
//! - Handshake: `ws://<host>:<port>/<timestamp>/SND`, then a sequence of
//!   `SET ...` **text** frames (`timestamp` is any process-unique value).
//!   Server responses (`MSG ...` parameter frames and `SND ...` IQ frames)
//!   both arrive as WebSocket **binary** frames with a 3-byte ASCII tag.
//! - `SET keepalive` must be sent roughly once a second for the life of the
//!   connection or the server stops sending `SND` frames.
//! - The real, per-device sample rate arrives as `MSG sample_rate=<float>`;
//!   it is not a round number and must never be hardcoded.
//! - **Critical, load-bearing finding beyond the original design spec**:
//!   the server also sends `MSG audio_rate=<int>` partway through its
//!   initial parameter batch, and the client **must** reply with
//!   `SET AR OK in=<audio_rate> out=<desired_rate>` or the server never
//!   starts streaming `SND` frames at all -- it silently closes the
//!   connection after a few seconds of otherwise-correct setup. This was
//!   not documented in the original design spec (whose brainstorming spike
//!   apparently got this "for free" some other way); it was found here by
//!   diffing wire traffic against the reference `jks-prv/kiwiclient` Python
//!   client's debug log against a real receiver. Without it, every `SET`
//!   command described in the design spec's handshake section is necessary
//!   but not sufficient.
//! - `SND` frame layout (confirmed against real captures *and* directly
//!   against `jks-prv/kiwiclient`'s `_process_aud` source, byte-for-byte):
//!   after the 3-byte `"SND"` tag: 1-byte flags, 4-byte little-endian seq,
//!   2-byte big-endian S-meter, then (IQ/stereo mode only) a 10-byte GPS
//!   block, then interleaved `I,Q,I,Q,...` 16-bit samples -- big-endian
//!   unless flags bit `0x80` is set (little-endian). Real captures were a
//!   constant 2068 bytes (20-byte header + 512 complex pairs) every frame.
//!
//! ## Threat model (MAN-60, docs/DECISIONS/2026-09-04-man60-kiwi-threat-model.md)
//!
//! This client connects outbound to an operator-chosen, third-party-operated
//! receiver and processes its server-controlled WebSocket/MSG/SND frames --
//! untrusted input from infrastructure manta doesn't control, the same risk
//! shape as MAN-11 (HPSDR)/MAN-12 (telnet/JSON-WS). Unlike a bad UDP
//! datagram, a WebSocket protocol violation is connection-terminal (RFC 6455
//! requires *failing the connection*, and tungstenite's state machine does
//! so) -- so "discard and keep reading the same socket" (the HPSDR fix
//! pattern) only applies to frames tungstenite itself accepts but this
//! module finds unusable (`MAX_CONSECUTIVE_UNUSABLE_FRAMES`, below). For a
//! rejected frame or a clean `Close`, this module bounds-and-reconnects
//! instead (`ReconnectPolicy`), zero-filling the outage so `Spot.sample_ts`
//! (the only spot time base) doesn't silently drift. See the doc for the
//! full STRIDE pass and every finding's disposition, including the
//! plaintext-password-over-`ws://` exposure (accepted risk, narrowed by
//! `MANTA_KIWI_PASSWORD` for the local argv-visibility half).

use crate::IqSource;
use anyhow::{anyhow, bail, Context, Result};
use num_complex::Complex32;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// Target output rate after resampling -- SPEC-decode-core.md §1.1's table
/// rate nearest KiwiSDR's native ~12 kHz.
const TARGET_RATE_HZ: usize = 96_000;

/// Resampler chunk size (in complex sample-pairs) fed to `rubato::Fft` per
/// `process_into_buffer` call.
///
/// **Real, empirically-verified finding** (not the naive guess of matching
/// KiwiSDR's native 512-sample SND frame size, and not simply "bigger is
/// better" -- see docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md for the
/// full derivation): with `FixedSync::Input`, `rubato::Fft`'s internal FFT block
/// size for the input side is (for the always-integer-Hz KiwiSDR rate,
/// gcd(rate_in, 96000) == 1 for essentially every real device, since a
/// crystal-derived rate near 12000 Hz shares no factors with
/// 96000 = 2^8*3*5^3) *equal to the rounded input rate itself* (~12000).
/// `rubato::Fft::new`'s chosen `chunk_size` must be >= that internal block
/// size for the resampler to produce output starting from its very first
/// `process_into_buffer` call; smaller chunk sizes (512, 4096, 8192 were
/// all measured) don't *break* anything, but do delay first real output by
/// however many extra calls it takes to internally accumulate ~12000
/// samples (confirmed: chunk=512 took 23 empty calls before any output).
/// 16384 comfortably exceeds any real KiwiSDR's native rate (measured today
/// against 3 live receivers: 11998.860-11998.964 Hz) with headroom for
/// device-to-device variation, while still being small enough that the
/// per-call working set is trivial.
///
/// Separately, and NOT controlled by this constant: `rubato::Fft::new`
/// reports `output_delay() == 48000` (0.5 s at 96 kHz) regardless of the
/// chunk size chosen here -- confirmed by direct testing across
/// {512, 4096, 8192, 11999, 12000, 16384, 32768}. That delay is a fixed
/// property of resampling between two coprime rates with the exact
/// rational `Fft` resampler (the smallest valid FFT block pair for a
/// gcd-1 rate pair is `(rate_in, rate_out)` itself), not something any
/// chunk-size choice can reduce -- callers needing to account for KiwiSDR
/// startup latency (analogous to `CALIBRATION_SECONDS`) should budget for
/// this ~0.5 s regardless of `RESAMPLER_CHUNK`.
const RESAMPLER_CHUNK: usize = 16_384;

/// TCP read timeout: bounds how long a single `socket.read()` call blocks,
/// keeping `read()` responsive to repeated calls (and, at the engine layer,
/// to Ctrl-C) rather than blocking indefinitely on a stalled network.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound on consecutive read timeouts (no data at all, no SND, no MSG)
/// before `read()` gives up and returns a real `Err`. At 250 ms per
/// timeout, 40 gives a ~10 s bound: long enough to ride out a single slow
/// frame or a burst of keepalive-only traffic, short enough that a truly
/// dead connection surfaces an error instead of hanging the caller forever.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 40;

/// How often `SET keepalive` must be resent or the server stops streaming
/// `SND` frames (confirmed live: an initial send is not enough).
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1000);

/// Caps inbound WebSocket frame/message size on the client side. Real SND
/// frames are a constant 2068 bytes (20-byte header + 512 complex pairs --
/// docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md), so 64 KiB is ~30x
/// headroom for parameter-batch MSG frames and device-to-device variation,
/// while being far below tungstenite's un-overridden 64 MiB message /
/// 16 MiB frame defaults. Same convention, and same reasoning, as this
/// workspace's server-side listener (manta-server/src/json_stream.rs's
/// `MAX_INBOUND_WS_MESSAGE_BYTES`): never inherit a third-party library's
/// size ceiling for untrusted input. MAN-60 finding 3.
const MAX_WS_FRAME_BYTES: usize = 64 * 1024;

/// Bound on consecutive frames that are neither a usable `SND` (>= 1
/// sample) nor a `MSG` carrying a recognized key, before `read()` treats
/// the connection as lost. Mirrors HPSDR's `MAX_CONSECUTIVE_MALFORMED`
/// (`hpsdr.rs`) exactly, for the same reason: a count bound, not a
/// wall-clock one, because unusable-frame arrival rate is peer-controlled.
/// A healthy receiver sends unusable frames routinely (an unrecognized MSG
/// key is common and harmless) but never this many consecutively with zero
/// usable SND/MSG in between. MAN-60 finding 2.
const MAX_CONSECUTIVE_UNUSABLE_FRAMES: u32 = 10_000;

/// Bounds a single TCP connect attempt (initial or reconnect). Without
/// this, `TcpStream::connect`'s OS-level timeout (which can run to minutes)
/// would make the "bounded recovery budget" below nominal rather than real.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum reconnect attempts before `read()` gives up with a real `Err`.
/// Resets only when a productive `SND` frame is seen (never merely on a
/// successful handshake) -- see `KiwiIqSource::reconnect`'s doc comment for
/// why that distinction matters. MAN-60 finding 1.
const MAX_RECONNECT_ATTEMPTS: u32 = 4;

/// Initial backoff before the first reconnect attempt, doubling per
/// subsequent attempt up to `RECONNECT_BACKOFF_MAX`.
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Backoff cap. Deliberately small: `read()` is blocking and
/// `manta_engine::listen` only checks its Ctrl-C flag between `read()`
/// calls, so this cap is also the worst-case shutdown latency while a
/// reconnect is in backoff.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// Caps how much zero-filled outage padding a single reconnect can emit, so
/// an unrealistic outage (e.g. a suspended laptop) can't force a multi-
/// minute burst of synthetic silence into the decode pipeline.
const MAX_ZERO_FILL_SECONDS: u64 = 60;

/// Observability counters for one `KiwiIqSource`. MAN-60; mirrors
/// `HpsdrIqSource::gap_stats()`'s role for the HPSDR driver -- lets callers
/// (and tests) distinguish "discarded and counted" from "silently dropped".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KiwiStats {
    /// Frames received that yielded neither samples nor a recognized MSG key.
    pub unusable_frames: u64,
    /// Connection-level failures (rejected frame, `Close`, stall, etc.)
    /// that triggered a reconnect attempt.
    pub connection_failures: u64,
    /// Successful reconnects.
    pub reconnects: u64,
    /// Zero samples emitted to cover a reconnect outage.
    pub zero_filled_samples: u64,
}

/// Bounded-recovery policy for `connect`/`reconnect`. Split out so tests can
/// drive a fast policy without `#[cfg(test)]` branches in production code
/// paths; `KiwiIqSource::connect` always uses `Default`.
#[derive(Debug, Clone, Copy)]
struct ReconnectPolicy {
    max_attempts: u32,
    backoff_base: Duration,
    backoff_max: Duration,
    connect_timeout: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        ReconnectPolicy {
            max_attempts: MAX_RECONNECT_ATTEMPTS,
            backoff_base: RECONNECT_BACKOFF_BASE,
            backoff_max: RECONNECT_BACKOFF_MAX,
            connect_timeout: CONNECT_TIMEOUT,
        }
    }
}

/// A KiwiSDR receiver (network SDR) as an `IqSource`. ARCHITECTURE §3.
pub struct KiwiIqSource {
    socket: WebSocket<TcpStream>,
    host: String,
    port: u16,
    password: String,
    policy: ReconnectPolicy,
    fs: f64,
    center_freq_hz: f64,
    resampler: Fft<f32>,
    /// Un-resampled input accumulator: interleaved `[I0, Q0, I1, Q1, ...]`.
    raw: Vec<f32>,
    /// Resampled-output chunk assembler; `read()` drains from here.
    pending: VecDeque<Complex32>,
    /// Samples of synthetic silence still owed to cover a just-recovered
    /// outage (see `reconnect`'s doc comment).
    zero_fill_owed: u64,
    /// Reconnect attempts made since the last productive `SND` frame.
    reconnect_attempts: u32,
    stats: KiwiStats,
    last_keepalive: Instant,
}

/// A frame's outcome once dispatched, driving both `MAX_CONSECUTIVE_UNUSABLE_FRAMES`
/// (any productive frame resets it) and `reconnect_attempts` (only a
/// productive **SND** frame resets that -- see `reconnect`'s doc comment).
enum FrameOutcome {
    ProductiveSnd,
    ProductiveMsg,
    Unusable,
    ConnectionEnded,
}

/// The result of trying to keep one connection's data flowing.
enum PumpError {
    /// This connection is finished; a reconnect may recover it. Covers a
    /// clean `Close`, any tungstenite protocol/capacity/IO error, a send
    /// failure, the silence stall, or the unusable-frame bound.
    Lost(anyhow::Error),
    /// Not connection-related (resampler misuse) -- reconnecting cannot help.
    Fatal(anyhow::Error),
}

impl KiwiIqSource {
    /// Connect to a KiwiSDR receiver, complete the SND-channel handshake in
    /// `mod=iq` (raw complex IQ), and construct the rational resampler up
    /// to `TARGET_RATE_HZ`. `password` is `""` for anonymous/no-password
    /// receivers (most public ones).
    pub fn connect(host: &str, port: u16, center_freq_hz: f64, password: &str) -> Result<Self> {
        Self::connect_with_policy(
            host,
            port,
            center_freq_hz,
            password,
            ReconnectPolicy::default(),
        )
    }

    fn connect_with_policy(
        host: &str,
        port: u16,
        center_freq_hz: f64,
        password: &str,
        policy: ReconnectPolicy,
    ) -> Result<Self> {
        let (socket, rate_in_hz) = handshake(host, port, center_freq_hz, password, policy)?;
        let resampler = build_resampler(rate_in_hz)?;

        Ok(KiwiIqSource {
            socket,
            host: host.to_string(),
            port,
            password: password.to_string(),
            policy,
            fs: TARGET_RATE_HZ as f64,
            center_freq_hz,
            resampler,
            raw: Vec::new(),
            pending: VecDeque::new(),
            zero_fill_owed: 0,
            reconnect_attempts: 0,
            stats: KiwiStats::default(),
            last_keepalive: Instant::now(),
        })
    }

    /// Observability counters for this source. MAN-60.
    pub fn stats(&self) -> KiwiStats {
        self.stats
    }

    fn send_keepalive_if_due(&mut self) -> Result<()> {
        if self.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            self.socket
                .send(Message::Text("SET keepalive".to_string().into()))
                .context("send SET keepalive")?;
            self.last_keepalive = Instant::now();
        }
        Ok(())
    }

    /// Handle one `MSG` frame's text: acknowledge the audio rate (required
    /// for the server to ever start streaming `SND`, see module docs) and
    /// report whether `text` carried a key this module actually acts on
    /// (MAN-60 finding 2's productive/unusable classification). Real,
    /// unknown MSG keys are common and harmless, but never "productive".
    fn handle_msg(&mut self, text: &str) -> Result<bool> {
        let audio_rate_seen = ack_audio_rate_if_present(&mut self.socket, text)?;
        let sample_rate_seen = parse_kv_f64(text, "sample_rate").is_some();
        Ok(audio_rate_seen || sample_rate_seen)
    }

    /// Feed newly-parsed raw (un-resampled) samples through the resampler,
    /// draining consumed input from `self.raw` and pushing resampled
    /// output onto `self.pending`, per RESAMPLER_CHUNK's doc comment.
    fn drain_resampler(&mut self) -> Result<()> {
        loop {
            let need = self.resampler.input_frames_next();
            if self.raw.len() < need * 2 {
                return Ok(());
            }
            let out_frames = self.resampler.output_frames_next();
            let mut out_buf = vec![0f32; out_frames * 2];
            let in_adapter = InterleavedSlice::new(&self.raw[..need * 2], 2, need)
                .map_err(|e| anyhow!("resampler input adapter: {e}"))?;
            let mut out_adapter = InterleavedSlice::new_mut(&mut out_buf, 2, out_frames)
                .map_err(|e| anyhow!("resampler output adapter: {e}"))?;
            let (used_in, produced_out) = self
                .resampler
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
                .map_err(|e| anyhow!("resampler process_into_buffer: {e}"))?;
            self.raw.drain(0..used_in * 2);
            for i in 0..produced_out {
                self.pending
                    .push_back(Complex32::new(out_buf[2 * i], out_buf[2 * i + 1]));
            }
        }
    }

    /// Classify and act on one dispatched frame. `Err` here means "not
    /// connection-related" (a resampler invariant violation) -- the caller
    /// maps it to `PumpError::Fatal`, never a reconnect.
    fn classify_and_apply(&mut self, msg: Message) -> Result<FrameOutcome> {
        match msg {
            Message::Binary(b) if b.len() >= 3 && &b[0..3] == b"SND" => {
                let samples = parse_snd_frame(&b[3..]);
                if samples.is_empty() {
                    return Ok(FrameOutcome::Unusable);
                }
                for s in samples {
                    self.raw.push(s.re);
                    self.raw.push(s.im);
                }
                self.drain_resampler()?;
                Ok(FrameOutcome::ProductiveSnd)
            }
            Message::Binary(b) if b.len() >= 3 && &b[0..3] == b"MSG" => {
                let text = String::from_utf8_lossy(&b[3..]).into_owned();
                if self.handle_msg(&text)? {
                    Ok(FrameOutcome::ProductiveMsg)
                } else {
                    Ok(FrameOutcome::Unusable)
                }
            }
            Message::Binary(_) => Ok(FrameOutcome::Unusable),
            Message::Text(_) => Ok(FrameOutcome::Unusable),
            Message::Ping(_) | Message::Pong(_) => Ok(FrameOutcome::Unusable),
            Message::Close(_) => Ok(FrameOutcome::ConnectionEnded),
            Message::Frame(_) => Ok(FrameOutcome::Unusable),
        }
    }

    /// Drain real, already-resampled output first -- it's legitimate and
    /// predates any outage. Never returns `Some(0)` (would read as EOF at
    /// the engine layer).
    fn take_pending(&mut self, buf: &mut [Complex32]) -> Option<usize> {
        let n = self.pending.len().min(buf.len());
        if n == 0 {
            return None;
        }
        for slot in buf.iter_mut().take(n) {
            if let Some(s) = self.pending.pop_front() {
                *slot = s;
            }
        }
        Some(n)
    }

    /// Drain owed zero-fill padding (a reconnect outage's compensation --
    /// see `reconnect`'s doc comment). Never returns `Some(0)`.
    fn take_zero_fill(&mut self, buf: &mut [Complex32]) -> Option<usize> {
        if self.zero_fill_owed == 0 {
            return None;
        }
        let n = (self.zero_fill_owed as usize).min(buf.len());
        for slot in buf.iter_mut().take(n) {
            *slot = Complex32::new(0.0, 0.0);
        }
        self.zero_fill_owed -= n as u64;
        self.stats.zero_filled_samples += n as u64;
        Some(n)
    }

    /// Read and dispatch frames until at least one produces output into
    /// `self.pending`, or the connection is judged lost/fatal.
    fn pump_until_samples(&mut self) -> Result<(), PumpError> {
        let mut consecutive_timeouts = 0u32;
        let mut consecutive_unusable = 0u32;
        loop {
            if !self.pending.is_empty() {
                return Ok(());
            }

            self.send_keepalive_if_due().map_err(PumpError::Lost)?;

            let msg = match self.socket.read() {
                Ok(m) => {
                    consecutive_timeouts = 0;
                    m
                }
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    consecutive_timeouts += 1;
                    if consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                        return Err(PumpError::Lost(anyhow!(
                            "KiwiSDR {}:{} stalled: no data for {} consecutive read timeouts",
                            self.host,
                            self.port,
                            MAX_CONSECUTIVE_TIMEOUTS
                        )));
                    }
                    continue;
                }
                Err(e) => {
                    return Err(PumpError::Lost(anyhow::Error::new(e).context(format!(
                        "KiwiSDR {}:{} WebSocket read",
                        self.host, self.port
                    ))));
                }
            };

            match self.classify_and_apply(msg) {
                Ok(FrameOutcome::ProductiveSnd) => {
                    consecutive_unusable = 0;
                    // See `reconnect`'s doc comment: only a real SND frame
                    // resets the reconnect budget, not merely completing a
                    // handshake -- otherwise a receiver that accepts and
                    // immediately closes yields an infinite reconnect loop.
                    self.reconnect_attempts = 0;
                }
                Ok(FrameOutcome::ProductiveMsg) => {
                    consecutive_unusable = 0;
                }
                Ok(FrameOutcome::Unusable) => {
                    consecutive_unusable += 1;
                    self.stats.unusable_frames += 1;
                    if consecutive_unusable >= MAX_CONSECUTIVE_UNUSABLE_FRAMES {
                        return Err(PumpError::Lost(anyhow!(
                            "KiwiSDR {}:{} stalled: {} consecutive unusable frames with no usable SND/MSG data",
                            self.host,
                            self.port,
                            MAX_CONSECUTIVE_UNUSABLE_FRAMES
                        )));
                    }
                }
                Ok(FrameOutcome::ConnectionEnded) => {
                    return Err(PumpError::Lost(anyhow!(
                        "KiwiSDR {}:{} closed the connection",
                        self.host,
                        self.port
                    )));
                }
                Err(e) => return Err(PumpError::Fatal(e)),
            }
        }
    }

    /// Recover from a lost connection with a bounded number of reconnect
    /// attempts, doubling backoff between them, before giving up.
    ///
    /// Design (MAN-60 finding 1):
    /// - A WebSocket protocol violation is connection-terminal (RFC 6455
    ///   requires *failing the connection*; tungstenite's state machine
    ///   does so), unlike a bad UDP datagram -- so unlike HPSDR's
    ///   discard-and-continue fix, there is no usable socket left to keep
    ///   reading; reconnecting is the only way to satisfy "continues
    ///   operating normally". A clean `Close` is treated identically: a
    ///   receiver rebooting or an operator kicking the session is a
    ///   realistic non-adversarial event that today is indistinguishable
    ///   from an attack, and both should recover the same way.
    /// - `self.reconnect_attempts` persists across separate calls to this
    ///   method and resets ONLY when a productive SND frame is later seen
    ///   (`pump_until_samples`), never merely on a successful handshake.
    ///   Otherwise a receiver that accepts and immediately closes on every
    ///   connection would hand out an unlimited number of "successful"
    ///   reconnects and never trip the bound.
    /// - The outage is zero-filled: `Spot.sample_ts` (samples since session
    ///   start) is the only spot time base, converted to wall clock at the
    ///   server boundary as `epoch + sample_ts / fs`
    ///   (`manta-server/src/bus.rs`). A reconnect that emitted no samples
    ///   would silently back-date every subsequent spot by the outage
    ///   duration -- trading a crash for a correctness bug. Capped at
    ///   `MAX_ZERO_FILL_SECONDS` so an unrealistic outage can't force a
    ///   multi-minute burst of synthetic silence.
    fn reconnect(&mut self, cause: anyhow::Error) -> Result<()> {
        self.stats.connection_failures += 1;
        tracing::warn!(
            host = %self.host,
            port = self.port,
            cause = %cause,
            "KiwiSDR connection lost, reconnecting"
        );
        let outage_started = Instant::now();
        self.raw.clear();

        loop {
            if self.reconnect_attempts >= self.policy.max_attempts {
                bail!(
                    "KiwiSDR {}:{} unrecoverable after {} reconnect attempts: {cause}",
                    self.host,
                    self.port,
                    self.policy.max_attempts
                );
            }
            let backoff = backoff_for_attempt(self.policy, self.reconnect_attempts);
            if backoff > Duration::ZERO {
                std::thread::sleep(backoff);
            }
            self.reconnect_attempts += 1;

            match handshake(
                &self.host,
                self.port,
                self.center_freq_hz,
                &self.password,
                self.policy,
            ) {
                Ok((socket, rate_in_hz)) => {
                    let resampler = build_resampler(rate_in_hz)?;
                    self.socket = socket;
                    self.resampler = resampler;
                    self.last_keepalive = Instant::now();
                    let outage = outage_started.elapsed();
                    let owed = (outage.as_secs_f64().min(MAX_ZERO_FILL_SECONDS as f64)
                        * TARGET_RATE_HZ as f64) as u64;
                    self.zero_fill_owed += owed;
                    self.stats.reconnects += 1;
                    tracing::info!(
                        host = %self.host,
                        port = self.port,
                        attempt = self.reconnect_attempts,
                        outage_ms = outage.as_millis() as u64,
                        "KiwiSDR reconnected"
                    );
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        host = %self.host,
                        port = self.port,
                        attempt = self.reconnect_attempts,
                        error = %e,
                        "KiwiSDR reconnect attempt failed"
                    );
                }
            }
        }
    }
}

/// Backoff before reconnect attempt number `attempt` (0-indexed): doubling
/// from `policy.backoff_base`, capped at `policy.backoff_max`.
fn backoff_for_attempt(policy: ReconnectPolicy, attempt: u32) -> Duration {
    policy
        .backoff_base
        .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .min(policy.backoff_max)
}

fn build_resampler(rate_in_hz: usize) -> Result<Fft<f32>> {
    if rate_in_hz == 0 {
        bail!("KiwiSDR reported sample_rate=0");
    }
    Fft::<f32>::new(
        rate_in_hz,
        TARGET_RATE_HZ,
        RESAMPLER_CHUNK,
        2,
        FixedSync::Input,
    )
    .map_err(|e| anyhow!("construct KiwiSDR resampler ({rate_in_hz} -> {TARGET_RATE_HZ} Hz): {e}"))
}

/// Resolve `host:port` and connect with a bounded timeout -- DNS resolution
/// itself stays unbounded (std offers no alternative; see the threat-model
/// doc's filed follow-up), but the TCP connect phase no longer relies on
/// the OS's own (potentially multi-minute) connect timeout.
fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}:{port}"))?
        .collect();
    if addrs.is_empty() {
        bail!("{host}:{port} resolved to no addresses");
    }
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(tcp) => return Ok(tcp),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("addrs is non-empty, so the loop ran at least once"))
        .with_context(|| format!("TCP connect to {host}:{port}"))
}

/// Complete one full KiwiSDR handshake over a fresh TCP connection: connect,
/// WebSocket upgrade with an explicit size bound (MAN-60 finding 3), send
/// auth, wait for `sample_rate=` (acking `audio_rate=` along the way, see
/// module docs), then send the rest of the `SET` batch. Used by both
/// `connect_with_policy` (first connection) and `reconnect` (every
/// subsequent one) so the two can't drift apart.
fn handshake(
    host: &str,
    port: u16,
    center_freq_hz: f64,
    password: &str,
    policy: ReconnectPolicy,
) -> Result<(WebSocket<TcpStream>, usize)> {
    let tcp = connect_tcp(host, port, policy.connect_timeout)?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))
        .context("set TCP read timeout")?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let url = format!("ws://{host}:{port}/{timestamp}/SND");
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_FRAME_BYTES))
        .max_frame_size(Some(MAX_WS_FRAME_BYTES));
    let (mut socket, _resp) = tungstenite::client::client_with_config(url, tcp, Some(ws_config))
        .context("KiwiSDR WebSocket handshake")?;

    socket
        .send(Message::Text(
            format!("SET auth t=kiwi p={password}").into(),
        ))
        .context("send SET auth")?;

    // Read frames until the server reports the real, device-specific
    // sample rate. Everything else in the initial parameter batch
    // (rx_chans, chan_no_pwd, load_cfg, ...) is handled by read()'s
    // ongoing MSG dispatch once streaming begins -- EXCEPT audio_rate,
    // which routinely arrives in this same initial batch (order across
    // real nodes is not guaranteed relative to sample_rate) and, per the
    // module docs, MUST be ack'd via `SET AR OK` or the server silently
    // stops streaming a few seconds later.
    //
    // This loop mirrors read()'s bounded-timeout retry (see
    // MAX_CONSECUTIVE_TIMEOUTS) rather than propagating a single read
    // timeout as a hard error: ordinary network jitter during the
    // handshake shouldn't be fatal on a real node with higher latency
    // than the ones this was tested against.
    let mut consecutive_timeouts = 0u32;
    let rate_in_hz = loop {
        let msg = match socket.read() {
            Ok(m) => {
                consecutive_timeouts = 0;
                m
            }
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                consecutive_timeouts += 1;
                if consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                    bail!(
                        "KiwiSDR {host}:{port} handshake stalled: no data for {} consecutive read timeouts",
                        MAX_CONSECUTIVE_TIMEOUTS
                    );
                }
                continue;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("read during KiwiSDR {host}:{port} handshake"))
            }
        };
        let Message::Binary(b) = msg else {
            continue;
        };
        if b.len() < 3 || &b[0..3] != b"MSG" {
            continue;
        }
        let text = String::from_utf8_lossy(&b[3..]).into_owned();
        // `self` doesn't exist yet at this point (the resampler/rate
        // aren't known until this loop finds sample_rate=), so this can't
        // call a method; both this loop and `handle_msg` instead delegate
        // to the shared free function below so the audio_rate-ack logic
        // lives in exactly one place.
        ack_audio_rate_if_present(&mut socket, &text)?;
        if let Some(rate) = parse_kv_f64(&text, "sample_rate") {
            break rate.round() as usize;
        }
    };

    // NOTE: order deliberately differs from the design spec (which has
    // all four SET commands sent up front, before waiting on
    // sample_rate=). Live testing against real receivers required
    // sending `SET auth` alone first and waiting for sample_rate= to
    // come back before sending the rest -- sending everything up front
    // was not what was verified working end-to-end, so this order is
    // kept as-is rather than "corrected" back to the spec's sequence.
    //
    // ident_user/squelch/genattn/gen are undocumented in the design
    // spec; they were copied from the reference `jks-prv/kiwiclient`
    // client's handshake during live debugging of the audio_rate/SET AR
    // OK issue (see module docs) to maximize fidelity with a known-
    // working client while chasing that bug, and left in since removing
    // them was never verified safe against real nodes.
    for cmd in [
        "SET ident_user=manta".to_string(),
        format!(
            "SET mod=iq low_cut=-5000 high_cut=5000 freq={:.3}",
            center_freq_hz / 1000.0
        ),
        "SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50".to_string(),
        "SET squelch=0 max=0".to_string(),
        "SET genattn=0".to_string(),
        "SET gen=0 mix=-1".to_string(),
        "SET compression=0".to_string(),
        "SET keepalive".to_string(),
    ] {
        socket
            .send(Message::Text(cmd.into()))
            .context("send post-handshake SET command")?;
    }

    Ok((socket, rate_in_hz))
}

/// Shared by `handshake`'s parameter loop (before a `KiwiIqSource` exists)
/// and `handle_msg` (after): if `text` carries `audio_rate=`, reply with the
/// `SET AR OK` ack the server requires before it will ever start streaming
/// `SND` frames (see module docs). Returns whether `audio_rate=` was
/// present, so callers can fold it into MAN-60's productive/unusable
/// frame classification.
fn ack_audio_rate_if_present(socket: &mut WebSocket<TcpStream>, text: &str) -> Result<bool> {
    if let Some(rate) = parse_kv_f64(text, "audio_rate") {
        let cmd = format!("SET AR OK in={} out={TARGET_RATE_HZ}", rate as i64);
        socket
            .send(Message::Text(cmd.into()))
            .context("send SET AR OK")?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Parse `key=value` (whitespace-separated `MSG` parameter text) for `key`,
/// returning its value as `f64`. Used for both `sample_rate` (float) and
/// `audio_rate` (integer, but read as float for a single code path).
fn parse_kv_f64(text: &str, key: &str) -> Option<f64> {
    let prefix = format!("{key}=");
    text.split_whitespace()
        .find_map(|kv| kv.strip_prefix(prefix.as_str()))
        .and_then(|v| v.parse().ok())
}

/// Parse an SND frame's bytes (after the 3-byte `"SND"` tag) into raw,
/// un-resampled complex samples normalized to roughly [-1, 1] (matching
/// `WavIqSource`'s i16 convention). See module docs for the byte layout.
fn parse_snd_frame(body: &[u8]) -> Vec<Complex32> {
    const HEADER_LEN: usize = 1 + 4 + 2 + 10; // flags + seq + smeter + gps
    if body.len() <= HEADER_LEN {
        return Vec::new();
    }
    let flags = body[0];
    let little_endian = flags & 0x80 != 0;
    let payload = &body[HEADER_LEN..];
    let n_pairs = payload.len() / 4;
    let mut out = Vec::with_capacity(n_pairs);
    for i in 0..n_pairs {
        let off = i * 4;
        let ib = [payload[off], payload[off + 1]];
        let qb = [payload[off + 2], payload[off + 3]];
        let (i_raw, q_raw) = if little_endian {
            (i16::from_le_bytes(ib), i16::from_le_bytes(qb))
        } else {
            (i16::from_be_bytes(ib), i16::from_be_bytes(qb))
        };
        out.push(Complex32::new(
            i_raw as f32 / 32768.0,
            q_raw as f32 / 32768.0,
        ));
    }
    out
}

impl IqSource for KiwiIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        loop {
            if let Some(n) = self.take_pending(buf) {
                return Ok(n);
            }
            if let Some(n) = self.take_zero_fill(buf) {
                return Ok(n);
            }
            match self.pump_until_samples() {
                Ok(()) => continue,
                Err(PumpError::Fatal(e)) => return Err(e),
                Err(PumpError::Lost(cause)) => self.reconnect(cause)?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const FAKE_AUDIO_RATE: u32 = 12_000;
    const FAKE_SAMPLE_RATE: f64 = 11_998.937_786;

    fn fast_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: 3,
            backoff_base: Duration::from_millis(10),
            backoff_max: Duration::from_millis(30),
            connect_timeout: Duration::from_millis(500),
        }
    }

    fn send_msg(ws: &mut WebSocket<TcpStream>, text: &str) {
        let mut body = b"MSG".to_vec();
        body.extend_from_slice(text.as_bytes());
        let _ = ws.send(Message::Binary(body.into()));
    }

    /// A real-geometry SND frame carrying one repeated complex value.
    fn valid_snd_frame(value: Complex32) -> Message {
        let mut body = b"SND".to_vec();
        body.push(0x00); // flags: big-endian, stereo/IQ
        body.extend_from_slice(&0u32.to_le_bytes()); // seq
        body.extend_from_slice(&0i16.to_be_bytes()); // smeter
        body.extend_from_slice(&[0u8; 10]); // gps block
        let i = (value.re * 32767.0) as i16;
        let q = (value.im * 32767.0) as i16;
        for _ in 0..512 {
            body.extend_from_slice(&i.to_be_bytes());
            body.extend_from_slice(&q.to_be_bytes());
        }
        Message::Binary(body.into())
    }

    fn stream_valid_iq(ws: &mut WebSocket<TcpStream>, frames: usize) {
        for _ in 0..frames {
            if ws.send(valid_snd_frame(Complex32::new(0.1, -0.1))).is_err() {
                return;
            }
        }
    }

    fn unknown_tag_frame() -> Message {
        Message::Binary(b"XYZ0123456789".to_vec().into())
    }

    fn short_snd_frame() -> Message {
        let mut body = b"SND".to_vec();
        body.extend_from_slice(&[0u8; 5]);
        Message::Binary(body.into())
    }

    fn msg_no_recognized_key() -> Message {
        let mut body = b"MSG".to_vec();
        body.extend_from_slice(b" some_other_key=123");
        Message::Binary(body.into())
    }

    fn invalid_utf8_msg_frame() -> Message {
        let mut body = b"MSG".to_vec();
        body.push(0xFF);
        body.push(0xFE);
        Message::Binary(body.into())
    }

    /// Reads and records every inbound client text frame for at least
    /// `min_wait`, returning once nothing more has arrived by the deadline.
    fn drain_client_frames(
        ws: &mut WebSocket<TcpStream>,
        sent: &Arc<Mutex<Vec<String>>>,
        min_wait: Duration,
    ) {
        let deadline = Instant::now() + min_wait;
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => sent.lock().unwrap().push(t.to_string()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() >= deadline {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// Completes the same handshake `handshake()` expects: records the
    /// `SET auth` line, replies with `audio_rate=`/`sample_rate=`, then
    /// drains the post-handshake `SET` batch. `delay` (if any) is applied
    /// AFTER the WS upgrade but BEFORE replying with the parameter MSGs --
    /// the client's own `handshake()` retries on silence there (bounded by
    /// `MAX_CONSECUTIVE_TIMEOUTS`, ~10s), unlike the WS upgrade itself
    /// (`tungstenite::client_with_config`'s one-shot blocking read, not
    /// retried by this module), so this is where a test can manufacture a
    /// realistic slow-reconnect outage without spuriously failing the
    /// upgrade.
    fn complete_handshake(
        ws: &mut WebSocket<TcpStream>,
        sent: &Arc<Mutex<Vec<String>>>,
        delay: Duration,
    ) {
        drain_client_frames(ws, sent, Duration::from_millis(300));
        if delay > Duration::ZERO {
            std::thread::sleep(delay);
        }
        send_msg(ws, &format!(" audio_init=0 audio_rate={FAKE_AUDIO_RATE}"));
        send_msg(ws, &format!(" sample_rate={FAKE_SAMPLE_RATE}"));
        drain_client_frames(ws, sent, Duration::from_millis(300));
    }

    /// A scripted, loopback KiwiSDR receiver. Completes the handshake
    /// `KiwiIqSource::connect`/`reconnect` requires, then hands control to
    /// `scenario` for the rest of each accepted connection's life. Accepts
    /// repeatedly, so a test can exercise reconnect. `stop` lets a
    /// long-running scenario (a flood, an accept-forever loop) notice
    /// shutdown and return promptly.
    struct FakeKiwi {
        port: u16,
        sent_by_client: Arc<Mutex<Vec<String>>>,
        accepts: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeKiwi {
        fn spawn<S>(scenario: S) -> Self
        where
            S: Fn(usize, &mut WebSocket<TcpStream>, &Arc<Mutex<Vec<String>>>, &Arc<AtomicBool>)
                + Send
                + Sync
                + 'static,
        {
            Self::spawn_with_delay(|_| Duration::ZERO, scenario)
        }

        /// Like `spawn`, but delays completing connection `idx`'s handshake
        /// by `delay_for(idx)` -- used to manufacture a measurable outage
        /// for the zero-fill test.
        fn spawn_with_delay<D, S>(delay_for: D, scenario: S) -> Self
        where
            D: Fn(usize) -> Duration + Send + Sync + 'static,
            S: Fn(usize, &mut WebSocket<TcpStream>, &Arc<Mutex<Vec<String>>>, &Arc<AtomicBool>)
                + Send
                + Sync
                + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let port = listener.local_addr().expect("local_addr").port();
            let sent_by_client = Arc::new(Mutex::new(Vec::new()));
            let accepts = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));

            let sent_clone = sent_by_client.clone();
            let accepts_clone = accepts.clone();
            let stop_clone = stop.clone();
            let handle = std::thread::spawn(move || {
                let mut idx = 0usize;
                loop {
                    if stop_clone.load(Ordering::Relaxed) {
                        return;
                    }
                    match listener.accept() {
                        Ok((tcp, _addr)) => {
                            accepts_clone.fetch_add(1, Ordering::Relaxed);
                            let this_idx = idx;
                            idx += 1;
                            tcp.set_read_timeout(Some(Duration::from_millis(200))).ok();
                            let mut ws = match tungstenite::accept(tcp) {
                                Ok(ws) => ws,
                                Err(_) => continue,
                            };
                            complete_handshake(&mut ws, &sent_clone, delay_for(this_idx));
                            scenario(this_idx, &mut ws, &sent_clone, &stop_clone);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            FakeKiwi {
                port,
                sent_by_client,
                accepts,
                stop,
                handle: Some(handle),
            }
        }

        fn port(&self) -> u16 {
            self.port
        }

        fn accepts(&self) -> usize {
            self.accepts.load(Ordering::Relaxed)
        }

        fn client_lines(&self) -> Vec<String> {
            self.sent_by_client.lock().unwrap().clone()
        }
    }

    impl Drop for FakeKiwi {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn connect_refused_is_a_clean_error() {
        // Nothing listens on port 1 -- a fast, reliable, always-available
        // "connection refused" path, no real network dependency.
        let result = KiwiIqSource::connect("127.0.0.1", 1, 14_025_000.0, "");
        assert!(result.is_err(), "expected a clean Err, not a panic");
    }

    #[test]
    fn parses_kv_from_msg_text() {
        assert_eq!(
            parse_kv_f64(" sample_rate=11998.937786", "sample_rate"),
            Some(11998.937786)
        );
        assert_eq!(
            parse_kv_f64(" audio_init=0 audio_rate=12000", "audio_rate"),
            Some(12000.0)
        );
        assert_eq!(parse_kv_f64(" badp=0", "sample_rate"), None);
    }

    #[test]
    fn parses_snd_frame_be_and_le() {
        // 1 byte flags + 4 byte seq (LE) + 2 byte smeter (BE) + 10 byte gps
        // + 2 complex pairs (BE by default).
        let mut body = vec![0u8; 17];
        body[0] = 0x08; // stereo, big-endian
        body.extend_from_slice(&1000i16.to_be_bytes()); // I0
        body.extend_from_slice(&(-2000i16).to_be_bytes()); // Q0
        body.extend_from_slice(&32767i16.to_be_bytes()); // I1
        body.extend_from_slice(&(-32768i16).to_be_bytes()); // Q1
        let samples = parse_snd_frame(&body);
        assert_eq!(samples.len(), 2);
        assert!((samples[0].re - 1000.0 / 32768.0).abs() < 1e-6);
        assert!((samples[0].im - (-2000.0 / 32768.0)).abs() < 1e-6);
        assert!((samples[1].re - 1.0).abs() < 1e-3);
        assert!((samples[1].im - (-1.0)).abs() < 1e-6);

        // Same payload bytes, little-endian flag set: values decode differently.
        let mut le_body = body.clone();
        le_body[0] = 0x08 | 0x80;
        let le_samples = parse_snd_frame(&le_body);
        assert_eq!(le_samples.len(), 2);
        assert_ne!(le_samples[0].re, samples[0].re);
    }

    #[test]
    fn short_snd_frame_yields_no_samples() {
        assert!(parse_snd_frame(&[0u8; 10]).is_empty());
    }

    /// Resampling math alone, no network: construct the same `rubato::Fft`
    /// resampler this module uses, feed synthetic input at a real,
    /// live-measured KiwiSDR rate, and confirm the output/input ratio
    /// converges to the expected rate ratio once the resampler's internal
    /// accumulation (see RESAMPLER_CHUNK's doc comment) has run for enough
    /// calls to reach steady state.
    #[test]
    fn resampler_math_converges_to_expected_ratio() {
        let rate_in = 11_999usize; // real, live-measured (rounded) KiwiSDR rate
        let mut resampler = Fft::<f32>::new(
            rate_in,
            TARGET_RATE_HZ,
            RESAMPLER_CHUNK,
            2,
            FixedSync::Input,
        )
        .expect("construct resampler");
        assert_eq!(resampler.output_delay(), 48_000);

        let mut total_in = 0usize;
        let mut total_out = 0usize;
        for call in 0..20 {
            let n_in = resampler.input_frames_next();
            let mut in_buf = vec![0f32; n_in * 2];
            for i in 0..n_in {
                let t = (total_in + i) as f32 / rate_in as f32;
                let phase = 2.0 * std::f32::consts::PI * 1000.0 * t;
                in_buf[2 * i] = phase.cos();
                in_buf[2 * i + 1] = phase.sin();
            }
            total_in += n_in;
            let n_out = resampler.output_frames_next();
            let mut out_buf = vec![0f32; n_out * 2];
            let in_adapter = InterleavedSlice::new(&in_buf, 2, n_in).unwrap();
            let mut out_adapter = InterleavedSlice::new_mut(&mut out_buf, 2, n_out).unwrap();
            let (used_in, produced_out) = resampler
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
                .unwrap();
            assert_eq!(used_in, n_in);
            total_out += produced_out;
            if call == 0 {
                // With RESAMPLER_CHUNK >= the internal FFT block size for
                // this rate pair, real output starts on the very first call.
                assert!(produced_out > 0, "expected immediate output, got 0");
            }
        }
        let ratio = total_out as f64 / total_in as f64;
        let expected = TARGET_RATE_HZ as f64 / rate_in as f64;
        assert!(
            (ratio - expected).abs() / expected < 0.02,
            "ratio={ratio} expected={expected}"
        );
    }

    #[test]
    fn a_fake_receiver_completes_the_handshake_and_streams_iq() {
        let fake = FakeKiwi::spawn(|_idx, ws, _sent, _stop| {
            stream_valid_iq(ws, 40);
            std::thread::sleep(Duration::from_secs(2));
        });
        let mut src = KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "")
            .expect("fake receiver handshake should succeed");
        assert!((src.sample_rate() - TARGET_RATE_HZ as f64).abs() < 1.0);
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut total = 0;
        for _ in 0..20 {
            total += src
                .read(&mut buf)
                .expect("read real-shaped IQ from the fake receiver");
            if total > 0 {
                break;
            }
        }
        assert!(
            total > 0,
            "expected the fake receiver's IQ to flow through the pipeline"
        );
    }

    #[test]
    fn the_configured_password_is_sent_in_the_auth_frame() {
        let fake = FakeKiwi::spawn(|_idx, ws, _sent, _stop| {
            stream_valid_iq(ws, 5);
            std::thread::sleep(Duration::from_millis(500));
        });
        let _src = KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "s3cr3t")
            .expect("connect with password");
        let lines = fake.client_lines();
        assert!(
            lines.iter().any(|l| l == "SET auth t=kiwi p=s3cr3t"),
            "expected the auth frame on the wire, got {lines:?}"
        );
    }

    #[test]
    fn a_real_sized_snd_frame_is_well_within_the_frame_bound() {
        let Message::Binary(bytes) = valid_snd_frame(Complex32::new(0.1, -0.1)) else {
            panic!("expected a binary frame")
        };
        assert_eq!(
            bytes.len(),
            2068,
            "real SND frames are a constant 2068 bytes"
        );
        assert!(bytes.len() < MAX_WS_FRAME_BYTES / 8);
    }

    #[test]
    fn an_oversized_frame_is_recovered_by_reconnect_and_iq_resumes() {
        let fake = FakeKiwi::spawn(|idx, ws, _sent, _stop| {
            if idx == 0 {
                let oversized = Message::Binary(vec![0u8; MAX_WS_FRAME_BYTES + 4096].into());
                let _ = ws.send(oversized);
                let _ = ws.close(None);
            } else {
                stream_valid_iq(ws, 40);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let mut src = KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "")
            .expect("initial connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut total = 0;
        for _ in 0..50 {
            total += src
                .read(&mut buf)
                .expect("an oversized frame must be recovered, not fatal");
            if total > 0 {
                break;
            }
        }
        assert!(total > 0, "expected IQ to resume after the oversized frame");
        assert!(src.stats().reconnects >= 1);
    }

    #[test]
    fn garbage_frames_are_discarded_and_counted_then_valid_iq_still_reads() {
        let fake = FakeKiwi::spawn(|idx, ws, _sent, _stop| {
            if idx == 0 {
                let _ = ws.send(unknown_tag_frame());
                let _ = ws.send(short_snd_frame());
                let _ = ws.send(invalid_utf8_msg_frame());
                let _ = ws.send(msg_no_recognized_key());
                let _ = ws.send(Message::Text("hello".to_string().into()));
                stream_valid_iq(ws, 40);
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        let mut src =
            KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "").expect("connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut total = 0;
        for _ in 0..20 {
            total += src
                .read(&mut buf)
                .expect("garbage frames must be discarded, not fatal");
            if total > 0 {
                break;
            }
        }
        assert!(
            total > 0,
            "expected valid IQ to flow after the garbage frames"
        );
        assert_eq!(src.stats().unusable_frames, 5);
        assert_eq!(src.stats().connection_failures, 0);
    }

    #[test]
    fn a_sustained_flood_of_unusable_frames_gives_up_cleanly_instead_of_stalling() {
        let fake = FakeKiwi::spawn(|_idx, ws, _sent, stop| {
            while !stop.load(Ordering::Relaxed) {
                if ws.send(unknown_tag_frame()).is_err() {
                    return;
                }
            }
        });
        let policy = ReconnectPolicy {
            max_attempts: 1,
            ..fast_policy()
        };
        let mut src =
            KiwiIqSource::connect_with_policy("127.0.0.1", fake.port(), 14_025_000.0, "", policy)
                .expect("initial connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let start = Instant::now();
        let mut got_err = None;
        for _ in 0..10 {
            match src.read(&mut buf) {
                Ok(_) => continue,
                Err(e) => {
                    got_err = Some(e);
                    break;
                }
            }
        }
        let elapsed = start.elapsed();
        let err = got_err.expect("expected the flood to eventually bail rather than stall forever");
        assert!(
            err.to_string().to_lowercase().contains("unusable"),
            "error should explain the unusable-frame bound: {err}"
        );
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
        assert!(src.stats().unusable_frames >= MAX_CONSECUTIVE_UNUSABLE_FRAMES as u64);
    }

    #[test]
    fn a_clean_close_is_recovered_by_reconnect_not_fatal() {
        let fake = FakeKiwi::spawn(|idx, ws, _sent, _stop| {
            if idx == 0 {
                stream_valid_iq(ws, 5);
                let _ = ws.close(None);
            } else {
                stream_valid_iq(ws, 40);
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let mut src = KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "")
            .expect("initial connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut total = 0;
        for _ in 0..50 {
            total += src
                .read(&mut buf)
                .expect("a clean Close must be recovered, not fatal");
            if total > 0 && src.stats().reconnects >= 1 {
                break;
            }
        }
        assert!(
            src.stats().reconnects >= 1,
            "expected at least one reconnect"
        );
        assert!(fake.accepts() >= 2);
    }

    #[test]
    fn accept_then_close_forever_gives_up_after_the_reconnect_bound() {
        let fake = FakeKiwi::spawn(|_idx, ws, _sent, _stop| {
            let _ = ws.close(None);
        });
        let policy = fast_policy();
        let mut src =
            KiwiIqSource::connect_with_policy("127.0.0.1", fake.port(), 14_025_000.0, "", policy)
                .expect("initial handshake completes before the fake receiver closes");
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let mut got_err = None;
        for _ in 0..200 {
            match src.read(&mut buf) {
                Ok(_) => continue,
                Err(e) => {
                    got_err = Some(e);
                    break;
                }
            }
        }
        let err = got_err.expect("expected read() to eventually give up");
        let msg = err.to_string();
        assert!(
            msg.contains(&fake.port().to_string()),
            "error should name the port: {msg}"
        );
        assert_eq!(fake.accepts(), policy.max_attempts as usize + 1);
    }

    #[test]
    fn an_outage_is_covered_by_zero_fill_so_the_sample_clock_does_not_drift() {
        let outage_delay = Duration::from_millis(300);
        let fake = FakeKiwi::spawn_with_delay(
            move |idx| {
                if idx == 1 {
                    outage_delay
                } else {
                    Duration::ZERO
                }
            },
            |idx, ws, _sent, _stop| {
                if idx == 0 {
                    stream_valid_iq(ws, 40);
                    let _ = ws.close(None);
                } else {
                    stream_valid_iq(ws, 200);
                    std::thread::sleep(Duration::from_secs(2));
                }
            },
        );
        let mut src = KiwiIqSource::connect("127.0.0.1", fake.port(), 14_025_000.0, "")
            .expect("initial connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        for _ in 0..500 {
            let _ = src.read(&mut buf).expect("read across the outage");
            if src.stats().reconnects >= 1 {
                break;
            }
        }
        assert!(
            src.stats().reconnects >= 1,
            "expected at least one reconnect"
        );
        // Drain a bit more so the zero-fill queued right after the
        // reconnect gets consumed and counted.
        for _ in 0..50 {
            let _ = src.read(&mut buf);
        }
        let stats = src.stats();
        let expected = outage_delay.as_secs_f64() * TARGET_RATE_HZ as f64;
        assert!(
            stats.zero_filled_samples as f64 >= expected * 0.3,
            "zero_filled_samples={} expected around {expected}",
            stats.zero_filled_samples
        );
        assert!(
            (stats.zero_filled_samples as f64)
                <= (MAX_ZERO_FILL_SECONDS as f64) * TARGET_RATE_HZ as f64,
            "zero fill must stay capped, got {}",
            stats.zero_filled_samples
        );
    }

    #[test]
    fn a_productive_frame_resets_the_reconnect_budget() {
        let fake = FakeKiwi::spawn(|_idx, ws, _sent, _stop| {
            stream_valid_iq(ws, 40);
            let _ = ws.close(None);
        });
        let policy = ReconnectPolicy {
            max_attempts: 2,
            ..fast_policy()
        };
        let mut src =
            KiwiIqSource::connect_with_policy("127.0.0.1", fake.port(), 14_025_000.0, "", policy)
                .expect("initial connect");
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        // Every connection streams real SND frames before closing, so each
        // reconnect's budget should be replenished -- drive well past what
        // `max_attempts` alone would allow without ever seeing a fatal Err.
        let target_reconnects = policy.max_attempts as u64 + 3;
        for _ in 0..2000 {
            src.read(&mut buf)
                .expect("productive frames must keep the reconnect budget replenished");
            if src.stats().reconnects >= target_reconnects {
                break;
            }
        }
        assert!(
            src.stats().reconnects >= target_reconnects,
            "expected more reconnects than the budget alone would allow, got {}",
            src.stats().reconnects
        );
    }

    /// Real, live integration test: connects to a real public KiwiSDR
    /// receiver over the internet, completes the mod=iq handshake, and
    /// reads real streamed IQ samples. #[ignore]'d: network-dependent,
    /// third-party infrastructure, not run in default `cargo test`/CI.
    #[test]
    #[ignore]
    fn connects_to_a_real_public_receiver_and_streams_iq() {
        let mut src =
            KiwiIqSource::connect("kiwisdr.inf.dhbw-ravensburg.de", 8073, 14_025_000.0, "")
                .expect("connect to a real public KiwiSDR receiver");
        assert!(
            (src.sample_rate() - 96_000.0).abs() < 1.0,
            "expected resampled rate ~96000, got {}",
            src.sample_rate()
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let n = src.read(&mut buf).expect("read real IQ samples");
        assert!(n > 0, "expected real samples from a live receiver");
        let first_max_norm = buf[..n].iter().map(|s| s.norm()).fold(0.0f32, f32::max);

        // The resampler's overlap-add reconstruction is only "cold" for its
        // very first FFT block (no prior block to overlap with yet -- see
        // RESAMPLER_CHUNK's and output_delay's doc comments), which
        // attenuates roughly the first output_delay()-worth of output
        // samples. Keep reading (real wall-clock time: each RESAMPLER_CHUNK
        // of raw input takes a little over a second to arrive from a real
        // ~12 kHz receiver) until we're well past that, then check
        // amplitude on genuinely steady-state output.
        let mut total = n;
        let mut steady_max_norm = 0.0f32;
        let mut buf2 = vec![Complex32::new(0.0, 0.0); 8192];
        while total < 150_000 {
            let n2 = src.read(&mut buf2).expect("read more real IQ samples");
            assert!(n2 > 0, "expected more real samples from a live receiver");
            total += n2;
            if total > 100_000 {
                steady_max_norm =
                    steady_max_norm.max(buf2[..n2].iter().map(|s| s.norm()).fold(0.0f32, f32::max));
            }
        }
        eprintln!(
            "real KiwiSDR read: sample_rate={} first_n={n} first_max_norm={first_max_norm} total={total} steady_max_norm={steady_max_norm}",
            src.sample_rate(),
        );
        // Sanity: real RF noise/signal should not be all-zero once past
        // the resampler's cold-start transient.
        assert!(
            steady_max_norm > 1e-4,
            "expected non-trivial real RF amplitude past the startup transient, got {steady_max_norm}"
        );
    }
}
