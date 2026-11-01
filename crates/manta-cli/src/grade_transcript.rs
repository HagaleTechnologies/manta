//! MAN-97: a skimmer's `DX de` telnet output, saved to a file, as reference
//! reports for `manta grade --reference`. Lines are read by the relay's own
//! parser (`manta_server::relay::skimmer_line::parse`, both layouts).
//!
//! A line may start with its UTC arrival time
//! (`2026-10-31T00:05:00Z DX de ...`), which is then the report's time. A
//! line starting `DX de` takes its day from the file name and its minute
//! from its own `HHMMZ`, at the minute's middle (30 s in), so the time is at
//! most 30 s off either way; a minute more than 12 hours before the
//! previous date-less line's starts the next UTC day.

use manta_server::rbn_compare::grade::RbnReport;
use manta_server::relay::skimmer_line::{self, RelayLine};

/// One CW report from a transcript (or an RBN CSV row), with the skimmer's
/// own `dB` figure when the source has one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReferenceReport {
    pub report: RbnReport,
    /// The source's SNR figure, verbatim (500 Hz convention).
    pub snr_db: Option<i32>,
}

/// What `parse_transcript` skipped, for the summary header.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptStats {
    /// Well-formed spot lines in another mode (RTTY ...).
    pub non_cw: usize,
    /// Anything else that is not blank: banners, prompts, torn lines.
    pub other: usize,
}

/// A bare line's minute this far before the previous one's starts the next
/// UTC day: 12 hours.
const ROLLOVER_MINUTES: i64 = 12 * 60;

/// UTC midnight of the last real `YYYY-MM-DD` in `name`, as Unix seconds.
pub(crate) fn day_from_file_name(name: &str) -> Option<i64> {
    (0..name.len().saturating_sub(9))
        .rev()
        // `get` is `None` off a char boundary; a date is ASCII throughout.
        .filter_map(|i| name.get(i..i + 10))
        .filter(|w| is_date_shaped(w))
        .find_map(|w| manta_testkit::oracle::parse_utc_timestamp(&format!("{w}T00:00:00Z")).ok())
}

