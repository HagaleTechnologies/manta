//! MAN-172: strong single signals must retain one decoder through PFB leakage.
use manta_decode::decoder::{DecodeConfig, Engine};
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_engine::{decode_samples, PipelineConfig};

#[test]
fn high_snr_signal_holds_one_track_under_both_engines() {
    for mut spec in [manta_testkit::vectors::vr3(), manta_testkit::vectors::vr1()] {
        spec.duration_s = 30.0;
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        for engine in [Engine::Legacy, Engine::Hsmm] {
            let cfg = PipelineConfig {
                decode: DecodeConfig {
                    engine,
                    ..Default::default()
                },
                ..Default::default()
            };
            let report =
                decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap();
            let context = format!("{} {engine:?}: {:?}", spec.name, report.text);
            let promoted = report
                .events
                .iter()
                .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
                .count();
            assert_eq!(promoted, 1, "{context}");
            assert!(
                !report.events.iter().any(|e| matches!(
                    e,
                    DecoderEvent::TrackClosed {
                        closure: ClosureKind::Bookkeeping {
                            survivor_track_id: Some(_)
                        },
                        ..
                    }
                )),
                "unexpected merge: {context}"
            );
            assert!(!report.text.is_empty(), "{context}");
            // A 30 s cut can end inside a key-up transient: VR1 measures
            // 29.49 Hz error here. Keep the estimate within half a channel;
            // the full 120 s vectors are separately checked within 25 Hz.
            assert!(
                (report.freq_hz - 14_012_340.0).abs() < 93.75 / 2.0,
                "frequency {}: {context}",
                report.freq_hz
            );
        }
    }
}
