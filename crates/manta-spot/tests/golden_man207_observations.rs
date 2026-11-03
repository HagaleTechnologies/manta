//! MAN-207: the validator's observation record -- why each callsign
//! candidate was or was not spotted, kept after its track closes, with
//! later decisions appended as revisions. See
//! docs/DECISIONS/2026-11-03-man207-observation-record.md.

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::decision::{Decision, Reason, Verdict};
use manta_spot::observation::{Cause, Draft, EvidenceLink, Observation};
use manta_spot::{
    CallsignLookup, Existence, LookupStatus, ObservationId, ObservationPolicy, Spot, SpotType,
    Validator,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const FS: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for c in text.chars() {
        events.push(DecoderEvent::char_decoded(
            track_id,
            ts,
            Glyph::Char(c),
            0.95,
        ));
        ts += 100;
    }
    events.push(DecoderEvent::word_boundary(track_id, ts));
    ts += 100;
    (events, ts)
}

fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for word in words {
        let (mut w_events, next_ts) = word_events(track_id, word, ts);
        events.append(&mut w_events);
        ts = next_ts;
    }
    events
}

fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
    events.iter().flat_map(|e| v.ingest(e)).collect()
}

fn seed_meta(v: &mut Validator, track_id: u32) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        sample_ts: 0,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
    });
}

fn close_track(v: &mut Validator, track_id: u32, wpm: f32, closure: ClosureKind) -> Vec<Spot> {
    let mut spots = v.ingest(&DecoderEvent::SpeedUpdate { track_id, wpm });
    spots.extend(v.ingest(&DecoderEvent::TrackClosed { track_id, closure }));
    spots
}

fn observing(policy: ObservationPolicy) -> Validator {
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_observations(policy);
    seed_meta(&mut v, 1);
    v
}

fn decision(o: &Observation) -> &Decision {
    &o.draft.decision
}

fn records_for<'a>(v: &'a Validator, call: &str) -> Vec<&'a Observation> {
    v.observations()
        .iter()
        .filter(|o| o.draft.call() == call)
        .collect()
}

/// Scenario 1: a candidate that fails the repetition gate and is never
/// spotted.
fn once_sent(v: &mut Validator) -> Vec<Spot> {
    let mut spots = run(&transmission_events(1, &["DE", "K5ARH", "K"], 0), v);
    spots.extend(close_track(v, 1, 20.0, ClosureKind::SignalEnded));
    // 30 s of an unrelated track: the clock and the periodic sweeps move on.
    seed_meta(v, 2);
    for i in 0..10u64 {
        spots.extend(run(
            &transmission_events(2, &["QQQ", "RRR"], 300_000 * (i + 1)),
            v,
        ));
    }
    spots
}

#[test]
fn a_once_sent_call_withheld_for_repetition_stays_queryable_after_its_track_closes() {
    let mut v = observing(ObservationPolicy::records());
    assert!(once_sent(&mut v).is_empty());
    assert!(
        v.candidates_for(1, 3_000_000).is_empty(),
        "the track closed"
    );

    let records: Vec<&Observation> = v.observations().for_track(1).collect();
    assert_eq!(records.len(), 1, "{records:#?}");
    let r = records[0];
    let d = decision(r);
    assert_eq!(r.draft.call(), "K5ARH");
    assert_eq!(d.verdict, Verdict::Withheld);
    assert_eq!(d.reason, Some(Reason::Repetition));
    assert_eq!(d.reps, Some(1));
    assert_eq!(d.required_reps, 2);
    assert_eq!(r.draft.cause, Cause::Decided);
    assert_eq!(r.supersedes, None);
    assert_eq!(r.draft.spot_type(), SpotType::De);
    assert_eq!(r.draft.id.track_id, 1);
    assert_eq!(
        r.draft.id.to_string(),
        format!("t1.w{}", r.draft.id.word_seq)
    );
    // The evidence it was rejected on: the one occurrence the gate counted.
    assert_eq!(
        r.draft.evidence,
        vec![EvidenceLink {
            observation: r.draft.id,
            sample_ts: r.draft.span.unwrap().end_ts,
            message_distinct: true,
        }]
    );
    let span = r.draft.span.expect("the word was still in the window");
    assert!(span.start_ts < span.end_ts);
    assert!(span.end_ts <= d.sample_ts);
    let freq = r.draft.freq.expect("the track had metadata");
    assert_eq!((freq.lo_hz, freq.hi_hz), (14_000_000.0, 14_000_000.0));
    assert!(r.draft.scores.word_confidence.is_some());
    assert!(r.draft.scores.call_confidence.is_some());
    assert_eq!(r.draft.source.input, 0);
    assert_eq!(r.draft.source.epoch, 0);
    assert_eq!(r.draft.trace, None, "records detail keeps no text");
}

