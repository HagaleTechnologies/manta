//! KiwiSDR network IQ source: connects to a public/private KiwiSDR receiver
//! over its WebSocket protocol, requests raw complex-IQ mode (`mod=iq`), and
//! rational-resamples the receiver's native (device-specific, non-round)
//! sample rate up to 96000 Hz before handing samples to the rest of the
//! pipeline. ARCHITECTURE §3, docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md.
//!
//! Protocol notes (real, live-verified against several public receivers
//! during implementation -- see docs/superpowers/sdd/task-1-report.md for
//! the full findings and the design spec for the original brainstorming):
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

use crate::IqSource;
use anyhow::{anyhow, bail, Context, Result};
use num_complex::Complex32;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{Message, WebSocket};

/// Target output rate after resampling -- SPEC-decode-core.md §1.1's table
/// rate nearest KiwiSDR's native ~12 kHz.
const TARGET_RATE_HZ: usize = 96_000;

/// Resampler chunk size (in complex sample-pairs) fed to `rubato::Fft` per
/// `process_into_buffer` call.
///
/// **Real, empirically-verified finding** (not the naive guess of matching
/// KiwiSDR's native 512-sample SND frame size, and not simply "bigger is
/// better" -- see docs/superpowers/sdd/task-1-report.md for the full
/// derivation): with `FixedSync::Input`, `rubato::Fft`'s internal FFT block
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

/// A KiwiSDR receiver (network SDR) as an `IqSource`. ARCHITECTURE §3.
pub struct KiwiIqSource {
    socket: WebSocket<TcpStream>,
    fs: f64,
    center_freq_hz: f64,
    resampler: Fft<f32>,
    /// Un-resampled input accumulator: interleaved `[I0, Q0, I1, Q1, ...]`.
    raw: Vec<f32>,
    /// Resampled-output chunk assembler; `read()` drains from here.
    pending: VecDeque<Complex32>,
    last_keepalive: Instant,
}

impl KiwiIqSource {
    /// Connect to a KiwiSDR receiver, complete the SND-channel handshake in
    /// `mod=iq` (raw complex IQ), and construct the rational resampler up
    /// to `TARGET_RATE_HZ`. `password` is `""` for anonymous/no-password
    /// receivers (most public ones).
    pub fn connect(host: &str, port: u16, center_freq_hz: f64, password: &str) -> Result<Self> {
        let tcp = TcpStream::connect((host, port))
            .with_context(|| format!("TCP connect to {host}:{port}"))?;
        tcp.set_read_timeout(Some(READ_TIMEOUT))
            .context("set TCP read timeout")?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let url = format!("ws://{host}:{port}/{timestamp}/SND");
        let (mut socket, _resp) =
            tungstenite::client(url, tcp).context("KiwiSDR WebSocket handshake")?;

        socket
            .send(Message::Text(format!("SET auth t=kiwi p={password}")))
            .context("send SET auth")?;

        // Read frames until the server reports the real, device-specific
        // sample rate. Everything else in the initial parameter batch
        // (rx_chans, chan_no_pwd, load_cfg, ...) is handled by read()'s
        // ongoing MSG dispatch once streaming begins.
        let rate_in_hz = loop {
            let msg = socket.read().context("read during KiwiSDR handshake")?;
            let Message::Binary(b) = msg else {
                continue;
            };
            if b.len() < 3 || &b[0..3] != b"MSG" {
                continue;
            }
            let text = String::from_utf8_lossy(&b[3..]);
            if let Some(rate) = parse_kv_f64(&text, "sample_rate") {
                break rate.round() as usize;
            }
        };

        for cmd in [
            "SET ident_user=skimmer".to_string(),
            format!(
                "SET mod=iq low_cut=-5000 high_cut=5000 freq={:.3}",
                center_freq_hz / 1000.0
            ),
            "SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50".to_string(),
            "SET squelch=0 max=0".to_string(),
            "SET genattn=0".to_string(),
            "SET gen=0 mix=-1".to_string(),
            "SET keepalive".to_string(),
        ] {
            socket
                .send(Message::Text(cmd))
                .context("send post-handshake SET command")?;
        }

        if rate_in_hz == 0 {
            bail!("KiwiSDR reported sample_rate=0");
        }
        let resampler = Fft::<f32>::new(
            rate_in_hz,
            TARGET_RATE_HZ,
            RESAMPLER_CHUNK,
            2,
            FixedSync::Input,
        )
        .map_err(|e| {
            anyhow!("construct KiwiSDR resampler ({rate_in_hz} -> {TARGET_RATE_HZ} Hz): {e}")
        })?;

        Ok(KiwiIqSource {
            socket,
            fs: TARGET_RATE_HZ as f64,
            center_freq_hz,
            resampler,
            raw: Vec::new(),
            pending: VecDeque::new(),
            last_keepalive: Instant::now(),
        })
    }

    fn send_keepalive_if_due(&mut self) -> Result<()> {
        if self.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            self.socket
                .send(Message::Text("SET keepalive".to_string()))
                .context("send SET keepalive")?;
            self.last_keepalive = Instant::now();
        }
        Ok(())
    }

    /// Handle one `MSG` frame's text: acknowledge the audio rate (required
    /// for the server to ever start streaming `SND`, see module docs) and
    /// otherwise ignore. Real, unknown MSG keys are common and harmless.
    fn handle_msg(&mut self, text: &str) -> Result<()> {
        if let Some(rate) = parse_kv_f64(text, "audio_rate") {
            let cmd = format!("SET AR OK in={} out={TARGET_RATE_HZ}", rate as i64);
            self.socket
                .send(Message::Text(cmd))
                .context("send SET AR OK")?;
        }
        Ok(())
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
        let mut consecutive_timeouts = 0u32;
        loop {
            let n = self.pending.len().min(buf.len());
            if n > 0 {
                for slot in buf.iter_mut().take(n) {
                    *slot = self.pending.pop_front().unwrap();
                }
                return Ok(n);
            }

            self.send_keepalive_if_due()?;

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
                        bail!(
                            "KiwiSDR connection stalled: no data for {} consecutive read timeouts",
                            MAX_CONSECUTIVE_TIMEOUTS
                        );
                    }
                    continue;
                }
                Err(e) => return Err(e).context("KiwiSDR WebSocket read"),
            };

            match msg {
                Message::Binary(b) if b.len() >= 3 && &b[0..3] == b"SND" => {
                    let samples = parse_snd_frame(&b[3..]);
                    for s in samples {
                        self.raw.push(s.re);
                        self.raw.push(s.im);
                    }
                    self.drain_resampler()?;
                }
                Message::Binary(b) if b.len() >= 3 && &b[0..3] == b"MSG" => {
                    let text = String::from_utf8_lossy(&b[3..]);
                    self.handle_msg(&text)?;
                }
                Message::Binary(_) => {
                    // Unknown binary frame tag; ignore.
                }
                Message::Text(_) => {
                    // The server never sends text frames in practice; ignore.
                }
                Message::Ping(_) | Message::Pong(_) => {
                    // tungstenite auto-replies to Ping internally; nothing to do.
                }
                Message::Close(_) => {
                    bail!("KiwiSDR closed the connection");
                }
                Message::Frame(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
