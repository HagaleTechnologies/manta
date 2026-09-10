use manta_testkit::oracle::{run_oracle, score_text, OracleSpot};
use manta_testkit::scene::{render_scene, SignalSpec};

#[test]
fn score_text_matches_the_scorer_definitions() {
    assert_eq!(
        score_text("NJ3K", "CQ TEST NJ3K CQ TEST NJ3K"),
        (true, true, true)
    );
    assert_eq!(score_text("NJ3K", "CQTESTNJ3K"), (false, false, true));
    assert_eq!(score_text("NJ3K", "TEST NJ3K"), (true, true, true));
    assert_eq!(score_text("NJ3K", "DE NJ3K"), (true, true, true));
    assert_eq!(score_text("NJ3K", "NJ3K"), (true, false, true));
    assert_eq!(score_text("NJ3K", "CQ TEST NJ3N"), (false, false, false));
}

#[test]
fn oracle_recovers_a_synthetic_station_at_its_rbn_khz() {
    let sig = SignalSpec {
        text: "CQ TEST W5AU W5AU TEST".into(),
        loop_text: true,
        wpm: 30.0,
        offset_hz: 12_340.0,
        snr_2500_db: 25.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (iq, _) = render_scene(&[sig], 96_000.0, 60.0, Some(1)).unwrap();
    let spots = vec![OracleSpot {
        call: "W5AU".into(),
        khz: 14_012.3,
        t_sec: 20.0,
        snr_db: 25,
        wpm: 30,
    }];
    let (results, summary) = run_oracle(
        &iq,
        96_000.0,
        14_000_000.0,
        &spots,
        40.0,
        &Default::default(),
    )
    .unwrap();
    assert!(results[0].as_word, "text: {}", results[0].text);
    assert_eq!(summary.as_word, 1);
}
