//! MAN-301: `manta grade` -- grade a node's spot log against RBN's daily
//! archive, offline, through the same engine the live comparator uses
//! (`manta_server::rbn_compare::grade`). Its output depends only on its
//! inputs and flags: like `decode`/`oracle` it never reads `MANTA_*`.
//! MAN-97 adds a skimmer's `DX de` transcript as the reference
//! (`--reference`, [`crate::grade_transcript`]) and the two-way report
//! (`--paired`, [`crate::grade_paired`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use manta_server::config::{
    RBN_COMPARE_MAX_FREQ_TOL_HZ, RBN_COMPARE_MAX_SPEED_TOL_PCT, RBN_COMPARE_MAX_SPEED_TOL_WPM,
};
use manta_server::rbn_compare::grade::{
    grade_batch, normalize_spotter, parse_spot_message_line, Grade, GradeConfig, GradeInput,
    RbnReport, SpotGrade,
};

use crate::config;
use crate::grade_paired::{self, PairedInputs, PAIRED_WINDOW_SECS};
use crate::grade_transcript::{day_from_file_name, parse_transcript, ReferenceReport};

#[derive(clap::Args, Debug)]
#[command(group(
    clap::ArgGroup::new("reference_source")
        .required(true)
        .args(["rbn", "reference"])
))]
pub(crate) struct Args {
    /// SpotMessage JSON Lines files, or directories of them (their
    /// `spots-*.jsonl` files, sorted): the `field-node.py record-spots`
    /// archive. Repeatable. A file named twice, directly, through its
    /// directory or through a symlink, is read once.
    #[arg(long, value_name = "PATH", required = true, num_args = 1..)]
    spots: Vec<PathBuf>,
    /// RBN daily-dump CSVs, or directories of them (their `*.csv` files,
    /// sorted). Unzip RBN's `.zip` first. Repeatable. A file named twice,
    /// directly, through its directory or through a symlink, is read once.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    rbn: Vec<PathBuf>,
    /// A skimmer's `DX de` telnet output saved to files (for example CW
    /// Skimmer Server's port 7310), or directories of them (their `*.txt`
    /// and `*.log` files, sorted). Repeatable; instead of --rbn. A line may
    /// start with its UTC arrival time (2026-10-31T00:05:00Z); a line
    /// without one takes its day from the file name
    /// (reference-2026-10-31.txt) and its minute from the line's HHMMZ. The
    /// node's own calls are not excluded: the transcript holds only that
    /// skimmer's spots.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    reference: Vec<PathBuf>,
    /// Count only this RBN skimmer's reports, for example a co-located
    /// skimmer; with --reference, only this `DX de` spotter's lines.
    /// Repeatable. Replaces [rbn_compare].spotters from --config.
    #[arg(long, value_name = "CALL", value_parser = parse_spotter)]
    spotter: Vec<String>,
    /// Never count this RBN skimmer's reports. Repeatable. Replaces
    /// [rbn_compare].exclude_spotters from --config. Each spot's own
    /// `deCall`, with and without its SSID, is always excluded from that
    /// spot's grade.
    #[arg(long, value_name = "CALL", value_parser = parse_spotter)]
    exclude_spotter: Vec<String>,
    /// RBN reports this many seconds either side of a spot count (30 to
    /// 3600; default 180).
    #[arg(long, value_name = "SECS")]
    window_secs: Option<i64>,
    /// Further than this many Hz from RBN's modal frequency is a frequency
    /// disagreement (0 to 10000; default 500).
    #[arg(long, value_name = "HZ")]
    freq_tol_hz: Option<f64>,
    /// A speed disagreement needs more than this many WPM (0 to 20;
    /// default 3).
    #[arg(long, value_name = "WPM")]
    speed_tol_wpm: Option<f64>,
    /// A speed disagreement also needs more than this percent of RBN's
    /// median speed (0 to 100; default 20).
    #[arg(long, value_name = "PCT")]
    speed_tol_pct: Option<f64>,
    /// Take the [rbn_compare] tolerances from this config file, so a node's
    /// own config replays with its live settings. Flags win. MANTA_*
    /// variables are never read.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the grade records, one JSON object per line, sorted by
    /// windowClose then spotId, instead of the summary; with --paired, the
    /// report as one JSON object.
    #[arg(long)]
    json: bool,
    /// Grade both ways and print one report: recall (reference reports
    /// manta also spotted), the reports manta missed, and manta's spots the
    /// reference did not report (candidate false spots). Needs
    /// --passband-khz; with --rbn it also needs --spotter. The window
    /// defaults to 600 s (manta's re-spot interval); --window-secs changes
    /// it, and --config's window_secs is not used.
    #[arg(long, requires = "passband_khz")]
    paired: bool,
    /// With --paired: count only spots and reports inside this range, in
    /// kHz (for example 14000-14070). Repeatable. Spots and reports outside
    /// still match.
    #[arg(
        long,
        value_name = "LO-HI",
        requires = "paired",
        value_parser = grade_paired::parse_passband
    )]
    passband_khz: Vec<(f64, f64)>,
    /// With --paired: count only spots and reports at or after this time
    /// (2026-10-31T00:00:00Z). Spots and reports before it still match.
    #[arg(long, value_name = "UTC", requires = "paired", value_parser = parse_utc)]
    start: Option<i64>,
    /// With --paired: count only spots and reports before this time.
    #[arg(long, value_name = "UTC", requires = "paired", value_parser = parse_utc)]
    end: Option<i64>,
}

