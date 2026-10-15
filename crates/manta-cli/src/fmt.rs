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

/// MAN-234: `1 bad sample (NaN, …)` / `N bad samples (NaN, …)`, the one
/// place the replaced-sample wording and its limit (built from
/// `MAX_SAMPLE_MAGNITUDE`, so text and constant cannot drift) live.
fn bad_samples_phrase(n: u64) -> String {
    format!(
        "{n} bad sample{} (NaN, infinite, or larger than {:e} in magnitude)",
        if n == 1 { "" } else { "s" },
        manta_engine::MAX_SAMPLE_MAGNITUDE
    )
}

/// MAN-234: the end-of-run warning `decode`, `calibrate` and a live run's
/// final total print when `input` had `n` > 0 samples replaced.
pub fn bad_samples_warning(input: &str, n: u64) -> String {
    format!(
        "warning: {input} contained {}; each was replaced with silence and decoding \
         continued -- check the source's sample format or driver",
        bad_samples_phrase(n)
    )
}

/// MAN-234: the one line `run`/`listen` prints at the first replaced
/// sample; `daemon` (a `[server]` table) names the metric that keeps
/// counting.
pub fn bad_samples_live_warning(input: &str, n: u64, daemon: bool) -> String {
    format!(
        "warning: {input} delivered {}; replaced with silence, decoding continues. Later ones \
         are counted{} and totalled when the run ends",
        bad_samples_phrase(n),
        if daemon {
            " in manta_input_bad_samples_total"
        } else {
            ""
        }
    )
}

/// MAN-234: the `doctor`/`soak` report line, empty when nothing was replaced.
fn bad_samples_report_line(n: u64) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("input: {} replaced with silence\n", bad_samples_phrase(n))
    }
}

pub fn soak_report(report: &manta_engine::SoakReport) -> String {
    format!(
        "soak: {}\nevents: {}\nRSS growth: {:.1} MiB\npanicked: {}\n{}",
        if manta_engine::soak_passed(report) {
            "passed"
        } else {
            "failed"
        },
        report.events_emitted,
        report.rss_growth_bytes as f64 / (1024.0 * 1024.0),
        if report.panicked { "yes" } else { "no" },
        bad_samples_report_line(report.bad_samples)
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
tracks: {} promoted, {} TrackMeta updates, {} closed, {} detector confirmations\n\
SNR (2500 Hz reference): {}\n\
decode: {} chars ({} distinct), {} confirmed spots\n\
{}verdict: {}\n",
        report.sample_rate_hz,
        khz(report.center_freq_hz),
        report.duration.as_secs_f64(),
        report.tracks_promoted,
        report.track_meta_count,
        report.tracks_closed,
        report.detector_confirmations,
        snr,
        report.chars_decoded,
        report.distinct_chars,
        report.spots_confirmed,
        bad_samples_report_line(report.bad_samples),
        report.verdict().summary()
    )
}

/// The `--duration` a short WEAK_NO_DECODE run is told to retry with.
/// `manta gen v1`'s first confirmed spot lands between 20 s and 21 s.
pub const DOCTOR_RERUN_SECS: u64 = 60;