/// `YYYY-MM-DD` in shape (digits and two hyphens), not checked as a date.
fn is_date_shaped(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// The UTC stamp before `DX de`, and the `DX de` text: one token
/// (`2026-10-31T00:05:00Z`), or a date and a time apart
/// (`2026-10-31 00:05:00Z`). `None` when the line has no stamp form.
fn split_stamp(line: &str) -> Option<(String, &str)> {
    let (first, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    if rest.starts_with("DX de ") {
        return Some((first.to_string(), rest));
    }
    let (second, rest) = rest.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    (is_date_shaped(first) && rest.starts_with("DX de "))
        .then(|| (format!("{first} {second}"), rest))
}

/// The minute of the day a `HHMMZ` token names, or `None` past 23:59.
fn minute_of_day(hhmmz: &str) -> Option<i64> {
    let hh: i64 = hhmmz.get(0..2)?.parse().ok()?;
    let mm: i64 = hhmmz.get(2..4)?.parse().ok()?;
    (hh <= 23 && mm <= 59).then_some(hh * 60 + mm)
}

/// One file's body to reports, in file order; `day` is the file name's UTC
/// day ([`day_from_file_name`]). Errors read `line {n}: ...`; the caller
/// adds the path.
pub(crate) fn parse_transcript(
    body: &str,
    day: Option<i64>,
) -> std::result::Result<(Vec<ReferenceReport>, TranscriptStats), String> {
    // A file saved on Windows may start with a byte-order mark.
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    let mut reports = Vec::new();
    let mut stats = TranscriptStats::default();
    // The previous bare spot line's minute, and the days added so far.
    let mut prev_minute: Option<i64> = None;
    let mut day_offset = 0i64;
    for (i, raw) in body.split('\n').enumerate() {
        let n = i + 1;
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let (stamp, text) = if line.starts_with("DX de ") {
            (None, line)
        } else {
            match split_stamp(line) {
                Some((token, rest)) => {
                    let t = manta_testkit::oracle::parse_utc_timestamp(&token).map_err(|_| {
                        format!(
                            "line {n}: bad UTC stamp {} before DX de",
                            crate::fmt::quoted(&token)
                        )
                    })?;
                    (Some(t), rest)
                }
                None => {
                    stats.other += 1;
                    continue;
                }
            }
        };
        let parsed = skimmer_line::parse(text);
        if matches!(parsed, RelayLine::NotSpot) {
            stats.other += 1;
            continue;
        }
        let time = match stamp {
            Some(t) => t,
            None => {
                // `parse` has checked the last token is `HHMMZ`.
                let hhmmz = text.split_whitespace().last().unwrap_or_default();
                let Some(minute) = minute_of_day(hhmmz) else {
                    stats.other += 1;
                    continue;
                };
                if prev_minute.is_some_and(|p| p - minute > ROLLOVER_MINUTES) {
                    day_offset += 1;
                }
                prev_minute = Some(minute);
                if matches!(parsed, RelayLine::NotCw) {
                    stats.non_cw += 1;
                    continue;
                }
                let Some(day) = day else {
                    return Err(format!(
                        "line {n}: a DX de line with no UTC stamp needs its UTC day in the \
                         file name (as reference-2026-10-31.txt)"
                    ));
                };
                day + day_offset * 86_400 + minute * 60 + 30
            }
        };
        match parsed {
            RelayLine::Cw(s) => reports.push(ReferenceReport {
                report: RbnReport {
                    spotter: s.spotter,
                    call: s.call.to_ascii_uppercase(),
                    freq_hz: s.freq_khz * 1000.0,
                    wpm: Some(f64::from(s.wpm)).filter(|w| *w > 0.0),
                    time,
                },
                snr_db: Some(s.snr_db),
            }),
            RelayLine::NotCw => stats.non_cw += 1,
            RelayLine::NotSpot => stats.other += 1,
        }
    }
    Ok((reports, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-31 00:00:00Z.
    const DAY: i64 = 1_793_404_800;

    fn parse_ok(body: &str, day: Option<i64>) -> (Vec<ReferenceReport>, TranscriptStats) {
        parse_transcript(body, day).unwrap()
    }

    #[test]
    fn stamped_rbn_and_skimmer_layout_lines_become_cw_reports() {
        let body = "2026-10-31T00:05:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n\
                    2026-10-31T01:10:00Z DX de W3XYZ-#:  14030.0  DL1ABC         19 dB  28 WPM  CQ      0110Z\n";
        let (reports, stats) = parse_ok(body, None);
        assert_eq!(stats, TranscriptStats::default());
        assert_eq!(
            reports,
            vec![
                ReferenceReport {
                    report: RbnReport {
                        spotter: "W3XYZ".to_string(),
                        call: "W1AW".to_string(),
                        freq_hz: 14_025_000.0,
                        wpm: Some(25.0),
                        time: DAY + 300,
                    },
                    snr_db: Some(21),
                },
                ReferenceReport {
                    report: RbnReport {
                        spotter: "W3XYZ".to_string(),
                        call: "DL1ABC".to_string(),
                        freq_hz: 14_030_000.0,
                        wpm: Some(28.0),
                        time: DAY + 4200,
                    },
                    snr_db: Some(19),
                },
            ]
        );
    }

    #[test]
    fn a_bare_line_takes_the_file_day_and_the_middle_of_its_minute() {
        let body = "DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n";
        let (reports, _) = parse_ok(body, Some(DAY));
        assert_eq!(reports[0].report.time, DAY + 5 * 60 + 30);
    }

    #[test]
    fn a_bare_minute_twelve_hours_back_starts_the_next_day() {
        let body = "DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      2358Z\n\
                    DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0003Z\n";
        let (reports, _) = parse_ok(body, Some(DAY));
        assert_eq!(reports[0].report.time, DAY + (23 * 60 + 58) * 60 + 30);
        assert_eq!(reports[1].report.time, DAY + 86_400 + 3 * 60 + 30);
    }

    #[test]
    fn a_small_step_back_stays_on_the_same_day() {
        let body = "DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      1235Z\n\
                    DX de W3XYZ-#:  14025.0  K1ABC          CW    21 dB  25 WPM  CQ      1234Z\n";
        let (reports, _) = parse_ok(body, Some(DAY));
        assert_eq!(reports[1].report.time, DAY + (12 * 60 + 34) * 60 + 30);
    }

    #[test]
    fn a_bare_line_without_a_file_day_is_an_error_naming_its_line() {
        let body = "CW Skimmer Server\n\n\
                    DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n";
        assert_eq!(
            parse_transcript(body, None).unwrap_err(),
            "line 3: a DX de line with no UTC stamp needs its UTC day in the file name \
             (as reference-2026-10-31.txt)"
        );
    }

    #[test]
    fn a_bad_stamp_before_dx_de_is_an_error_naming_its_line() {
        let body = "banner\n\
                    2026-10-31T25:00:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n";
        assert_eq!(
            parse_transcript(body, Some(DAY)).unwrap_err(),
            "line 2: bad UTC stamp \"2026-10-31T25:00:00Z\" before DX de"
        );
    }

    #[test]
    fn non_cw_banner_prompt_and_torn_lines_are_counted_not_errors() {
        let body = "2026-10-31T00:00:01Z CW Skimmer Server\n\
                    Please enter your callsign:\n\
                    2026-10-31T02:10:00Z DX de W3XYZ-#:  14080.0  RY1AB          RTTY  10 dB  45 BPS  CQ      0210Z\n\
                    2026-10-31T02:11:00Z DX de W3XYZ-#:  14025.0  W1A";
        let (reports, stats) = parse_ok(body, None);
        assert!(reports.is_empty());
        assert_eq!(
            stats,
            TranscriptStats {
                non_cw: 1,
                other: 3
            }
        );
    }

    #[test]
    fn a_stamp_may_be_a_date_and_a_time_apart() {
        let body = "2026-10-31 00:05:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n";
        let (reports, stats) = parse_ok(body, None);
        assert_eq!(stats, TranscriptStats::default());
        assert_eq!(reports[0].report.time, DAY + 300);
    }

    #[test]
    fn a_leading_byte_order_mark_is_ignored() {
        let body = "\u{feff}2026-10-31T00:05:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n";
        let (reports, _) = parse_ok(body, None);
        assert_eq!(reports[0].report.time, DAY + 300);
    }

    #[test]
    fn blank_lines_and_crlf_are_ignored() {
        let body = "\r\n  \r\n2026-10-31T00:05:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\r\n\r\n";
        let (reports, stats) = parse_ok(body, None);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].report.call, "W1AW");
        assert_eq!(stats, TranscriptStats::default());
    }

    #[test]
    fn file_day_is_the_last_real_date_in_the_name() {
        assert_eq!(day_from_file_name("reference-2026-10-31.txt"), Some(DAY));
        assert_eq!(
            day_from_file_name("skimsrv_2026-10-30_to_2026-10-31.log"),
            Some(DAY)
        );
        assert_eq!(
            day_from_file_name("skimsrv_2026-10-30_to_2026-13-40.log"),
            Some(DAY - 86_400)
        );
        assert_eq!(day_from_file_name("capture.txt"), None);
        assert_eq!(day_from_file_name("x-2026-13-40.txt"), None);
        assert_eq!(day_from_file_name(""), None);
    }
}