/// `--start`/`--end`: a whole-second UTC time, `T` or space separated.
fn parse_utc(s: &str) -> std::result::Result<i64, String> {
    manta_testkit::oracle::parse_utc_timestamp(s).map_err(|e| format!("{e:#}"))
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

/// What `load_rbn_csv` or `load_reference` read, for the summary header.
#[derive(Debug, Default)]
struct RbnStats {
    files: usize,
    non_cw: usize,
    /// Transcript lines that are neither CW nor another mode's spot.
    other: usize,
}

/// The SpotMessage fields the paired report lists beside a spot, which
/// grading itself never reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SpotExtra {
    pub snr: Option<i64>,
    pub decode_confidence: Option<f64>,
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
    // A transcript holds only its own skimmer's spots, so the node's own
    // calls are neither excluded nor refused as --spotter (MAN-97 D6).
    let transcript = !args.reference.is_empty();
    let (mut cfg, excludes) = grade_config(args)?;
    // The exclusion wins, so a spotter on both lists would grade every spot
    // it alone covers `none`: refuse it, as `[rbn_compare]` does.
    if let Some(call) = cfg.spotters.iter().find(|s| excludes.contains(s)) {
        bail!(
            "spotter {call} is both counted (--spotter or [rbn_compare].spotters) and \
             excluded (--exclude-spotter or [rbn_compare].exclude_spotters): its reports \
             would never count"
        );
    }
    if args.paired {
        if !transcript && cfg.spotters.is_empty() {
            bail!(
                "--paired with --rbn needs --spotter (the co-located skimmer): against every \
                 RBN skimmer, each call heard anywhere would count as a miss"
            );
        }
        if let (Some(start), Some(end)) = (args.start, args.end) {
            if end <= start {
                bail!(
                    "--end ({}) must be after --start ({})",
                    date_time(end),
                    date_time(start)
                );
            }
        }
    }
    let (spots, spot_stats) = load_node_spots(&args.spots)?;
    // Likewise a node's own call, which its spots always exclude.
    if !transcript {
        for de in &spot_stats.de_calls {
            if let Some(call) = own_calls(de).into_iter().find(|c| cfg.spotters.contains(c)) {
                bail!(
                    "spotter {call} is node {de}'s own call: its reports never count for that \
                     node's spots, so every one would grade none"
                );
            }
        }
    }
    if args.paired {
        let mut nodes: Vec<&str> = Vec::new();
        for (de, _, _) in &spots {
            let de = if de.is_empty() { "no deCall" } else { de };
            if !nodes.contains(&de) {
                nodes.push(de);
            }
        }
        if nodes.len() > 1 {
            bail!(
                "--paired compares one node with its reference; the spots carry {} nodes \
                 ({}): grade each node's archive apart",
                nodes.len(),
                nodes.join(", ")
            );
        }
    }
    let (archives, rbn_stats) = if transcript {
        load_reference(&args.reference)?
    } else {
        load_rbn_csv(&args.rbn)?
    };
    cfg.exclude_spotters = excludes.iter().cloned().collect();
    let loaded = Loaded {
        transcript,
        cfg,
        excludes,
        spots,
        spot_stats,
        archives,
        rbn_stats,
    };
    if args.paired {
        render_paired(args, &loaded, out)
    } else {
        render_one_way(args, &loaded, out)
    }
}

