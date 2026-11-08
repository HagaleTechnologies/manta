use super::*;

#[test]
fn audio_silence_warning_names_the_device_the_rate_and_the_hint() {
    let w = audio_silence_warning("\"Test Mic\"");
    assert!(
        w.starts_with(
            "warning: audio input \"Test Mic\" is delivering digital silence at 48000 Hz \
                 (every sample is exactly zero) -- "
        ),
        "{w}"
    );
    assert!(w.ends_with(manta_input::audio::audio_input_hint()), "{w}");
}

/// MAN-233: pins the design section's exact wording, so a future edit
/// cannot quietly change what the operator is told to watch
/// (`ka9q_agc_warning_is_design_section_3s_text` precedent).
#[cfg(feature = "soapy")]
#[test]
fn soapy_overflow_warning_names_the_driver_the_counter_and_the_interval() {
    assert_eq!(
        soapy_overflow_warning("driver=rtlsdr"),
        "warning: SoapySDR input (driver=rtlsdr) dropped samples: the driver reported a \
             stream overflow -- the host is not keeping up; every overflow error is counted in \
             manta_input_gaps_detected_total{source=\"soapy\"} (this warning is \
             rate-limited to once per 60 s)"
    );
}

#[test]
fn recording_center_warning_covers_a_missing_and_a_non_positive_sidecar() {
    let wav = Path::new("/rec/x.wav");
    let missing = recording_center_warning(wav, None).unwrap();
    assert!(
        missing.starts_with("warning: no sidecar /rec/x.json next to /rec/x.wav -- "),
        "{missing}"
    );
    let zero = recording_center_warning(wav, Some(0.0)).unwrap();
    for w in [&missing, &zero] {
        assert!(
            w.contains("baseband offsets") && w.contains("--center-freq-hz 14000000"),
            "{w}"
        );
    }
    assert!(
        zero.starts_with(
            "warning: sidecar /rec/x.json gives center_freq_hz = 0, not an RF frequency -- "
        ),
        "{zero}"
    );
    assert!(recording_center_warning(wav, Some(-5.0)).is_some());
    assert!(recording_center_warning(wav, Some(14_000_000.0)).is_none());
}

#[test]
fn live_source_spec_reports_reconnectable_only_for_live_kinds() {
    assert!(LiveSourceSpec::Kiwi(KiwiOpts {
        host: Some("h".into()),
        port: 8073,
        freq: Some(14_000_000.0),
        password: String::new(),
    })
    .is_reconnectable());
    assert!(LiveSourceSpec::AudioDevice(None).is_reconnectable());
    assert!(!LiveSourceSpec::File {
        path: PathBuf::from("rec.wav"),
        source_iq: false,
        raw: None,
    }
    .is_reconnectable());
}

#[test]
fn live_source_spec_name_matches_metrics_label() {
    // MAN-73: `name()` feeds `set_source_health`'s label directly --
    // a mismatch here would silently split one source's health series
    // across two metric labels.
    assert_eq!(
        LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: Some(14_000_000.0),
            password: String::new(),
        })
        .name(),
        "kiwi"
    );
    assert_eq!(LiveSourceSpec::AudioDevice(None).name(), "audio");
    assert_eq!(
        LiveSourceSpec::File {
            path: PathBuf::from("rec.wav"),
            source_iq: false,
            raw: None,
        }
        .name(),
        "file"
    );
}

fn open_error(spec: LiveSourceSpec) -> String {
    match spec.open(None, None) {
        Ok(_) => panic!("expected {} open to fail", spec.name()),
        Err(e) => e.to_string(),
    }
}

/// MAN-135 renamed the frequency/rate flags to `*-hz`; `run` opens its
/// input through `LiveSourceSpec::open`, so its "missing flag" errors
/// must name the same flags `doctor`/`soak` (`open_source`) do.
#[test]
fn live_source_spec_open_errors_name_the_hz_flags() {
    assert_eq!(
        open_error(LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: None,
            password: String::new(),
        })),
        "--kiwi-freq-hz is required with --kiwi-host"
    );
}

#[test]
fn live_source_spec_open_errors_name_the_hpsdr_hz_flags() {
    let hpsdr = |freq, rate| {
        LiveSourceSpec::Hpsdr(HpsdrOpts {
            host: Some("192.168.1.100".into()),
            mac: None,
            port: Some(manta_input::hpsdr::CONTROL_PORT),
            protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
            freq,
            rate,
            auto_attenuation: None,
        })
    };
    assert_eq!(
        open_error(hpsdr(None, None)),
        "--hpsdr-freq-hz is required with the hpsdr source"
    );
    assert_eq!(
        open_error(hpsdr(Some(7_030_000.0), None)),
        "--hpsdr-rate-hz is required with the hpsdr source"
    );
}