#[test]
fn a_spot_links_the_earlier_withheld_occurrence_it_was_confirmed_by() {
    let mut v = observing(ObservationPolicy::records());
    let mut spots = run(
        &transmission_events(1, &["CQ", "DE", "K5ARH", "K"], 0),
        &mut v,
    );
    spots.extend(run(
        &transmission_events(1, &["CQ", "DE", "K5ARH", "K"], 300_000),
        &mut v,
    ));
    assert_eq!(spots.len(), 1);
    let records = records_for(&v, "K5ARH");
    let withheld = records
        .iter()
        .find(|o| decision(o).reason == Some(Reason::Repetition))
        .expect("the first copy was withheld");
    let spotted = records
        .iter()
        .find(|o| decision(o).verdict == Verdict::Spotted)
        .expect("the second copy spotted");
    let linked: Vec<ObservationId> = spotted
        .draft
        .evidence
        .iter()
        .map(|e| e.observation)
        .collect();
    assert_eq!(linked, vec![withheld.draft.id, spotted.draft.id]);
    assert_ne!(withheld.draft.id, spotted.draft.id);
    assert_eq!(decision(spotted).spot.as_ref(), Some(&spots[0]));
}

#[test]
fn summary_detail_records_nothing_and_counts_everything() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    seed_meta(&mut v, 1);
    once_sent(&mut v);
    assert!(v.observations().is_empty());
    let counts = v.observation_counts();
    assert_eq!(
        counts.decided(Verdict::Withheld, Some(Reason::Repetition)),
        1
    );
    assert_eq!(counts.total(), 1);
}

type Scenario = fn(&mut Validator) -> Vec<Spot>;

fn twice_cq(v: &mut Validator) -> Vec<Spot> {
    let mut spots = Vec::new();
    for i in 0..3u64 {
        spots.extend(run(
            &transmission_events(1, &["CQ", "DE", "K5ARH", "K"], i * 300_000),
            v,
        ));
    }
    spots
}

fn beacons(v: &mut Validator) -> Vec<Spot> {
    let mut spots = Vec::new();
    for i in 0..2u64 {
        spots.extend(run(
            &transmission_events(1, &["V", "V", "V", "W6DPH", "K"], i * 300_000),
            v,
        ));
    }
    spots.extend(run(
        &transmission_events(1, &["V", "V", "V", "W6DPG", "K"], 900_000),
        v,
    ));
    spots.extend(close_track(v, 1, 22.0, ClosureKind::SignalEnded));
    spots
}

fn variants(v: &mut Validator) -> Vec<Spot> {
    seed_meta(v, 2);
    let mut spots = Vec::new();
    for i in 0..3u64 {
        spots.extend(run(
            &transmission_events(1, &["CQ", "DE", "K5ARH", "K"], i * 300_000),
            v,
        ));
    }
    for i in 0..2u64 {
        spots.extend(run(
            &transmission_events(2, &["CQ", "DE", "K5AR", "K"], i * 300_000),
            v,
        ));
    }
    spots
}

const SCENARIOS: [(&str, Scenario); 4] = [
    ("once_sent", once_sent),
    ("twice_cq", twice_cq),
    ("beacons", beacons),
    ("variants", variants),
];

#[test]
fn the_summary_counts_equal_the_decision_log() {
    for (name, scenario) in SCENARIOS {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.record_decisions(true);
        scenario(&mut v);
        let mut by_reason: BTreeMap<String, u64> = BTreeMap::new();
        for d in v.take_decisions() {
            *by_reason.entry(format!("{:?}", d.reason)).or_default() += 1;
        }
        let counts: BTreeMap<String, u64> = v
            .observation_counts()
            .iter()
            .filter(|&(_, _, n)| n > 0)
            .map(|(_, r, n)| (format!("{r:?}"), n))
            .collect();
        assert_eq!(counts, by_reason, "{name}");
        assert!(!counts.is_empty(), "{name}");
    }
}

