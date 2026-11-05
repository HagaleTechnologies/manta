//! MAN-30: restrictions operate before decoders, in offline and streaming paths.
use manta_engine::{
    cw_segments::{Policy, Ranges},
    decode_samples, decode_source, listen, PipelineConfig,
};
use manta_input::IqSource;
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::sync::{atomic::AtomicBool, Arc};
const FS: f64 = 48_000.;
const CENTER: f64 = 14_030_000.;
struct Source {
    iq: Arc<Vec<Complex32>>,
    at: usize,
    chunk: usize,
    rate: f64,
}
impl IqSource for Source {
    fn sample_rate(&self) -> f64 {
        self.rate
    }
    fn center_freq_hz(&self) -> f64 {
        CENTER
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.chunk).min(self.iq.len() - self.at);
        buf[..n].copy_from_slice(&self.iq[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}
fn scene() -> Arc<Vec<Complex32>> {
    scene_rate(FS)
}
fn scene_rate(fs: f64) -> Arc<Vec<Complex32>> {
    let signals: Vec<_> = [("W1AW", -3000.), ("K5ARH", 0.), ("K1ABC", 3000.)]
        .into_iter()
        .map(|(call, offset_hz)| SignalSpec {
            text: format!("CQ CQ DE {call} {call} K"),
            loop_text: true,
            wpm: 20.,
            offset_hz,
            snr_2500_db: 25.,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.,
            dit_units: 1.,
            element_gap_units: 1.,
            char_gap_units: 3.,
            word_gap_units: 7.,
            rise_ms: 5.,
        })
        .collect();
    Arc::new(render_scene(&signals, fs, 60., Some(30)).unwrap().0)
}
fn ranges(v: Vec<(f64, f64)>) -> Ranges {
    Ranges::new(v).unwrap()
}
fn source(iq: &Arc<Vec<Complex32>>, chunk: usize) -> Source {
    Source {
        iq: iq.clone(),
        at: 0,
        chunk,
        rate: FS,
    }
}
#[test]
fn static_ranges_admit_only_included_signals_offline_and_streaming() {
    let iq = scene();
    let control = decode_samples(&iq, FS, CENTER, &PipelineConfig::default()).unwrap();
    assert!(control.spots.len() >= 3, "control: {:?}", control.spots);
    let cfg = PipelineConfig {
        cw_policy: Policy::new(
            Some(ranges(vec![
                (CENTER - 3100., CENTER - 2900.),
                (CENTER + 2900., CENTER + 3100.),
            ])),
            vec![],
            vec![],
        )
        .unwrap(),
        ..Default::default()
    };
    let report = decode_samples(&iq, FS, CENTER, &cfg).unwrap();
    assert!(!report.spots.is_empty());
    assert!(report
        .spots
        .iter()
        .all(|s| (s.freq_hz - CENTER).abs() > 2900.));
    let empty = PipelineConfig {
        cw_policy: Policy::new(Some(Ranges::default()), vec![], vec![]).unwrap(),
        ..Default::default()
    };
    assert!(decode_samples(&iq, FS, CENTER, &empty)
        .unwrap_err()
        .to_string()
        .contains("no signal"));
    let mut spots = Vec::new();
    listen(
        Box::new(source(&iq, 8192)),
        &cfg,
        Arc::new(AtomicBool::new(false)),
        |_| {},
        |s| spots.push(s.clone()),
    )
    .unwrap();
    assert!(!spots.is_empty());
    assert!(spots.iter().all(|s| (s.freq_hz - CENTER).abs() > 2900.));
}
#[test]
fn scheduled_switch_is_sample_timed_and_chunk_invariant() {
    let iq = scene();
    let cfg = PipelineConfig {
        cw_epoch: 30,
        cw_policy: Policy::new(
            None,
            vec![
                (0, ranges(vec![(CENTER - 3100., CENTER + 100.)])),
                (1, ranges(vec![(CENTER - 100., CENTER + 3100.)])),
            ],
            vec![],
        )
        .unwrap(),
        ..Default::default()
    };
    let a = decode_source(&mut source(&iq, 8192), &cfg).unwrap();
    let b = decode_source(&mut source(&iq, 137), &cfg).unwrap();
    assert_eq!(
        serde_json::to_string(&a.spots).unwrap(),
        serde_json::to_string(&b.spots).unwrap()
    );
    let boundary = (30. * FS) as u64;
    let first: Vec<_> = a
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::TrackPromoted {
                sample_ts, freq_hz, ..
            } => Some((*sample_ts, *freq_hz)),
            _ => None,
        })
        .collect();
    assert!(
        first
            .iter()
            .any(|&(t, f)| t < boundary && f < CENTER - 2900.),
        "{first:?}"
    );
    assert!(
        first
            .iter()
            .any(|&(t, f)| t >= boundary && f > CENTER + 2900.),
        "{first:?}"
    );
    assert!(!first
        .iter()
        .any(|&(t, f)| t < boundary && f > CENTER + 2900.));
}