/// MAN-334: what `calibrate` plans on before opening. Each case mirrors
/// what `LiveSourceSpec::open` composes: the source's values, then
/// `maybe_decimate`, then the `--dial-freq-hz` override.
#[test]
fn configured_center_and_passband_mirrors_open() {
    let kiwi = |freq| {
        LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq,
            password: String::new(),
        })
    };
    let trusted = |fs_out: f64| manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * fs_out;
    let k = kiwi(Some(7_030_000.0));
    assert_eq!(
        k.configured_center_and_passband(None, None),
        Some((7_030_000.0, (-5_000.0, 5_000.0)))
    );
    // The dial override relabels the centre only.
    assert_eq!(
        k.configured_center_and_passband(None, Some(7_031_000.0)),
        Some((7_031_000.0, (-5_000.0, 5_000.0)))
    );
    // Decimation from KiwiSDR's 96 kS/s: 12 kS/s keeps +/-5000 (trusted
    // 5431.2), 6 kS/s clips it.
    assert_eq!(
        k.configured_center_and_passband(Some(12_000.0), None),
        Some((7_030_000.0, (-5_000.0, 5_000.0)))
    );
    assert_eq!(
        k.configured_center_and_passband(Some(6_000.0), None),
        Some((7_030_000.0, (-trusted(6_000.0), trusted(6_000.0))))
    );
    // The native rate itself is no decimation.
    assert_eq!(
        k.configured_center_and_passband(Some(96_000.0), None),
        Some((7_030_000.0, (-5_000.0, 5_000.0)))
    );
    // A rate `open` refuses, or a missing frequency: no preflight, `open`
    // reports it.
    assert_eq!(k.configured_center_and_passband(Some(50_000.0), None), None);
    assert_eq!(kiwi(None).configured_center_and_passband(None, None), None);

    let hpsdr = |freq, rate| {
        LiveSourceSpec::Hpsdr(HpsdrOpts {
            host: Some("h".into()),
            mac: None,
            port: Some(manta_input::hpsdr::CONTROL_PORT),
            protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
            freq,
            rate,
            auto_attenuation: None,
        })
    };
    assert_eq!(
        hpsdr(Some(7_030_000.0), Some(48_000.0)).configured_center_and_passband(None, None),
        Some((7_030_000.0, (-24_000.0, 24_000.0)))
    );
    assert_eq!(
        hpsdr(Some(7_030_000.0), Some(192_000.0))
            .configured_center_and_passband(Some(48_000.0), None),
        Some((7_030_000.0, (-trusted(48_000.0), trusted(48_000.0))))
    );
    assert_eq!(
        hpsdr(Some(7_030_000.0), None).configured_center_and_passband(None, None),
        None
    );
    assert_eq!(
        hpsdr(None, Some(48_000.0)).configured_center_and_passband(None, None),
        None
    );
    // A rate Protocol 1 cannot encode, or a fractional frequency:
    // `HpsdrDevice::open` refuses them before any I/O, so it reports them.
    assert_eq!(
        hpsdr(Some(7_030_000.0), Some(50_000.0)).configured_center_and_passband(None, None),
        None
    );
    assert_eq!(
        hpsdr(Some(7_030_000.5), Some(48_000.0)).configured_center_and_passband(None, None),
        None
    );
    // A frequency protocol 1 accepts but protocol 2 refuses (not below
    // its 61.44 MHz ADC Nyquist limit): with `--hpsdr-protocol 2`,
    // `Protocol2IqSource::open` refuses it before any I/O, so it
    // reports it; under `Auto` discovery picks the protocol, so the
    // preflight plans on protocol 1's validation.
    let protocol2 = LiveSourceSpec::Hpsdr(HpsdrOpts {
        host: Some("h".into()),
        mac: None,
        port: Some(manta_input::hpsdr::CONTROL_PORT),
        protocol: manta_input::hpsdr::HpsdrProtocol::Two,
        freq: Some(100_000_000.0),
        rate: Some(48_000.0),
        auto_attenuation: None,
    });
    assert_eq!(protocol2.configured_center_and_passband(None, None), None);
    let protocol2 = LiveSourceSpec::Hpsdr(HpsdrOpts {
        host: Some("h".into()),
        mac: None,
        port: Some(manta_input::hpsdr::CONTROL_PORT),
        protocol: manta_input::hpsdr::HpsdrProtocol::Two,
        freq: Some(7_030_000.0),
        rate: Some(48_000.0),
        auto_attenuation: None,
    });
    assert_eq!(
        protocol2.configured_center_and_passband(None, None),
        Some((7_030_000.0, (-24_000.0, 24_000.0)))
    );
    assert_eq!(
        hpsdr(Some(100_000_000.0), Some(48_000.0)).configured_center_and_passband(None, None),
        Some((100_000_000.0, (-24_000.0, 24_000.0)))
    );

    // A sound card's centre is the dial; without one there is none.
    let audio = LiveSourceSpec::AudioDevice(None);
    assert_eq!(
        audio.configured_center_and_passband(None, Some(7_030_000.0)),
        Some((7_030_000.0, (300.0, 3_000.0)))
    );
    assert_eq!(audio.configured_center_and_passband(None, None), None);

    // Only opening tells: radiod's status, a recording's header and
    // sidecar.
    let ka9q = LiveSourceSpec::Ka9q(Ka9qOpts {
        group: Some("239.1.2.3".into()),
        ..Ka9qOpts::unset()
    });
    assert_eq!(
        ka9q.configured_center_and_passband(None, Some(7_030_000.0)),
        None
    );
    for source_iq in [false, true] {
        let file = LiveSourceSpec::File {
            path: PathBuf::from("rec.wav"),
            source_iq,
            raw: None,
        };
        assert_eq!(
            file.configured_center_and_passband(None, Some(7_030_000.0)),
            None
        );
    }
}

