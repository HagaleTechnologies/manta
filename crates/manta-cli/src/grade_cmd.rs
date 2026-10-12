//! MAN-301: `manta grade` -- grade a node's spot log against RBN's daily
//! archive, offline, through the same engine the live comparator uses
//! (`manta_server::rbn_compare::grade`). Its output depends only on its
//! inputs and flags: like `decode`/`oracle` it never reads `MANTA_*`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use manta_server::rbn_compare::grade::{
    grade_batch, normalize_spotter, parse_spot_message_line, Grade, GradeConfig, GradeInput,
    RbnReport, SpotGrade,
};

use crate::config;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// SpotMessage JSON Lines files, or directories of them (their
    /// `spots-*.jsonl` files, sorted): the `field-node.py record-spots`
    /// archive. Repeatable.
    #[arg(long, value_name = "PATH", required = true, num_args = 1..)]
    spots: Vec<PathBuf>,
    /// RBN daily-dump CSVs, or directories of them (their `*.csv` files,
    /// sorted). Unzip RBN's `.zip` first. Repeatable.
    #[arg(long, value_name = "PATH", required = true, num_args = 1..)]
    rbn: Vec<PathBuf>,
    /// Count only this RBN skimmer's reports, for example a co-located
    /// skimmer. Repeatable. Replaces [rbn_compare].spotters from --config.
    #[arg(long, value_name = "CALL", value_parser = parse_spotter)]
    spotter: Vec<String>,
    /// Never count this RBN skimmer's reports. Repeatable. Replaces
    /// [rbn_compare].exclude_spotters from --config. The spots' own `deCall`
    /// is always excluded, with and without its SSID.
    #[arg(long, value_name = "CALL", value_parser = parse_spotter)]
    exclude_spotter: Vec<String>,
    /// RBN reports this many seconds either side of a spot count (30 to
    /// 3600; default 180).
    #[arg(long, value_name = "SECS")]
    window_secs: Option<i64>,
    /// Further than this many Hz from RBN's modal frequency is a frequency
    /// disagreement (default 500).
    #[arg(long, value_name = "HZ")]
    freq_tol_hz: Option<f64>,
    /// A speed disagreement needs more than this many WPM (default 3).
    #[arg(long, value_name = "WPM")]
    speed_tol_wpm: Option<f64>,
    /// A speed disagreement also needs more than this percent of RBN's
    /// median speed (default 20).
    #[arg(long, value_name = "PCT")]
    speed_tol_pct: Option<f64>,
    /// Take the [rbn_compare] tolerances from this config file, so a node's
    /// own config replays with its live settings. Flags win. MANTA_*
    /// variables are never read.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the grade records, one JSON object per line, sorted by
    /// windowClose then spotId, instead of the summary.
    #[arg(long)]
    json: bool,
}

/// `--spotter`/`--exclude-spotter`: the same callsign rule as
/// `[rbn_compare].spotters` and `run --rbn-spotter`.
fn parse_spotter(s: &str) -> std::result::Result<String, String> {
    manta_server::config::normalize_rbn_spotter(s)
}

pub(crate) fn run(args: Args) -> Result<()> {
    let mut out = String::new();
    render(&args, &mut out)?;
    print!("{out}");
    Ok(())
}

/// What `load_node_spots` read, for the summary header.
#[derive(Debug, Default)]
struct SpotStats {
    files: usize,
    grade_records: usize,
    torn: usize,
    /// Every `deCall` seen, uppercase, in first-seen order.
    de_calls: Vec<String>,
}

/// What `load_rbn_csv` read, for the summary header.
#[derive(Debug, Default)]
struct RbnStats {
    files: usize,
    non_cw: usize,
}

