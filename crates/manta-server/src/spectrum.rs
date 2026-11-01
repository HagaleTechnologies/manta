//! MAN-141: N1MM+ Spectrum Display datagrams (the format CW Skimmer's
//! "Send Spectrum via UDP" feeds) and the per-feed sender thread.
//!
//! The decode side hands averaged frames to a [`FrameSender`], which only
//! queues them (it never blocks). One thread per feed formats each frame as
//! N1MM+'s published `<Spectrum>` XML and sends it with an unconnected
//! `send_to`, so N1MM+ may start before or after manta. Every frame that
//! does not reach the OS is counted in [`SpectrumStats`] by reason.

use std::fmt::Write as _;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The UDP port N1MM+ listens on for Spectrum Display data.
pub const N1MM_SPECTRUM_PORT: u16 = 13064;
/// dB per level in `SpectrumData`, sent as `ScalingFactor`.
pub const SCALING_FACTOR_DB: f64 = 0.5;
/// The dB value sent as level 0.
pub const FLOOR_DB: f64 = -140.0;
/// The shortest wall-clock gap between two datagrams of one feed.
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Frames the decode side can queue before it drops one.
pub const QUEUE_FRAMES: usize = 4;

/// The constant `app` value of N1MM+'s published example.
const APP: &str = "WaterfallBandmap";

/// One averaged spectrum: ascending frequency, evenly spaced, `low_hz` and
/// `high_hz` the first and last bin centres in calibrated RF Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub low_hz: f64,
    pub high_hz: f64,
    pub power_db: Vec<f32>,
}

/// `round((dB - FLOOR_DB) / SCALING_FACTOR_DB)`, clamped to `0..=65535`;
/// NaN is level 0.
pub fn levels(power_db: &[f32]) -> Vec<u16> {
    power_db
        .iter()
        .map(|&db| {
            let level = ((f64::from(db) - FLOOR_DB) / SCALING_FACTOR_DB).round();
            if level.is_nan() {
                0
            } else {
                // `as` saturates, and the clamp makes the range explicit.
                level.clamp(0.0, f64::from(u16::MAX)) as u16
            }
        })
        .collect()
}

/// Hz as kHz with at most 3 decimals, trailing zeros (and a bare dot)
/// trimmed: `14_000_000.0` is `14000`, `14_145_906.25` is `14145.906`.
fn khz(hz: f64) -> String {
    let text = format!("{:.3}", hz / 1000.0);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

/// Escapes the five XML special characters.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One N1MM+ Spectrum Display datagram: UTF-8, CRLF line ends, one tab of
/// indent, no trailing CRLF after `</Spectrum>`.
pub fn n1mm_datagram(name: &str, frame: &Frame) -> String {
    let values = levels(&frame.power_db);
    let mut data = String::with_capacity(values.len() * 4);
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            data.push(',');
        }
        let _ = write!(data, "{v}");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <Spectrum>\r\n\
         \t<app>{APP}</app>\r\n\
         \t<Name>{}</Name>\r\n\
         \t<LowScopeFrequency>{}</LowScopeFrequency>\r\n\
         \t<HighScopeFrequency>{}</HighScopeFrequency>\r\n\
         \t<ScalingFactor>{SCALING_FACTOR_DB}</ScalingFactor>\r\n\
         \t<DataCount>{}</DataCount>\r\n\
         \t<SpectrumData>{data}</SpectrumData>\r\n\
         </Spectrum>",
        xml_escape(name),
        khz(frame.low_hz),
        khz(frame.high_hz),
        values.len(),
    )
}

/// Why a frame was not handed to the OS; the `reason` label of
/// `manta_spectrum_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpectrumDrop {
    /// The decode side found the feed's queue full.
    QueueFull,
    /// The frame arrived less than [`MIN_INTERVAL`] after the last datagram.
    RateLimit,
    /// `send_to` failed.
    SendError,
}

impl SpectrumDrop {
    /// Every reason, in render order.
    pub const ALL: [SpectrumDrop; 3] = [
        SpectrumDrop::QueueFull,
        SpectrumDrop::RateLimit,
        SpectrumDrop::SendError,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SpectrumDrop::QueueFull => "queue_full",
            SpectrumDrop::RateLimit => "rate_limit",
            SpectrumDrop::SendError => "send_error",
        }
    }
}

/// One feed's counters, shared by the decode side, the sender thread and
/// `/metrics`.
#[derive(Debug, Default)]
pub struct SpectrumStats {
    sent: AtomicU64,
    queue_full: AtomicU64,
    rate_limit: AtomicU64,
    send_error: AtomicU64,
}

