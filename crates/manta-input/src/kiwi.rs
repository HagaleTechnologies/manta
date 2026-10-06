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

use crate::{InputHealthCounters, IqSource};
use anyhow::{anyhow, bail, Context, Result};
use num_complex::Complex32;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::{BTreeSet, VecDeque};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
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

/// Bounds each resolved address's `TcpStream::connect_timeout` attempt
/// (MAN-73): a target that silently black-holes SYNs (e.g. a firewall drop,
/// not a refusal) would otherwise leave `connect()` pending for the OS's
/// own timeout (commonly minutes). Mirrors `uplink.rs`'s own per-address
/// `CONNECT_TIMEOUT` value.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds DNS resolution plus every per-address connect attempt together
/// (MAN-73). Unlike `uplink.rs`'s connect, this one runs on
/// `ReconnectingSource`'s blocking read-loop thread, which cannot be
/// interrupted by Ctrl-C mid-connect, so a stop signal landing mid-reopen
/// waits up to this long, then the WebSocket handshake, before
/// `manta-cli`'s up-to-50s shutdown drain even starts -- more, in that
/// worst case, than the 60s `docker stop -t 60` grace the README
/// recommends. 3x `CONNECT_TIMEOUT`, matching `uplink.rs`'s
/// `OVERALL_CONNECT_TIMEOUT` and for the same reason (PR #80 review, round
/// 2): a window equal to one address's `CONNECT_TIMEOUT` is used up by a
/// black-holed first address (e.g. a dropped AAAA record), and since every
/// reconnect re-resolves and starts from that same first address, a
/// reachable later address would never be tried at all.
const OVERALL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolves `host:port` and connects to the first address that accepts,
/// the whole operation bounded by `OVERALL_CONNECT_TIMEOUT` (MAN-73, code
/// review round 1): `(host, port).to_socket_addrs()` is a synchronous,
/// blocking `getaddrinfo(3)` call with no timeout of its own, so a stalled
/// system resolver would otherwise hang this uninterruptible thread
/// indefinitely before `TcpStream::connect_timeout` ever got a chance to
/// run -- `uplink.rs`'s `connect_any_resolved_address` bounds the
/// equivalent async call with `tokio::time::timeout`; there is no Tokio
/// runtime here, so resolution plus every per-address connect attempt
/// instead run on a detached thread (see `run_with_overall_timeout`).
fn resolve_and_connect(host: &str, port: u16) -> Result<TcpStream> {
    let owned_host = host.to_string();
    run_with_overall_timeout(host, port, move || {
        (owned_host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| anyhow!("resolve {owned_host}:{port}: {e}"))
            .and_then(|addrs| {
                let mut last_err = None;
                for addr in addrs {
                    match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                        Ok(stream) => return Ok(stream),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err
                    .map(anyhow::Error::from)
                    .unwrap_or_else(|| anyhow!("no addresses resolved for {owned_host}:{port}")))
            })
    })
}

/// `(host, port)` targets whose `run_with_overall_timeout` worker thread is
/// still running, including workers whose caller already timed out and
/// moved on (MAN-73, PR #207 review). `ReconnectingSource` retries a lost
/// Kiwi forever, so without this a resolver or connect that keeps
/// outliving `OVERALL_CONNECT_TIMEOUT` would leave one more abandoned
/// thread behind on every attempt. With it, at most one worker per target
/// is ever outstanding -- the same bound `uplink.rs`'s `resolver_slot`
/// gives its lookups, and keyed per target for the same reason (PR #80
/// review round 11): one stuck target must not block another.
static OUTSTANDING_CONNECTS: Mutex<BTreeSet<(String, u16)>> = Mutex::new(BTreeSet::new());

fn outstanding_connects() -> MutexGuard<'static, BTreeSet<(String, u16)>> {
    // The set stays consistent even if a holder panicked: every critical
    // section is a single insert or remove.
    OUTSTANDING_CONNECTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Holds one target's `OUTSTANDING_CONNECTS` entry; removes it on drop.
struct OutstandingConnect((String, u16));

impl Drop for OutstandingConnect {
    fn drop(&mut self) {
        outstanding_connects().remove(&self.0);
    }
}