/// MAN-235: the line printed after `verdict:` when a run asked for less
/// than `DOCTOR_RERUN_SECS` reads WEAK_NO_DECODE, so a short default run
/// does not leave a healthy chain blaming mode, WPM or the grammar gate.
/// Kept out of `Verdict::summary()`, whose wording MAN-270 pins.
pub fn doctor_rerun_hint(
    report: &manta_engine::DoctorReport,
    requested: std::time::Duration,
) -> Option<String> {
    if report.verdict() != manta_engine::Verdict::WeakNoDecode
        || requested >= std::time::Duration::from_secs(DOCTOR_RERUN_SECS)
    {
        return None;
    }
    Some(format!(
        "hint: a {:.1} s run can end before a working signal is confirmed as a spot; \
a clean signal can need 20 s or more. Rerun with --duration {DOCTOR_RERUN_SECS} before \
looking for another cause.\n",
        report.duration.as_secs_f64()
    ))
}

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
            timing: None,
            rtty: Vec::new(),
            bad_samples: 0,
            sample_rate_hz: 48_000.0,
        };
        assert_eq!(
            decode_summary(&report),
            "frequency: 14012.3 kHz  speed: unknown  spots: 0"
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
            bad_samples: 0,
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
            detector_confirmations: 70,
            track_meta_count: 64,
            tracks_closed: 36,
            snr_db_min: Some(20.4),
            snr_db_median: Some(58.9),
            snr_db_max: Some(68.0),
            chars_decoded: 42,
            distinct_chars: 15,
            spots_confirmed: 0,
            bad_samples: 0,
        };
        assert_eq!(doctor_report(&r), format!("source: 48000 Hz sample rate, 14000.0 kHz center, observed for 10.0s\ntracks: 61 promoted, 64 TrackMeta updates, 36 closed, 70 detector confirmations\nSNR (2500 Hz reference): min=20 dB median=59 dB max=68 dB\ndecode: 42 chars (15 distinct), 0 confirmed spots\nverdict: {}\n", r.verdict().summary()));
        r.snr_db_min = None;
        r.snr_db_median = None;
        r.snr_db_max = None;
        assert!(doctor_report(&r).contains("SNR (2500 Hz reference): a track was promoted but no TrackMeta ever landed for it before this run ended\n"));
        r.tracks_promoted = 0;
        assert!(doctor_report(&r)
            .contains("SNR (2500 Hz reference): no TrackMeta events -- no track ever promoted\n"));
    }

    #[test]
    fn doctor_rerun_hint_only_for_a_short_weak_run() {
        use manta_engine::Verdict;
        use std::time::Duration;
        let ten = Duration::from_secs(10);
        let mut r = manta_engine::DoctorReport {
            sample_rate_hz: 96000.0,
            center_freq_hz: 14_000_000.0,
            duration: ten,
            tracks_promoted: 1,
            detector_confirmations: 1,
            track_meta_count: 9,
            tracks_closed: 1,
            snr_db_min: Some(21.0),
            snr_db_median: Some(21.0),
            snr_db_max: Some(23.0),
            chars_decoded: 12,
            distinct_chars: 9,
            spots_confirmed: 0,
            bad_samples: 0,
        };
        assert_eq!(r.verdict(), Verdict::WeakNoDecode);
        let hint = doctor_rerun_hint(&r, ten).expect("a 10 s WEAK_NO_DECODE run hints");
        assert_eq!(
            hint,
            "hint: a 10.0 s run can end before a working signal is confirmed as a spot; a \
             clean signal can need 20 s or more. Rerun with --duration 60 before looking for \
             another cause.\n"
        );
        assert!(hint.contains("Rerun with --duration 60"));
        assert!(doctor_rerun_hint(&r, Duration::from_secs(59)).is_some());
        assert_eq!(
            doctor_rerun_hint(&r, Duration::from_secs(DOCTOR_RERUN_SECS)),
            None
        );
        r.spots_confirmed = 1;
        assert_eq!(r.verdict(), Verdict::Decoding);
        assert_eq!(doctor_rerun_hint(&r, ten), None);
        r = manta_engine::DoctorReport {
            tracks_promoted: 0,
            detector_confirmations: 0,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_median: None,
            snr_db_max: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
            ..r
        };
        assert_eq!(r.verdict(), Verdict::NoSignal);
        assert_eq!(doctor_rerun_hint(&r, ten), None);
    }

    #[test]
    fn bad_samples_warning_names_the_input_the_count_and_the_limit() {
        assert_eq!(
            bad_samples_warning("nan30.wav", 1),
            "warning: nan30.wav contained 1 bad sample (NaN, infinite, or larger than 1e17 in \
             magnitude); each was replaced with silence and decoding continued -- check the \
             source's sample format or driver"
        );
        assert!(bad_samples_warning("the soapy input", 2)
            .starts_with("warning: the soapy input contained 2 bad samples (NaN"));
    }

    #[test]
    fn bad_samples_live_warning_mentions_the_metric_only_with_a_server() {
        assert_eq!(
            bad_samples_live_warning("nan30.wav", 1, false),
            "warning: nan30.wav delivered 1 bad sample (NaN, infinite, or larger than 1e17 in \
             magnitude); replaced with silence, decoding continues. Later ones are counted and \
             totalled when the run ends"
        );
        assert!(bad_samples_live_warning("nan30.wav", 1, true).ends_with(
            "Later ones are counted in manta_input_bad_samples_total and totalled when the run \
             ends"
        ));
    }

    #[test]
    fn doctor_report_adds_the_input_line_only_when_samples_were_replaced() {
        let mut r = manta_engine::DoctorReport {
            sample_rate_hz: 96000.0,
            center_freq_hz: 14_000_000.0,
            duration: std::time::Duration::from_secs(20),
            tracks_promoted: 1,
            detector_confirmations: 0,
            track_meta_count: 19,
            tracks_closed: 1,
            snr_db_min: Some(16.0),
            snr_db_median: Some(21.0),
            snr_db_max: Some(23.0),
            chars_decoded: 20,
            distinct_chars: 9,
            spots_confirmed: 0,
            bad_samples: 0,
        };
        assert!(!doctor_report(&r).contains("input:"));
        r.bad_samples = 1;
        assert!(doctor_report(&r).contains(&format!(
            "confirmed spots\ninput: 1 bad sample (NaN, infinite, or larger than 1e17 in \
             magnitude) replaced with silence\nverdict: {}\n",
            r.verdict().summary()
        )));
    }

    #[test]
    fn soak_report_adds_the_input_line_only_when_samples_were_replaced() {
        let mut r = manta_engine::SoakReport {
            events_emitted: 0,
            rss_growth_bytes: 0,
            panicked: false,
            bad_samples: 0,
        };
        assert!(soak_report(&r).ends_with("panicked: no\n"));
        r.bad_samples = 3;
        assert!(soak_report(&r).ends_with(
            "panicked: no\ninput: 3 bad samples (NaN, infinite, or larger than 1e17 in \
             magnitude) replaced with silence\n"
        ));
    }

    /// MAN-313: a run with confirmed signals but no keyed CW prints its
    /// confirmation count and the `NOT_CW` verdict.
    #[test]
    fn doctor_report_prints_detector_confirmations_and_not_cw() {
        let r = manta_engine::DoctorReport {
            sample_rate_hz: 96000.0,
            center_freq_hz: 14_000_000.0,
            duration: std::time::Duration::from_secs(10),
            tracks_promoted: 0,
            detector_confirmations: 14,
            track_meta_count: 0,
            tracks_closed: 0,
            snr_db_min: None,
            snr_db_median: None,
            snr_db_max: None,
            chars_decoded: 0,
            distinct_chars: 0,
            spots_confirmed: 0,
            bad_samples: 0,
        };
        let out = doctor_report(&r);
        assert!(
            out.contains(
                "tracks: 0 promoted, 0 TrackMeta updates, 0 closed, 14 detector confirmations\n"
            ),
            "{out}"
        );
        assert!(out.contains("\nverdict: NOT_CW -- "), "{out}");
    }
}
