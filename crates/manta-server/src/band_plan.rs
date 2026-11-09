//! MAN-310: the standard CW skimming segments, and the plan that turns an
//! `[input]`'s `bands` into receiver streams. Pure: no I/O, no DSP.
//!
//! [`CW_SEGMENTS`] is the one table (docs/CONFIG.md's band table mirrors
//! it, and a docs test holds the two equal). [`plan`] takes the stream's
//! usable width as an argument, because the decimator's trusted fraction
//! lives in `manta-dsp`, which this crate does not depend on.

/// One band's CW skimming segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CwSegment {
    pub band: &'static str,
    pub cw_lo_hz: f64,
    pub cw_hi_hz: f64,
    /// The NCDXF/IARU beacon frequency, where the band has one.
    pub beacon_hz: Option<f64>,
}

impl CwSegment {
    /// What a plan skims: the CW segment, widened up to the beacon.
    pub fn span_hz(&self) -> (f64, f64) {
        (
            self.cw_lo_hz,
            self.beacon_hz
                .map_or(self.cw_hi_hz, |b| b.max(self.cw_hi_hz)),
        )
    }

    /// Whether the segment is HF: its span tops out at or below
    /// [`HF_TOP_HZ`]. `"all"` always includes HF segments, and includes
    /// the others (6 m) only when the tuning range overlaps them (MAN-543).
    pub fn is_hf(&self) -> bool {
        self.span_hz().1 <= HF_TOP_HZ
    }
}

const fn seg(band: &'static str, lo_khz: f64, hi_khz: f64, beacon_khz: Option<f64>) -> CwSegment {
    CwSegment {
        band,
        cw_lo_hz: lo_khz * 1000.0,
        cw_hi_hz: hi_khz * 1000.0,
        beacon_hz: match beacon_khz {
            Some(b) => Some(b * 1000.0),
            None => None,
        },
    }
}

/// Ascending by frequency; the rows of docs/CONFIG.md's band table. 80 m
/// is IARU Region 1's CW segment plus the Region 2 skimming practice; 60 m
/// is the WRC-15 allocation; the beacons are the NCDXF/IARU frequencies
/// (`manta_engine::calibrate::REFERENCES`' beacon rows). 6 m is the
/// 50.000 to 50.100 MHz CW segment, whose bottom is the 6 m beacon
/// sub-band; it has no NCDXF beacon (MAN-543).
pub const CW_SEGMENTS: &[CwSegment] = &[
    seg("160m", 1810.0, 1840.0, None),
    seg("80m", 3500.0, 3580.0, None),
    seg("60m", 5351.5, 5366.5, None),
    seg("40m", 7000.0, 7040.0, None),
    seg("30m", 10100.0, 10130.0, None),
    seg("20m", 14000.0, 14070.0, Some(14100.0)),
    seg("17m", 18068.0, 18095.0, Some(18110.0)),
    seg("15m", 21000.0, 21070.0, Some(21150.0)),
    seg("12m", 24890.0, 24915.0, Some(24930.0)),
    seg("10m", 28000.0, 28070.0, Some(28200.0)),
    seg("6m", 50000.0, 50100.0, None),
];

/// The top of HF, 30 MHz: segments at or below it are [`CwSegment::is_hf`].
pub const HF_TOP_HZ: f64 = 30_000_000.0;

/// `input.tuning_range_hz`'s default: an HF receiver, 0 to 30 MHz.
pub const DEFAULT_TUNING_RANGE_HZ: (f64, f64) = (0.0, HF_TOP_HZ);

/// One channel (`manta_dsp::channelizer::CHANNEL_SPACING_HZ`); the D6
/// cross-input dedupe tolerance. A split stream may spawn this far past
/// the split point, so a signal on it is always spawnable by one stream.
pub const SPLIT_GUARD_HZ: f64 = 93.75;