fn render(args: &Args, out: &mut String) -> Result<()> {
    // `.zip` first, so the refusal names the archive before anything is read.
    for p in &args.rbn {
        if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            bail!(
                "manta grade reads RBN's CSV; unzip {} first, or use \
                 scripts/score-against-rbn.py --node-log, which unzips",
                p.display()
            );
        }
    }
    let (mut cfg, mut excludes) = grade_config(args)?;
    let (spots, spot_stats) = load_node_spots(&args.spots)?;
    let (reports, rbn_stats) = load_rbn_csv(&args.rbn)?;
    // shadow-compare's rule: the node's own reports never count, with and
    // without its SSID.
    for de in &spot_stats.de_calls {
        push_unique(&mut excludes, normalize_spotter(de));
        push_unique(
            &mut excludes,
            normalize_spotter(manta_server::config::strip_ssid(de)),
        );
    }
    cfg.exclude_spotters = excludes.iter().cloned().collect();
    let time_range = reports
        .iter()
        .map(|r| r.time)
        .min()
        .zip(reports.iter().map(|r| r.time).max());
    let n_reports = reports.len();

    let mut grades = grade_batch(&spots, reports, &cfg);
    grades.sort_by(|a, b| (a.window_close, &a.spot_id).cmp(&(b.window_close, &b.spot_id)));

    if args.json {
        for g in &grades {
            writeln!(out, "{}", serde_json::to_string(g)?)?;
        }
        return Ok(());
    }

    writeln!(
        out,
        "spots: {} from {} ({}, {})",
        spots.len(),
        plural(spot_stats.files, "file", "files"),
        plural(
            spot_stats.grade_records,
            "grade record skipped",
            "grade records skipped"
        ),
        plural(spot_stats.torn, "torn line", "torn lines"),
    )?;
    let mut rbn_line = format!(
        "rbn: {} from {} ({})",
        plural(n_reports, "CW report", "CW reports"),
        plural(rbn_stats.files, "file", "files"),
        plural(
            rbn_stats.non_cw,
            "non-CW row skipped",
            "non-CW rows skipped"
        ),
    );
    if let Some((lo, hi)) = time_range {
        let (lo_date, lo_time) = utc_date_time(lo);
        let (hi_date, hi_time) = utc_date_time(hi);
        if lo_date == hi_date {
            write!(rbn_line, ", {lo_date} {lo_time}Z to {hi_time}Z")?;
        } else {
            write!(rbn_line, ", {lo_date} {lo_time}Z to {hi_date} {hi_time}Z")?;
        }
    }
    writeln!(out, "{rbn_line}")?;
    let spotters = if cfg.spotters.is_empty() {
        "all".to_string()
    } else {
        cfg.spotters.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    if excludes.is_empty() {
        writeln!(out, "spotters: {spotters}")?;
    } else {
        writeln!(
            out,
            "spotters: {spotters} (excluding {})",
            excludes.join(", ")
        )?;
    }
    writeln!(
        out,
        "window: {} s; frequency disagreement over {} Hz; speed disagreement over {} WPM \
         and {}% of RBN's median",
        cfg.window_secs, cfg.freq_tol_hz, cfg.speed_tol_wpm, cfg.speed_tol_pct
    )?;
    writeln!(out)?;
    render_table(&grades, out)?;
    Ok(())
}

/// One band's row of the summary table.
#[derive(Default)]
struct BandRow {
    spots: usize,
    by_grade: [usize; 5],
    offsets: Vec<i64>,
}

fn grade_column(g: Grade) -> usize {
    match g {
        Grade::Match => 0,
        Grade::Disagree => 1,
        Grade::Elsewhere => 2,
        Grade::None => 3,
        Grade::Unknown => 4,
    }
}

/// Rows run from the highest band down (10m first), `unknown` last.
fn band_rank(band: &str) -> (usize, usize) {
    let table = manta_server::band::allocations();
    match table.iter().position(|(name, _, _)| *name == band) {
        Some(i) => (0, table.len() - i),
        None => (1, 0),
    }
}

fn render_table(grades: &[SpotGrade], out: &mut String) -> Result<()> {
    let mut rows: BTreeMap<((usize, usize), &str), BandRow> = BTreeMap::new();
    for g in grades {
        let row = rows.entry((band_rank(g.band), g.band)).or_default();
        row.spots += 1;
        row.by_grade[grade_column(g.grade)] += 1;
        if matches!(g.grade, Grade::Match | Grade::Disagree) {
            if let Some(off) = g.freq_offset_hz {
                row.offsets.push(off);
            }
        }
    }
    writeln!(
        out,
        "{:<5}{:>7}{:>7}{:>10}{:>11}{:>6}{:>9}  median offset",
        "band", "spots", "match", "disagree", "elsewhere", "none", "unknown"
    )?;
    let mut total = BandRow::default();
    for ((_, band), row) in &rows {
        writeln!(
            out,
            "{}  {}",
            table_cells(band, row),
            median_offset(&row.offsets)
        )?;
        total.spots += row.spots;
        for (t, n) in total.by_grade.iter_mut().zip(row.by_grade) {
            *t += n;
        }
    }
    writeln!(out, "{}", table_cells("total", &total))?;
    Ok(())
}

fn table_cells(label: &str, row: &BandRow) -> String {
    let [m, d, e, n, u] = row.by_grade;
    format!("{label:<5}{:>7}{m:>7}{d:>10}{e:>11}{n:>6}{u:>9}", row.spots)
}