#[cfg(feature = "soapy")]
#[test]
fn configured_center_and_passband_uses_the_requested_soapy_values() {
    let s = LiveSourceSpec::Soapy(SoapyOpts {
        driver: Some("driver=rtlsdr".into()),
        freq: Some(7_030_000.0),
        rate: Some(240_000.0),
        gain: None,
    });
    assert_eq!(
        s.configured_center_and_passband(None, None),
        Some((7_030_000.0, (-120_000.0, 120_000.0)))
    );
}

struct GapProbeSource {
    gap: Option<u64>,
}

impl IqSource for GapProbeSource {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        14_000_000.0
    }
    fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
        Ok(0)
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.gap.take()
    }
}

#[test]
fn fixed_center_freq_source_forwards_take_discontinuity() {
    let mut src = FixedCenterFreqSource {
        inner: Box::new(GapProbeSource { gap: Some(48_000) }),
        freq_hz: 14_000_000.0,
    };
    assert_eq!(src.take_discontinuity(), Some(48_000));
    assert_eq!(src.take_discontinuity(), None);
}

/// MAN-305: the inner source's spliced loss reaches `listen()`, once.
#[test]
fn fixed_center_freq_source_forwards_take_missing_samples() {
    struct MissingProbeSource {
        missing: Option<u64>,
    }
    impl IqSource for MissingProbeSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn take_missing_samples(&mut self) -> Option<u64> {
            self.missing.take()
        }
    }
    let mut src = FixedCenterFreqSource {
        inner: Box::new(MissingProbeSource {
            missing: Some(11_900),
        }),
        freq_hz: 14_000_000.0,
    };
    assert_eq!(src.take_missing_samples(), Some(11_900));
    assert_eq!(src.take_missing_samples(), None);
    assert_eq!(src.take_discontinuity(), None);
}

/// MAN-202: the inner source's unknown-length loss reaches `listen()`,
/// once, beside the counted hooks.
#[test]
fn fixed_center_freq_source_forwards_take_unknown_gap() {
    struct UnknownProbeSource {
        unknown: bool,
        missing: Option<u64>,
    }
    impl IqSource for UnknownProbeSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn take_missing_samples(&mut self) -> Option<u64> {
            self.missing.take()
        }
        fn take_unknown_gap(&mut self) -> bool {
            std::mem::take(&mut self.unknown)
        }
    }
    let mut src = FixedCenterFreqSource {
        inner: Box::new(UnknownProbeSource {
            unknown: true,
            missing: Some(5),
        }),
        freq_hz: 14_000_000.0,
    };
    assert!(src.take_unknown_gap());
    assert!(!src.take_unknown_gap());
    assert_eq!(src.take_missing_samples(), Some(5));
    assert_eq!(src.take_discontinuity(), None);
}

