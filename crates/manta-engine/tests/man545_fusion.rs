//! MAN-545: the offline legacy/HSMM evidence fuser's rules and item 16's
//! two invariants, on hand-built event streams. No audio is decoded, so
//! this runs in CI in milliseconds.
#[path = "support/man545.rs"]
mod support;

use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use support::*;

const F_HZ: f64 = 14_025_000.0;

/// One track's events for `text`: a `TrackPromoted` at `freq_hz`, then a
/// `CharDecoded` per character (confidence `conf`, alternatives from
/// `alts` by character index) and a `WordBoundary` per space.
fn track(
    track_id: u32,
    freq_hz: f64,
    text: &str,
    conf: f32,
    alts: &[(usize, Vec<(char, f32)>)],
) -> Vec<DecoderEvent> {
    let mut out = vec![DecoderEvent::TrackPromoted {
        track_id,
        sample_ts: 0,
        freq_hz,
    }];
    for (i, c) in text.chars().enumerate() {
        let sample_ts = 1_000 * (i as u64 + 1);
        if c == ' ' {
            out.push(DecoderEvent::word_boundary(track_id, sample_ts));
            continue;
        }
        out.push(DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            glyph: Glyph::Char(c),
            confidence: conf,
            alternatives: alts
                .iter()
                .filter(|(at, _)| *at == i)
                .flat_map(|(_, a)| a.iter().map(|&(g, m)| (Glyph::Char(g), m)))
                .collect(),
        });
    }
    out
}

fn items(events: &[DecoderEvent]) -> Vec<Item> {
    station_items(events, &track_freqs(events), F_HZ)
}

fn text(items: &[Item]) -> String {
    chars(items).into_iter().collect()
}

fn bytes(events: &[DecoderEvent]) -> Vec<u8> {
    serde_json::to_vec(events).unwrap()
}

/// Fuses one single-station recording.
fn fused(policy: Policy, legacy: &[DecoderEvent], hsmm: &[DecoderEvent], truth: &str) -> String {
    fused_with(policy, legacy, hsmm, truth, None)
}

fn fused_with(
    policy: Policy,
    legacy: &[DecoderEvent],
    hsmm: &[DecoderEvent],
    truth: &str,
    cal: Option<&Calibration>,
) -> String {
    let (l, h, t) = (items(legacy), items(hsmm), normalize(truth));
    let st = StationInput {
        legacy: &l,
        hsmm: &h,
        truth: &t,
    };
    text(&items(&fuse(policy, legacy, &[st], cal)))
}

/// The substitution-unit policies.
const ALL: [Policy; 4] = [
    Policy::Noop,
    Policy::OracleLegacyAlts,
    Policy::OracleSub,
    Policy::CalSub,
];

/// Every policy, the span unit included.
const EVERY: [Policy; 6] = [
    Policy::Noop,
    Policy::OracleLegacyAlts,
    Policy::OracleSub,
    Policy::CalSub,
    Policy::OracleSpan,
    Policy::CalSpan,
];

#[test]
fn alignment_is_deterministic() {
    // "AB" against "B": deleting A or substituting A->B then deleting B
    // cost the same; the traceback prefers the diagonal first.
    let ops = |l: &str, r: &str| {
        align(
            &l.chars().collect::<Vec<_>>(),
            &r.chars().collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|(op, _, _)| op)
        .collect::<Vec<_>>()
    };
    assert_eq!(ops("AB", "B"), vec![Op::LeftOnly, Op::Match]);
    assert_eq!(ops("AB", "BA"), vec![Op::Sub, Op::Sub]);
    assert_eq!(ops("A", "BA"), vec![Op::RightOnly, Op::Match]);
    assert_eq!(ops("AA", "A"), vec![Op::LeftOnly, Op::Match]);
    assert_eq!(ops("AB", "CD"), ops("AB", "CD"));
    assert_eq!(ops("", "AB"), vec![Op::RightOnly, Op::RightOnly]);
}