/// What `render` read and resolved, for its two output paths.
struct Loaded {
    /// `--reference` (a transcript) rather than `--rbn`.
    transcript: bool,
    cfg: GradeConfig,
    /// The exclusion list in display order.
    excludes: Vec<String>,
    spots: Vec<(String, GradeInput, SpotExtra)>,
    spot_stats: SpotStats,
    archives: Vec<Vec<ReferenceReport>>,
    rbn_stats: RbnStats,
}

/// `--paired`: one node against its reference, both ways.
fn render_paired(args: &Args, l: &Loaded, out: &mut String) -> Result<()> {
    let node = l.spots.first().map(|(de, _, _)| de.as_str()).unwrap_or("");
    let mut node_cfg = l.cfg.clone();
    if !l.transcript {
        node_cfg.exclude_spotters.extend(own_calls(node));
    }
    let node_spots: Vec<(GradeInput, SpotExtra)> = l
        .spots
        .iter()
        .map(|(_, s, e)| (s.clone(), e.clone()))
        .collect();
    let report = grade_paired::build(PairedInputs {
        node,
        reference_source: if l.transcript { "transcript" } else { "rbn" },
        cfg: &node_cfg,
        spots: &node_spots,
        archives: &l.archives,
        passbands: &args.passband_khz,
        start: args.start,
        end: args.end,
    });
    if args.json {
        out.push_str(&grade_paired::render_json(&report)?);
        return Ok(());
    }
    render_header(l, out)?;
    grade_paired::render_text(&report, out)
}

/// The one-way grade: the grade records with `--json`, else the header and
/// the per-band table.
fn render_one_way(args: &Args, l: &Loaded, out: &mut String) -> Result<()> {
    let grades = grade_one_way(&l.cfg, l.transcript, &l.spots, &l.archives);
    if args.json {
        for g in &grades {
            writeln!(out, "{}", serde_json::to_string(g)?)?;
        }
        return Ok(());
    }
    render_header(l, out)?;
    writeln!(out)?;
    render_table(&grades, out)?;
    Ok(())
}

/// Every spot graded against every reference: one grade per spot, sorted
/// by `(window_close, spot_id)`.
fn grade_one_way(
    cfg: &GradeConfig,
    transcript: bool,
    spots: &[(String, GradeInput, SpotExtra)],
    archives: &[Vec<ReferenceReport>],
) -> Vec<SpotGrade> {
    let archives: Vec<Vec<RbnReport>> = archives
        .iter()
        .map(|a| a.iter().map(|r| r.report.clone()).collect())
        .collect();
    // shadow-compare's rule: a node's own reports never count, with and
    // without its SSID. Each node's spots are graded apart, excluding only
    // that node's call, as its live comparator does, so a second node's
    // archive in `--spots` never hides that node's reports from the first.
    // A transcript holds no other skimmer's reports, so it excludes none.
    let mut nodes: BTreeMap<&str, Vec<GradeInput>> = BTreeMap::new();
    for (de, spot, _) in spots {
        nodes.entry(de.as_str()).or_default().push(spot.clone());
    }
    let mut grades = Vec::new();
    let mut archives = Some(archives);
    let mut nodes = nodes.into_iter().peekable();
    while let Some((de, node_spots)) = nodes.next() {
        let mut node_cfg = cfg.clone();
        if !transcript {
            node_cfg.exclude_spotters.extend(own_calls(de));
        }
        // The last node takes the reports; earlier ones grade a copy.
        let node_reports = if nodes.peek().is_some() {
            archives.clone().unwrap_or_default()
        } else {
            archives.take().unwrap_or_default()
        };
        grades.extend(grade_batch(&node_spots, node_reports, &node_cfg));
    }
    grades.sort_by(|a, b| (a.window_close, &a.spot_id).cmp(&(b.window_close, &b.spot_id)));
    grades
}

