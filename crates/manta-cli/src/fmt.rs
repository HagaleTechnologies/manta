//! Human reports and terminal error presentation.

use manta_server::human::{confidence, db, khz, wpm};
use manta_server::status_doc::escape_for_terminal;

pub fn wpm_opt(value: Option<f32>) -> String {
    value.map(wpm).unwrap_or_else(|| "unknown".into())
}

pub fn spot_line(spot: &manta_spot::Spot) -> String {
    format!(
        "SPOT: {} ({}) {} {} {} conf={}",
        spot.callsign,
        spot.spot_type,
        khz(spot.freq_hz),
        db(spot.snr_db),
        wpm(spot.wpm),
        confidence(spot.confidence)
    )
}

pub fn decode_summary(report: &manta_engine::DecodeReport) -> String {
    format!(
        "frequency: {}  speed: {}  spots: {}",
        khz(report.freq_hz),
        wpm_opt(report.wpm),
        report.spots.len()
    )
}

/// Keep every non-control character, including leading and trailing spaces.
pub fn cause_text(err: &anyhow::Error) -> String {
    err.chain()
        .map(|cause| escape_for_terminal(&cause.to_string()))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

pub fn render_error(err: &anyhow::Error) -> String {
    format!("error: {}", cause_text(err))
}

pub use manta_server::human::quoted;

pub fn soak_report(report: &manta_engine::SoakReport) -> String {
    format!(
        "soak: {}\nevents: {}\nRSS growth: {:.1} MiB\npanicked: {}\n",
        if manta_engine::soak_passed(report) {
            "passed"
        } else {
            "failed"
        },
        report.events_emitted,
        report.rss_growth_bytes as f64 / (1024.0 * 1024.0),
        if report.panicked { "yes" } else { "no" }
    )
}

pub fn doctor_report(report: &manta_engine::DoctorReport) -> String {
    let snr = match (report.snr_db_min, report.snr_db_median, report.snr_db_max) {
        (Some(min), Some(median), Some(max)) => {
            format!("min={} median={} max={}", db(min), db(median), db(max))
        }
        _ if report.tracks_promoted == 0 => "no TrackMeta events -- no track ever promoted".into(),
        _ => {
            "a track was promoted but no TrackMeta ever landed for it before this run ended".into()
        }
    };
    format!(
        "source: {:.0} Hz sample rate, {} center, observed for {:.1}s\n\
tracks: {} promoted, {} TrackMeta updates, {} closed\n\
SNR (2500 Hz reference): {}\n\
decode: {} chars ({} distinct), {} confirmed spots\n\
verdict: {}\n",
        report.sample_rate_hz,
        khz(report.center_freq_hz),
        report.duration.as_secs_f64(),
        report.tracks_promoted,
        report.track_meta_count,
        report.tracks_closed,
        snr,
        report.chars_decoded,
        report.distinct_chars,
        report.spots_confirmed,
        report.verdict().summary()
    )
}

// All monitor state and writes share this lock. A diagnostic also holds the
// stderr lock for its entire record, including the preceding newline.
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

#[derive(Default)]
struct MonitorLine {
    open: bool,
}

impl MonitorLine {
    fn append(&mut self, writer: &mut impl Write, text: &str) {
        if text.is_empty() {
            return;
        }
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.flush();
        self.open = !text.ends_with('\n');
    }

    fn terminate(&mut self, writer: &mut impl Write) {
        if self.open {
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
            self.open = false;
        }
    }
}

static STDERR_MONITOR: Mutex<MonitorLine> = Mutex::new(MonitorLine { open: false });

pub fn monitor_write(text: &str) {
    let mut monitor = STDERR_MONITOR.lock().expect("monitor lock poisoned");
    monitor.append(&mut std::io::stderr().lock(), text);
}

pub fn end_monitor_line() {
    let mut monitor = STDERR_MONITOR.lock().expect("monitor lock poisoned");
    monitor.terminate(&mut std::io::stderr().lock());
}

pub struct DiagnosticWriter {
    _monitor: MutexGuard<'static, MonitorLine>,
    stderr: std::io::StderrLock<'static>,
}

impl Write for DiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stderr.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stderr.flush()
    }
}

/// Tracing retains this writer (and both locks) until the record is complete.
pub fn monitor_aware_stderr() -> DiagnosticWriter {
    let mut monitor = STDERR_MONITOR.lock().expect("monitor lock poisoned");
    let mut stderr = std::io::stderr().lock();
    monitor.terminate(&mut stderr);
    DiagnosticWriter {
        _monitor: monitor,
        stderr,
    }
}

macro_rules! diagnostic {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!($crate::fmt::monitor_aware_stderr(), $($arg)*);
    }};
}
pub(crate) use diagnostic;

#[cfg(test)]
mod tests {
    #[test]
    fn empty_decode_has_unknown_speed_and_no_fabricated_measurement() {
        let report = manta_engine::DecodeReport {
            freq_hz: 14_012_349.9,
            wpm: None,
            text: String::new(),
            events: Vec::new(),
            spots: Vec::new(),
        };
        assert_eq!(
            decode_summary(&report),
            "frequency: 14012.3 kHz  speed: unknown  spots: 0"
        );
    }