#[test]
fn station_transcript_matches_bench_join() {
    let mut events = track(4, F_HZ + 100.0, "W1AW K", 0.5, &[]);
    events.extend(track(2, F_HZ - 150.0, " CQ DE ", 0.5, &[]));
    events.extend(track(3, F_HZ + 151.0, "XX", 0.5, &[]));
    // Ascending track id, one separator between tracks, the 151 Hz track
    // left out, leading and trailing spaces trimmed per track.
    assert_eq!(text(&items(&events)), "CQ DE\nW1AW K");
    assert_eq!(
        normalize("  cq  de w1aw "),
        "CQ DE W1AW".chars().collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_branch_changes_nothing_sub() {
    let legacy = track(
        1,
        F_HZ,
        "CQ DE W1?XW W1AW KK",
        0.4,
        &[(8, vec![('A', 0.1)]), (2, vec![('E', 0.05)])],
    );
    let (l, t) = (items(&legacy), normalize("CQ DE W1AW W1AW K"));
    let cal = Calibration::default();
    for policy in [Policy::Noop, Policy::OracleSub, Policy::CalSub] {
        let st = StationInput {
            legacy: &l,
            hsmm: &l,
            truth: &t,
        };
        let out = fuse(policy, &legacy, &[st], Some(&cal));
        assert_eq!(bytes(&out), bytes(&legacy), "{policy:?}");
    }
}

#[test]
fn unknown_span_is_never_erased() {
    let legacy = track(1, F_HZ, "CQ DE W1?AW W1?AW K", 0.4, &[]);
    let hsmm = track(1, F_HZ, "CQ DE W1AW W1AW K", 0.9, &[]);
    let cal = tables(0.4, 0.9);
    for policy in EVERY {
        let out = fused_with(policy, &legacy, &hsmm, "CQ DE W1AW W1AW K", Some(&cal));
        assert_eq!(out, "CQ DE W1?AW W1?AW K", "{policy:?}");
    }
    // A `?` aligned against a real HSMM glyph is kept too.
    let legacy = track(1, F_HZ, "W1?W", 0.4, &[]);
    let hsmm = track(1, F_HZ, "W1AW", 0.9, &[]);
    for policy in ALL {
        assert_eq!(fused(policy, &legacy, &hsmm, "W1AW"), "W1?W", "{policy:?}");
    }
}

#[test]
fn question_mark_never_replaces_a_glyph() {
    let legacy = track(1, F_HZ, "W1AW", 0.2, &[(2, vec![('?', 0.1)])]);
    let hsmm = track(1, F_HZ, "W1?W", 0.9, &[]);
    let cal = tables(0.2, 0.9);
    for policy in EVERY {
        let out = fused_with(policy, &legacy, &hsmm, "W1?W", Some(&cal));
        assert_eq!(out, "W1AW", "{policy:?}");
    }
}

/// Tables where a confidence in bin `hi` is right 9 of 10 times and one
/// in bin `lo` 1 of 10 times, for both engines' winner characters.
fn tables(lo: f32, hi: f32) -> Calibration {
    let mut rows = Vec::new();
    for engine in [Engine::Legacy, Engine::Hsmm] {
        for k in 0..10 {
            rows.push((
                0,
                engine,
                Label::WinChar {
                    conf: hi,
                    right: k < 9,
                },
            ));
            rows.push((
                0,
                engine,
                Label::WinChar {
                    conf: lo,
                    right: k < 1,
                },
            ));
        }
    }
    Calibration::fit(&rows, None)
}

#[test]
fn cal_sub_ties_keep_legacy() {
    let legacy = track(1, F_HZ, "W1AW", 0.5, &[]);
    let hsmm = track(1, F_HZ, "W1EW", 0.5, &[]);
    let cal = tables(0.1, 0.9);
    assert_eq!(
        fused_with(Policy::CalSub, &legacy, &hsmm, "W1EW", Some(&cal)),
        "W1AW"
    );
    // Flat tables (no calibration at all) tie too.
    assert_eq!(fused(Policy::CalSub, &legacy, &hsmm, "W1EW"), "W1AW");
}

#[test]
fn cal_sub_takes_the_better_supported_glyph() {
    let legacy = track(1, F_HZ, "W1AW", 0.1, &[]);
    let hsmm = track(1, F_HZ, "W1EW", 0.9, &[]);
    let cal = tables(0.1, 0.9);
    let (l, h, t) = (items(&legacy), items(&hsmm), normalize("W1EW"));
    let st = StationInput {
        legacy: &l,
        hsmm: &h,
        truth: &t,
    };
    let out = fuse(Policy::CalSub, &legacy, &[st], Some(&cal));
    assert_eq!(text(&items(&out)), "W1EW");
    let replaced = out
        .iter()
        .find_map(|e| match e {
            DecoderEvent::CharDecoded {
                glyph: Glyph::Char('E'),
                confidence,
                sample_ts,
                track_id,
                ..
            } => Some((*confidence, *sample_ts, *track_id)),
            _ => None,
        })
        .unwrap();
    // HSMM's raw confidence on legacy's track and timestamp.
    assert_eq!(replaced, (0.9, 3_000, 1));
    assert!(replaced.0.is_finite() && replaced.0 > 0.0);
}

#[test]
fn oracle_sub_takes_truth_from_any_proposal() {
    let truth = "W1AW";
    // From HSMM's winner.
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[]);
    let hsmm = track(1, F_HZ, "W1AW", 0.5, &[]);
    assert_eq!(fused(Policy::OracleSub, &legacy, &hsmm, truth), "W1AW");
    // From HSMM's alternatives.
    let hsmm = track(1, F_HZ, "W1IW", 0.5, &[(2, vec![('A', 0.2)])]);
    assert_eq!(fused(Policy::OracleSub, &legacy, &hsmm, truth), "W1AW");
    // From legacy's runner-ups.
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[(2, vec![('A', 0.2)])]);
    let hsmm = track(1, F_HZ, "W1IW", 0.5, &[]);
    assert_eq!(fused(Policy::OracleSub, &legacy, &hsmm, truth), "W1AW");
    // Nobody proposes it: unchanged.
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[]);
    assert_eq!(fused(Policy::OracleSub, &legacy, &hsmm, truth), "W1EW");
}