/// `+750 Hz (2)`: the median (an even count averages the middle two) and
/// how many grades it is over; `n/a` when the band has none.
fn median_offset(offsets: &[i64]) -> String {
    if offsets.is_empty() {
        return "n/a".to_string();
    }
    let mut v = offsets.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    let twice = if v.len() % 2 == 1 {
        2 * v[mid]
    } else {
        v[mid - 1] + v[mid]
    };
    let median = if twice % 2 == 0 {
        format!("{:+}", twice / 2)
    } else {
        format!("{:+.1}", twice as f64 / 2.0)
    };
    format!("{median} Hz ({})", v.len())
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !s.is_empty() && !v.contains(&s) {
        v.push(s);
    }
}

/// The `--config` file's `[rbn_compare]` tolerances (env ignored), with the
/// flags on top. Returns the config plus the exclusion list in display order.
fn grade_config(args: &Args) -> Result<(GradeConfig, Vec<String>)> {
    let mut cfg = GradeConfig::default();
    let mut spotters: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    if let Some(path) = &args.config {
        let loaded = config::load(Some(path), config::Env::Ignore)?;
        if let Some(rc) = &loaded.rbn_compare {
            cfg.window_secs = rc.window_secs;
            cfg.freq_tol_hz = rc.freq_tol_hz;
            cfg.speed_tol_wpm = rc.speed_tol_wpm;
            cfg.speed_tol_pct = rc.speed_tol_pct;
            spotters = rc.spotters.clone();
            excludes = rc.exclude_spotters.clone();
        }
    }
    if let Some(w) = args.window_secs {
        if !(30..=3600).contains(&w) {
            bail!("--window-secs must be 30 to 3600, got {w}");
        }
        cfg.window_secs = w;
    }
    for (flag, value, slot) in [
        ("--freq-tol-hz", args.freq_tol_hz, &mut cfg.freq_tol_hz),
        (
            "--speed-tol-wpm",
            args.speed_tol_wpm,
            &mut cfg.speed_tol_wpm,
        ),
        (
            "--speed-tol-pct",
            args.speed_tol_pct,
            &mut cfg.speed_tol_pct,
        ),
    ] {
        if let Some(v) = value {
            if !v.is_finite() || v < 0.0 {
                bail!("{flag} must be a finite number >= 0, got {v}");
            }
            *slot = v;
        }
    }
    if !args.spotter.is_empty() {
        spotters = args.spotter.clone();
    }
    if !args.exclude_spotter.is_empty() {
        excludes = args.exclude_spotter.clone();
    }
    cfg.spotters = spotters
        .iter()
        .map(|s| normalize_spotter(s))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::new();
    for e in &excludes {
        push_unique(&mut ordered, normalize_spotter(e));
    }
    Ok((cfg, ordered))
}

/// Files and directories to a file list: a directory contributes its files
/// that `keep` accepts, sorted by name; a file is taken as given.
fn expand_paths(
    paths: &[PathBuf],
    what: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("cannot read directory {}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|f| f.is_file())
                .filter(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(&keep))
                .collect();
            if found.is_empty() {
                bail!("{} has no {what} files", p.display());
            }
            found.sort();
            files.extend(found);
        } else {
            files.push(p.clone());
        }
    }
    Ok(files)
}

/// SpotMessage JSON Lines to grade inputs. Lines carrying a `kind` key
/// (`spotGrade` records) are skipped and counted. Only a file's final line
/// with no newline (a write torn by a kill) may be malformed; any other
/// malformed line is an error naming the file and line.
fn load_node_spots(paths: &[PathBuf]) -> Result<(Vec<GradeInput>, SpotStats)> {
    let files = expand_paths(paths, "spots-*.jsonl", |n| {
        n.starts_with("spots-") && n.ends_with(".jsonl")
    })?;
    let mut stats = SpotStats {
        files: files.len(),
        ..SpotStats::default()
    };
    let mut spots = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let ends_with_newline = body.ends_with('\n');
        let lines: Vec<&str> = body.split('\n').collect();
        // `split` leaves an empty final piece after a trailing newline.
        let last_nonempty = lines.iter().rposition(|l| !l.trim().is_empty());
        for (i, raw) in lines.iter().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let torn_ok = Some(i) == last_nonempty && !ends_with_newline;
            let malformed = |why: String| -> anyhow::Error {
                anyhow::anyhow!(
                    "{} line {}: malformed node spot record ({why})",
                    path.display(),
                    i + 1
                )
            };
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) if torn_ok => {
                    stats.torn += 1;
                    continue;
                }
                Err(e) => return Err(malformed(e.to_string())),
            };
            if value.get("kind").is_some() {
                stats.grade_records += 1;
                continue;
            }
            let spot = match parse_spot_message_line(line) {
                Ok(s) => s,
                Err(_) if torn_ok => {
                    stats.torn += 1;
                    continue;
                }
                Err(e) => return Err(malformed(e)),
            };
            if let Some(de) = value.get("deCall").and_then(|d| d.as_str()) {
                let de = de.trim().to_ascii_uppercase();
                if !de.is_empty() && !stats.de_calls.contains(&de) {
                    stats.de_calls.push(de);
                }
            }
            spots.push(spot);
        }
    }
    Ok((spots, stats))
}

