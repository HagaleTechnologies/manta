//! SPEC-decode-core.md §7.1 V59 to V65 (MAN-200): the same samples count
//! once, and with `[spot] repetition_fusion` damaged copies of one
//! station's self-ID can together confirm its call. Event-level vectors at
//! the `Validator::ingest` contract, like V53 to V57.

use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_spot::{
    Confirmation, Decision, EmissionState, Reason, Spot, SpotType, Validator, Verdict,
};

const FS: f64 = 96_000.0;
const F: f64 = 14_000_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

/// One word: its characters at `char_ts` (confidence 0.95 unless `alt`
/// names a position, which then reads at 0.45 with that alternative at
/// 0.40), then its boundary.
fn word_at(
    track_id: u32,
    text: &str,
    char_ts: &[u64],
    boundary_ts: u64,
    alt: Option<(usize, char)>,
) -> Vec<DecoderEvent> {
    let mut events: Vec<DecoderEvent> = text
        .chars()
        .zip(char_ts)
        .enumerate()
        .map(|(i, (c, &ts))| match alt {
            Some((pos, a)) if pos == i => char_with_alts(track_id, ts, c, 0.45, &[(a, 0.40)]),
            _ => char_with_alts(track_id, ts, c, 0.95, &[]),
        })
        .collect();
    events.push(DecoderEvent::word_boundary(track_id, boundary_ts));
    events
}

fn char_with_alts(
    track_id: u32,
    sample_ts: u64,
    c: char,
    confidence: f32,
    alts: &[(char, f32)],
) -> DecoderEvent {
    DecoderEvent::CharDecoded {
        track_id,
        sample_ts,
        glyph: Glyph::Char(c),
        confidence: if c == '?' { 0.0 } else { confidence },
        alternatives: alts.iter().map(|&(g, a)| (Glyph::Char(g), a)).collect(),
    }
}

/// `CQ <call> K` starting at `start`, 100 samples per character, with an
/// optional alternative on one character of the call.
fn cq(track_id: u32, call: &str, start: u64, alt: Option<(usize, char)>) -> Vec<DecoderEvent> {
    message(track_id, &["CQ", call, "K"], 1, start, alt)
}

fn message(
    track_id: u32,
    words: &[&str],
    alt_word: usize,
    start: u64,
    alt: Option<(usize, char)>,
) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    let mut ts = start;
    for (w, text) in words.iter().enumerate() {
        let char_ts: Vec<u64> = (0..text.len() as u64).map(|i| ts + i * 100).collect();
        let boundary = ts + text.len() as u64 * 100;
        events.extend(word_at(
            track_id,
            text,
            &char_ts,
            boundary,
            if w == alt_word { alt } else { None },
        ));
        ts = boundary + 100;
    }
    events
}

fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
    events.iter().flat_map(|e| v.ingest(e)).collect()
}

fn meta(v: &mut Validator, track_id: u32, freq_hz: f64) {
    v.ingest(&DecoderEvent::TrackMeta {
        track_id,
        sample_ts: 0,
        snr_2500_db: 20.0,
        freq_hz,
        last_char_ts: 0,
    });
}

fn close_track(v: &mut Validator, track_id: u32) -> Vec<Spot> {
    let mut spots = v.ingest(&DecoderEvent::SpeedUpdate {
        track_id,
        wpm: 22.0,
    });
    spots.extend(v.ingest(&DecoderEvent::TrackClosed {
        track_id,
        closure: ClosureKind::SignalEnded,
    }));
    spots
}

fn validator(fusion: bool) -> Validator {
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_repetition_fusion(fusion);
    v.record_decisions(true);
    meta(&mut v, 1, F);
    v
}

/// Runs `messages` on track 1, closes it, and returns the spots and decisions.
fn scenario(fusion: bool, messages: &[Vec<DecoderEvent>]) -> (Vec<Spot>, Vec<Decision>) {
    let mut v = validator(fusion);
    let mut spots = Vec::new();
    for m in messages {
        spots.extend(run(m, &mut v));
    }
    spots.extend(close_track(&mut v, 1));
    (spots, v.take_decisions())
}

fn calls(spots: &[Spot]) -> Vec<&str> {
    spots.iter().map(|s| s.callsign.as_str()).collect()
}

fn named<'a>(decisions: &'a [Decision], call: &str) -> Vec<&'a Decision> {
    decisions.iter().filter(|d| d.call == call).collect()
}

const ALT_W: Option<(usize, char)> = Some((3, 'W'));