#[test]
fn oracle_legacy_alts_ignores_hsmm() {
    let truth = "W1AW";
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[(2, vec![('A', 0.2)])]);
    let empty: Vec<DecoderEvent> = Vec::new();
    let hsmm = track(1, F_HZ, "W1IW", 0.5, &[]);
    assert_eq!(
        fused(Policy::OracleLegacyAlts, &legacy, &empty, truth),
        "W1AW"
    );
    assert_eq!(
        fused(Policy::OracleLegacyAlts, &legacy, &hsmm, truth),
        "W1AW"
    );
    // HSMM's correct winner is not a legacy runner-up: unchanged.
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[]);
    let hsmm = track(1, F_HZ, "W1AW", 0.5, &[]);
    assert_eq!(
        fused(Policy::OracleLegacyAlts, &legacy, &hsmm, truth),
        "W1EW"
    );
    // A runner-up whose mass underflowed to 0 carries no evidence.
    let legacy = track(1, F_HZ, "W1EW", 0.5, &[(2, vec![('A', 0.0)])]);
    assert_eq!(
        fused(Policy::OracleLegacyAlts, &legacy, &empty, truth),
        "W1EW"
    );
}

#[test]
fn calibration_cross_fit_excludes_the_held_out_trial() {
    let win = |conf: f32, right: bool| Label::WinChar { conf, right };
    let rows = vec![
        (0, Engine::Legacy, win(0.52, true)),
        (0, Engine::Legacy, win(0.53, true)),
        (2, Engine::Legacy, win(0.51, true)),
        (2, Engine::Legacy, win(0.54, false)),
        // Trial 1's own labels: all wrong, and left out of its tables.
        (1, Engine::Legacy, win(0.52, false)),
        (1, Engine::Legacy, win(0.52, false)),
    ];
    let cal = Calibration::fit(&rows, Some(1));
    let item = |conf: f32| Item {
        c: 'A',
        conf,
        alts: Vec::new(),
        track: Some(1),
        ev: Some(0),
        ts: 0,
    };
    // k = 3 of n = 4 in bin 10: (3 + 1) / (4 + 2).
    assert_eq!(cal.p_win(Engine::Legacy, &item(0.5)), 4.0 / 6.0);
    // An empty bin and an unseen engine read 1/2.
    assert_eq!(cal.p_win(Engine::Legacy, &item(0.9)), 0.5);
    assert_eq!(cal.p_win(Engine::Hsmm, &item(0.5)), 0.5);
    assert_eq!(bin(f32::NAN), 0);
    assert_eq!(bin(-0.5), 0);
    assert_eq!(bin(1.0), BINS - 1);
    assert_eq!(bin(0.999), BINS - 1);
    assert_eq!(bin(0.05), 1);
}

#[test]
fn truth_labels_mark_matches_and_alternatives() {
    let legacy = track(1, F_HZ, "W1EW K", 0.5, &[(2, vec![('A', 0.2), ('I', 0.1)])]);
    let labels = truth_labels(&normalize("W1AW K"), &items(&legacy));
    let wins: Vec<bool> = labels
        .iter()
        .filter_map(|l| match l {
            Label::WinChar { right, .. } => Some(*right),
            _ => None,
        })
        .collect();
    assert_eq!(wins, vec![true, true, false, true, true]);
    let alts: Vec<bool> = labels
        .iter()
        .filter_map(|l| match l {
            Label::Alt { right, .. } => Some(*right),
            _ => None,
        })
        .collect();
    assert_eq!(alts, vec![true, false]);
    assert!(labels
        .iter()
        .any(|l| matches!(l, Label::WinSpace { right: true, .. })));
}

