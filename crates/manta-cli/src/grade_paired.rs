//! MAN-97: `manta grade --paired` -- one node's spots against one co-located
//! skimmer, both ways: recall and misses from `grade_references`, candidate
//! false spots from `grade_batch`. Filters choose what is counted, never what
//! can match. Pure: formats into a String; `grade_cmd::run` prints.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result;
use manta_server::rbn_compare::grade::{
    grade_batch, grade_references, Grade, GradeConfig, GradeInput, RbnReport, Recall, SpotGrade,
};
use serde::Serialize;

use crate::grade_cmd::{band_rank, date_time, plural, utc_date_time, SpotExtra};
use crate::grade_transcript::ReferenceReport;

/// The `--paired` window when `--window-secs` is not given: manta's re-spot
/// suppression interval (`manta_spot::dedupe`'s `SUPPRESSION_SECONDS`), so
/// a station both decoders hear throughout always has a report within the
/// window of each manta spot (MAN-96 D-H). `--config`'s `window_secs` (the
/// live comparator's, sized for RBN's many skimmers) is not used.
pub(crate) const PAIRED_WINDOW_SECS: i64 = 600;

/// What the report reads, all of it match material; the filters below
/// choose only what is counted.
pub(crate) struct PairedInputs<'a> {
    /// The spots' `deCall` (empty when they carry none).
    pub node: &'a str,
    /// `"transcript"` (`--reference`) or `"rbn"` (`--rbn`).
    pub reference_source: &'static str,
    /// The node's grade config: spotter filter, exclusions, window and
    /// tolerances.
    pub cfg: &'a GradeConfig,
    /// Every loaded spot of the node.
    pub spots: &'a [(GradeInput, SpotExtra)],
    /// Every loaded reference report, one list per file.
    pub archives: &'a [Vec<ReferenceReport>],
    /// `--passband-khz` ranges, inclusive.
    pub passbands: &'a [(f64, f64)],
    /// `--start`, inclusive.
    pub start: Option<i64>,
    /// `--end`, exclusive.
    pub end: Option<i64>,
}

