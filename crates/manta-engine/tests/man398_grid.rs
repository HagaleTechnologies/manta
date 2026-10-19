//! MAN-398 A/B grid: 288 deterministic looped-text cases (24 texts, 12-40
//! WPM, +/-39 kHz, 12-30 dB), CER at 14 s and 30 s plus the 30 s decode, one
//! tab-separated `GRID` row per case. Run once on each build and diff:
//! `cargo test --release -p manta-engine --test man398_grid -- --ignored --nocapture | grep ^GRID`
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

fn spec(text: &str, wpm: f32, off_khz: f64, snr: f32) -> SignalSpec {
    SignalSpec {
        text: text.to_string(),
        loop_text: true,
        wpm,
        offset_hz: off_khz * 1000.0,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    }
}

#[test]
#[ignore]
fn man398_grid() {
    let texts = [
        "AU",
        "HH",
        "5NN",
        "K7X",
        "CQ",
        "TEST",
        "W1AW",
        "TU",
        "73",
        "EE",
        "SS",
        "IS",
        "CQ TEST W5AU",
        "599 001",
        "5NN 0042",
        "E 0000",
        "TEST 000",
        "OO",
        "MO",
        "T0M",
        "R 5NN TU",
        "CQ CQ DE K1ABC",
        "0000",
        "EIS 5",
    ];
    let mut x: u64 = 0xD1B54A32D192ED03;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..288 {
        let tx = texts[i % texts.len()];
        let wpm = 12.0 + (next() % 2800) as f32 / 100.0;
        let mut off = (next() % 7800) as f64 / 100.0 - 39.0;
        if off.abs() < 1.0 {
            off = 5.0;
        }
        let snr = 12.0 + (next() % 1800) as f32 / 100.0;
        let seed = next();
        let s = spec(tx, wpm, off, snr);
        let mut row = format!("GRID {i}\t{tx}\t{wpm:?}\t{off:?}\t{snr:?}\t{seed}");
        for d in [14.0, 30.0] {
            let fs = 96_000.0;
            let (iq, keyed) = render_scene(std::slice::from_ref(&s), fs, d, Some(seed)).unwrap();
            let text = match decode_samples(&iq, fs, 0.0, &PipelineConfig::default()) {
                Ok(r) => r.text,
                Err(_) => String::new(),
            };
            row += &format!("\t{:.4}", cer(&keyed[0], &text));
            if d == 30.0 {
                row += &format!("\t{:?}", text);
            }
        }
        println!("{row}");
    }
}