/// Runs `attempt` on a detached thread, raced against this function's own
/// `recv_timeout(OVERALL_CONNECT_TIMEOUT)`. A stall past that window
/// returns an `Err` to the
/// caller immediately; the detached thread is abandoned (matching the
/// accepted tradeoff `uplink.rs` documents for its own blocking
/// `getaddrinfo` call) rather than left to block a reconnect attempt
/// forever. While an earlier worker for the same `host:port` is still
/// running, this returns an `Err` at once without spawning another (see
/// `OUTSTANDING_CONNECTS`).
fn run_with_overall_timeout<T: Send + 'static>(
    host: &str,
    port: u16,
    attempt: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let target = (host.to_string(), port);
    if !outstanding_connects().insert(target.clone()) {
        bail!(
            "TCP connect to {host}:{port} skipped: a previous attempt's DNS resolution or \
             connect is still running; not starting another thread"
        );
    }
    let slot = OutstandingConnect(target);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = attempt();
        // Released before the send, so a caller that has received this
        // outcome can always start its next attempt straight away.
        drop(slot);
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(OVERALL_CONNECT_TIMEOUT) {
        Ok(outcome) => outcome.with_context(|| format!("TCP connect to {host}:{port}")),
        Err(_) => Err(anyhow!(
            "TCP connect to {host}:{port} timed out (DNS resolution or connect exceeded {OVERALL_CONNECT_TIMEOUT:?})"
        )),
    }
}

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

/// Bound on a forward `seq` jump (MAN-128, generalizing MAN-56's gap-stat
/// wiring beyond HPSDR) before it's treated as a counter reset rather than a
/// genuine loss event. Upstream `jks-prv/kiwiclient`'s `kiwirecorder.py:312`
/// treats SND `seq` as a simple per-block counter (`Block: %08x`) with no
/// documented wrap tolerance narrower than this; a jump bigger than this is
/// far more likely to be a server-side reconnect/reset than real loss, so it
/// re-baselines silently instead of reporting a nonsensical gap count.
const MAX_PLAUSIBLE_SEQ_JUMP: u32 = 65_536;

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
    /// MAN-128: shared packet-loss/malformed counters, fed from each SND
    /// frame's `seq` field by `account_snd_frame`. Surfaced via
    /// `health_counters()`, same as HPSDR's `GapDetector` handle.
    health: Arc<InputHealthCounters>,
    seq: SndSeqTracker,
}

