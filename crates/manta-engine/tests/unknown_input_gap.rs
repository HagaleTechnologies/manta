//! MAN-202 Scenario 1: an input gap of unknown length (a SoapySDR driver
//! overflow, `IqSource::take_unknown_gap`) invalidates each track's
//! continuity-dependent decode state instead of being decoded as Morse
//! timing. Measured on V1 (30 s, 20 WPM, "CQ CQ DE W1AW W1AW K" looped):
//! no wrong character, no extra closure, at most two dropped characters
//! per cut; the same cut with nothing reported corrupts the text.

#[path = "support/man202.rs"]
mod support;

use manta_decode::events::{ClosureKind, DecoderEvent};
use support::{
    closures, decode_events, edits, listen_events, listen_inputs_events, skeleton, text, v1_30s,
    Chunked, Cut, Report,
};

const IRREGULAR: &[usize] = &[97, 1500, 4000, 333, 2048, 1, 1999, 777];

#[test]
fn an_unknown_gap_emits_no_wrong_character_and_closes_no_track() {
    let scene = v1_30s();
    let base = listen_events(Chunked::new(&scene, &[2048], None));
    let truth = text(&base);
    assert!(truth.len() > 20, "the uncut scene must decode: {truth:?}");
    let cuts: Vec<(f64, f64)> = [10.0, 12.0, 20.0]
        .into_iter()
        .flat_map(|at| [5.0, 20.0, 60.0, 150.0, 400.0].map(|ms| (at, ms)))
        .collect();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = cuts
            .iter()
            .map(|&(at, ms)| {
                let scene = &scene;
                s.spawn(move || {
                    let cut = Cut::ms(at, ms, scene.fs, Report::Unknown);
                    (
                        (at, ms),
                        listen_events(Chunked::new(scene, &[2048], Some(cut))),
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for ((at, ms), events) in results {
        let got = text(&events);
        let (wrong, missing) = edits(&truth, &got);
        let got: String = got.iter().collect();
        assert_eq!(wrong, 0, "{ms} ms at {at} s: wrong characters in {got:?}");
        assert!(
            missing <= 2,
            "{ms} ms at {at} s: {missing} dropped in {got:?}"
        );
        assert_eq!(
            closures(&events),
            closures(&base),
            "{ms} ms at {at} s: the gap closed a track"
        );
    }
}

#[test]
fn the_unsignalled_gap_still_corrupts_text() {
    let scene = v1_30s();
    let truth = text(&listen_events(Chunked::new(&scene, &[2048], None)));
    let cut = Cut::ms(12.0, 150.0, scene.fs, Report::Silent);
    let got = text(&listen_events(Chunked::new(&scene, &[2048], Some(cut))));
    let (wrong, _) = edits(&truth, &got);
    assert!(
        wrong >= 1,
        "a silent 150 ms splice decoded cleanly; the counterfactual no longer holds"
    );
}

#[test]
fn an_unknown_gap_is_read_schedule_invariant() {
    let scene = v1_30s();
    std::thread::scope(|s| {
        for (at, ms) in [(12.0, 150.0), (16.0, 20.0)] {
            let scene = &scene;
            s.spawn(move || {
                let cut = Cut::ms(at, ms, scene.fs, Report::Unknown);
                let regular = listen_events(Chunked::new(scene, &[2048], Some(cut)));
                let irregular = listen_events(Chunked::new(scene, IRREGULAR, Some(cut)));
                assert_eq!(
                    skeleton(&regular),
                    skeleton(&irregular),
                    "{ms} ms at {at} s"
                );
            });
        }
    });
}

#[test]
fn an_unknown_gap_matches_across_file_and_live_paths() {
    let scene = v1_30s();
    let cut = Cut::ms(12.0, 150.0, scene.fs, Report::Unknown);
    let (file, live, one_input) = std::thread::scope(|s| {
        let scene = &scene;
        let file = s.spawn(move || decode_events(Chunked::new(scene, &[4096], Some(cut))));
        let live = s.spawn(move || listen_events(Chunked::new(scene, &[2048], Some(cut))));
        let one = s.spawn(move || listen_inputs_events(Chunked::new(scene, &[2048], Some(cut))));
        (
            file.join().unwrap(),
            live.join().unwrap(),
            one.join().unwrap(),
        )
    });
    assert_eq!(skeleton(&file), skeleton(&live), "decode_source vs listen");
    assert_eq!(
        skeleton(&live),
        skeleton(&one_input),
        "listen vs listen_inputs"
    );
}

#[test]
fn an_unknown_gap_during_calibration_is_applied_after_the_fill() {
    let scene = v1_30s();
    let truth = text(&listen_events(Chunked::new(&scene, &[2048], None)));
    let cut = Cut::ms(1.0, 150.0, scene.fs, Report::Unknown);
    let got = text(&listen_events(Chunked::new(&scene, &[2048], Some(cut))));
    let (wrong, _) = edits(&truth, &got);
    assert_eq!(wrong, 0, "{:?}", got.iter().collect::<String>());
}

#[test]
fn a_discontinuity_wins_over_an_unknown_gap_on_the_same_read() {
    let scene = v1_30s();
    let gap = (0.15 * scene.fs) as u64;
    let both = Cut::ms(12.0, 150.0, scene.fs, Report::DiscAndUnknown(gap));
    let disc = Cut::ms(12.0, 150.0, scene.fs, Report::Disc(gap));
    let both = listen_events(Chunked::new(&scene, &[2048], Some(both)));
    let disc = listen_events(Chunked::new(&scene, &[2048], Some(disc)));
    assert_eq!(both, disc);
    assert!(
        both.iter().any(|e| matches!(
            e,
            DecoderEvent::TrackClosed {
                closure: ClosureKind::Bookkeeping {
                    survivor_track_id: None
                },
                ..
            }
        )),
        "the discontinuity must rebuild the segment"
    );
}

#[test]
fn an_unknown_gap_pending_at_end_of_stream_closes_as_an_outage() {
    let scene = v1_30s();
    let events = listen_events(Chunked::new(&scene, &[2048], None).with_unknown_gap_at_end());
    let closed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackClosed { closure, .. } => Some(*closure),
            _ => None,
        })
        .collect();
    assert!(!closed.is_empty());
    assert!(
        closed.iter().all(|c| *c
            == ClosureKind::Bookkeeping {
                survivor_track_id: None
            }),
        "{closed:?}"
    );
}
