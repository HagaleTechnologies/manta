//! MAN-202 Scenario 2: the same IQ, delivered once as a file replay and
//! once as a live capture with irregular chunk boundaries, gives the same
//! `TrackPromoted`/`CharDecoded`/`WordBoundary`/`TrackClosed` sequence --
//! without a gap, and across an unknown-length input gap. The full event
//! stream is not compared: a `CharDecoded` can land on either side of a
//! batch boundary from a later-stamped `TrackMeta`/`TimingUpdate`, a
//! pre-existing placement effect (follow-up 8 in the MAN-202 record).

#[path = "support/man202.rs"]
mod support;

use manta_input::{RawIqFormat, RawIqSource};
use support::{
    decode_events, listen_events, listen_inputs_events, skeleton, two_signal_30s, v1_30s,
    write_cf32, Chunked, Cut, Report,
};

const SCHEDULES: [&[usize]; 3] = [
    &[2048],
    &[97, 1500, 4000, 333, 2048, 1, 1999, 777],
    &[5, 3000, 12, 2047, 1024],
];

#[test]
fn file_replay_and_live_reads_give_the_same_semantic_events() {
    let scenes = [v1_30s(), two_signal_30s()];
    std::thread::scope(|s| {
        for (i, scene) in scenes.iter().enumerate() {
            s.spawn(move || {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(format!("scene{i}.cf32"));
                write_cf32(&path, &scene.samples);
                let file = decode_events(
                    RawIqSource::open(&path, RawIqFormat::Cf32, scene.fs, scene.center_freq_hz)
                        .unwrap(),
                );
                let want = skeleton(&file);
                assert!(
                    want.iter().any(|e| e.starts_with('C')),
                    "scene {i} must decode"
                );
                for schedule in SCHEDULES {
                    let live = listen_events(Chunked::new(scene, schedule, None));
                    assert_eq!(want, skeleton(&live), "scene {i}, schedule {schedule:?}");
                }
                let one_input = listen_inputs_events(Chunked::new(scene, &[2048], None));
                assert_eq!(want, skeleton(&one_input), "scene {i}, listen_inputs");
            });
        }
    });
}

#[test]
fn file_replay_and_live_reads_match_across_an_unknown_gap() {
    let scenes = [v1_30s(), two_signal_30s()];
    std::thread::scope(|s| {
        for (i, scene) in scenes.iter().enumerate() {
            s.spawn(move || {
                let cut = Cut::ms(12.0, 150.0, scene.fs, Report::Unknown);
                // A cf32 file cannot carry a gap, so the file side is
                // `decode_source` over an in-memory source in its own
                // 4096-sample reads.
                let file = decode_events(Chunked::new(scene, &[4096], Some(cut)));
                let want = skeleton(&file);
                assert!(want.iter().any(|e| e.starts_with('C')));
                for schedule in &SCHEDULES[..2] {
                    let live = listen_events(Chunked::new(scene, schedule, Some(cut)));
                    assert_eq!(want, skeleton(&live), "scene {i}, schedule {schedule:?}");
                }
            });
        }
    });
}