impl KiwiIqSource {
    /// Connect to a KiwiSDR receiver, complete the SND-channel handshake in
    /// `mod=iq` (raw complex IQ), and construct the rational resampler up
    /// to `TARGET_RATE_HZ`. `password` is `""` for anonymous/no-password
    /// receivers (most public ones).
    pub fn connect(host: &str, port: u16, center_freq_hz: f64, password: &str) -> Result<Self> {
        let tcp = resolve_and_connect(host, port)?;
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
        // stops streaming a few seconds later. So every MSG frame seen here
        // is routed through the same `handle_msg` dispatch `read()` uses
        // later, not just inspected for `sample_rate=`; only that key
        // additionally ends the loop, once handle_msg has already had a
        // chance to react to it.
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
                            "KiwiSDR handshake stalled: no data for {} consecutive read timeouts",
                            MAX_CONSECUTIVE_TIMEOUTS
                        );
                    }
                    continue;
                }
                Err(e) => return Err(e).context("read during KiwiSDR handshake"),
            };
            let Message::Binary(b) = msg else {
                continue;
            };
            if b.len() < 3 || &b[0..3] != b"MSG" {
                continue;
            }
            let text = String::from_utf8_lossy(&b[3..]).into_owned();
            // `self` doesn't exist yet at this point in connect() (the
            // resampler/rate aren't known until this loop finds
            // sample_rate=), so this can't call `self.handle_msg` directly;
            // both this loop and handle_msg instead delegate to the shared
            // free function below so the audio_rate-ack logic lives in
            // exactly one place.
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
            health: Arc::new(InputHealthCounters::new()),
            seq: SndSeqTracker::default(),
        })
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
    /// otherwise ignore. Real, unknown MSG keys are common and harmless.
    fn handle_msg(&mut self, text: &str) -> Result<()> {
        ack_audio_rate_if_present(&mut self.socket, text)
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

/// Shared by `connect()`'s handshake loop (before a `KiwiIqSource` exists)
/// and `handle_msg` (after): if `text` carries `audio_rate=`, reply with the
/// `SET AR OK` ack the server requires before it will ever start streaming
/// `SND` frames (see module docs). No-op if `audio_rate=` isn't present.
fn ack_audio_rate_if_present(socket: &mut WebSocket<TcpStream>, text: &str) -> Result<()> {
    if let Some(rate) = parse_kv_f64(text, "audio_rate") {
        let cmd = format!("SET AR OK in={} out={TARGET_RATE_HZ}", rate as i64);
        socket
            .send(Message::Text(cmd.into()))
            .context("send SET AR OK")?;
    }
    Ok(())
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

/// One parsed `SND` frame: its sequence number (MAN-128 gap tracking) and
/// raw, un-resampled complex samples.
struct SndFrame {
    seq: u32,
    samples: Vec<Complex32>,
}

/// Parse an SND frame's bytes (after the 3-byte `"SND"` tag). Returns `None`
/// when the frame is shorter than the fixed header (malformed -- MAN-128,
/// mirroring MAN-22's HPSDR malformed-packet counting); a frame exactly as
/// long as the header is valid, with zero samples. Sample values are
/// normalized to roughly [-1, 1] (matching `WavIqSource`'s i16 convention).
/// See module docs for the byte layout.
fn parse_snd_frame(body: &[u8]) -> Option<SndFrame> {
    const HEADER_LEN: usize = 1 + 4 + 2 + 10; // flags + seq + smeter + gps
    if body.len() < HEADER_LEN {
        return None;
    }
    let flags = body[0];
    let seq = u32::from_le_bytes([body[1], body[2], body[3], body[4]]);
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
    Some(SndFrame { seq, samples: out })
}

/// Tracks the `seq` field across consecutive `SND` frames (MAN-128,
/// generalizing MAN-56's HPSDR gap-stat wiring to KiwiSDR) to detect lost
/// frames. KiwiSDR's SND `seq` advances by exactly 1 per frame in normal
/// operation (confirmed against kiwirecorder.py's own `Block: %08x` log,
/// `jks-prv/kiwiclient`).
#[derive(Default)]
struct SndSeqTracker {
    last: Option<u32>,
    /// Malformed frames received since the last valid one: they arrived, so
    /// the next `seq` delta must not count them as lost.
    malformed_since_last: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum SeqObservation {
    /// The very first frame seen: nothing to compare against yet.
    Baseline,
    /// `seq` advanced by exactly 1: no loss.
    InOrder,
    /// `seq` advanced by more than 1 (bounded by `MAX_PLAUSIBLE_SEQ_JUMP`):
    /// `missing` frames were dropped in transit.
    Gap { missing: u32 },
    /// A duplicate, backward, or implausibly large jump: most likely a
    /// server-side counter reset, not a real loss event. Re-baselines
    /// silently rather than reporting a nonsensical gap.
    Resync,
}

impl SndSeqTracker {
    fn observe(&mut self, seq: u32) -> SeqObservation {
        let observation = match self.last {
            None => SeqObservation::Baseline,
            Some(last) => match seq.wrapping_sub(last) {
                0 => SeqObservation::Resync,
                1 => SeqObservation::InOrder,
                delta if delta <= MAX_PLAUSIBLE_SEQ_JUMP => {
                    match (delta - 1).saturating_sub(self.malformed_since_last) {
                        0 => SeqObservation::InOrder,
                        missing => SeqObservation::Gap { missing },
                    }
                }
                _ => SeqObservation::Resync,
            },
        };
        self.last = Some(seq);
        self.malformed_since_last = 0;
        observation
    }

    /// Note a malformed frame: its `seq` bytes may be missing or garbage, so
    /// it doesn't move the baseline, but it is excluded from the next delta.
    fn observe_malformed(&mut self) {
        self.malformed_since_last = self.malformed_since_last.saturating_add(1);
    }
}

/// Parse one SND frame's body and account it against `counters`/`tracker`:
/// a malformed (too-short) frame counts as one malformed packet, and a
/// forward `seq` jump counts as one gap event plus its missing-frame count,
/// net of malformed frames received since the previous valid one.
/// Returns the frame's samples, or `None` if the frame was malformed.
fn account_snd_frame(
    counters: &InputHealthCounters,
    tracker: &mut SndSeqTracker,
    body: &[u8],
) -> Option<Vec<Complex32>> {
    let frame = match parse_snd_frame(body) {
        Some(frame) => frame,
        None => {
            counters.record_malformed();
            tracker.observe_malformed();
            return None;
        }
    };
    if let SeqObservation::Gap { missing } = tracker.observe(frame.seq) {
        counters.record_gap();
        counters.record_dropped(missing as u64);
    }
    Some(frame.samples)
}

impl IqSource for KiwiIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        Some(self.health.clone())
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut consecutive_timeouts = 0u32;
        loop {
            let n = self.pending.len().min(buf.len());
            if n > 0 {
                // Safe: `n` is bounded above by `self.pending.len()`, so
                // every one of these `n` pops has an element to take.
                for slot in buf.iter_mut().take(n) {
                    if let Some(s) = self.pending.pop_front() {
                        *slot = s;
                    }
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
                    if let Some(samples) = account_snd_frame(&self.health, &mut self.seq, &b[3..]) {
                        for s in samples {
                            self.raw.push(s.re);
                            self.raw.push(s.im);
                        }
                        self.drain_resampler()?;
                    }
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
    fn connect_to_a_resolvable_but_refusing_address_fails_within_the_connect_timeout() {
        // MAN-73: exercises the new per-address `connect_timeout` loop. A
        // refused connection returns near-instantly regardless of
        // CONNECT_TIMEOUT, but this guards against a future regression
        // back to an unbounded `TcpStream::connect` -- a black-holed
        // address (not exercised here; needs network control) would hang
        // for the OS's own connect timeout instead of this crate's bound.
        let start = Instant::now();
        let result = KiwiIqSource::connect("127.0.0.1", 1, 14_025_000.0, "");
        assert!(result.is_err(), "expected a clean Err, not a panic");
        assert!(
            start.elapsed() < CONNECT_TIMEOUT,
            "connect must fail within CONNECT_TIMEOUT, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn overall_connect_window_outlasts_a_black_holed_first_address() {
        // MAN-73 validate finding: a black-holed first address uses its
        // whole CONNECT_TIMEOUT (plus however long resolution took) before
        // the per-address loop moves on. The overall window must still be
        // open when the next, reachable address accepts -- a window of one
        // CONNECT_TIMEOUT expired first, so every reconnect failed forever.
        let outcome = run_with_overall_timeout("kiwi.example", 8073, || {
            std::thread::sleep(CONNECT_TIMEOUT + Duration::from_millis(200));
            Ok("second address")
        });
        assert_eq!(outcome.unwrap(), "second address");
    }

    #[test]
    fn a_still_running_connect_worker_blocks_another_for_the_same_target() {
        // MAN-73, PR #207 review: ReconnectingSource retries forever, so a
        // connect worker that is still running (a hung resolver, here a
        // worker parked on a channel) must not be joined by a new one on
        // every retry. The slot is moved into the worker thread, so it is
        // held the same way after the caller's own window has expired.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let first = std::thread::spawn(move || {
            run_with_overall_timeout("kiwi.example", 8074, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok("first")
            })
        });
        started_rx.recv().unwrap();

        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_attempt = ran.clone();
        let err = run_with_overall_timeout("kiwi.example", 8074, move || {
            ran_in_attempt.store(true, Ordering::SeqCst);
            Ok("second")
        })
        .expect_err("a second worker for the same target must not start");
        assert!(err.to_string().contains("still running"), "{err:#}");
        assert!(
            !ran.load(Ordering::SeqCst),
            "the second attempt must never run"
        );

        assert_eq!(
            run_with_overall_timeout("kiwi.example", 8075, || Ok("other target")).unwrap(),
            "other target",
            "a stuck target must not block a different one"
        );

        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), "first");
        assert_eq!(
            run_with_overall_timeout("kiwi.example", 8074, || Ok("third")).unwrap(),
            "third",
            "the slot is free again once the earlier worker has finished"
        );
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
        body[1..5].copy_from_slice(&42u32.to_le_bytes()); // seq
        body.extend_from_slice(&1000i16.to_be_bytes()); // I0
        body.extend_from_slice(&(-2000i16).to_be_bytes()); // Q0
        body.extend_from_slice(&32767i16.to_be_bytes()); // I1
        body.extend_from_slice(&(-32768i16).to_be_bytes()); // Q1
        let frame = parse_snd_frame(&body).expect("valid frame");
        assert_eq!(frame.seq, 42);
        let samples = frame.samples;
        assert_eq!(samples.len(), 2);
        assert!((samples[0].re - 1000.0 / 32768.0).abs() < 1e-6);
        assert!((samples[0].im - (-2000.0 / 32768.0)).abs() < 1e-6);
        assert!((samples[1].re - 1.0).abs() < 1e-3);
        assert!((samples[1].im - (-1.0)).abs() < 1e-6);

        // Same payload bytes, little-endian flag set: values decode differently.
        let mut le_body = body.clone();
        le_body[0] = 0x08 | 0x80;
        let le_samples = parse_snd_frame(&le_body).expect("valid frame").samples;
        assert_eq!(le_samples.len(), 2);
        assert_ne!(le_samples[0].re, samples[0].re);
    }

    #[test]
    fn frame_shorter_than_header_is_malformed() {
        assert!(parse_snd_frame(&[0u8; 10]).is_none());
    }

    #[test]
    fn header_only_frame_is_valid_with_zero_samples() {
        let mut body = vec![0u8; 17];
        body[1..5].copy_from_slice(&7u32.to_le_bytes());
        let frame = parse_snd_frame(&body).expect("header-only frame is valid");
        assert_eq!(frame.seq, 7);
        assert!(frame.samples.is_empty());
    }

    #[test]
    fn seq_tracker_first_frame_sets_baseline_without_loss() {
        let mut t = SndSeqTracker::default();
        assert_eq!(t.observe(7), SeqObservation::Baseline);
    }

    #[test]
    fn seq_tracker_consecutive_frames_report_no_gap() {
        let mut t = SndSeqTracker::default();
        t.observe(7);
        assert_eq!(t.observe(8), SeqObservation::InOrder);
        assert_eq!(t.observe(9), SeqObservation::InOrder);
    }

    #[test]
    fn seq_tracker_forward_jump_reports_one_gap_and_missing_frames() {
        let mut t = SndSeqTracker::default();
        t.observe(7);
        assert_eq!(t.observe(11), SeqObservation::Gap { missing: 3 });
    }

    #[test]
    fn seq_tracker_wraps_at_u32_max() {
        let mut t = SndSeqTracker::default();
        t.observe(u32::MAX);
        assert_eq!(t.observe(0), SeqObservation::InOrder);

        let mut t2 = SndSeqTracker::default();
        t2.observe(u32::MAX);
        assert_eq!(t2.observe(2), SeqObservation::Gap { missing: 2 });
    }

    #[test]
    fn seq_tracker_duplicate_or_backward_rebaselines_without_counting() {
        let mut t = SndSeqTracker::default();
        t.observe(100);
        assert_eq!(t.observe(100), SeqObservation::Resync);

        let mut t2 = SndSeqTracker::default();
        t2.observe(100);
        assert_eq!(t2.observe(50), SeqObservation::Resync);
        assert_eq!(t2.observe(51), SeqObservation::InOrder);
    }

    #[test]
    fn seq_tracker_implausibly_large_jump_rebaselines() {
        let mut t = SndSeqTracker::default();
        t.observe(0);
        assert_eq!(
            t.observe(MAX_PLAUSIBLE_SEQ_JUMP + 1),
            SeqObservation::Resync
        );
    }

    #[test]
    fn kiwi_health_counters_apply_tracker_results() {
        let counters = InputHealthCounters::new();
        let mut tracker = SndSeqTracker::default();

        let frame_with_seq = |seq: u32| {
            let mut body = vec![0u8; 17];
            body[1..5].copy_from_slice(&seq.to_le_bytes());
            body
        };

        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(1)).is_some());
        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(2)).is_some());
        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(5)).is_some());
        assert!(account_snd_frame(&counters, &mut tracker, &[0u8; 5]).is_none());

        assert_eq!(counters.gaps_detected(), 1);
        assert_eq!(counters.dropped_packets(), 2);
        assert_eq!(counters.malformed_packets(), 1);
    }

    /// A malformed frame was received, not lost: it must not also be counted
    /// as a gap/dropped frame by the next valid frame's `seq` delta.
    #[test]
    fn malformed_frame_between_valid_frames_is_not_counted_as_lost() {
        let counters = InputHealthCounters::new();
        let mut tracker = SndSeqTracker::default();

        let frame_with_seq = |seq: u32| {
            let mut body = vec![0u8; 17];
            body[1..5].copy_from_slice(&seq.to_le_bytes());
            body
        };

        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(1)).is_some());
        assert!(account_snd_frame(&counters, &mut tracker, &[0u8; 5]).is_none());
        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(3)).is_some());

        assert_eq!(counters.malformed_packets(), 1);
        assert_eq!(counters.gaps_detected(), 0);
        assert_eq!(counters.dropped_packets(), 0);

        // Real loss alongside a malformed arrival still counts, net of it:
        // of seq 4-6, one arrived malformed and two were lost.
        assert!(account_snd_frame(&counters, &mut tracker, &[0u8; 5]).is_none());
        assert!(account_snd_frame(&counters, &mut tracker, &frame_with_seq(7)).is_some());

        assert_eq!(counters.malformed_packets(), 2);
        assert_eq!(counters.gaps_detected(), 1);
        assert_eq!(counters.dropped_packets(), 2);
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