/// MAN-259: the reconnect wrapper reads the acquisition-start offset
/// off the opener's result, which is this wrapper whenever
/// `--dial-freq-hz` or a fixed centre is in effect.
#[test]
fn fixed_center_freq_source_forwards_acquisition_start_offset() {
    struct OffsetSource;
    impl IqSource for OffsetSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn acquisition_start_offset(&self) -> std::time::Duration {
            std::time::Duration::from_millis(250)
        }
    }

    let src = FixedCenterFreqSource {
        inner: Box::new(OffsetSource),
        freq_hz: 14_000_000.0,
    };
    assert_eq!(
        src.acquisition_start_offset(),
        std::time::Duration::from_millis(250)
    );
}

// MAN-56: input-layer health counters wiring.

/// A wrapper `IqSource` that forgets to forward `health_counters`
/// silently swallows the inner source's counters via the trait's
/// `None` default -- the metrics would just be absent, with nothing
/// failing loudly. Same hazard `confirmed_live_handle` carries; both
/// are asserted here.
#[test]
fn fixed_center_freq_source_forwards_both_optional_trait_signals() {
    use manta_input::InputHealthCounters;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct StubSource {
        counters: Arc<InputHealthCounters>,
        live: Arc<AtomicBool>,
    }
    impl IqSource for StubSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
            Some(self.live.clone())
        }
        fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
            Some(self.counters.clone())
        }
    }

    let counters = Arc::new(InputHealthCounters::new());
    let live = Arc::new(AtomicBool::new(false));
    let wrapped = FixedCenterFreqSource {
        inner: Box::new(StubSource {
            counters: counters.clone(),
            live: live.clone(),
        }),
        freq_hz: 14_025_000.0,
    };

    assert!(Arc::ptr_eq(&wrapped.health_counters().unwrap(), &counters));
    assert!(Arc::ptr_eq(
        &wrapped.confirmed_live_handle().unwrap(),
        &live
    ));
}

#[test]
fn name_matches_today_source_names() {
    let kiwi = || KiwiOpts {
        host: Some("h".into()),
        port: 8073,
        freq: Some(7e6),
        password: String::new(),
    };
    assert_eq!(LiveSourceSpec::Kiwi(kiwi()).name(), "kiwi");
    assert_eq!(LiveSourceSpec::AudioDevice(None).name(), "audio");
    assert_eq!(
        LiveSourceSpec::File {
            path: PathBuf::from("x.wav"),
            source_iq: false,
            raw: None,
        }
        .name(),
        "file"
    );
    #[cfg(feature = "soapy")]
    assert_eq!(
        LiveSourceSpec::Soapy(SoapyOpts {
            driver: Some("d".into()),
            freq: Some(7e6),
            rate: Some(48e3),
            gain: None
        })
        .name(),
        "soapy"
    );
    assert_eq!(
        LiveSourceSpec::Hpsdr(HpsdrOpts {
            host: Some("h".into()),
            mac: None,
            port: Some(1024),
            protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
            freq: Some(7e6),
            rate: Some(48e3),
            auto_attenuation: None,
        })
        .name(),
        "hpsdr"
    );
}

#[test]
fn attenuation_line_names_the_radio_cause_and_levels() {
    use manta_input::hpsdr::{AttenuationCause, AttenuationChange};
    let change = |from_db, to_db, cause| AttenuationChange {
        radio: "192.0.2.10:1024".into(),
        from_db,
        to_db,
        cause,
    };
    assert_eq!(
        attenuation_line(&change(0, 6, AttenuationCause::Overload)),
        "note: HPSDR radio 192.0.2.10:1024: ADC overload for 2 s; automatic attenuation \
             raised the step attenuator from 0 dB to 6 dB"
    );
    assert_eq!(
        attenuation_line(&change(24, 30, AttenuationCause::Overload)),
        "note: HPSDR radio 192.0.2.10:1024: ADC overload for 2 s; automatic attenuation \
             raised the step attenuator from 24 dB to 30 dB, its maximum"
    );
    assert_eq!(
        attenuation_line(&change(12, 6, AttenuationCause::Quiet)),
        "note: HPSDR radio 192.0.2.10:1024: no ADC overload for 10 min; automatic \
             attenuation lowered the step attenuator from 12 dB to 6 dB"
    );
}

