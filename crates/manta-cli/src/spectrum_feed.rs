//! MAN-141: an input's `spectrum_udp` feed to N1MM+'s Spectrum Display.
//! `manta-cli` binds the socket (as for `[[wsjtx_relay]]`, MAN-320 D11) and
//! glues the engine's write-only `SpectrumSink` to `manta-server`'s sender
//! thread, so neither crate sees the other's types.

use anyhow::{anyhow, Result};
use manta_engine::{SpectrumFrame, SpectrumSink, SpectrumSpan};
use manta_server::metrics::Metrics;
use manta_server::spectrum::{self, FrameSender, SpectrumStats};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::SpectrumOut;

/// A configured feed whose socket is bound, before the source opens.
pub(crate) struct BoundFeed {
    /// The input name the feed's thread, metrics and messages use.
    pub input: String,
    /// The N1MM+ source name (`spectrum_name` or `manta <input>`).
    pub name: String,
    pub dest: SocketAddr,
    socket: UdpSocket,
}

/// Binds an unconnected UDP socket for `out` (design section 2): loopback
/// for a loopback destination, the wildcard otherwise, 1 s write timeout.
pub(crate) fn bind(input: &str, out: &SpectrumOut) -> Result<BoundFeed> {
    let dest = out.dest;
    let socket = manta_input::local_bind::bind_udp_for(dest.ip())
        .and_then(|s| {
            s.set_write_timeout(Some(Duration::from_secs(1)))?;
            Ok(s)
        })
        .map_err(|e| {
            anyhow!("input.spectrum_udp: cannot open a UDP socket to send to {dest}: {e}")
        })?;
    Ok(BoundFeed {
        input: input.to_string(),
        name: out.source_name(input),
        dest,
        socket,
    })
}

/// The engine-side sink: maps each frame to `manta-server`'s type and
/// offers it (never blocks; a full queue is counted as `queue_full`).
struct SpectrumFeed(FrameSender);

impl SpectrumSink for SpectrumFeed {
    fn publish(&self, frame: SpectrumFrame) {
        self.0.offer(spectrum::Frame {
            low_hz: frame.low_hz,
            high_hz: frame.high_hz,
            power_db: frame.power_db,
        });
    }
}

/// A running feed: the engine's sink and the sender thread to join.
pub(crate) type StartedFeed = (Arc<dyn SpectrumSink>, JoinHandle<()>);

/// Starts `feed`'s sender thread. Its counters join `/metrics` only with
/// `[server]`; the first `send_to` failure logs one warning.
pub(crate) fn start(
    feed: BoundFeed,
    metrics: Option<&Metrics>,
) -> Result<StartedFeed> {
    let stats = match metrics {
        Some(m) => m.register_spectrum(feed.input.clone()),
        None => Arc::new(SpectrumStats::default()),
    };
    let input = feed.input.clone();
    let dest = feed.dest;
    let on_first_error: Box<dyn FnOnce(&std::io::Error) + Send> = Box::new(move |e| {
        crate::logging::warning(&send_error_warning(&input, dest, e));
    });
    let (sender, handle) = spectrum::spawn(
        feed.socket,
        feed.dest,
        &feed.input,
        feed.name,
        stats,
        on_first_error,
    )
    .map_err(|e| anyhow!("input.spectrum_udp: cannot start the spectrum sender: {e}"))?;
    Ok((Arc::new(SpectrumFeed(sender)), handle))
}

fn send_error_warning(input: &str, dest: SocketAddr, e: &std::io::Error) -> String {
    format!(
        "warning: spectrum: input \"{input}\" cannot send to {dest}: {e}; manta keeps trying \
         and counts each failure in manta_spectrum_dropped_total{{reason=\"send_error\"}}"
    )
}

