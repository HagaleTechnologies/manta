//! MAN-311: default exclusions and explicit CW opt-in across offline and
//! streaming receivers. Candidate counts are covered by track.rs unit tests.

use manta_decode::decoder::events_to_text;
use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, listen, PipelineConfig};
use manta_input::IqSource;
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const FS: f64 = 48_000.0;
const DIGITAL_HZ: f64 = 14_074_500.0;
const REFERENCE_HZ: f64 = 14_063_340.0;

struct MemorySource {
    samples: Arc<Vec<Complex32>>,
    cursor: usize,
    center: f64,
}

impl IqSource for MemorySource {
    fn sample_rate(&self) -> f64 {
        FS
    }
    fn center_freq_hz(&self) -> f64 {
        self.center
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

fn station(call: &str, offset_hz: f64) -> SignalSpec {
    SignalSpec {
        text: format!("CQ CQ DE {call} {call} K"),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    }
}

fn scene(center: f64) -> Arc<Vec<Complex32>> {
    Arc::new(
        render_scene(
            &[
                station("K5ARH", DIGITAL_HZ - center),
                station("W1AW", REFERENCE_HZ - center),
            ],
            FS,
            30.0,
            Some(311),
        )
        .unwrap()
        .0,
    )
}

fn text_near(events: &[DecoderEvent], freq: f64) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            let DecoderEvent::TrackPromoted {
                track_id, freq_hz, ..
            } = event
            else {
                return None;
            };
            if (freq_hz - freq).abs() > 100.0 {
                return None;
            }
            let chars: Vec<_> = events
                .iter()
                .filter(|e| match e {
                    DecoderEvent::CharDecoded { track_id: id, .. }
                    | DecoderEvent::WordBoundary { track_id: id, .. } => id == track_id,
                    _ => false,
                })
                .cloned()
                .collect();
            Some(events_to_text(&chars))
        })
        .collect()
}

fn assert_policy(events: &[DecoderEvent], enabled: bool) {
    let reference = text_near(events, REFERENCE_HZ);
    assert!(
        reference.iter().any(|s| s.contains("W1AW")),
        "reference: {reference:?}"
    );
    let digital = text_near(events, DIGITAL_HZ);
    if enabled {
        assert!(digital.is_empty(), "excluded station promoted: {digital:?}");
    } else {
        assert!(
            digital.iter().any(|s| s.contains("K5ARH")),
            "opt-in did not decode: {digital:?}"
        );
    }
}

#[test]
fn digital_window_offline_and_streaming_cw_opt_in() {
    // A second receiver puts the same station on a negative channel offset.
    for center in [14_070_000.0, 14_080_500.0] {
        let samples = scene(center);
        for enabled in [true, false] {
            let mut cfg = PipelineConfig::default();
            cfg.detector.digital_window_filter = enabled;
            let report = decode_samples(&samples, FS, center, &cfg).unwrap();
            assert_policy(&report.events, enabled);
            let repeated = decode_samples(&samples, FS, center, &cfg).unwrap();
            assert_eq!(
                format!("{report:?}"),
                format!("{repeated:?}"),
                "offline determinism"
            );

            let run = || {
                let mut events = Vec::new();
                let mut spots = Vec::new();
                listen(
                    Box::new(MemorySource {
                        samples: samples.clone(),
                        cursor: 0,
                        center,
                    }),
                    &cfg,
                    Arc::new(AtomicBool::new(false)),
                    |event| events.push(event.clone()),
                    |spot| spots.push(spot.clone()),
                )
                .unwrap();
                (events, spots)
            };
            let streamed = run();
            assert_policy(&streamed.0, enabled);
            assert_eq!(
                format!("{streamed:?}"),
                format!("{:?}", run()),
                "streaming determinism"
            );
        }
    }
}

#[test]
fn digital_window_each_service_input_uses_its_own_rf_center() {
    use manta_engine::{listen_inputs, open_inputs, InputSpec, SessionClock, SharedObservers};

    let receivers: Vec<_> = [14_070_000.0, 14_080_500.0]
        .into_iter()
        .map(|center| (center, scene(center)))
        .collect();
    for enabled in [true, false] {
        let mut cfg = PipelineConfig::default();
        cfg.detector.digital_window_filter = enabled;
        let specs = receivers
            .iter()
            .enumerate()
            .map(|(index, (center, samples))| {
                let (center, samples) = (*center, samples.clone());
                InputSpec {
                    name: format!("receiver-{index}"),
                    open: Box::new(move || {
                        Ok(Box::new(MemorySource {
                            samples,
                            cursor: 0,
                            center,
                        }) as Box<dyn IqSource>)
                    }),
                    freq_correction_ppm: 0.0,
                    observers: Default::default(),
                    spawn_band_hz: None,
                }
            })
            .collect();
        let opened = open_inputs(specs, Arc::new(AtomicBool::new(false))).unwrap();
        let mut events = Vec::new();
        let ends = listen_inputs(
            opened,
            &cfg,
            SessionClock::Replay,
            SharedObservers::default(),
            |event| events.push(event.clone()),
            |_| {},
            |_, _| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(ends.len(), 2);
        for index in 0..2 {
            // Input i owns IDs 1+i, 3+i, ... . Keep only its promotions;
            // text_near then selects characters belonging to those tracks.
            let from_input: Vec<_> = events
                .iter()
                .filter(|event| {
                    !matches!(event,
                        DecoderEvent::TrackPromoted { track_id, .. } if (*track_id - 1) % 2 != index
                    )
                })
                .cloned()
                .collect();
            assert_policy(&from_input, enabled);
        }
    }
}