/// The summary's header lines, shared by the one-way table and the paired
/// report: what was read, which spotters count, and the tolerances.
fn render_header(l: &Loaded, out: &mut String) -> Result<()> {
    let Loaded {
        transcript,
        cfg,
        excludes,
        spots,
        spot_stats,
        archives,
        rbn_stats,
    } = l;
    let transcript = *transcript;
    let times = || archives.iter().flatten().map(|r| r.report.time);
    let time_range = times().min().zip(times().max());
    let n_reports = archives.iter().map(Vec::len).sum::<usize>();
    let n_nodes = spots
        .iter()
        .map(|(de, _, _)| de.as_str())
        .collect::<BTreeSet<_>>()
        .len();
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
    let mut rbn_line = if transcript {
        format!(
            "reference: {} from {} ({}, {})",
            plural(n_reports, "CW report", "CW reports"),
            plural(rbn_stats.files, "file", "files"),
            plural(rbn_stats.non_cw, "non-CW line", "non-CW lines"),
            plural(rbn_stats.other, "other line skipped", "other lines skipped"),
        )
    } else {
        format!(
            "rbn: {} from {} ({})",
            plural(n_reports, "CW report", "CW reports"),
            plural(rbn_stats.files, "file", "files"),
            plural(
                rbn_stats.non_cw,
                "non-CW row skipped",
                "non-CW rows skipped"
            ),
        )
    };
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
    // One node's own calls join the exclusion list. Several nodes' (spots
    // with no `deCall` count as a node) are listed apart: each is excluded
    // from its own node's spots only.
    let mut shown = excludes.clone();
    let mut own = Vec::new();
    if !transcript {
        for de in &spot_stats.de_calls {
            for c in own_calls(de) {
                push_unique(&mut own, c);
            }
        }
    }
    if n_nodes <= 1 {
        for c in own.drain(..) {
            push_unique(&mut shown, c);
        }
    }
    if shown.is_empty() {
        writeln!(out, "spotters: {spotters}")?;
    } else {
        writeln!(out, "spotters: {spotters} (excluding {})", shown.join(", "))?;
    }
    if transcript {
        writeln!(
            out,
            "own calls are not excluded from a --reference transcript"
        )?;
    } else if !own.is_empty() {
        writeln!(
            out,
            "own calls, excluded only from that node's spots: {}",
            own.join(", ")
        )?;
    }
    writeln!(
        out,
        "window: {} s; frequency disagreement over {} Hz; speed disagreement over {} WPM \
         and {}% of RBN's median",
        cfg.window_secs, cfg.freq_tol_hz, cfg.speed_tol_wpm, cfg.speed_tol_pct
    )?;
    Ok(())
}

