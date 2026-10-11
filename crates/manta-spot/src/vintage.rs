//! Age of the built-in cty.dat for the startup warning.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// UTC retrieval date of data/cty.dat, pinned to data/SOURCES.md by a test.
pub const CTY_DAT_RETRIEVED: &str = "2026-07-25";
/// Startup warns after this many whole days.
pub const CTY_DAT_STALE_AFTER_DAYS: u64 = 180;

/// Retrieval midnight in UTC; None only for a malformed constant.
pub fn cty_dat_retrieved_at() -> Option<SystemTime> {
    date_at_midnight_utc(CTY_DAT_RETRIEVED)
}
/// Whole days since retrieval, or None for a clock before retrieval.
pub fn cty_dat_age_days(now: SystemTime) -> Option<u64> {
    Some(now.duration_since(cty_dat_retrieved_at()?).ok()?.as_secs() / 86_400)
}
/// The age only when the bundled table exceeds the warning threshold.
pub fn stale_cty_dat_age_days(now: SystemTime) -> Option<u64> {
    cty_dat_age_days(now).filter(|&days| days > CTY_DAT_STALE_AFTER_DAYS)
}

#[cfg(test)]
fn age_days_since(date: &str, now: SystemTime) -> Option<u64> {
    Some(
        now.duration_since(date_at_midnight_utc(date)?)
            .ok()?
            .as_secs()
            / 86_400,
    )
}

fn date_at_midnight_utc(date: &str) -> Option<SystemTime> {
    if date.len() != 10 {
        return None;
    }
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let seconds = days_from_civil(year, month, day).checked_mul(86_400)?;
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

// Proleptic Gregorian date to days since 1970-01-01 (H. Hinnant).
fn days_from_civil(mut y: i64, m: u32, d: u32) -> i64 {
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let month = i64::from(m) + if m > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    // No test here may hardcode CTY_DAT_RETRIEVED's value: refreshing the
    // vendored file changes it, and only `cty_dat_retrieved_matches_sources_md`
    // should then need attention (and it passes once SOURCES.md matches).
    const DAY: u64 = 86_400;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }
    fn retrieved_plus(days: u64) -> SystemTime {
        cty_dat_retrieved_at().expect("constant parses") + Duration::from_secs(days * DAY)
    }

    #[test]
    fn cty_dat_retrieved_matches_sources_md() {
        // The `- Retrieved: YYYY-MM-DD` line under `## cty.dat` in data/SOURCES.md.
        let sources = include_str!("../data/SOURCES.md");
        let section = sources
            .split("## ")
            .find(|s| s.starts_with("cty.dat"))
            .unwrap();
        let line = section
            .lines()
            .find(|l| l.starts_with("- Retrieved: "))
            .unwrap();
        assert_eq!(
            line.trim_start_matches("- Retrieved: ").trim(),
            CTY_DAT_RETRIEVED
        );
        assert!(
            cty_dat_retrieved_at().is_some(),
            "{CTY_DAT_RETRIEVED} must be YYYY-MM-DD"
        );
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 7, 25), 20_659);
    }

    #[test]
    fn age_is_whole_days_since_midnight_utc() {
        // A fixed date, so this test never moves with a refresh.
        let midnight = 1_784_937_600; // 2026-07-25T00:00:00Z
        assert_eq!(age_days_since("2026-07-25", at(midnight)), Some(0));
        assert_eq!(
            age_days_since("2026-07-25", at(midnight + DAY - 1)),
            Some(0)
        );
        assert_eq!(age_days_since("2026-07-25", at(midnight + DAY)), Some(1));
        assert_eq!(age_days_since("2026-07-25", at(1_791_590_400)), Some(77)); // 2026-10-10
        assert_eq!(cty_dat_age_days(retrieved_plus(5)), Some(5));
    }

    #[test]
    fn a_clock_before_the_retrieval_date_has_no_age() {
        assert_eq!(cty_dat_age_days(UNIX_EPOCH), None);
        let just_before = cty_dat_retrieved_at().unwrap() - Duration::from_secs(1);
        assert_eq!(cty_dat_age_days(just_before), None);
    }

    #[test]
    fn stale_only_past_the_threshold() {
        assert_eq!(CTY_DAT_STALE_AFTER_DAYS, 180);
        assert_eq!(stale_cty_dat_age_days(retrieved_plus(180)), None);
        assert_eq!(stale_cty_dat_age_days(retrieved_plus(181)), Some(181));
    }

    #[test]
    fn a_malformed_date_has_no_age() {
        for bad in ["2026-13-01", "2026-07", "not-a-date", ""] {
            assert_eq!(age_days_since(bad, at(1_791_590_400)), None, "{bad}");
        }
    }
}