#[test]
fn different_inputs_keep_independent_coverage() {
    use manta_engine::{listen_inputs, open_inputs, InputSpec, SessionClock, SharedObservers};
    let iq = scene();
    let specs = (0..2)
        .map(|index| {
            let mut src = if index == 0 {
                source(&iq, 8192)
            } else {
                source(&scene_rate(96_000.), 137)
            };
            if index == 1 {
                src.rate = 96_000.;
            }
            InputSpec {
                name: format!("rx{index}"),
                cw_policy: Policy::new(
                    Some(ranges(if index == 0 {
                        vec![(CENTER - 3100., CENTER - 2900.)]
                    } else {
                        vec![(CENTER + 2900., CENTER + 3100.)]
                    })),
                    vec![],
                    vec![],
                )
                .unwrap(),
                open: Box::new(move || Ok(Box::new(src) as Box<dyn IqSource>)),
                freq_correction_ppm: 0.,
                conditioning: Default::default(),
                observers: Default::default(),
                spawn_band_hz: None,
            }
        })
        .collect();
    let opened = open_inputs(specs, Arc::new(AtomicBool::new(false))).unwrap();
    let mut births = Vec::new();
    listen_inputs(
        opened,
        &PipelineConfig::default(),
        SessionClock::Replay,
        SharedObservers::default(),
        |e| {
            if let manta_decode::events::DecoderEvent::TrackPromoted {
                track_id, freq_hz, ..
            } = e
            {
                births.push((*track_id, *freq_hz));
            }
        },
        |_| {},
        |_, _| {},
        |_| {},
    )
    .unwrap();
    assert!(!births.is_empty());
    for (id, hz) in births {
        if id % 2 == 1 {
            assert!(hz < CENTER - 2900.);
        } else {
            assert!(hz > CENTER + 2900.);
        }
    }
}

// A source can report a discontinuity or a spliced loss before its next sample.
struct Lost {
    inner: Source,
    at: usize,
    missing: u64,
    discontinuity: bool,
    pending: bool,
    done: bool,
}
impl IqSource for Lost {
    fn sample_rate(&self) -> f64 {
        FS
    }
    fn center_freq_hz(&self) -> f64 {
        CENTER
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        if !self.done && self.inner.at == self.at {
            self.pending = true;
            self.done = true;
        }
        let limit = if self.done {
            buf.len()
        } else {
            buf.len().min(self.at - self.inner.at)
        };
        self.inner.read(&mut buf[..limit])
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        if self.discontinuity && std::mem::take(&mut self.pending) {
            Some(self.missing)
        } else {
            None
        }
    }
    fn take_missing_samples(&mut self) -> Option<u64> {
        if !self.discontinuity && std::mem::take(&mut self.pending) {
            Some(self.missing)
        } else {
            None
        }
    }
}
#[test]
fn schedules_follow_corrected_samples_through_losses_and_segment_rebuilds() {
    let iq = scene();
    let cfg = PipelineConfig {
        cw_epoch: 30,
        cw_policy: Policy::new(
            None,
            vec![
                (0, ranges(vec![(CENTER - 3100., CENTER + 3100.)])),
                (1, Ranges::default()),
            ],
            vec![],
        )
        .unwrap(),
        ..Default::default()
    };
    for discontinuity in [false, true] {
        let run = |chunk| {
            let mut src = Lost {
                inner: source(&iq, chunk),
                at: (20. * FS) as usize,
                missing: (20. * FS) as u64,
                discontinuity,
                pending: false,
                done: false,
            };
            decode_source(&mut src, &cfg).unwrap()
        };
        let a = run(8192);
        let b = run(137);
        assert_eq!(
            serde_json::to_string(&a.spots).unwrap(),
            serde_json::to_string(&b.spots).unwrap()
        );
        assert!(a.spots.iter().all(|s| s.sample_ts < (30. * FS) as u64));
        // Once the lost samples cross the transition, later RF never acquires a decoder.
        assert!(a.events.iter().all(|e|!matches!(e,manta_decode::events::DecoderEvent::TrackPromoted{sample_ts,..} if *sample_ts >= (30.*FS) as u64)));
    }
}