/// One item of `input.bands`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BandItem {
    All,
    Band(&'static CwSegment),
    /// A bare centre frequency, Hz (written in kHz).
    CentreHz(f64),
}

fn band_names() -> String {
    let names: Vec<&str> = CW_SEGMENTS.iter().map(|s| s.band).collect();
    match names.split_last() {
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Parses one item: `"all"`, a band name (case-insensitive) or a centre
/// frequency in kHz.
pub fn parse_item(text: &str) -> Result<BandItem, String> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("all") {
        return Ok(BandItem::All);
    }
    if let Some(s) = CW_SEGMENTS.iter().find(|s| s.band.eq_ignore_ascii_case(t)) {
        return Ok(BandItem::Band(s));
    }
    match t.parse::<f64>() {
        Ok(khz) if khz.is_finite() && khz > 0.0 => Ok(BandItem::CentreHz((khz * 1000.0).round())),
        _ => Err(format!(
            "input.bands: \"{text}\" is not \"all\", a band ({}) or a centre frequency in kHz \
             such as \"7030\"",
            band_names()
        )),
    }
}

/// One stream of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedStream {
    /// `60m`, `60m-2`, `7030`: the suffix of a split entry's stream names.
    pub label: String,
    pub band: Option<&'static str>,
    pub centre_hz: f64,
    /// Hz from `centre_hz`; infinite on a side with no overlapping sibling
    /// stream, `None` when no sibling overlaps it.
    pub spawn_band_hz: Option<(f64, f64)>,
}

/// What [`plan`] made of an entry's `bands`.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPlan {
    pub streams: Vec<PlannedStream>,
    notes: Vec<String>,
}

impl StreamPlan {
    /// `14 streams: 160m, 80m, …, 10m (3)`, or `1 stream: 7030 kHz`.
    pub fn summary(&self) -> String {
        format!(
            "{}: {}",
            count_streams(self.streams.len()),
            self.band_list()
        )
    }