#[test]
fn noop_reproduces_the_input() {
    let legacy = track(1, F_HZ, "CQ DE W1EW", 0.3, &[(8, vec![('A', 0.2)])]);
    let hsmm = track(1, F_HZ, "CQ DE W1AW", 0.9, &[]);
    let (l, h, t) = (items(&legacy), items(&hsmm), normalize("CQ DE W1AW"));
    let st = StationInput {
        legacy: &l,
        hsmm: &h,
        truth: &t,
    };
    assert_eq!(
        bytes(&fuse(Policy::Noop, &legacy, &[st], None)),
        bytes(&legacy)
    );
}

#[test]
fn rbn_correction_matches_the_bench_widening() {
    assert_eq!(RBN_CORR_DB, 6.989_7_f32 as f64);
    assert_ne!(RBN_CORR_DB, 6.989_7);
    assert_eq!(MATCH_HZ, 150.0);
}

#[test]
fn regions_never_cross_a_track_separator() {
    let mut legacy = track(1, F_HZ, "CQ", 0.1, &[]);
    legacy.extend(track(2, F_HZ + 50.0, "DE", 0.1, &[]));
    let hsmm = track(5, F_HZ, "CQXDE", 0.9, &[]);
    let (l, h) = (items(&legacy), items(&hsmm));
    assert_eq!(text(&l), "CQ\nDE");
    let rs = regions(&l, &h);
    assert_eq!(rs.len(), 1);
    assert_eq!(chars(&l)[rs[0].l[0]], SEPARATOR);
    // The region holds the separator, so no span policy replaces it.
    let cal = tables(0.1, 0.9);
    for policy in [Policy::OracleSpan, Policy::CalSpan] {
        let out = fused_with(policy, &legacy, &hsmm, "CQXDE", Some(&cal));
        assert_eq!(out, "CQ\nDE", "{policy:?}");
    }
}

#[test]
fn duplicate_branch_changes_nothing_span() {
    let legacy = track(
        1,
        F_HZ,
        "CQ DE W1?XW W1AW KK",
        0.4,
        &[(8, vec![('A', 0.1)])],
    );
    let (l, t) = (items(&legacy), normalize("CQ DE W1AW W1AW K"));
    for cal in [Calibration::default(), tables(0.1, 0.4)] {
        for policy in [Policy::OracleSpan, Policy::CalSpan] {
            let st = StationInput {
                legacy: &l,
                hsmm: &l,
                truth: &t,
            };
            let out = fuse(policy, &legacy, &[st], Some(&cal));
            assert_eq!(bytes(&out), bytes(&legacy), "{policy:?}");
        }
    }
}

#[test]
fn cal_span_rules() {
    let cal = tables(0.1, 0.9);
    // A strictly better HSMM side replaces legacy's, insertions included.
    let legacy = track(1, F_HZ, "W1EEW K", 0.1, &[]);
    let hsmm = track(1, F_HZ, "W1AW K", 0.9, &[]);
    assert_eq!(
        fused_with(Policy::CalSpan, &legacy, &hsmm, "", Some(&cal)),
        "W1AW K"
    );
    // Equal scores keep legacy.
    let legacy = track(1, F_HZ, "W1EEW K", 0.9, &[]);
    assert_eq!(
        fused_with(Policy::CalSpan, &legacy, &hsmm, "", Some(&cal)),
        "W1EEW K"
    );
    // An empty legacy side takes HSMM's characters when they are likely right.
    let legacy = track(1, F_HZ, "W1W K", 0.9, &[]);
    assert_eq!(
        fused_with(Policy::CalSpan, &legacy, &hsmm, "", Some(&cal)),
        "W1AW K"
    );
    // An empty HSMM side deletes legacy's characters only when they are
    // more likely wrong than right, and never a `?`.
    let hsmm = track(1, F_HZ, "W1W K", 0.9, &[]);
    let likely_wrong = track(1, F_HZ, "W1AW K", 0.1, &[]);
    let likely_right = track(1, F_HZ, "W1AW K", 0.9, &[]);
    let unknown = track(1, F_HZ, "W1?W K", 0.1, &[]);
    let fuse_cal = |l: &[DecoderEvent]| fused_with(Policy::CalSpan, l, &hsmm, "", Some(&cal));
    assert_eq!(fuse_cal(&likely_wrong), "W1W K");
    assert_eq!(fuse_cal(&likely_right), "W1AW K");
    assert_eq!(fuse_cal(&unknown), "W1?W K");
}