/// The RBN CSV columns `manta grade` cannot work without
/// (`shadow-compare.py`'s `RBN_REQUIRED_COLUMNS`).
const RBN_REQUIRED_COLUMNS: [&str; 4] = ["callsign", "freq", "dx", "date"];

/// RBN daily-dump CSVs to CW reports: rows whose `tx_mode` (when the column
/// exists) is not `CW` are dropped and counted; `freq` is in kHz; `date` is
/// UTC; spotters lose a `-#` suffix.
fn load_rbn_csv(paths: &[PathBuf]) -> Result<(Vec<RbnReport>, RbnStats)> {
    let files = expand_paths(paths, "*.csv", |n| {
        Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("csv"))
    })?;
    let mut stats = RbnStats {
        files: files.len(),
        ..RbnStats::default()
    };
    let mut reports = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut lines = body.lines().enumerate();
        let header = loop {
            match lines.next() {
                Some((_, l)) if l.trim().is_empty() => continue,
                Some((_, l)) => break split_csv(l.trim_start_matches('\u{feff}')),
                None => bail!("{}: empty RBN CSV (no header row)", path.display()),
            }
        };
        let col = |name: &str| header.iter().position(|h| h == name);
        let missing: Vec<&str> = RBN_REQUIRED_COLUMNS
            .iter()
            .copied()
            .filter(|c| col(c).is_none())
            .collect();
        if !missing.is_empty() {
            bail!(
                "{}: RBN CSV lacks column(s) {}",
                path.display(),
                missing.join(", ")
            );
        }
        let (c_spotter, c_freq, c_dx, c_date) = (
            col("callsign").unwrap_or_default(),
            col("freq").unwrap_or_default(),
            col("dx").unwrap_or_default(),
            col("date").unwrap_or_default(),
        );
        let (c_tx_mode, c_speed) = (col("tx_mode"), col("speed"));
        for (i, line) in lines {
            if line.trim().is_empty() {
                continue;
            }
            let row = split_csv(line);
            let field = |c: usize| row.get(c).map(String::as_str).unwrap_or("");
            let at = || format!("{} line {}", path.display(), i + 1);
            if let Some(c) = c_tx_mode {
                if !field(c).eq_ignore_ascii_case("CW") {
                    stats.non_cw += 1;
                    continue;
                }
            }
            let freq_khz: f64 = field(c_freq)
                .parse()
                .ok()
                .filter(|f: &f64| f.is_finite())
                .with_context(|| {
                    format!("{}: bad freq {}", at(), crate::fmt::quoted(field(c_freq)))
                })?;
            let time =
                manta_testkit::oracle::parse_utc_timestamp(field(c_date)).with_context(|| {
                    format!("{}: bad date {}", at(), crate::fmt::quoted(field(c_date)))
                })?;
            let wpm = c_speed
                .and_then(|c| field(c).parse::<f64>().ok())
                .filter(|w| w.is_finite() && *w > 0.0);
            reports.push(RbnReport {
                spotter: normalize_spotter(field(c_spotter)),
                call: field(c_dx).to_ascii_uppercase(),
                freq_hz: freq_khz * 1000.0,
                wpm,
                time,
            });
        }
    }
    Ok((reports, stats))
}

/// One CSV line's fields, trimmed, with surrounding double quotes removed
/// (RBN's dumps quote nothing, but a re-saved file may).
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.trim_end_matches('\r').chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    fields.push(cur.trim().to_string());
    fields
}

/// Unix seconds to (`YYYY-MM-DD`, `HH:MM:SS`), UTC, proleptic Gregorian
/// (Howard Hinnant's civil-from-days).
fn utc_date_time(t: i64) -> (String, String) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_date_time_matches_known_instants() {
        assert_eq!(
            utc_date_time(1_791_720_000),
            ("2026-10-11".to_string(), "12:00:00".to_string())
        );
        assert_eq!(
            utc_date_time(0),
            ("1970-01-01".to_string(), "00:00:00".to_string())
        );
    }

    #[test]
    fn median_offset_formats_sign_count_and_halves() {
        assert_eq!(median_offset(&[100, 1400]), "+750 Hz (2)");
        assert_eq!(median_offset(&[-20]), "-20 Hz (1)");
        assert_eq!(median_offset(&[100, 101]), "+100.5 Hz (2)");
        assert_eq!(median_offset(&[]), "n/a");
    }

    #[test]
    fn split_csv_handles_quotes_and_crlf() {
        assert_eq!(split_csv("a, \"b,c\" ,d\r"), vec!["a", "b,c", "d"]);
    }
}