/// The paired report: one JSON object, or the text form. Field order is
/// the wire order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PairedReport {
    kind: &'static str,
    node: String,
    reference_source: &'static str,
    spotters: Vec<String>,
    start: Option<i64>,
    end: Option<i64>,
    passbands_khz: Vec<(f64, f64)>,
    window_s: i64,
    manta_spots: MantaCounts,
    reference_reports: ReferenceCounts,
    recall: Option<f64>,
    candidate_false_rate: Option<f64>,
    reference_span: Option<ReferenceSpan>,
    bands: Vec<BandCounts>,
    missed: Vec<MissedRow>,
    candidate_false: Vec<CandidateRow>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct MantaCounts {
    loaded: usize,
    out_of_passband: usize,
    out_of_window: usize,
    counted: usize,
    #[serde(rename = "match")]
    matched: usize,
    disagree: usize,
    elsewhere: usize,
    none: usize,
    unknown: usize,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceCounts {
    loaded: usize,
    other_spotter: usize,
    out_of_passband: usize,
    out_of_window: usize,
    implausible_call: usize,
    counted: usize,
    found: usize,
    missed: usize,
    missed_elsewhere: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceSpan {
    from: i64,
    to: i64,
    /// `None` with fewer than two admitted reports in the window.
    longest_silence_s: Option<i64>,
    longest_silence_from: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct BandCounts {
    band: &'static str,
    reference_reports: usize,
    found: usize,
    missed: usize,
    manta_spots: usize,
    #[serde(rename = "match")]
    matched: usize,
    disagree: usize,
    elsewhere: usize,
    none: usize,
    unknown: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MissedRow {
    time: i64,
    call: String,
    band: &'static str,
    freq_hz: i64,
    snr_db: Option<i32>,
    wpm: Option<f64>,
    spotter: String,
    manta_bands: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateRow {
    spot_id: String,
    time: i64,
    call: String,
    band: &'static str,
    freq_hz: i64,
    snr: Option<i64>,
    decode_confidence: Option<f64>,
    grade: Grade,
    reference_bands: Vec<&'static str>,
}

/// `--passband-khz LO-HI`: finite kHz with LO below HI.
pub(crate) fn parse_passband(s: &str) -> std::result::Result<(f64, f64), String> {
    let bad = || {
        format!(
            "passband needs finite LO < HI in kHz, got {}",
            crate::fmt::quoted(s)
        )
    };
    let (lo, hi) = s.split_once('-').ok_or_else(bad)?;
    let lo: f64 = lo.trim().parse().map_err(|_| bad())?;
    let hi: f64 = hi.trim().parse().map_err(|_| bad())?;
    if !(lo.is_finite() && hi.is_finite() && lo < hi) {
        return Err(bad());
    }
    Ok((lo, hi))
}

fn in_passband(passbands: &[(f64, f64)], freq_hz: f64) -> bool {
    let khz = freq_hz / 1000.0;
    passbands.iter().any(|&(lo, hi)| lo <= khz && khz <= hi)
}

fn in_window(start: Option<i64>, end: Option<i64>, t: i64) -> bool {
    start.is_none_or(|s| t >= s) && end.is_none_or(|e| t < e)
}

fn ratio(n: usize, d: usize) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

/// Grades both ways and counts what the filters admit.
pub(crate) fn build(inputs: PairedInputs<'_>) -> PairedReport {
    let PairedInputs {
        node,
        reference_source,
        cfg,
        spots,
        archives,
        passbands,
        start,
        end,
    } = inputs;
    let all_spots: Vec<GradeInput> = spots.iter().map(|(s, _)| s.clone()).collect();
    let extras: BTreeMap<&str, &SpotExtra> =
        spots.iter().map(|(s, e)| (s.spot_id.as_str(), e)).collect();

    // manta's side: passband, then window.
    let mut manta = MantaCounts {
        loaded: spots.len(),
        ..MantaCounts::default()
    };
    let mut counted_spots = Vec::new();
    for s in &all_spots {
        if !in_passband(passbands, s.freq_hz as f64) {
            manta.out_of_passband += 1;
        } else if !in_window(start, end, s.time) {
            manta.out_of_window += 1;
        } else {
            counted_spots.push(s.clone());
        }
    }
    manta.counted = counted_spots.len();

    // The reference's side: passband, window, spotter filter, grammar. The
    // reverse grade is `None` exactly where the spotter filter rejects.
    let reports: Vec<&ReferenceReport> = archives.iter().flatten().collect();
    let plain: Vec<RbnReport> = reports.iter().map(|r| r.report.clone()).collect();
    let backward = grade_references(&plain, &all_spots, cfg);

    // Each file's span comes from its admitted spotters' reports only, so
    // another skimmer in an RBN CSV or a mixed transcript cannot cover a
    // window for the reference. A rejected report could not match anyway.
    let mut admitted = backward.iter();
    let rbn_archives: Vec<Vec<RbnReport>> = archives
        .iter()
        .map(|a| {
            a.iter()
                .zip(admitted.by_ref())
                .filter(|(_, g)| g.is_some())
                .map(|(r, _)| r.report.clone())
                .collect()
        })
        .collect();
    let forward = grade_batch(&counted_spots, rbn_archives, cfg);

    let mut reference = ReferenceCounts {
        loaded: reports.len(),
        ..ReferenceCounts::default()
    };
    let mut bands: BTreeMap<((usize, usize), &'static str), BandCounts> = BTreeMap::new();
    fn band_row<'b>(
        bands: &'b mut BTreeMap<((usize, usize), &'static str), BandCounts>,
        band: &'static str,
    ) -> &'b mut BandCounts {
        bands.entry((band_rank(band), band)).or_insert(BandCounts {
            band,
            ..BandCounts::default()
        })
    }
    let mut missed = Vec::new();
    let mut admitted_times = Vec::new();
    for (r, g) in reports.iter().zip(&backward) {
        let rep = &r.report;
        if g.is_some() {
            admitted_times.push(rep.time);
        }
        if !in_passband(passbands, rep.freq_hz) {
            reference.out_of_passband += 1;
            continue;
        }
        if !in_window(start, end, rep.time) {
            reference.out_of_window += 1;
            continue;
        }
        let Some(g) = g else {
            reference.other_spotter += 1;
            continue;
        };
        if !manta_spot::grammar::is_plausible(rep.call.trim()) {
            reference.implausible_call += 1;
            continue;
        }
        reference.counted += 1;
        let row = band_row(&mut bands, g.band);
        row.reference_reports += 1;
        if g.recall == Recall::Found {
            reference.found += 1;
            row.found += 1;
            continue;
        }
        reference.missed += 1;
        row.missed += 1;
        if g.recall == Recall::Elsewhere {
            reference.missed_elsewhere += 1;
        }
        missed.push(MissedRow {
            time: rep.time,
            call: rep.call.trim().to_ascii_uppercase(),
            band: g.band,
            freq_hz: rep.freq_hz.round() as i64,
            snr_db: r.snr_db,
            wpm: rep.wpm,
            spotter: manta_server::rbn_compare::grade::normalize_spotter(&rep.spotter),
            manta_bands: g.manta_bands.clone(),
        });
    }
    missed.sort_by(|a, b| {
        (a.time, &a.call)
            .cmp(&(b.time, &b.call))
            .then(a.freq_hz.cmp(&b.freq_hz))
    });

    let mut candidate_false = Vec::new();
    for g in &forward {
        let row = band_row(&mut bands, g.band);
        row.manta_spots += 1;
        let (total, cell) = match g.grade {
            Grade::Match => (&mut manta.matched, &mut row.matched),
            Grade::Disagree => (&mut manta.disagree, &mut row.disagree),
            Grade::Elsewhere => (&mut manta.elsewhere, &mut row.elsewhere),
            Grade::None => (&mut manta.none, &mut row.none),
            Grade::Unknown => (&mut manta.unknown, &mut row.unknown),
        };
        *total += 1;
        *cell += 1;
        if matches!(g.grade, Grade::Elsewhere | Grade::None) {
            candidate_false.push(candidate_row(g, extras.get(g.spot_id.as_str()).copied()));
        }
    }
    candidate_false.sort_by(|a, b| (a.time, &a.spot_id).cmp(&(b.time, &b.spot_id)));

    let reference_span = reference_span(&admitted_times, start, end);
    let covered = manta.counted - manta.unknown;
    PairedReport {
        kind: "pairedReport",
        node: node.to_string(),
        reference_source,
        spotters: cfg.spotters.iter().cloned().collect(),
        start,
        end,
        passbands_khz: passbands.to_vec(),
        window_s: cfg.window_secs,
        recall: ratio(reference.found, reference.counted),
        candidate_false_rate: ratio(manta.elsewhere + manta.none, covered),
        manta_spots: manta,
        reference_reports: reference,
        reference_span,
        bands: bands.into_values().collect(),
        missed,
        candidate_false,
    }
}

fn candidate_row(g: &SpotGrade, extra: Option<&SpotExtra>) -> CandidateRow {
    CandidateRow {
        spot_id: g.spot_id.clone(),
        time: g.spot_time,
        call: g.call.trim().to_ascii_uppercase(),
        band: g.band,
        freq_hz: g.manta_freq_hz,
        snr: extra.and_then(|e| e.snr),
        decode_confidence: extra.and_then(|e| e.decode_confidence),
        grade: g.grade,
        reference_bands: g.rbn_bands.clone(),
    }
}

/// First to last admitted report, and the longest gap between consecutive
/// admitted reports inside the window: a hint that the reference was down,
/// never used to grade.
fn reference_span(times: &[i64], start: Option<i64>, end: Option<i64>) -> Option<ReferenceSpan> {
    let from = times.iter().copied().min()?;
    let to = times.iter().copied().max()?;
    let mut inside: Vec<i64> = times
        .iter()
        .copied()
        .filter(|&t| in_window(start, end, t))
        .collect();
    inside.sort_unstable();
    // The first of equal gaps wins, so the earliest silence is named.
    let longest = inside.windows(2).map(|w| (w[1] - w[0], w[0])).fold(
        None,
        |best: Option<(i64, i64)>, (gap, at)| match best {
            Some((g, _)) if g >= gap => best,
            _ => Some((gap, at)),
        },
    );
    Some(ReferenceSpan {
        from,
        to,
        longest_silence_s: longest.map(|(g, _)| g),
        longest_silence_from: longest.map(|(_, at)| at),
    })
}

/// `60.0%`, or `n/a` with nothing to divide by.
fn rate(n: usize, d: usize) -> String {
    match ratio(n, d) {
        Some(r) => format!("{:.1}%", 100.0 * r),
        None => "n/a".to_string(),
    }
}

/// `H:MM:SS`.
fn duration(secs: i64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn bands_or_dash(bands: &[&str]) -> String {
    if bands.is_empty() {
        "-".to_string()
    } else {
        bands.join(", ")
    }
}

fn or_dash<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}

/// The paired lines that follow `manta grade`'s shared header.
pub(crate) fn render_text(r: &PairedReport, out: &mut String) -> Result<()> {
    let span = match (r.start, r.end) {
        (Some(s), Some(e)) => {
            let ((sd, st), (ed, et)) = (utc_date_time(s), utc_date_time(e));
            if sd == ed {
                format!("{sd} {st}Z to {et}Z")
            } else {
                format!("{sd} {st}Z to {ed} {et}Z")
            }
        }
        (Some(s), None) => format!("from {}", date_time(s)),
        (None, Some(e)) => format!("before {}", date_time(e)),
        (None, None) => "all times".to_string(),
    };
    let passbands: Vec<String> = r
        .passbands_khz
        .iter()
        .map(|(lo, hi)| format!("{lo}-{hi}"))
        .collect();
    let node = if r.node.is_empty() {
        "(no deCall)"
    } else {
        &r.node
    };
    writeln!(
        out,
        "paired: node {node}, {span}, {} {} kHz",
        if passbands.len() == 1 {
            "passband"
        } else {
            "passbands"
        },
        passbands.join(", ")
    )?;
    let m = &r.manta_spots;
    writeln!(
        out,
        "manta spots: {} counted ({}, {})",
        m.counted,
        plural(m.out_of_passband, "out of passband", "out of passband"),
        plural(m.out_of_window, "out of window", "out of window"),
    )?;
    let f = &r.reference_reports;
    writeln!(
        out,
        "reference reports: {} counted ({}, {}, {}, {})",
        f.counted,
        plural(f.other_spotter, "other spotter", "other spotters"),
        plural(f.out_of_passband, "out of passband", "out of passband"),
        plural(f.out_of_window, "out of window", "out of window"),
        plural(
            f.implausible_call,
            "not a plausible callsign",
            "not plausible callsigns"
        ),
    )?;
    match &r.reference_span {
        None if f.loaded == 0 => writeln!(out, "reference silence: n/a (no CW reports)")?,
        None => writeln!(
            out,
            "reference silence: n/a (no CW reports from an admitted spotter)"
        )?,
        Some(ReferenceSpan {
            longest_silence_s: Some(gap),
            longest_silence_from: Some(at),
            ..
        }) => writeln!(
            out,
            "reference silence: longest {} from {}",
            duration(*gap),
            date_time(*at)
        )?,
        Some(_) => writeln!(
            out,
            "reference silence: n/a (fewer than 2 CW reports in the window)"
        )?,
    }
    writeln!(out)?;

    let reports_noun = |n: usize| {
        if n == 1 {
            "reference report"
        } else {
            "reference reports"
        }
    };
    let spots_noun = |n: usize| if n == 1 { "manta spot" } else { "manta spots" };
    writeln!(
        out,
        "recall: {} / {} {} that manta also spotted ({})",
        f.found,
        f.counted,
        reports_noun(f.counted),
        rate(f.found, f.counted)
    )?;
    writeln!(
        out,
        "missed: {} {} ({} of them spotted by manta on another band)",
        f.missed,
        reports_noun(f.missed),
        f.missed_elsewhere
    )?;
    let confirmed = m.matched + m.disagree;
    writeln!(
        out,
        "confirmed: {confirmed} {} the reference also reported (match {}, disagree {})",
        spots_noun(confirmed),
        m.matched,
        m.disagree
    )?;
    let covered = m.counted - m.unknown;
    let candidates = m.none + m.elsewhere;
    writeln!(
        out,
        "candidate false: {candidates} / {covered} {} the reference's span covered ({}): \
         none {}, elsewhere {}",
        spots_noun(covered),
        rate(candidates, covered),
        m.none,
        m.elsewhere
    )?;
    writeln!(
        out,
        "unknown: {} {} whose window the reference's span did not cover",
        m.unknown,
        spots_noun(m.unknown)
    )?;
    writeln!(out)?;

    writeln!(
        out,
        "{:<5}{:>9}{:>7}{:>8}{:>8}{:>7}{:>11}{:>6}{:>11}{:>9}",
        "band",
        "reports",
        "found",
        "missed",
        "recall",
        "spots",
        "confirmed",
        "none",
        "elsewhere",
        "unknown"
    )?;
    let mut total = BandCounts::default();
    for b in &r.bands {
        writeln!(out, "{}", band_cells(b.band, b))?;
        total.reference_reports += b.reference_reports;
        total.found += b.found;
        total.missed += b.missed;
        total.manta_spots += b.manta_spots;
        total.matched += b.matched;
        total.disagree += b.disagree;
        total.elsewhere += b.elsewhere;
        total.none += b.none;
        total.unknown += b.unknown;
    }
    writeln!(out, "{}", band_cells("total", &total))?;
    writeln!(out)?;

    if r.missed.is_empty() {
        writeln!(out, "missed by manta: none")?;
    } else {
        writeln!(
            out,
            "missed by manta (no manta spot of the call on the report's band within {} s):",
            r.window_s
        )?;
        writeln!(
            out,
            "{:<22}{:<10}{:>10}{:>4}{:>5}  manta heard it on",
            "time", "call", "freq kHz", "dB", "WPM"
        )?;
        for row in &r.missed {
            writeln!(
                out,
                "{:<22}{:<10}{:>10.1}{:>4}{:>5}  {}",
                date_time(row.time),
                row.call,
                row.freq_hz as f64 / 1000.0,
                or_dash(row.snr_db),
                or_dash(row.wpm.map(|w| format!("{w:.0}"))),
                bands_or_dash(&row.manta_bands)
            )?;
        }
    }
    writeln!(out)?;
    if r.candidate_false.is_empty() {
        writeln!(out, "candidate false spots: none")?;
    } else {
        writeln!(
            out,
            "candidate false spots (no reference report of the call on the spot's band \
             within {} s):",
            r.window_s
        )?;
        writeln!(
            out,
            "{:<22}{:<10}{:>10}{:>5}{:>6}  {:<9}  reference heard it on",
            "time", "call", "freq kHz", "snr", "conf", "grade"
        )?;
        for row in &r.candidate_false {
            let grade = match row.grade {
                Grade::None => "none",
                _ => "elsewhere",
            };
            writeln!(
                out,
                "{:<22}{:<10}{:>10.1}{:>5}{:>6}  {:<9}  {}",
                date_time(row.time),
                row.call,
                row.freq_hz as f64 / 1000.0,
                or_dash(row.snr),
                or_dash(row.decode_confidence.map(|c| format!("{c:.2}"))),
                grade,
                bands_or_dash(&row.reference_bands)
            )?;
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "dB is the reference's own figure (500 Hz); snr is manta's stream snr (2500 Hz)."
    )?;
    Ok(())
}

fn band_cells(label: &str, b: &BandCounts) -> String {
    let confirmed = b.matched + b.disagree;
    format!(
        "{label:<5}{:>9}{:>7}{:>8}{:>8}{:>7}{confirmed:>11}{:>6}{:>11}{:>9}",
        b.reference_reports,
        b.found,
        b.missed,
        rate(b.found, b.reference_reports),
        b.manta_spots,
        b.none,
        b.elsewhere,
        b.unknown
    )
}

/// The report as one JSON line.
pub(crate) fn render_json(r: &PairedReport) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string(r)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// 2026-10-31 00:00:00Z.
    const DAY: i64 = 1_793_404_800;
    const H: i64 = 3600;

    fn rep(spotter: &str, call: &str, khz: f64, time: i64) -> ReferenceReport {
        ReferenceReport {
            report: RbnReport {
                spotter: spotter.to_string(),
                call: call.to_string(),
                freq_hz: khz * 1000.0,
                wpm: Some(25.0),
                time,
            },
            snr_db: Some(20),
        }
    }

    fn spot(id: &str, call: &str, freq_hz: i64, time: i64) -> (GradeInput, SpotExtra) {
        (
            GradeInput {
                spot_id: id.to_string(),
                call: call.to_string(),
                time,
                freq_hz,
                wpm: Some(25),
            },
            SpotExtra {
                snr: Some(10),
                decode_confidence: Some(0.5),
            },
        )
    }

    fn report(
        spots: &[(GradeInput, SpotExtra)],
        archives: &[Vec<ReferenceReport>],
        cfg: &GradeConfig,
        start: Option<i64>,
        end: Option<i64>,
    ) -> PairedReport {
        build(PairedInputs {
            node: "W3XYZ-1",
            reference_source: "transcript",
            cfg,
            spots,
            archives,
            passbands: &[(14_000.0, 14_070.0)],
            start,
            end,
        })
    }

    fn paired_cfg() -> GradeConfig {
        GradeConfig {
            window_secs: PAIRED_WINDOW_SECS,
            ..GradeConfig::default()
        }
    }

    #[test]
    fn passband_parses_lo_hi_and_refuses_bad_ranges() {
        assert_eq!(parse_passband("14000-14070"), Ok((14_000.0, 14_070.0)));
        assert_eq!(parse_passband("7000.5-7040"), Ok((7_000.5, 7_040.0)));
        for bad in ["14070-14000", "x-1", "1-inf", "14000", "14000-14000", ""] {
            let err = parse_passband(bad).unwrap_err();
            assert!(
                err.starts_with("passband needs finite LO < HI in kHz, got "),
                "{bad}: {err}"
            );
        }
        assert_eq!(
            parse_passband("14070-14000").unwrap_err(),
            "passband needs finite LO < HI in kHz, got \"14070-14000\""
        );
    }

    #[test]
    fn rate_prints_one_decimal_or_n_a() {
        assert_eq!(rate(3, 5), "60.0%");
        assert_eq!(rate(1, 3), "33.3%");
        assert_eq!(rate(0, 0), "n/a");
    }

    #[test]
    fn duration_prints_h_mm_ss() {
        assert_eq!(duration(13_800), "3:50:00");
        assert_eq!(duration(59), "0:00:59");
        assert_eq!(duration(36_000 + 61), "10:01:01");
    }

    #[test]
    fn filters_choose_what_counts_not_what_matches() {
        let end = DAY + 6 * H;
        // The spot is just before --end, the report just after it.
        let spots = [spot("a", "W1AW", 14_025_000, end - 60)];
        let archives = [vec![
            rep("W3XYZ", "K1ABC", 14_020.0, DAY),
            rep("W3XYZ", "W1AW", 14_025.0, end + 60),
            rep("W3XYZ", "K1ABC", 14_020.0, end + H),
        ]];
        let r = report(&spots, &archives, &paired_cfg(), Some(DAY), Some(end));
        assert_eq!(r.manta_spots.counted, 1);
        assert_eq!(r.manta_spots.matched, 1);
        assert_eq!(r.reference_reports.loaded, 3);
        assert_eq!(r.reference_reports.out_of_window, 2);
        assert_eq!(r.reference_reports.counted, 1);
        // The W1AW report confirmed the spot but is not counted as found.
        assert_eq!(r.reference_reports.found, 0);
        assert_eq!(r.reference_reports.missed, 1);
        assert_eq!(r.missed[0].call, "K1ABC");
    }

    #[test]
    fn longest_silence_uses_admitted_reports_on_any_band_in_the_window() {
        let at = |h: i64, m: i64| DAY + h * H + m * 60;
        let archives = [vec![
            rep("W3XYZ", "W1AW", 14_025.0, at(0, 5)),
            rep("W3XYZ", "N4RV", 14_021.0, at(0, 20)),
            rep("W3XYZ", "K1ABC", 14_025.0, at(0, 40)),
            rep("W3XYZ", "DL1ABC", 14_030.0, at(1, 10)),
            rep("W3XYZ", "K5XYZ", 7_012.3, at(1, 30)),
            rep("W3XYZ", "TEST", 14_035.0, at(2, 0)),
            // Another spotter, rejected by the filter: not a report the
            // reference skimmer made.
            rep("K5TR", "W1AW", 14_025.0, at(4, 0)),
            rep("W3XYZ", "W1AW", 14_025.0, at(5, 50)),
            rep("W3XYZ", "W1AW", 14_025.0, at(6, 2)),
        ]];
        let cfg = GradeConfig {
            spotters: BTreeSet::from(["W3XYZ".to_string()]),
            ..paired_cfg()
        };
        let r = report(&[], &archives, &cfg, Some(DAY), Some(at(6, 0)));
        let span = r.reference_span.as_ref().expect("a span");
        assert_eq!((span.from, span.to), (at(0, 5), at(6, 2)));
        assert_eq!(span.longest_silence_s, Some(13_800));
        assert_eq!(span.longest_silence_from, Some(at(2, 0)));
        assert_eq!(r.reference_reports.other_spotter, 1);
        // Every report from another spotter: the span names the filter.
        let only_n0call = GradeConfig {
            spotters: BTreeSet::from(["N0CALL".to_string()]),
            ..paired_cfg()
        };
        let mut text = String::new();
        render_text(
            &report(&[], &archives, &only_n0call, Some(DAY), Some(at(0, 30))),
            &mut text,
        )
        .unwrap();
        assert!(
            text.contains("reference silence: n/a (no CW reports from an admitted spotter)\n"),
            "{text}"
        );
        let mut text = String::new();
        render_text(&r, &mut text).unwrap();
        assert!(
            text.contains("reference silence: longest 3:50:00 from 2026-10-31 02:00:00Z\n"),
            "{text}"
        );
    }

    #[test]
    fn only_an_admitted_spotter_s_reports_cover_a_window() {
        let spots = [
            spot("a", "W1AW", 14_025_000, DAY + H),
            spot("b", "K1ABC", 14_020_000, DAY + 2 * H),
        ];
        // The selected skimmer reported nothing; another spotter's reports
        // span the night (an RBN CSV, or a transcript holding two spotters).
        let cfg = GradeConfig {
            spotters: BTreeSet::from(["W3XYZ".to_string()]),
            ..paired_cfg()
        };
        let other = vec![
            rep("K5TR", "N4RV", 14_021.0, DAY),
            rep("K5TR", "N4RV", 14_021.0, DAY + 3 * H),
        ];
        let r = report(&spots, std::slice::from_ref(&other), &cfg, None, None);
        assert!(r.reference_span.is_none());
        assert_eq!(r.manta_spots.unknown, 2);
        assert_eq!(r.manta_spots.none, 0);
        assert_eq!(r.candidate_false_rate, None);
        assert!(r.candidate_false.is_empty());
        // The selected skimmer's own span ends at 01:15: it covers the
        // 01:00 spot's window, not the 02:00 one's, in one mixed file or
        // in a file of its own after the other spotter's.
        let own = vec![
            rep("W3XYZ", "N4RV", 14_021.0, DAY),
            rep("W3XYZ", "N4RV", 14_021.0, DAY + H + 900),
        ];
        let mixed: Vec<ReferenceReport> = own.iter().chain(&other).cloned().collect();
        for archives in [vec![mixed], vec![other, own]] {
            let r = report(&spots, &archives, &cfg, None, None);
            assert_eq!((r.manta_spots.none, r.manta_spots.unknown), (1, 1));
            assert_eq!(r.candidate_false.len(), 1);
            assert_eq!(r.candidate_false[0].spot_id, "a");
        }
    }

    #[test]
    fn empty_reference_gives_null_rates_and_unknown_spots() {
        let spots = [
            spot("a", "W1AW", 14_025_000, DAY + H),
            spot("b", "K1ABC", 14_020_000, DAY + 2 * H),
        ];
        let r = report(&spots, &[Vec::new()], &paired_cfg(), None, None);
        assert_eq!(r.manta_spots.counted, 2);
        assert_eq!(r.manta_spots.unknown, 2);
        assert_eq!(r.recall, None);
        assert_eq!(r.candidate_false_rate, None);
        assert!(r.reference_span.is_none());
        let json = render_json(&r).unwrap();
        assert!(json.contains(r#""recall":null,"candidateFalseRate":null,"referenceSpan":null"#));
        assert!(json.ends_with("}\n") && json.lines().count() == 1);
        let mut text = String::new();
        render_text(&r, &mut text).unwrap();
        for line in [
            "reference silence: n/a (no CW reports)\n",
            "recall: 0 / 0 reference reports that manta also spotted (n/a)\n",
            "candidate false: 0 / 0 manta spots the reference's span covered (n/a): none 0, \
             elsewhere 0\n",
            "unknown: 2 manta spots whose window the reference's span did not cover\n",
            "20m          0      0       0     n/a      2          0     0          0        2\n",
            "missed by manta: none\n",
            "candidate false spots: none\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
    }
}