#[test]
fn protocol_1_attenuation_warning_names_the_key() {
    assert_eq!(
        protocol_1_attenuation_warning("192.0.2.10", 1024),
        "warning: HPSDR radio 192.0.2.10:1024 speaks openHPSDR protocol 1; \
             input.auto_attenuation needs protocol 2, so manta leaves this radio's attenuator \
             alone"
    );
}

const HPSDR_P2_REPLY: manta_input::hpsdr::DiscoveryReply =
    manta_input::hpsdr::DiscoveryReply::Protocol2 {
        status: 2,
        mac: [0; 6],
        board: 5,
        protocol_version: 39,
        firmware: 21,
        ddc_count: 4,
    };

#[test]
fn hpsdr_protocol_2_note_names_board_version_firmware() {
    use manta_input::hpsdr::{Discovery, HpsdrProtocol};
    assert_eq!(
        hpsdr_open_line(
            "192.0.2.10",
            1024,
            HpsdrProtocol::Two,
            &Discovery::Answered(HPSDR_P2_REPLY)
        ),
        Some(HpsdrOpenLine::Note(
            "note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 2 \
                 (board 5, Orion II; protocol version 39; firmware 21); connecting with protocol 2"
                .into()
        ))
    );
}

#[test]
fn hpsdr_protocol_1_note() {
    use manta_input::hpsdr::{Discovery, DiscoveryReply, HpsdrProtocol};
    let reply = DiscoveryReply::Protocol1 {
        status: 2,
        mac: [0; 6],
        firmware: 73,
        board: 6,
    };
    assert_eq!(
        hpsdr_open_line(
            "192.0.2.10",
            1024,
            HpsdrProtocol::One,
            &Discovery::Answered(reply)
        ),
        Some(HpsdrOpenLine::Note(
            "note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 1 \
                 (board 6; firmware 73); connecting with protocol 1"
                .into()
        ))
    );
}

#[test]
fn hpsdr_no_answer_warning_names_the_flag_and_key() {
    use manta_input::hpsdr::{Discovery, HpsdrProtocol};
    assert_eq!(
        hpsdr_open_line("192.0.2.10", 1024, HpsdrProtocol::One, &Discovery::NoAnswer),
        Some(HpsdrOpenLine::Warning(
            "warning: HPSDR radio 192.0.2.10:1024 did not answer discovery within 1 s; \
                 connecting with protocol 1 -- pass --hpsdr-protocol 2 (protocol = \"2\" under \
                 [input]) for a protocol-2 radio"
                .into()
        ))
    );
}

#[test]
fn hpsdr_forced_protocol_2_note() {
    use manta_input::hpsdr::{Discovery, HpsdrProtocol};
    assert_eq!(
        hpsdr_open_line("192.0.2.10", 1024, HpsdrProtocol::Two, &Discovery::Skipped),
        Some(HpsdrOpenLine::Note(
            "note: HPSDR radio 192.0.2.10:1024: protocol 2 as configured; discovery \
                 skipped, board unknown, front-end filters bypassed"
                .into()
        ))
    );
    // `--hpsdr-protocol 1` prints nothing new.
    assert_eq!(
        hpsdr_open_line("192.0.2.10", 1024, HpsdrProtocol::One, &Discovery::Skipped),
        None
    );
}

#[test]
fn hpsdr_unknown_board_note() {
    use manta_input::hpsdr::{Discovery, DiscoveryReply, HpsdrProtocol};
    let reply = DiscoveryReply::Protocol2 {
        status: 2,
        mac: [0; 6],
        board: 42,
        protocol_version: 41,
        firmware: 12,
        ddc_count: 2,
    };
    assert_eq!(
        hpsdr_open_line(
            "192.0.2.10",
            1024,
            HpsdrProtocol::Two,
            &Discovery::Answered(reply)
        ),
        Some(HpsdrOpenLine::Note(
            "note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 2 \
                 (board 42, unknown board; protocol version 41; firmware 12); connecting with \
                 protocol 2, front-end filters bypassed"
                .into()
        ))
    );
}

#[test]
fn ka9q_agc_warning_is_design_section_3s_text() {
    assert_eq!(
        ka9q_agc_warning(14_100_000),
        "warning: ka9q-radio channel ssrc 14100000 has AGC on; set agc = off and a fixed \
             gain in its radiod section, or one strong signal changes the gain for every \
             signal in the passband"
    );
}