#[test]
fn v59_a_replay_of_the_same_samples_is_not_a_repetition() {
    let first = [
        word_at(1, "CQ", &[10, 20], 30, None),
        word_at(1, "W1AW", &[100, 200, 300, 350], 400, None),
        word_at(1, "K", &[405], 408, None),
    ]
    .concat();
    let replay = [
        word_at(1, "CQ", &[110, 120], 130, None),
        word_at(1, "W1AW", &[150, 250, 350, 400], 420, None),
        word_at(1, "K", &[425], 430, None),
    ]
    .concat();
    for fusion in [false, true] {
        let (spots, decisions) = scenario(fusion, &[first.clone(), replay.clone()]);
        assert!(spots.is_empty(), "fusion {fusion}: {spots:?}");
        let w1aw = named(&decisions, "W1AW");
        assert_eq!(w1aw.len(), 2, "{w1aw:?}");
        for d in w1aw {
            assert_eq!((d.reason, d.reps), (Some(Reason::Repetition), Some(1)));
        }
    }
    // Control: the same second message 100 000 samples later is a repetition.
    let later = cq(1, "W1AW", 100_000, None);
    let (spots, decisions) = scenario(false, &[first, later]);
    assert_eq!(calls(&spots), vec!["W1AW"]);
    assert_eq!(spots[0].emission_state, EmissionState::Confirmed);
    let spotted = named(&decisions, "W1AW")
        .into_iter()
        .find(|d| d.verdict == Verdict::Spotted)
        .expect("spotted");
    assert_eq!(spotted.reps, Some(2));
    assert_eq!(spotted.confirmation, Some(Confirmation::ExactRepeat));
}

#[test]
fn v60_complementary_copies_confirm_the_call_both_list() {
    let messages = [cq(1, "W1AV", 0, ALT_W), cq(1, "W1AQ", 100_000, ALT_W)];
    let (spots, decisions) = scenario(true, &messages);
    assert_eq!(calls(&spots), vec!["W1AW"]);
    let spot = &spots[0];
    assert_eq!(spot.spot_type, SpotType::Cq);
    assert_eq!(spot.emission_state, EmissionState::Confirmed);
    // CQ (300) then W1AQ's boundary 400 samples after its first character.
    assert_eq!(spot.sample_ts, 100_000 + 300 + 400);
    let order: Vec<(&str, Verdict, Option<u32>, Option<Confirmation>)> = decisions
        .iter()
        .filter(|d| ["W1AV", "W1AQ", "W1AW"].contains(&d.call.as_str()))
        .map(|d| (d.call.as_str(), d.verdict, d.reps, d.confirmation))
        .collect();
    assert_eq!(
        order,
        vec![
            ("W1AV", Verdict::Withheld, Some(1), None),
            ("W1AQ", Verdict::Withheld, Some(1), None),
            ("W1AW", Verdict::Spotted, Some(2), Some(Confirmation::Fused)),
        ]
    );

    let (spots, _) = scenario(false, &messages);
    assert!(spots.is_empty(), "{spots:?}");
}

#[test]
fn v61_top1_copies_fuse_by_majority_and_align_indels() {
    let two = [cq(1, "W1AV", 0, None), cq(1, "W1EW", 100_000, None)];
    let (spots, _) = scenario(true, &two);
    assert!(spots.is_empty(), "{spots:?}");

    let three = [
        cq(1, "W1AV", 0, None),
        cq(1, "W1EW", 100_000, None),
        cq(1, "M1AW", 200_000, None),
    ];
    let (spots, decisions) = scenario(true, &three);
    assert_eq!(calls(&spots), vec!["W1AW"]);
    let m1aw = named(&decisions, "M1AW");
    assert_eq!(m1aw.len(), 1);
    assert_eq!(m1aw[0].reason, Some(Reason::Cty));
    let w1aw = named(&decisions, "W1AW");
    assert_eq!(w1aw.len(), 1);
    assert_eq!(w1aw[0].confirmation, Some(Confirmation::Fused));
    assert_eq!(w1aw[0].reps, Some(3));
    let (spots, _) = scenario(false, &three);
    assert!(spots.is_empty(), "{spots:?}");

    let indels = [
        cq(1, "W1A", 0, None),
        cq(1, "W1AWE", 100_000, None),
        cq(1, "W1AW", 200_000, None),
    ];
    let (spots, decisions) = scenario(true, &indels);
    assert_eq!(calls(&spots), vec!["W1AW"]);
    let w1aw = named(&decisions, "W1AW");
    assert_eq!(
        w1aw.len(),
        1,
        "no Repetition decision for the third word: {w1aw:?}"
    );
    assert_eq!(w1aw[0].verdict, Verdict::Spotted);
    assert_eq!(w1aw[0].confirmation, Some(Confirmation::Fused));
}