/// `YYYY-MM-DD HH:MM:SSZ`.
fn date_time(t: i64) -> String {
    let (d, h) = utc_date_time(t);
    format!("{d} {h}Z")
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
pub(crate) fn band_rank(band: &str) -> (usize, usize) {
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

pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A node's `deCall` as the spotters its own spots exclude: as given and
/// without its SSID. None for a spot with no `deCall`.
fn own_calls(de: &str) -> Vec<String> {
    if de.is_empty() {
        return Vec::new();
    }
    vec![
        normalize_spotter(de),
        normalize_spotter(manta_server::config::strip_ssid(de)),
    ]
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
    // MAN-97 D3: the paired window is manta's re-spot interval, not the
    // live comparator's (`manta_spot::dedupe`'s `SUPPRESSION_SECONDS`,
    // MAN-96 D-H); --config's other tolerances still apply.
    if args.paired && args.window_secs.is_none() {
        cfg.window_secs = PAIRED_WINDOW_SECS;
    }
    if let Some(w) = args.window_secs {
        if !(30..=3600).contains(&w) {
            bail!("--window-secs must be 30 to 3600, got {w}");
        }
        cfg.window_secs = w;
    }
    // The same maximums `RbnCompareConfig::validate` puts on the keys.
    for (flag, value, max, slot) in [
        (
            "--freq-tol-hz",
            args.freq_tol_hz,
            RBN_COMPARE_MAX_FREQ_TOL_HZ,
            &mut cfg.freq_tol_hz,
        ),
        (
            "--speed-tol-wpm",
            args.speed_tol_wpm,
            RBN_COMPARE_MAX_SPEED_TOL_WPM,
            &mut cfg.speed_tol_wpm,
        ),
        (
            "--speed-tol-pct",
            args.speed_tol_pct,
            RBN_COMPARE_MAX_SPEED_TOL_PCT,
            &mut cfg.speed_tol_pct,
        ),
    ] {
        if let Some(v) = value {
            if !v.is_finite() || !(0.0..=max).contains(&v) {
                bail!("{flag} must be a number from 0 to {max}, got {v}");
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
/// that `keep` accepts, sorted by name; a file is taken as given. A file
/// reached twice (directly and through its directory, with `./` or `..`,
/// or through a symlink) is listed once, as first spelled, keyed by its
/// canonical path (MAN-343); a path with no canonical path, such as a
/// missing file, keys as given, so the loader's error names it as typed.
fn expand_paths(
    paths: &[PathBuf],
    what: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for p in paths {
        let found = if p.is_dir() {
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
            found
        } else {
            vec![p.clone()]
        };
        for f in found {
            let key = std::fs::canonicalize(&f).unwrap_or_else(|_| f.clone());
            if seen.insert(key) {
                files.push(f);
            }
        }
    }
    Ok(files)
}

/// SpotMessage JSON Lines to grade inputs, each with its `deCall`
/// (uppercase; empty when absent) and its `snr`/`decodeConfidence` (only
/// the paired report lists them). Lines carrying a `kind` key
/// (`spotGrade` records) are skipped and counted. Only a file's final line
/// with no newline (a write torn by a kill) may be malformed; any other
/// malformed line is an error naming the file and line.
#[allow(clippy::type_complexity)]
fn load_node_spots(paths: &[PathBuf]) -> Result<(Vec<(String, GradeInput, SpotExtra)>, SpotStats)> {
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
            if let Some(kind) = value.get("kind") {
                if kind == "spotGrade" {
                    stats.grade_records += 1;
                }
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
            let de = value
                .get("deCall")
                .and_then(|d| d.as_str())
                .map(|d| d.trim().to_ascii_uppercase())
                .unwrap_or_default();
            if !de.is_empty() && !stats.de_calls.contains(&de) {
                stats.de_calls.push(de.clone());
            }
            let extra = SpotExtra {
                snr: value.get("snr").and_then(serde_json::Value::as_i64),
                decode_confidence: value
                    .get("decodeConfidence")
                    .and_then(serde_json::Value::as_f64)
                    .filter(|c| c.is_finite()),
            };
            spots.push((de, spot, extra));
        }
    }
    Ok((spots, stats))
}

/// The RBN CSV columns `manta grade` cannot work without
/// (`shadow-compare.py`'s `RBN_REQUIRED_COLUMNS` less `tx_mode`, which only
/// that script requires: MAN-290).
const RBN_REQUIRED_COLUMNS: [&str; 4] = ["callsign", "freq", "dx", "date"];

/// RBN daily-dump CSVs to CW reports, one list per file (`grade_batch`
/// covers each file's own span): rows whose `tx_mode` (when the column
/// exists) is not `CW` are dropped and counted; `freq` is in kHz; `date` is
/// UTC; spotters lose a `-#` suffix; the optional `db` column is the
/// report's SNR.
fn load_rbn_csv(paths: &[PathBuf]) -> Result<(Vec<Vec<ReferenceReport>>, RbnStats)> {
    let files = expand_paths(paths, "*.csv", |n| {
        Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("csv"))
    })?;
    let mut stats = RbnStats {
        files: files.len(),
        ..RbnStats::default()
    };
    let mut archives = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut reports = Vec::new();
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
        let (c_tx_mode, c_speed, c_db) = (col("tx_mode"), col("speed"), col("db"));
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
            reports.push(ReferenceReport {
                report: RbnReport {
                    spotter: normalize_spotter(field(c_spotter)),
                    call: field(c_dx).to_ascii_uppercase(),
                    freq_hz: freq_khz * 1000.0,
                    wpm,
                    time,
                },
                snr_db: c_db.and_then(|c| field(c).parse::<i32>().ok()),
            });
        }
        archives.push(reports);
    }
    Ok((archives, stats))
}

/// A skimmer's saved `DX de` lines to CW reports, one list per file (each
/// file covers its own span, as each CSV does). A line without a UTC stamp
/// takes its day from the file name (`day_from_file_name`).
fn load_reference(paths: &[PathBuf]) -> Result<(Vec<Vec<ReferenceReport>>, RbnStats)> {
    let files = expand_paths(paths, "*.txt or *.log", |n| {
        Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt") || e.eq_ignore_ascii_case("log"))
    })?;
    let mut stats = RbnStats {
        files: files.len(),
        ..RbnStats::default()
    };
    let mut archives = Vec::new();
    for path in &files {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let day = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(day_from_file_name);
        let (reports, file_stats) =
            parse_transcript(&body, day).map_err(|e| anyhow::anyhow!("{} {e}", path.display()))?;
        stats.non_cw += file_stats.non_cw;
        stats.other += file_stats.other;
        archives.push(reports);
    }
    Ok((archives, stats))
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
pub(crate) fn utc_date_time(t: i64) -> (String, String) {
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

    #[test]
    fn expand_paths_keeps_each_file_once_at_its_first_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.csv"), dir.path().join("b.csv"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let args = [
            b.clone(),
            dir.path().to_path_buf(),
            dir.path().join(".").join("a.csv"),
        ];
        let files = expand_paths(&args, "*.csv", |n| n.ends_with(".csv")).unwrap();
        assert_eq!(files, vec![b, a]);
    }
}