/// Trims a kHz value to at most three decimals, as the datagram prints it.
fn khz(hz: f64) -> String {
    let s = format!("{:.3}", hz / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The startup note (design section 2), from the same span the engine uses.
pub(crate) fn startup_note(feed: &BoundFeed, span: &SpectrumSpan) -> String {
    format!(
        "spectrum: input \"{}\" sends {} levels, {} to {} kHz, to {} as \"{}\" for N1MM+'s \
         Spectrum Display, 3 per second",
        feed.input,
        span.bins.len(),
        khz(span.low_hz),
        khz(span.high_hz),
        feed.dest,
        feed.name
    )
}

/// The warning when the opened source has no spectrum to send (no RF
/// centre, or no channel inside its passband); the feed then sends nothing.
pub(crate) fn no_span_warning(feed: &BoundFeed) -> String {
    format!(
        "warning: spectrum: input \"{}\" reports no RF passband the spectrum feed can send; \
         sending nothing to {}",
        feed.input, feed.dest
    )
}

/// Builds the engine sink for an opened source, printing the startup note,
/// or warns and returns `None` when the source has no span to send.
pub(crate) fn open(
    feed: BoundFeed,
    sample_rate: f64,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    freq_correction_ppm: f64,
    metrics: Option<&Metrics>,
) -> Result<Option<StartedFeed>> {
    let factor =
        manta_spot::calibration_factor_from_ppm(freq_correction_ppm).map_err(|e| anyhow!(e))?;
    match manta_engine::spectrum_span(sample_rate, center_freq_hz, rf_passband_hz, factor) {
        Some(span) => {
            crate::logging::note(&startup_note(&feed, &span));
            start(feed, metrics).map(Some)
        }
        None => {
            crate::logging::warning(&no_span_warning(&feed));
            Ok(None)
        }
    }
}

/// A multi-input feed's engine sink, set once its input has opened and its
/// sender started; a frame before then (none is produced) is dropped.
#[derive(Default)]
pub(crate) struct LateSink(std::sync::OnceLock<Arc<dyn SpectrumSink>>);

impl LateSink {
    pub(crate) fn set(&self, sink: Arc<dyn SpectrumSink>) {
        let _ = self.0.set(sink);
    }
}

impl SpectrumSink for LateSink {
    fn publish(&self, frame: SpectrumFrame) {
        if let Some(sink) = self.0.get() {
            sink.publish(frame);
        }
    }
}

/// The soak/doctor note: they decode without sending (as for `record_iq`).
pub(crate) fn ignored_note(command: &str, loaded: &crate::config::Loaded) -> Option<String> {
    let set = loaded.input.shared.spectrum.is_some()
        || loaded
            .inputs
            .iter()
            .any(|n| n.input.shared.spectrum.is_some());
    set.then(|| {
        format!(
            "note: {command} does not send spectrum; ignoring input.spectrum_udp from {}",
            loaded.origin
        )
    })
}

/// The `run` refusal when a feed's receiver reports no RF frequency.
pub(crate) const NEEDS_RF_MESSAGE: &str = "input.spectrum_udp needs a real RF frequency: this \
     receiver reports none, so N1MM+ would draw audio offsets; set input.center_freq_hz to the \
     radio's dial frequency";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_note_names_the_span_destination_and_source() {
        let out = SpectrumOut {
            dest: "127.0.0.1:13064".parse().unwrap(),
            name: None,
        };
        let feed = bind("hpsdr", &out).unwrap();
        let span = manta_engine::spectrum_span(192_000.0, 14_050_000.0, (-96_000.0, 96_000.0), 1.0)
            .unwrap();
        assert_eq!(
            startup_note(&feed, &span),
            "spectrum: input \"hpsdr\" sends 2048 levels, 13954 to 14145.906 kHz, to \
             127.0.0.1:13064 as \"manta hpsdr\" for N1MM+'s Spectrum Display, 3 per second"
        );
    }

    #[test]
    fn the_send_warning_names_the_metric() {
        let e = std::io::Error::other("boom");
        assert_eq!(
            send_error_warning("hpsdr", "192.168.1.50:13064".parse().unwrap(), &e),
            "warning: spectrum: input \"hpsdr\" cannot send to 192.168.1.50:13064: boom; manta \
             keeps trying and counts each failure in \
             manta_spectrum_dropped_total{reason=\"send_error\"}"
        );
    }
}