    #[test]
    fn monitor_diagnostics_error_and_eof_have_complete_lines() {
        let mut monitor = MonitorLine::default();
        let mut out = Vec::new();
        monitor.terminate(&mut out);
        monitor.append(&mut out, "CQ DE ");
        monitor.append(&mut out, "W1AW");
        monitor.terminate(&mut out);
        out.extend_from_slice(b"log record\n");
        monitor.append(&mut out, "K");
        monitor.terminate(&mut out);
        out.extend_from_slice(b"error: disconnected\n");
        monitor.terminate(&mut out);
        monitor.append(&mut out, "CQ");
        monitor.terminate(&mut out);
        monitor.terminate(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "CQ DE W1AW\nlog record\nK\nerror: disconnected\nCQ\n"
        );
    }

    use super::*;
    use manta_spot::{Spot, SpotType};

    #[test]
    fn spot_specimen_and_unchanged_enum_serialization() {
        let spot = Spot {
            callsign: "W1AW".into(),
            freq_hz: 14_000_759.2,
            snr_db: 29.2,
            wpm: 19.9,
            confidence: 0.794,
            spot_type: SpotType::Cq,
            track_id: 1,
            sample_ts: 0,
        };
        assert_eq!(
            spot_line(&spot),
            "SPOT: W1AW (CQ) 14000.8 kHz 29 dB 20 WPM conf=0.79"
        );
        for (value, human, json) in [
            (SpotType::Cq, "CQ", "Cq"),
            (SpotType::De, "DE", "De"),
            (SpotType::Beacon, "BEACON", "Beacon"),
            (SpotType::Unknown, "unknown", "Unknown"),
        ] {
            assert_eq!(value.to_string(), human);
            assert_eq!(serde_json::to_value(value).unwrap(), json);
        }
        assert_eq!(wpm_opt(None), "unknown");
        assert_eq!(wpm_opt(Some(17.647_058)), "18 WPM");
    }

    #[test]
    fn error_chain_preserves_spaces_unicode_and_escapes_only_controls() {
        let err = anyhow::anyhow!("  café  ")
            .context("")
            .context("open  file  ");
        assert_eq!(render_error(&err), "error: open  file  :   café  ");
        let raw = "a\n\r\n\t\0\u{1b}\u{7f}\u{85}  ";
        assert_eq!(
            cause_text(&anyhow::anyhow!(raw)),
            r"a\n\r\n\t\u{0}\u{1b}\u{7f}\u{85}  "
        );
        assert_eq!(
            cause_text(&anyhow::anyhow!(r"already\n escaped")),
            r"already\n escaped"
        );
    }

    #[test]
    fn soak_reports_use_existing_pass_rule() {
        let mut r = manta_engine::SoakReport {
            events_emitted: 0,
            rss_growth_bytes: 0,
            panicked: false,
        };
        assert_eq!(
            soak_report(&r),
            "soak: passed\nevents: 0\nRSS growth: 0.0 MiB\npanicked: no\n"
        );
        r.rss_growth_bytes = 200 * 1024 * 1024;
        assert!(soak_report(&r).starts_with("soak: failed\n"));
        r.panicked = true;
        assert_eq!(
            soak_report(&r),
            "soak: failed\nevents: 0\nRSS growth: 200.0 MiB\npanicked: yes\n"
        );
        r.rss_growth_bytes = 0;
        assert!(soak_report(&r).starts_with("soak: failed\n"));
    }

    #[test]
    fn doctor_reports_populated_and_both_absent_snr_states() {
        let mut r = manta_engine::DoctorReport {
            sample_rate_hz: 48000.0,
            center_freq_hz: 14_000_000.0,
            duration: std::time::Duration::from_secs(10),
            tracks_promoted: 61,
            track_meta_count: 64,
            tracks_closed: 36,
            snr_db_min: Some(20.4),
            snr_db_median: Some(58.9),
            snr_db_max: Some(68.0),
            chars_decoded: 42,
            distinct_chars: 15,
            spots_confirmed: 0,
        };
        assert_eq!(doctor_report(&r), format!("source: 48000 Hz sample rate, 14000.0 kHz center, observed for 10.0s\ntracks: 61 promoted, 64 TrackMeta updates, 36 closed\nSNR (2500 Hz reference): min=20 dB median=59 dB max=68 dB\ndecode: 42 chars (15 distinct), 0 confirmed spots\nverdict: {}\n", r.verdict().summary()));
        r.snr_db_min = None;
        r.snr_db_median = None;
        r.snr_db_max = None;
        assert!(doctor_report(&r).contains("SNR (2500 Hz reference): a track was promoted but no TrackMeta ever landed for it before this run ended\n"));
        r.tracks_promoted = 0;
        assert!(doctor_report(&r)
            .contains("SNR (2500 Hz reference): no TrackMeta events -- no track ever promoted\n"));
    }
}
