//! MAN-372 measurement harness: SSB voice residual tracks under the
//! non-CW filter, the CW-next-to-SSB coexistence scene, and the real-CW
//! control scenes, plus the `measure` mode that produced the MAN-372
//! decision record's tables. Kept (D7): it renders every scene the
//! non-CW gates use — the tool for the next non-CW ticket.
//!
//! Run with `MANTA_MAN372_DIAG=1` for the per-evaluation qualification
//! trace on stderr; `measure` sets it itself.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::noise::amplitude_for_snr_2500;
use manta_testkit::noncw::add_ssb_voice;
use manta_testkit::scene::{render_scene, SignalSpec, MASTER_SCALE};
use num_complex::Complex32;

const FS: f64 = 96_000.0;
const NOISE_SEED: u64 = 11;
const CENTER: f64 = 14_000_000.0;
const VOICE_SEED: u64 = 1;
const CQ: &str = "CQ CQ DE K5ARH K5ARH K";

fn scene(cws: &[SignalSpec], duration_s: f64) -> Vec<Complex32> {
    render_scene(cws, FS, duration_s, Some(NOISE_SEED))
        .unwrap()
        .0
}

fn cw(offset_hz: f64, snr: f32, wpm: f32, rise_ms: f64) -> SignalSpec {
    SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: CQ.into(),
        loop_text: true,
        wpm,
        offset_hz,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms,
    }
}

fn ssb_scene(snr: f32, duration_s: f64) -> Vec<Complex32> {
    let mut iq = scene(&[], duration_s);
    add_ssb_voice(&mut iq, FS, 40_000.0, snr, VOICE_SEED, 0.0);
    iq
}

