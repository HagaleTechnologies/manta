//! MAN-112: two independent signals must keep distinct, accurate tracks.
mod common;
use common::decode::{decode_report, per_track, promoted_track_ids};

fn check(a: f64, spacing: f64, sa: f64, sb: f64) {
    let spec = manta_testkit::vectors::close_pair("close_pair", a, spacing, sa, sb);
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    assert_eq!(promoted_track_ids(&report).len(), 2, "promoted census");
    assert_eq!(tracks.len(), 2, "decoded census");
    let mut matched = std::collections::BTreeSet::new();
    for (i, text) in manifest.keyed_texts.iter().enumerate() {
        let expected = manifest.expected_freqs_hz[i];
        let (id, (decoded, freq)) = tracks
            .iter()
            .min_by(|a, b| {
                (a.1 .1.unwrap_or(f64::MAX) - expected)
                    .abs()
                    .total_cmp(&(b.1 .1.unwrap_or(f64::MAX) - expected).abs())
            })
            .unwrap();
        assert!(matched.insert(id), "each signal needs its own track");
        let cer = manta_testkit::cer::cer(text, decoded);
        assert!(cer <= 0.05, "signal {i}: CER {cer}; {decoded:?}");
        assert!(
            (freq.unwrap() - expected).abs() <= 15.0,
            "signal {i}: frequency {freq:?} expected {expected}"
        );
    }
}
#[test]
fn close_pair_on_bin_150() {
    check(10_031.25, 150.0, 15.0, 15.0);
}
#[test]
fn close_pair_quarter_bin_125() {
    check(10_054.687_5, 125.0, 15.0, 15.0);
}
#[test]
fn close_pair_on_bin_100() {
    check(10_031.25, 100.0, 15.0, 15.0);
}
#[test]
fn close_pair_mid_bin_150() {
    check(10_078.125, 150.0, 15.0, 15.0);
}
#[test]
fn close_pair_strong_weak_on_bin_150() {
    check(10_031.25, 150.0, 25.0, 10.0);
}
#[test]
fn close_pair_weak_strong_mid_bin_200() {
    check(10_078.125, 200.0, 10.0, 25.0);
}
#[test]
#[ignore = "MAN-112 frontend not promoted: mid-bin 100 Hz fails the copy gate (promotion criterion 4)"]
fn close_pair_mid_bin_100() {
    check(10_078.125, 100.0, 15.0, 15.0);
}

// The original six scenes missed this F1 regression: max-power phase
// tracking biased the weak mid-bin carrier toward the strong neighbor.
#[test]
fn close_pair_weak_strong_mid_bin_150() {
    check(10_078.125, 150.0, 10.0, 25.0);
}

#[test]
fn close_pair_mid_bin_125() {
    check(10_078.125, 125.0, 15.0, 15.0);
}