#[test]
fn a_settled_beacon_supersedes_its_pending_record() {
    let mut v = observing(ObservationPolicy::records());
    run(
        &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
        &mut v,
    );
    close_track(&mut v, 1, 60.0, ClosureKind::SignalEnded);
    let records = records_for(&v, "K5ARH");
    assert_eq!(records.len(), 2, "{records:#?}");
    let (pending, settled) = (records[0], records[1]);
    assert_eq!(decision(pending).verdict, Verdict::Deferred);
    assert_eq!(decision(pending).reason, Some(Reason::BeaconPending));
    assert_eq!(pending.draft.cause, Cause::Decided);
    assert_eq!(decision(settled).reason, Some(Reason::BeaconSpeed));
    assert_eq!(settled.draft.cause, Cause::Settled);
    assert_eq!(settled.supersedes, Some(pending.seq));
    assert_eq!(settled.draft.id, pending.draft.id);
}

#[test]
fn a_migrated_beacon_keeps_its_origin_observation_id() {
    let mut v = observing(ObservationPolicy::records());
    seed_meta(&mut v, 2);
    run(
        &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
        &mut v,
    );
    v.ingest(&DecoderEvent::TrackClosed {
        track_id: 1,
        closure: ClosureKind::Bookkeeping {
            survivor_track_id: Some(2),
        },
    });
    close_track(&mut v, 2, 60.0, ClosureKind::SignalEnded);
    let records = records_for(&v, "K5ARH");
    assert_eq!(records.len(), 2, "{records:#?}");
    let settled = records[1];
    assert_eq!(decision(settled).reason, Some(Reason::BeaconSpeed));
    assert_eq!(decision(settled).track_id, 2, "decided under the survivor");
    assert_eq!(
        settled.draft.id.track_id, 1,
        "about the origin's occurrence"
    );
    assert_eq!(settled.draft.id, records[0].draft.id);
    assert_eq!(settled.supersedes, Some(records[0].seq));
}

/// Pending on `check`, then reports every checked call as `NotFound`.
#[derive(Default)]
struct SlowNotFound {
    asked: Mutex<Vec<String>>,
}

impl CallsignLookup for SlowNotFound {
    fn check(&self, call: &str) -> LookupStatus {
        self.asked.lock().unwrap().push(call.to_string());
        LookupStatus::Pending
    }

    fn settled(&self) -> Vec<(String, Existence)> {
        std::mem::take(&mut *self.asked.lock().unwrap())
            .into_iter()
            .map(|c| (c, Existence::NotFound))
            .collect()
    }

    fn settle_all(&self) -> Vec<(String, Existence)> {
        self.settled()
    }
}

#[test]
fn a_settled_lookup_supersedes_its_pending_record() {
    let mut v = observing(ObservationPolicy::records());
    v.set_callsign_lookup(Arc::new(SlowNotFound::default()));
    twice_cq(&mut v);
    assert!(v.poll_lookups().is_empty());
    let records = records_for(&v, "K5ARH");
    let pending = records
        .iter()
        .find(|o| decision(o).reason == Some(Reason::LookupPending))
        .expect("held on its lookup");
    let settled = records
        .iter()
        .find(|o| decision(o).reason == Some(Reason::LookupNotFound))
        .expect("settled as not found");
    assert_eq!(decision(pending).verdict, Verdict::Deferred);
    assert_eq!(settled.draft.cause, Cause::Settled);
    assert_eq!(settled.supersedes, Some(pending.seq));
    assert_eq!(settled.draft.id, pending.draft.id);
}

#[test]
fn a_reclassified_word_supersedes_its_first_record() {
    let mut v = observing(ObservationPolicy::records());
    v.allowlist("K5ARH");
    run(&transmission_events(1, &["K5ARH", "UP"], 0), &mut v);
    let records = records_for(&v, "K5ARH");
    assert_eq!(records.len(), 2, "{records:#?}");
    assert_eq!(records[0].draft.spot_type(), SpotType::Unknown);
    assert_eq!(records[0].draft.cause, Cause::Decided);
    assert_eq!(records[1].draft.spot_type(), SpotType::De);
    assert_eq!(records[1].draft.cause, Cause::Reclassified);
    assert_eq!(records[1].supersedes, Some(records[0].seq));
    assert_eq!(records[1].draft.id, records[0].draft.id);
    assert!(
        records[1].draft.evidence.is_empty(),
        "a reclassification makes no new gate call"
    );
}