#[test]
fn v62_fusion_abstains_when_the_copies_do_not_decide() {
    let (spots, decisions) = scenario(
        true,
        &[cq(1, "W1AV", 0, None), cq(1, "W1AQ", 100_000, None)],
    );
    assert!(spots.is_empty(), "{spots:?}");
    assert!(named(&decisions, "W1AW").is_empty());

    let (spots, _) = scenario(
        true,
        &[cq(1, "W1A?", 0, None), cq(1, "W1AV", 100_000, None)],
    );
    assert!(spots.is_empty(), "{spots:?}");

    // A wrong repeated reading spots exactly as with fusion off.
    let twice = [cq(1, "W1AV", 0, None), cq(1, "W1AV", 100_000, None)];
    let (on, decisions) = scenario(true, &twice);
    let (off, _) = scenario(false, &twice);
    assert_eq!(on, off);
    assert_eq!(calls(&on), vec!["W1AV"]);
    let spotted = named(&decisions, "W1AV")
        .into_iter()
        .find(|d| d.verdict == Verdict::Spotted)
        .expect("spotted");
    assert_eq!(spotted.confirmation, Some(Confirmation::ExactRepeat));
}

#[test]
fn v63_only_self_ids_feed_fusion() {
    let addressed = [
        message(1, &["W1AV", "DE", "K1ABC", "K"], 0, 0, ALT_W),
        message(1, &["W1AQ", "DE", "K1ABC", "K"], 0, 100_000, ALT_W),
    ];
    let (spots, decisions) = scenario(true, &addressed);
    assert_eq!(calls(&spots), vec!["K1ABC"]);
    assert_eq!(spots[0].spot_type, SpotType::De);
    assert!(named(&decisions, "W1AW").is_empty());

    let mentioned = [
        message(1, &["DE", "K1ABC", "OP", "W1AV", "K"], 3, 0, ALT_W),
        cq(1, "W1AQ", 100_000, ALT_W),
    ];
    let (spots, decisions) = scenario(true, &mentioned);
    assert!(calls(&spots).iter().all(|c| *c != "W1AW"), "{spots:?}");
    assert!(named(&decisions, "W1AW").is_empty());
}

#[test]
fn v64_the_same_samples_are_one_observation() {
    for offset in [0.0, 60.0] {
        let mut v = validator(true);
        meta(&mut v, 2, F + offset);
        let mut spots = run(&cq(1, "W1AV", 0, ALT_W), &mut v);
        spots.extend(run(&cq(2, "W1AQ", 50, ALT_W), &mut v));
        spots.extend(close_track(&mut v, 1));
        spots.extend(close_track(&mut v, 2));
        assert!(
            calls(&spots).iter().all(|c| *c != "W1AW"),
            "{offset}: {spots:?}"
        );
    }
    // A copy 300 Hz away is another channel's episode.
    let mut v = validator(true);
    meta(&mut v, 3, F + 300.0);
    let mut spots = run(&cq(1, "W1AV", 0, ALT_W), &mut v);
    spots.extend(run(&cq(3, "W1AQ", 100_000, ALT_W), &mut v));
    spots.extend(close_track(&mut v, 1));
    spots.extend(close_track(&mut v, 3));
    assert!(calls(&spots).iter().all(|c| *c != "W1AW"), "{spots:?}");
    // A same-track replay of the same samples.
    let (spots, _) = scenario(true, &[cq(1, "W1AV", 0, ALT_W), cq(1, "W1AQ", 50, ALT_W)]);
    assert!(calls(&spots).iter().all(|c| *c != "W1AW"), "{spots:?}");
}

#[test]
fn v65_the_tier_bar_and_a_spotted_copy_still_hold() {
    let exact = |n: u64| -> Vec<Vec<DecoderEvent>> {
        (0..n).map(|i| cq(1, "AB2KLK", i * 100_000, None)).collect()
    };
    let mut three = exact(3);
    three.push(cq(1, "AB2KLX", 300_000, None));
    let (spots, _) = scenario(true, &three);
    assert!(spots.is_empty(), "{spots:?}");

    let mut four = exact(4);
    four.push(cq(1, "AB2KLX", 400_000, None));
    let (spots, decisions) = scenario(true, &four);
    assert_eq!(calls(&spots), vec!["AB2KLK"]);
    assert_eq!(spots[0].sample_ts, 400_000 + 300 + 600);
    let spotted = named(&decisions, "AB2KLK")
        .into_iter()
        .find(|d| d.verdict == Verdict::Spotted)
        .expect("spotted");
    assert_eq!(spotted.reps, Some(5));
    assert_eq!(spotted.confirmation, Some(Confirmation::Fused));

    // K1AV already spotted here, so it is a station, not damage.
    let s = FS as u64;
    let messages = [
        cq(1, "K1AV", 0, None),
        cq(1, "K1AV", 70 * s, None),
        cq(1, "W1AW", 140 * s, None),
        cq(1, "W1AV", 141 * s, None),
    ];
    let (spots, _) = scenario(true, &messages);
    assert_eq!(calls(&spots), vec!["K1AV"]);
}
