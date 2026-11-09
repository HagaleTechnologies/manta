//! MAN-515: refinement replays only where the engine flip can change a
//! word. A legacy or edge-legacy base never flips to HSMM and replays its
//! seeded timing once per stable speed; an HSMM base keeps the MAN-208
//! ladder (legacy flip, then seeded timing). The outcome counters report
//! the share of replays that changed a word.

#[path = "support/refine_scene.rs"]
mod support;

use manta_decode::decoder::Engine;
use manta_decode::events::DecoderEvent;
use manta_spot::observation::Cause;
use support::{causal_events, scene};

/// Three scheduler passes over an episode whose replay can never resolve
/// it (no repetition, the retained waveform is the causal reading), with
/// later words between the first and second pass, so the MAN-208
/// `later_word` signal would re-admit it. Returns the scene.
fn three_passes_with_later_words(engine: Engine) -> support::Scene {
    let mut s = scene("K3ABC", "K3ABC", engine, true, false);
    for i in 0..3u64 {
        if i == 1 {
            s.later_words(&["TU", "EE"], s.span.1 + 60_000);
        }
        s.step(s.span.1 + 50_000 + i * 32_000);
    }
    s
}

#[test]
fn a_legacy_base_never_flips_the_engine() {
    for engine in [Engine::Legacy, Engine::EdgeLegacy] {
        let s = three_passes_with_later_words(engine);
        let c = s.scheduler.counters();
        assert_eq!(c.engine_flips_total, 0, "{engine:?}: {c:?}");
        assert_eq!(
            (c.admitted_total, c.replays_total),
            (1, 1),
            "{engine:?}: a later word alone never re-admits a flip-less episode: {c:?}"
        );
        let refined: Vec<_> = s
            .validator
            .observations()
            .iter()
            .filter(|o| o.draft.cause == Cause::Refined)
            .collect();
        assert!(!refined.is_empty(), "{engine:?}: the replay was applied");
        for o in refined {
            let trace = o.draft.trace.as_ref().expect("text detail keeps a trace");
            assert!(
                trace.context.iter().any(|w| w == "wpm-seed"),
                "{engine:?}: the replay is the seeded timing, not a flip: {:?}",
                trace.context
            );
        }
    }
    // The control: the same scene on an HSMM base stays on the MAN-208
    // ladder, so the later word re-admits it for its second rung.
    let s = three_passes_with_later_words(Engine::Hsmm);
    let c = s.scheduler.counters();
    assert_eq!(c.engine_flips_total, 1, "{c:?}");
    assert_eq!(c.admitted_total, 2, "{c:?}");
}

#[test]
fn a_legacy_base_reseeds_only_on_a_new_stable_speed() {
    let mut s = scene("K3ABC", "K3ABC", Engine::Legacy, true, false);
    s.step(s.span.1 + 50_000);
    assert_eq!(s.scheduler.counters().replays_total, 1);
    // A speed 4 WPM away, held across `STABLE_SPEED_WORDS` words.
    s.validator.ingest(&DecoderEvent::SpeedUpdate {
        track_id: 1,
        wpm: 29.0,
    });
    s.later_words(&["TU", "EE"], s.span.1 + 60_000);
    s.step(s.span.1 + 82_000);
    let c = *s.scheduler.counters();
    assert_eq!(c.replays_total, 2, "a new stable speed re-admits: {c:?}");
    assert_eq!(c.engine_flips_total, 0, "{c:?}");
    // More words at the same speed add nothing.
    s.later_words(&["TU", "EE"], s.span.1 + 90_000);
    s.step(s.span.1 + 114_000);
    assert_eq!(*s.scheduler.counters(), c, "no new speed, no new replay");
}

#[test]
fn an_hsmm_base_still_flips_first() {
    let mut s = scene("K3ABD", "K3ABC", Engine::Hsmm, true, true);
    assert!(s.causal_spots.is_empty(), "{:?}", s.causal_spots);
    let spots = s.step(s.rep_boundary + 32_000);
    assert_eq!(
        spots
            .iter()
            .map(|sp| sp.callsign.as_str())
            .collect::<Vec<_>>(),
        ["K3ABD"]
    );
    let c = s.scheduler.counters();
    assert_eq!(c.engine_flips_total, 1, "{c:?}");
    assert_eq!(c.rescues_total, 1, "{c:?}");
}

#[test]
fn counters_report_the_share_of_replays_that_change_a_word() {
    // A replay that corrects the word.
    let mut s = scene("K3ABD", "K3ABC", Engine::Hsmm, true, true);
    s.step(s.rep_boundary + 32_000);
    let c = s.scheduler.counters();
    assert_eq!(
        (c.replays_total, c.changed_total, c.rejected_total),
        (1, 1, 0),
        "{c:?}"
    );
    // A replay that reads the word it already had.
    let mut s = scene("K3ABC", "K3ABC", Engine::Hsmm, true, false);
    s.step(s.span.1 + 50_000);
    let c = s.scheduler.counters();
    assert_eq!(c.replays_total, 1, "{c:?}");
    assert_eq!(c.changed_total, 0, "{c:?}");
    assert!(c.changed_total + c.rejected_total <= c.replays_total);
    // A replay that would turn the spottable call into garbage (the
    // `N3NXI` -> `EEETT` shape): refused and counted, and the call's own
    // later clean repetition still spots it.
    let mut s = scene("EEETT", "K3ABD", Engine::Hsmm, true, false);
    s.step(s.span.1 + 50_000);
    let c = *s.scheduler.counters();
    assert_eq!(c.replays_total, 1, "{c:?}");
    assert_eq!((c.changed_total, c.rejected_total), (0, 1), "{c:?}");
    assert!(c.changed_total + c.rejected_total <= c.replays_total);
    let rep_base = 1_000_000 + 768_000;
    let (rep, _) = causal_events(1, "K3ABD", rep_base, rep_base, false);
    let spots: Vec<_> = rep.iter().flat_map(|ev| s.validator.ingest(ev)).collect();
    assert_eq!(
        spots
            .iter()
            .map(|sp| sp.callsign.as_str())
            .collect::<Vec<_>>(),
        ["K3ABD"],
        "the refused reading took no support from the call"
    );
}

#[test]
fn refinement_is_deterministic_under_the_gate() {
    let run = || {
        let mut s = scene("K3ABD", "K3ABC", Engine::Legacy, true, true);
        let spots = s.step(s.rep_boundary + 32_000);
        (
            spots
                .into_iter()
                .map(|sp| {
                    (
                        sp.callsign,
                        sp.sample_ts,
                        sp.freq_hz.to_bits(),
                        sp.confidence.to_bits(),
                        sp.wpm.to_bits(),
                    )
                })
                .collect::<Vec<_>>(),
            *s.scheduler.counters(),
        )
    };
    let (spots, counters) = run();
    assert_eq!(counters.engine_flips_total, 0);
    assert_eq!(
        run(),
        (spots, counters),
        "identical input explores an identical hypothesis set"
    );
}