#[test]
fn alternatives_reach_the_text_trace() {
    let mut v = observing(ObservationPolicy::text());
    let mut ts = 0;
    let mut events = transmission_events(1, &["DE"], ts);
    ts += 1_000;
    for (i, c) in "K5ARH".chars().enumerate() {
        let alternatives = if i == 4 {
            vec![(Glyph::Char('M'), 0.3)]
        } else {
            Vec::new()
        };
        events.push(DecoderEvent::CharDecoded {
            track_id: 1,
            sample_ts: ts,
            glyph: Glyph::Char(c),
            confidence: 0.9,
            alternatives,
        });
        ts += 100;
    }
    events.push(DecoderEvent::word_boundary(1, ts));
    events.extend(transmission_events(1, &["K"], ts + 100));
    run(&events, &mut v);
    let records = records_for(&v, "K5ARH");
    assert_eq!(records.len(), 1, "{records:#?}");
    let trace = records[0].draft.trace.as_ref().expect("text detail");
    assert_eq!(trace.text, "K5ARH");
    assert_eq!(trace.confidences, vec![0.9; 5]);
    assert!(trace.erasures.is_empty());
    assert_eq!(trace.alternatives.len(), 5);
    assert_eq!(trace.alternatives[4], vec![(Glyph::Char('M'), 0.3)]);
    assert!(trace.alternatives[..4].iter().all(Vec::is_empty));
    assert!(!trace.truncated);
}

#[test]
fn records_detail_holds_no_trace() {
    let mut v = observing(ObservationPolicy::records());
    twice_cq(&mut v);
    assert!(!v.observations().is_empty());
    assert!(v.observations().iter().all(|o| o.draft.trace.is_none()));
}

#[test]
fn a_refinement_does_not_touch_the_repetition_gate() {
    let mut v = observing(ObservationPolicy::records());
    once_sent(&mut v);
    let original = v.observations().for_track(1).next().unwrap().clone();
    let gate_total = v.gate_records_total();
    let reps = v.repetition_counts();

    let mut refined: Draft = original.draft.clone();
    refined.decision.call = "K5ARM".to_string();
    let now = 3_300_000;
    let seq = v
        .observations_mut()
        .revise(original.draft.id, refined, now)
        .unwrap();

    assert_eq!(v.gate_records_total(), gate_total);
    assert_eq!(v.repetition_counts(), reps);
    assert_eq!(v.observations().get(original.seq), Some(&original));
    let revision = v.observations().get(seq).unwrap();
    assert_eq!(revision.supersedes, Some(original.seq));
    assert_eq!(revision.draft.cause, Cause::Refined);
    assert_eq!(revision.draft.call(), "K5ARM");
    let history: Vec<u64> = v
        .observations()
        .history(original.draft.id)
        .iter()
        .map(|o| o.seq)
        .collect();
    assert_eq!(history, vec![original.seq, seq]);
}

#[test]
fn observation_detail_never_changes_spots() {
    for (name, scenario) in SCENARIOS {
        let spots_at = |policy: Option<ObservationPolicy>, decisions: bool| {
            let mut v = Validator::new(FS, CTY_FIXTURE, None);
            if let Some(policy) = policy {
                v = v.with_observations(policy);
            }
            v.record_decisions(decisions);
            seed_meta(&mut v, 1);
            let spots = scenario(&mut v);
            (spots, v.observation_counts())
        };
        let (baseline, counts) = spots_at(None, false);
        for policy in [
            ObservationPolicy::summary(),
            ObservationPolicy::records(),
            ObservationPolicy::text(),
        ] {
            for decisions in [false, true] {
                let (spots, c) = spots_at(Some(policy), decisions);
                assert_eq!(
                    spots, baseline,
                    "{name} at {policy:?}, decisions {decisions}"
                );
                assert_eq!(c, counts, "{name} at {policy:?}, decisions {decisions}");
            }
        }
    }
}