#[test]
fn span_replacement_keeps_track_timestamps_non_decreasing() {
    let cal = tables(0.1, 0.9);
    let legacy = track(3, F_HZ, "CQ W1EEW K", 0.1, &[]);
    // HSMM's timestamps run far ahead of legacy's: they must be clamped.
    let mut hsmm = track(9, F_HZ, "CQ W1AW K", 0.9, &[]);
    for e in &mut hsmm {
        if let DecoderEvent::CharDecoded { sample_ts, .. } = e {
            *sample_ts *= 50;
        }
    }
    let (l, h, t) = (items(&legacy), items(&hsmm), normalize("CQ W1AW K"));
    let st = StationInput {
        legacy: &l,
        hsmm: &h,
        truth: &t,
    };
    for policy in [Policy::CalSpan, Policy::OracleSpan] {
        let out = fuse(policy, &legacy, &[st_clone(&st)], Some(&cal));
        assert_eq!(text(&items(&out)), "CQ W1AW K", "{policy:?}");
        let ts: Vec<u64> = out
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded {
                    track_id,
                    sample_ts,
                    ..
                }
                | DecoderEvent::WordBoundary {
                    track_id,
                    sample_ts,
                    ..
                } => {
                    assert_eq!(*track_id, 3, "inserted events carry the legacy track");
                    Some(*sample_ts)
                }
                _ => None,
            })
            .collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "{ts:?}");
        assert!(out.iter().all(|e| match e {
            DecoderEvent::CharDecoded { confidence, .. } => *confidence > 0.0,
            _ => true,
        }));
    }
}

fn st_clone<'a>(st: &StationInput<'a>) -> StationInput<'a> {
    StationInput {
        legacy: st.legacy,
        hsmm: st.hsmm,
        truth: st.truth,
    }
}

#[test]
fn oracle_span_prefers_the_closer_side_and_ties_to_legacy() {
    let legacy = track(1, F_HZ, "W1EEW K", 0.9, &[]);
    let hsmm = track(1, F_HZ, "W1AW K", 0.1, &[]);
    assert_eq!(
        fused(Policy::OracleSpan, &legacy, &hsmm, "W1AW K"),
        "W1AW K"
    );
    // HSMM's side is as close (a tie) or farther: keep legacy.
    for side in ["W1IIW K", "W1IIIW K"] {
        let hsmm = track(1, F_HZ, side, 0.9, &[]);
        assert_eq!(
            fused(Policy::OracleSpan, &legacy, &hsmm, "W1AW K"),
            "W1EEW K",
            "{side}"
        );
    }
    let hsmm = track(1, F_HZ, "W1IW K", 0.9, &[]);
    // Legacy already right: unchanged.
    let legacy = track(1, F_HZ, "W1AW K", 0.1, &[]);
    assert_eq!(
        fused(Policy::OracleSpan, &legacy, &hsmm, "W1AW K"),
        "W1AW K"
    );
}

#[test]
fn span_edits_apply_at_their_anchor() {
    let edit = |remove: Vec<usize>, at: usize, before: bool| SpanEdit {
        remove,
        at,
        before,
        track: 1,
        insert: vec![('X', 0.5, 0)],
    };
    let events = track(1, F_HZ, "ABCD", 0.5, &[]);
    // Event 2 ('B') removed by one edit; another inserts after it.
    let out = apply_spans(&events, &[edit(vec![2], 2, true)]);
    assert_eq!(text(&items(&out)), "AXCD");
    // Opposite sides of one anchor are both applied.
    let out = apply_spans(&events, &[edit(vec![], 2, true), edit(vec![], 2, false)]);
    assert_eq!(text(&items(&out)), "AXBXCD");
}

#[test]
fn span_policy_replaces_a_question_mark_only_one_for_one() {
    let cal = tables(0.1, 0.9);
    let legacy = track(1, F_HZ, "W?AW", 0.1, &[]);
    let hsmm = track(1, F_HZ, "W1AW", 0.9, &[]);
    assert_eq!(
        fused_with(Policy::CalSpan, &legacy, &hsmm, "W1AW", Some(&cal)),
        "W1AW"
    );
    let hsmm = track(1, F_HZ, "W12AW", 0.9, &[]);
    assert_eq!(
        fused_with(Policy::CalSpan, &legacy, &hsmm, "W12AW", Some(&cal)),
        "W?AW"
    );
}