    /// `160m, 80m, 20m (2), 7030 kHz`: each band (with its stream count
    /// when split) and each bare centre, in stream order.
    pub fn band_list(&self) -> String {
        let mut parts: Vec<(String, usize)> = Vec::new();
        for s in &self.streams {
            let key = match s.band {
                Some(b) => b.to_string(),
                None => format!("{} kHz", s.label),
            };
            match parts.last_mut() {
                Some((k, n)) if *k == key && s.band.is_some() => *n += 1,
                _ => parts.push((key, 1)),
            }
        }
        parts
            .iter()
            .map(|(k, n)| match n {
                1 => k.clone(),
                n => format!("{k} ({n})"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The skip and clip sentences, in item order.
    pub fn notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

/// `1 stream`, `14 streams`.
pub fn count_streams(n: usize) -> String {
    match n {
        1 => "1 stream".to_string(),
        n => format!("{n} streams"),
    }
}

fn range_text(lo: f64, hi: f64) -> String {
    format!("{} to {} kHz", exact_khz(lo), exact_khz(hi))
}

/// The most streams one entry may plan. Far above any real plan ("all" on
/// a Kiwi-width stream is 14), it bounds what [`plan`] materializes when a
/// tiny stream width would otherwise split a band into millions of slices
/// (MAN-310, Codex review PR #345).
pub const MAX_PLANNED_STREAMS: usize = 256;

/// Plans an entry's streams: each item's span clipped to the tuning range,
/// split into `ceil(span / usable_width_hz)` equal slices centred on their
/// midpoints (whole Hz), sorted by centre. Overlapping streams split the
/// overlap at the midpoint of their centres, plus [`SPLIT_GUARD_HZ`].
pub fn plan(
    items: &[BandItem],
    tuning_range_hz: (f64, f64),
    usable_width_hz: f64,
) -> Result<StreamPlan, String> {
    if items.is_empty() {
        return Err(
            "input.bands is empty; list \"all\", bands such as \"40m\", or centre frequencies \
             in kHz such as \"7030\""
                .to_string(),
        );
    }
    let has_all = items.contains(&BandItem::All);
    let (r_lo, r_hi) = tuning_range_hz;
    // What "all" means: every HF segment, plus any other segment (6 m)
    // that the tuning range overlaps (MAN-543).
    let all_set: Vec<&'static CwSegment> = CW_SEGMENTS
        .iter()
        .filter(|s| {
            let (lo, hi) = s.span_hz();
            s.is_hf() || hi.min(r_hi) > lo.max(r_lo)
        })
        .collect();
    // Expand "all" in place, refusing repeats.
    let mut bands: Vec<&'static CwSegment> = Vec::new();
    let mut centres: Vec<f64> = Vec::new();
    let mut all_seen = false;
    for item in items {
        match item {
            BandItem::All if all_seen => return Err("input.bands lists \"all\" twice".into()),
            BandItem::All => all_seen = true,
            BandItem::Band(s) if has_all && all_set.iter().any(|a| a.band == s.band) => {
                return Err(format!(
                    "input.bands lists {}, which \"all\" already includes",
                    s.band
                ))
            }
            BandItem::Band(s) if bands.iter().any(|b| b.band == s.band) => {
                return Err(format!("input.bands lists {} twice", s.band))
            }
            BandItem::Band(s) => bands.push(s),
            BandItem::CentreHz(c) if centres.contains(c) => {
                return Err(format!("input.bands lists \"{}\" twice", exact_khz(*c)))
            }
            BandItem::CentreHz(c) => centres.push(*c),
        }
    }
    if has_all {
        bands.extend(all_set.iter().copied());
    }
    for c in &centres {
        if let Some(s) = bands.iter().find(|s| {
            let (lo, hi) = s.span_hz();
            *c >= lo && *c <= hi
        }) {
            let (lo, hi) = s.span_hz();
            return Err(format!(
                "input.bands lists \"{}\", which is inside {} ({}), already listed",
                exact_khz(*c),
                s.band,
                range_text(lo, hi)
            ));
        }
    }

    let range = range_text(r_lo, r_hi);
    let mut streams: Vec<PlannedStream> = Vec::new();
    let mut notes = Vec::new();
    let mut outside = Vec::new();
    // Item order for notes: bands as listed (or table order), then centres
    // in the order listed relative to bands.
    for item in items {
        let segs: Vec<&'static CwSegment> = match item {
            BandItem::All => all_set.clone(),
            BandItem::Band(s) => vec![*s],
            BandItem::CentreHz(c) => {
                if *c < r_lo || *c > r_hi {
                    notes.push(format!(
                        "{} kHz skipped: outside the receiver's tuning range, {range}",
                        exact_khz(*c)
                    ));
                    outside.push(format!("{} kHz", exact_khz(*c)));
                } else {
                    streams.push(PlannedStream {
                        label: exact_khz(*c),
                        band: None,
                        centre_hz: *c,
                        spawn_band_hz: None,
                    });
                }
                continue;
            }
        };
        for s in segs {
            let (lo, hi) = s.span_hz();
            let (c_lo, c_hi) = (lo.max(r_lo), hi.min(r_hi));
            if c_hi <= c_lo {
                notes.push(format!(
                    "{} skipped: {} is outside the receiver's tuning range, {range}",
                    s.band,
                    range_text(lo, hi)
                ));
                outside.push(format!("{} ({})", s.band, range_text(lo, hi)));
                continue;
            }
            if c_lo > lo || c_hi < hi {
                notes.push(format!(
                    "{} clipped to {}, the part inside the receiver's tuning range, {range}",
                    s.band,
                    range_text(c_lo, c_hi)
                ));
            }
            let width = c_hi - c_lo;
            let n = ((width / usable_width_hz).ceil() as usize).max(1);
            if streams.len().saturating_add(n) > MAX_PLANNED_STREAMS {
                return Err(format!(
                    "input.bands would plan more than {MAX_PLANNED_STREAMS} streams: a {} kHz \
                     stream skims too little of {} ({}); raise the stream's rate or list fewer \
                     bands",
                    exact_khz(usable_width_hz),
                    s.band,
                    range_text(c_lo, c_hi)
                ));
            }
            let slice = width / n as f64;
            for i in 0..n {
                streams.push(PlannedStream {
                    label: match n {
                        1 => s.band.to_string(),
                        _ => format!("{}-{}", s.band, i + 1),
                    },
                    band: Some(s.band),
                    centre_hz: (c_lo + (i as f64 + 0.5) * slice).round(),
                    spawn_band_hz: None,
                });
            }
        }
    }
    if streams.is_empty() {
        let verb = if outside.len() == 1 { "is" } else { "are" };
        return Err(format!(
            "input.bands leaves nothing to skim inside the receiver's tuning range, {range}: {} \
             {verb} outside it",
            outside.join(", ")
        ));
    }
    streams.sort_by(|a, b| a.centre_hz.total_cmp(&b.centre_hz));
    // Nearer-centre ownership among overlapping streams.
    for i in 0..streams.len() {
        let c = streams[i].centre_hz;
        let lower = i
            .checked_sub(1)
            .map(|j| streams[j].centre_hz)
            .filter(|l| c - l <= usable_width_hz);
        let upper = streams
            .get(i + 1)
            .map(|s| s.centre_hz)
            .filter(|u| u - c <= usable_width_hz);
        if lower.is_none() && upper.is_none() {
            continue;
        }
        let lo = lower.map_or(f64::NEG_INFINITY, |l| (l + c) / 2.0 - c - SPLIT_GUARD_HZ);
        let hi = upper.map_or(f64::INFINITY, |u| (c + u) / 2.0 - c + SPLIT_GUARD_HZ);
        streams[i].spawn_band_hz = Some((lo, hi));
    }
    Ok(StreamPlan { streams, notes })
}

/// kHz with up to three decimals, trailing zeros trimmed: `1810`,
/// `5351.5`, `28033.333`.
pub fn exact_khz(hz: f64) -> String {
    let s = format!("{:.3}", hz / 1000.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const U96: f64 = 2.0 * 0.4526 * 96_000.0;
    const UKIWI: f64 = 2.0 * 0.4526 * 10_000.0;

    fn items(texts: &[&str]) -> Vec<BandItem> {
        texts.iter().map(|t| parse_item(t).unwrap()).collect()
    }

    fn centres(p: &StreamPlan) -> Vec<f64> {
        p.streams.iter().map(|s| s.centre_hz).collect()
    }

    #[test]
    fn all_on_a_0_to_30_mhz_receiver_plans_every_cw_segment_160_through_10m() {
        let p = plan(&items(&["all"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(
            centres(&p),
            vec![
                1_825_000.0,
                3_540_000.0,
                5_359_000.0,
                7_020_000.0,
                10_115_000.0,
                14_025_000.0,
                14_075_000.0,
                18_089_000.0,
                21_037_500.0,
                21_112_500.0,
                24_910_000.0,
                28_033_333.0,
                28_100_000.0,
                28_166_667.0,
            ]
        );
        let labels: Vec<&str> = p.streams.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels.join(" "),
            "160m 80m 60m 40m 30m 20m-1 20m-2 17m 15m-1 15m-2 12m 10m-1 10m-2 10m-3"
        );
        for s in CW_SEGMENTS.iter().filter(|s| s.is_hf()) {
            assert!(
                p.streams.iter().any(|st| st.band == Some(s.band)),
                "{} has no stream",
                s.band
            );
            if let Some(b) = s.beacon_hz {
                assert!(
                    p.streams
                        .iter()
                        .any(|st| (b - st.centre_hz).abs() <= U96 / 2.0),
                    "the {} beacon at {b} Hz is not skimmed",
                    s.band
                );
            }
        }
        assert!(p.streams.iter().all(|st| st.band != Some("6m")));
        assert!(p.notes().is_empty());
    }

    const U192: f64 = 2.0 * 0.4526 * 192_000.0;
    const VHF_RANGE: (f64, f64) = (0.0, 61_440_000.0);

    #[test]
    fn six_metres_plans_one_192k_stream_centred_on_50050_khz() {
        let s6 = CW_SEGMENTS.iter().find(|s| s.band == "6m").unwrap();
        assert_eq!(s6.span_hz(), (50_000_000.0, 50_100_000.0));
        assert_eq!(s6.beacon_hz, None);
        assert!(!s6.is_hf());
        let p = plan(&items(&["6m"]), VHF_RANGE, U192).unwrap();
        assert_eq!(centres(&p), vec![50_050_000.0]);
        assert_eq!(p.streams[0].label, "6m");
        assert_eq!(p.streams[0].band, Some("6m"));
        assert!(p.notes().is_empty());
        assert_eq!(p.summary(), "1 stream: 6m");
    }

    #[test]
    fn six_metres_on_96k_streams_is_two_streams() {
        let p = plan(&items(&["6m"]), VHF_RANGE, U96).unwrap();
        assert_eq!(centres(&p), vec![50_025_000.0, 50_075_000.0]);
        let labels: Vec<&str> = p.streams.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["6m-1", "6m-2"]);
    }

    #[test]
    fn six_metres_on_the_default_range_leaves_nothing_to_skim() {
        assert_eq!(
            plan(&items(&["6m"]), DEFAULT_TUNING_RANGE_HZ, U192).unwrap_err(),
            "input.bands leaves nothing to skim inside the receiver's tuning range, 0 to \
             30000 kHz: 6m (50000 to 50100 kHz) is outside it"
        );
    }

    #[test]
    fn all_adds_6m_only_when_the_tuning_range_reaches_it() {
        let p = plan(&items(&["all"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(
            p.summary(),
            "14 streams: 160m, 80m, 60m, 40m, 30m, 20m (2), 17m, 15m (2), 12m, 10m (3)"
        );
        assert!(p.notes().is_empty());

        let p = plan(&items(&["all"]), VHF_RANGE, U96).unwrap();
        assert_eq!(p.streams.len(), 16);
        assert_eq!(
            p.summary(),
            "16 streams: 160m, 80m, 60m, 40m, 30m, 20m (2), 17m, 15m (2), 12m, 10m (3), 6m (2)"
        );
        assert!(p.notes().is_empty());

        // A range ending exactly at 50 MHz overlaps 6m by zero width.
        let p = plan(&items(&["all"]), (0.0, 50_000_000.0), U96).unwrap();
        assert!(p.streams.iter().all(|s| s.band != Some("6m")));
        assert_eq!(p.streams.len(), 14);
        assert!(p.notes().is_empty());

        // A VHF-only range: 6m is planned and every HF band is noted.
        let p = plan(&items(&["all"]), (49_000_000.0, 55_000_000.0), U192).unwrap();
        assert_eq!(p.summary(), "1 stream: 6m");
        assert_eq!(p.notes().len(), 10);
        assert!(p.notes().iter().all(|n| n.contains(" skipped: ")));
    }

    #[test]
    fn all_on_a_3_to_30_mhz_receiver_skips_160m_with_an_out_of_range_note() {
        let p = plan(&items(&["all"]), (3_000_000.0, 30_000_000.0), U96).unwrap();
        assert_eq!(p.streams.len(), 13);
        assert!(p.streams.iter().all(|s| s.label != "160m"));
        assert_eq!(
            p.notes(),
            vec![
                "160m skipped: 1810 to 1840 kHz is outside the receiver's tuning range, 3000 to \
                 30000 kHz"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_150_khz_segment_on_96_khz_streams_opens_two_streams_split_at_the_midpoint() {
        let s15 = CW_SEGMENTS.iter().find(|s| s.band == "15m").unwrap();
        assert_eq!(s15.span_hz(), (21_000_000.0, 21_150_000.0));
        let p = plan(&items(&["15m"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(centres(&p), vec![21_037_500.0, 21_112_500.0]);
        assert_eq!(
            p.streams[0].spawn_band_hz,
            Some((f64::NEG_INFINITY, 37_593.75))
        );
        assert_eq!(
            p.streams[1].spawn_band_hz,
            Some((-37_593.75, f64::INFINITY))
        );
    }

    #[test]
    fn a_bare_centre_plans_one_stream_on_that_frequency() {
        for (text, hz, label) in [
            ("7030", 7_030_000.0, "7030"),
            ("7030.5", 7_030_500.0, "7030.5"),
        ] {
            let p = plan(&items(&[text]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
            assert_eq!(p.streams.len(), 1);
            assert_eq!(p.streams[0].centre_hz, hz);
            assert_eq!(p.streams[0].label, label);
            assert_eq!(p.streams[0].spawn_band_hz, None);
        }
        // The number 7030 reaches parse_item as its TOML text.
        assert_eq!(
            parse_item(&7030.to_string()).unwrap(),
            BandItem::CentreHz(7_030_000.0)
        );
    }

    #[test]
    fn the_kiwi_width_splits_60m_into_two_streams() {
        let p = plan(&items(&["60m"]), DEFAULT_TUNING_RANGE_HZ, UKIWI).unwrap();
        assert_eq!(centres(&p), vec![5_355_250.0, 5_362_750.0]);
        assert_eq!(
            p.streams[0].spawn_band_hz,
            Some((f64::NEG_INFINITY, 3_843.75))
        );
        assert_eq!(p.streams[1].spawn_band_hz, Some((-3_843.75, f64::INFINITY)));
        assert_eq!(p.streams[0].label, "60m-1");
        assert_eq!(p.streams[1].label, "60m-2");
    }

    #[test]
    fn stream_counts_follow_the_stream_width() {
        for (width, n) in [
            (192_000.0, 11),
            (96_000.0, 14),
            (48_000.0, 20),
            (10_000.0, 86),
        ] {
            let p = plan(
                &items(&["all"]),
                DEFAULT_TUNING_RANGE_HZ,
                2.0 * 0.4526 * width,
            )
            .unwrap();
            assert_eq!(p.streams.len(), n, "at {width} Hz");
        }
    }

    #[test]
    fn a_partly_reachable_band_is_clipped_and_noted() {
        let p = plan(&items(&["80m"]), (3_520_000.0, 30_000_000.0), U96).unwrap();
        assert_eq!(centres(&p), vec![3_550_000.0]);
        assert_eq!(
            p.notes(),
            vec![
                "80m clipped to 3520 to 3580 kHz, the part inside the receiver's tuning range, \
                 3520 to 30000 kHz"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_bare_centre_outside_the_range_is_skipped_with_a_note() {
        let p = plan(&items(&["40m", "31000"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(p.streams.len(), 1);
        assert_eq!(
            p.notes(),
            vec![
                "31000 kHz skipped: outside the receiver's tuning range, 0 to 30000 kHz"
                    .to_string()
            ]
        );
    }

    #[test]
    fn nothing_left_in_range_is_an_error() {
        assert_eq!(
            plan(&items(&["160m"]), (3_000_000.0, 30_000_000.0), U96).unwrap_err(),
            "input.bands leaves nothing to skim inside the receiver's tuning range, 3000 to \
             30000 kHz: 160m (1810 to 1840 kHz) is outside it"
        );
        assert_eq!(
            plan(&[], DEFAULT_TUNING_RANGE_HZ, U96).unwrap_err(),
            "input.bands is empty; list \"all\", bands such as \"40m\", or centre frequencies \
             in kHz such as \"7030\""
        );
    }

    #[test]
    fn items_parse_case_insensitively_and_unknown_items_list_the_vocabulary() {
        assert_eq!(parse_item("All").unwrap(), BandItem::All);
        assert_eq!(
            parse_item("40M").unwrap(),
            BandItem::Band(CW_SEGMENTS.iter().find(|s| s.band == "40m").unwrap())
        );
        assert_eq!(
            parse_item("6M").unwrap(),
            BandItem::Band(CW_SEGMENTS.iter().find(|s| s.band == "6m").unwrap())
        );
        for bad in ["4m", "abc", "0", "-5"] {
            assert_eq!(
                parse_item(bad).unwrap_err(),
                format!(
                    "input.bands: \"{bad}\" is not \"all\", a band (160m, 80m, 60m, 40m, 30m, \
                     20m, 17m, 15m, 12m, 10m or 6m) or a centre frequency in kHz such as \"7030\""
                )
            );
        }
    }

    #[test]
    fn a_tiny_stream_width_is_refused_before_materializing_the_plan() {
        // MAN-310 (Codex review, PR #345): rate_hz = 0.001 on "all" would
        // otherwise build hundreds of millions of streams.
        let err = plan(&items(&["all"]), (0.0, 30e6), 0.001).unwrap_err();
        assert!(
            err.contains(&format!("more than {MAX_PLANNED_STREAMS} streams")),
            "{err}"
        );
    }

    #[test]
    fn repeats_are_refused() {
        let err = |t: &[&str]| plan(&items(t), DEFAULT_TUNING_RANGE_HZ, U96).unwrap_err();
        assert_eq!(err(&["40m", "40m"]), "input.bands lists 40m twice");
        assert_eq!(
            err(&["all", "20m"]),
            "input.bands lists 20m, which \"all\" already includes"
        );
        assert_eq!(
            err(&["7030", "40m"]),
            "input.bands lists \"7030\", which is inside 40m (7000 to 7040 kHz), already listed"
        );
        // MAN-543: 6m repeats "all" only when "all" includes it, which is
        // when the tuning range reaches it.
        assert_eq!(
            plan(&items(&["all", "6m"]), (0.0, 61_440_000.0), U96).unwrap_err(),
            "input.bands lists 6m, which \"all\" already includes"
        );
        // A bare centre inside 6m repeats "all" on such a range, too.
        assert_eq!(
            plan(&items(&["all", "50050"]), (0.0, 61_440_000.0), U96).unwrap_err(),
            "input.bands lists \"50050\", which is inside 6m (50000 to 50100 kHz), already listed"
        );
        let p = plan(&items(&["all", "6m"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(p.streams.len(), 14);
        assert_eq!(
            p.notes(),
            vec![
                "6m skipped: 50000 to 50100 kHz is outside the receiver's tuning range, 0 to \
                 30000 kHz"
                    .to_string()
            ]
        );
    }

    #[test]
    fn every_segment_lies_inside_its_band_allocation() {
        for s in CW_SEGMENTS {
            let (lo, hi) = s.span_hz();
            assert_eq!(crate::band::band_for_freq_hz(lo), s.band);
            assert_eq!(crate::band::band_for_freq_hz(hi), s.band);
        }
    }

    #[test]
    fn exact_khz_trims_to_three_decimals() {
        assert_eq!(exact_khz(1_810_000.0), "1810");
        assert_eq!(exact_khz(5_351_500.0), "5351.5");
        assert_eq!(exact_khz(28_033_333.0), "28033.333");
        assert_eq!(exact_khz(5_359_093.75), "5359.094");
    }

    #[test]
    fn summary_counts_streams_per_band() {
        let p = plan(&items(&["all"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(
            p.summary(),
            "14 streams: 160m, 80m, 60m, 40m, 30m, 20m (2), 17m, 15m (2), 12m, 10m (3)"
        );
        let p = plan(&items(&["7030"]), DEFAULT_TUNING_RANGE_HZ, U96).unwrap();
        assert_eq!(p.summary(), "1 stream: 7030 kHz");
    }
}