fn report(name: &str, iq: &[Complex32], cfg: &PipelineConfig) {
    let lo = CENTER + 40_200.0;
    let hi = CENTER + 42_900.0;
    match decode_samples(iq, FS, CENTER, cfg) {
        Ok(r) => {
            let mut in_band = 0usize;
            let mut events: Vec<(u32, u64, f64)> = r
                .events
                .iter()
                .filter_map(|e| match e {
                    manta_decode::events::DecoderEvent::TrackPromoted {
                        track_id,
                        sample_ts,
                        freq_hz,
                    } => Some((*track_id, *sample_ts, *freq_hz)),
                    _ => None,
                })
                .collect();
            events.sort_by_key(|&(_, ts, _)| ts);
            for (id, ts, f) in &events {
                if *f >= lo && *f <= hi {
                    in_band += 1;
                }
                println!(
                    "{name}: promoted id={id} t={:.3}s freq={:.1}{}",
                    *ts as f64 / FS,
                    f,
                    if *f >= lo && *f <= hi { " IN-BAND" } else { "" }
                );
            }
            println!(
                "{name}: {} TrackPromoted total, {in_band} in band [{lo:.0}, {hi:.0}], {} spots: {:?}",
                events.len(),
                r.spots.len(),
                r.spots
                    .iter()
                    .map(|s| format!("{}@{:.0}", s.callsign, s.freq_hz))
                    .collect::<Vec<_>>()
            );
            println!("{name}: text {:?}", r.text);
        }
        Err(e) => println!("{name}: Err({e})"),
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "ssb".into());
    match mode.as_str() {
        // MAN-372 Phase 1 go/no-go measurement: run the SSB and control
        // scenes with the per-evaluation band-feature trace on (stderr),
        // plus each scene's promotions (stdout). `MANTA_MAN372_DIAG` is
        // read once per process, so it must be set before the first
        // decode. Scene names select a subset; default is all of them.
        "measure" => {
            std::env::set_var("MANTA_MAN372_DIAG", "1");
            let want: Vec<String> = std::env::args().skip(2).collect();
            let all = [
                "ssb30", "ssb45", "coexist", "weakrtty", "loudcw", "cw", "v8w",
            ];
            for name in all {
                if !want.is_empty() && !want.iter().any(|w| w == name) {
                    continue;
                }
                eprintln!("=== scene {name} ===");
                match name {
                    "ssb30" => report(
                        "ssb+30 filter-on",
                        &ssb_scene(30.0, 40.0),
                        &PipelineConfig::default(),
                    ),
                    "ssb45" => report(
                        "ssb+45 filter-on",
                        &ssb_scene(45.0, 40.0),
                        &PipelineConfig::default(),
                    ),
                    "coexist" => {
                        let mut iq = scene(&[cw(43_000.0, 15.0, 20.0, 5.0)], 40.0);
                        add_ssb_voice(&mut iq, FS, 40_000.0, 30.0, VOICE_SEED, 0.0);
                        report("coexist filter-on", &iq, &PipelineConfig::default());
                    }
                    "weakrtty" => {
                        use manta_testkit::fsk::{add_fsk, FskSpec};
                        let mut iq = scene(&[cw(12_340.0, 10.0, 25.0, 5.0)], 40.0);
                        add_fsk(
                            &mut iq,
                            FS,
                            &FskSpec {
                                mark_below_space: false,
                                mark_offset_hz: 12_040.0,
                                shift_hz: 170.0,
                                baud: 45.45,
                                snr_2500_db: 30.0,
                                text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
                                start_s: 0.0,
                                watterson: None,
                                seed: 3,
                            },
                        );
                        report("weakrtty filter-on", &iq, &PipelineConfig::default());
                    }
                    "loudcw" => {
                        let iq = scene(&[cw(12_340.0, 45.0, 20.0, 1.0)], 30.0);
                        report("loudcw filter-on", &iq, &PipelineConfig::default());
                    }
                    "cw" => {
                        let iq = scene(&[cw(12_340.0, 20.0, 20.0, 5.0)], 30.0);
                        report("cw-only filter-on", &iq, &PipelineConfig::default());
                    }
                    "v8w" => {
                        let spec = manta_testkit::vectors::v8w();
                        let iq = manta_testkit::vectors::render(&spec).unwrap().samples;
                        report("v8w filter-on", &iq, &PipelineConfig::default());
                    }
                    _ => unreachable!(),
                }
            }
        }
        // SSB-only at one or more levels.
        "ssb" => {
            let levels: Vec<f32> = std::env::args()
                .skip(2)
                .map(|a| a.parse().expect("snr f32"))
                .collect();
            let levels = if levels.is_empty() {
                vec![30.0, 45.0]
            } else {
                levels
            };
            for snr in levels {
                let iq = ssb_scene(snr, 40.0);
                report(
                    &format!("ssb+{snr} filter-on"),
                    &iq,
                    &PipelineConfig::default(),
                );
            }
        }
        // CW beside SSB, the coexistence scene from non_cw.rs.
        "coexist" => {
            let mut iq = scene(&[cw(43_000.0, 15.0, 20.0, 5.0)], 40.0);
            add_ssb_voice(&mut iq, FS, 40_000.0, 30.0, VOICE_SEED, 0.0);
            report("coexist filter-on", &iq, &PipelineConfig::default());
        }
        // CW only, golden-vector sanity at one level.
        "cw" => {
            let iq = scene(&[cw(12_340.0, 20.0, 20.0, 5.0)], 30.0);
            report("cw-only filter-on", &iq, &PipelineConfig::default());
        }
        // V8w (54 Hz/s drift) through the same harness.
        "v8w" => {
            let spec = manta_testkit::vectors::v8w();
            let iq = manta_testkit::vectors::render(&spec).unwrap().samples;
            report("v8w filter-on", &iq, &PipelineConfig::default());
        }
        // weak_cw_beside_a_louder_signal's RTTY leg: weak CW at 12340
        // (+10 dB), RTTY at 12040 (+30 dB).
        "weakrtty" => {
            use manta_testkit::fsk::{add_fsk, FskSpec};
            let mut iq = scene(&[cw(12_340.0, 10.0, 25.0, 5.0)], 40.0);
            add_fsk(
                &mut iq,
                FS,
                &FskSpec {
                    mark_below_space: false,
                    mark_offset_hz: 12_040.0,
                    shift_hz: 170.0,
                    baud: 45.45,
                    snr_2500_db: 30.0,
                    text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
                    start_s: 0.0,
                    watterson: None,
                    seed: 3,
                },
            );
            report("weakrtty filter-on", &iq, &PipelineConfig::default());
        }
        // loud_cw_decodes_identically's loud leg: 45 dB, 1 ms rise.
        "loudcw" => {
            let iq = scene(&[cw(12_340.0, 45.0, 20.0, 1.0)], 30.0);
            report("loudcw filter-on", &iq, &PipelineConfig::default());
        }
        other => {
            let amp = amplitude_for_snr_2500(30.0, FS) * MASTER_SCALE;
            panic!("unknown mode {other} (amp check {amp:.4})");
        }
    }
}