impl SpectrumStats {
    /// Datagrams handed to the OS.
    pub fn datagrams(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Frames not sent for `reason`.
    pub fn dropped(&self, reason: SpectrumDrop) -> u64 {
        self.counter(reason).load(Ordering::Relaxed)
    }

    fn counter(&self, reason: SpectrumDrop) -> &AtomicU64 {
        match reason {
            SpectrumDrop::QueueFull => &self.queue_full,
            SpectrumDrop::RateLimit => &self.rate_limit,
            SpectrumDrop::SendError => &self.send_error,
        }
    }

    fn record_drop(&self, reason: SpectrumDrop) {
        self.counter(reason).fetch_add(1, Ordering::Relaxed);
    }

    fn record_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// The decode side's handle: [`FrameSender::offer`] never blocks. The
/// sender thread ends once every handle is dropped.
#[derive(Debug)]
pub struct FrameSender {
    tx: SyncSender<Frame>,
    stats: Arc<SpectrumStats>,
}

impl FrameSender {
    /// Queues `frame` for the sender thread, or counts it as `queue_full`
    /// when the queue is full (or the thread has gone).
    pub fn offer(&self, frame: Frame) {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(frame)
        {
            self.stats.record_drop(SpectrumDrop::QueueFull);
        }
    }

    pub fn stats(&self) -> &Arc<SpectrumStats> {
        &self.stats
    }
}

/// Starts `manta-spectrum-<input>`. `socket` is already bound (by
/// `manta-cli`). `on_first_error` is called once, with the error, the first
/// time `send_to` fails.
pub fn spawn(
    socket: UdpSocket,
    dest: SocketAddr,
    input: &str,
    name: String,
    stats: Arc<SpectrumStats>,
    on_first_error: Box<dyn FnOnce(&io::Error) + Send>,
) -> io::Result<(FrameSender, JoinHandle<()>)> {
    let (tx, rx) = sync_channel(QUEUE_FRAMES);
    let thread_stats = stats.clone();
    let handle = std::thread::Builder::new()
        .name(format!("manta-spectrum-{input}"))
        .spawn(move || run(socket, dest, &name, &rx, &thread_stats, on_first_error))?;
    Ok((FrameSender { tx, stats }, handle))
}

fn run(
    socket: UdpSocket,
    dest: SocketAddr,
    name: &str,
    rx: &Receiver<Frame>,
    stats: &SpectrumStats,
    on_first_error: Box<dyn FnOnce(&io::Error) + Send>,
) {
    let mut on_first_error = Some(on_first_error);
    let mut last_send: Option<Instant> = None;
    while let Ok(frame) = rx.recv() {
        if last_send.is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
            stats.record_drop(SpectrumDrop::RateLimit);
            continue;
        }
        let datagram = n1mm_datagram(name, &frame);
        last_send = Some(Instant::now());
        match socket.send_to(datagram.as_bytes(), dest) {
            Ok(_) => stats.record_sent(),
            Err(e) => {
                stats.record_drop(SpectrumDrop::SendError);
                if let Some(warn) = on_first_error.take() {
                    warn(&e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_matches_the_documented_layout_byte_for_byte() {
        let f = Frame {
            low_hz: 14_000_000.0,
            high_hz: 14_000_187.5,
            power_db: vec![-140.0, -20.0, 0.0],
        };
        assert_eq!(
            n1mm_datagram("manta hpsdr", &f),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n<Spectrum>\r\n\t<app>WaterfallBandmap</app>\r\n\
             \t<Name>manta hpsdr</Name>\r\n\t<LowScopeFrequency>14000</LowScopeFrequency>\r\n\
             \t<HighScopeFrequency>14000.188</HighScopeFrequency>\r\n\t<ScalingFactor>0.5</ScalingFactor>\r\n\
             \t<DataCount>3</DataCount>\r\n\t<SpectrumData>0,240,280</SpectrumData>\r\n</Spectrum>"
        );
    }

    #[test]
    fn levels_round_clamp_and_map_nan_to_zero() {
        assert_eq!(
            levels(&[-141.0, -139.7, -20.0, 1e6, f32::NAN]),
            vec![0, 1, 240, 65535, 0]
        );
    }

    #[test]
    fn khz_prints_at_most_three_decimals_without_trailing_zeros() {
        assert_eq!(khz(14_000_000.0), "14000");
        assert_eq!(khz(14_145_906.25), "14145.906");
        assert_eq!(khz(7_000_500.0), "7000.5");
        // Rust's {:.3} prints 14000.1875 as 14000.188.
        assert_eq!(khz(14_000_187.5), "14000.188");
    }

    #[test]
    fn names_are_xml_escaped() {
        assert_eq!(
            xml_escape("A&B <x> \"q\" 'a'"),
            "A&amp;B &lt;x&gt; &quot;q&quot; &apos;a&apos;"
        );
        let f = Frame {
            low_hz: 1.0,
            high_hz: 2.0,
            power_db: vec![],
        };
        assert!(n1mm_datagram("a<b", &f).contains("\t<Name>a&lt;b</Name>\r\n"));
    }

    #[test]
    fn a_full_queue_drops_and_counts_the_frame() {
        let (tx, _rx) = sync_channel(QUEUE_FRAMES);
        let stats = Arc::new(SpectrumStats::default());
        let sender = FrameSender {
            tx,
            stats: stats.clone(),
        };
        let frame = Frame {
            low_hz: 1.0,
            high_hz: 2.0,
            power_db: vec![0.0],
        };
        for _ in 0..QUEUE_FRAMES + 1 {
            sender.offer(frame.clone());
        }
        assert_eq!(stats.dropped(SpectrumDrop::QueueFull), 1);
        assert_eq!(stats.dropped(SpectrumDrop::RateLimit), 0);
        assert_eq!(stats.datagrams(), 0);
    }
}
