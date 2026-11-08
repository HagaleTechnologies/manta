use super::*;
use crate::attach_language_prior;
use crate::build_pipeline_config;
use crate::spectrum_feed;
use crate::test_support::*;
use manta_decode::decoder::Engine;
use manta_server::spot_message::geography_is_unresolved;

#[test]
fn repetition_fusion_reaches_the_pipeline_config() {
    for on in [false, true] {
        let spot = config::SpotFile {
            repetition_fusion: on,
            ..Default::default()
        };
        let r = resolve_spot(vec![], None, None, None, None, None, &spot);
        let (cfg, _) = build_pipeline_config(0.0, &r, Default::default(), Engine::Legacy).unwrap();
        assert_eq!(cfg.repetition_fusion, on);
    }
}

#[test]
fn cli_patterns_override_file_and_preserve_validation_level() {
    let spot = config::SpotFile {
        pattern_path: Some("file.lst".into()),
        validation_level: manta_spot::ValidationLevel::Minimal,
        ..Default::default()
    };
    let r = resolve_spot(
        vec![],
        None,
        None,
        None,
        None,
        Some("cli.lst".into()),
        &spot,
    );
    assert_eq!(r.patterns, Some("cli.lst".into()));
    assert_eq!(r.validation_level, manta_spot::ValidationLevel::Minimal);
}

#[test]
fn cli_cty_beats_cty_path() {
    let spot = config::SpotFile {
        cty_path: Some("file.dat".into()),
        scp_path: Some("file.scp".into()),
        ..Default::default()
    };
    let r = resolve_spot(
        vec![],
        None,
        None,
        Some("cli.dat".into()),
        None,
        None,
        &spot,
    );
    assert_eq!(r.cty, Some("cli.dat".into()));
    assert_eq!(r.scp, Some("file.scp".into()));
}

#[test]
fn cli_scp_beats_scp_path() {
    let spot = config::SpotFile {
        cty_path: Some("file.dat".into()),
        scp_path: Some("file.scp".into()),
        ..Default::default()
    };
    let r = resolve_spot(
        vec![],
        None,
        None,
        None,
        Some("cli.scp".into()),
        None,
        &spot,
    );
    assert_eq!(r.cty, Some("file.dat".into()));
    assert_eq!(r.scp, Some("cli.scp".into()));
}

#[test]
fn unset_cty_and_scp_stay_built_in() {
    let r = resolve_spot(
        vec![],
        None,
        None,
        None,
        None,
        None,
        &config::SpotFile::default(),
    );
    let (cfg, _) = build_pipeline_config(0.0, &r, Default::default(), Engine::Legacy).unwrap();
    assert!(cfg.cty.is_none() && cfg.scp.is_none());
}

/// `--cty-sha256` > `MANTA_SPOT_CTY_SHA256` > `[spot].cty_sha256`. An
/// all-digit hash from the environment stays a string.
#[test]
fn cli_pin_beats_env_beats_file() {
    use std::ffi::OsString;
    let (file, env, cli) = ("a".repeat(64), "1".repeat(64), "c".repeat(64));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manta.toml");
    std::fs::write(&path, format!("[spot]\ncty_sha256 = '{file}'\n")).unwrap();
    let loaded = config::load(Some(&path), config::Env::Read(&[])).unwrap();
    assert_eq!(loaded.spot.cty_sha256.as_deref(), Some(file.as_str()));
    let vars = [(
        OsString::from("MANTA_SPOT_CTY_SHA256"),
        OsString::from(&env),
    )];
    let loaded = config::load(Some(&path), config::Env::Read(&vars)).unwrap();
    assert_eq!(loaded.spot.cty_sha256.as_deref(), Some(env.as_str()));
    let none = resolve_spot_pinned(
        vec![],
        None,
        None,
        None,
        None,
        CliPins::default(),
        None,
        &loaded.spot,
    );
    assert_eq!(none.cty_sha256.as_deref(), Some(env.as_str()));
    let pins = CliPins {
        cty_sha256: Some(cli.clone()),
        scp_sha256: None,
    };
    let r = resolve_spot_pinned(vec![], None, None, None, None, pins, None, &loaded.spot);
    assert_eq!(r.cty_sha256.as_deref(), Some(cli.as_str()));
}

#[test]
fn prior_pipeline_loads_local_calls_heard_file() {
    let file = write_temp_file(b"W1AW 12\nK1ABC 3\nAB2KLK\n");
    let spot = resolve_spot(
        vec![],
        None,
        None,
        None,
        None,
        None,
        &config::SpotFile {
            calls_heard_path: Some(file.path().into()),
            ..Default::default()
        },
    );
    let (mut pipeline, _) =
        build_pipeline_config(0.0, &spot, Default::default(), Engine::Legacy).unwrap();
    pipeline.decode.prior_enabled = true;
    attach_language_prior(&mut pipeline, &spot, None).unwrap();
    assert!(
        pipeline
            .decode
            .prior
            .as_ref()
            .unwrap()
            .reading_bonus("W1AW")
            > 0.0
    );
}

#[test]
fn soak_and_doctor_say_they_ignore_record_iq() {
    let loaded = loaded_from("[input]\ntype = \"audio\"\nrecord_iq = \"s.cf32\"\n");
    let note = record_iq_ignored_note("soak", &loaded).unwrap();
    assert!(
        note.starts_with("note: soak does not record IQ; ignoring input.record_iq from "),
        "{note}"
    );
    assert_eq!(
        record_iq_ignored_note("doctor", &loaded_from("[input]\ntype = \"audio\"\n")),
        None
    );
}

#[test]
fn spectrum_needs_an_rf_reference_like_a_server_does() {
    // MAN-141: `run`'s feed guard is MAN-34's rule with a server
    // assumed: plain audio without a dial is refused, with one is not.
    let audio = LiveSourceSpec::AudioDevice(None);
    assert!(needs_dial_freq(true, audio.is_rf_aware(), None));
    assert!(!needs_dial_freq(
        true,
        audio.is_rf_aware(),
        Some(14_000_000.0)
    ));
    assert!(spectrum_feed::NEEDS_RF_MESSAGE.contains("set input.center_freq_hz"));
}

/// MAN-306 (PR #359 review, P1): a hostless reopen's discovery is
/// outage, so the opened source's acquisition offset includes it.
#[test]
fn hpsdr_discovery_time_is_inside_the_acquisition_offset() {
    struct Offset(std::time::Duration);
    impl IqSource for Offset {
        fn sample_rate(&self) -> f64 {
            192_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
        fn acquisition_start_offset(&self) -> std::time::Duration {
            self.0
        }
    }
    let discovery = std::time::Duration::from_millis(200);
    let mut options = CliOverrides::none().hpsdr;
    options.mac = Some([2, 17, 34, 51, 68, 85]);
    let start = std::time::Instant::now();
    let (cfg, opened) = options
        .open_with(
            7_030_000.0,
            192_000.0,
            |_, _| {
                std::thread::sleep(discovery);
                Ok("192.0.2.1".into())
            },
            // Stands in for `open_receiver_since`: acquisition begins
            // the moment the open is called, measured from `entered`.
            |_, _, entered| {
                Ok(manta_input::hpsdr::OpenedHpsdr {
                    source: Box::new(Offset(entered.elapsed())),
                    protocol: manta_input::hpsdr::HpsdrProtocol::One,
                    discovery: manta_input::hpsdr::Discovery::Skipped,
                })
            },
        )
        .unwrap();
    let took = start.elapsed();
    assert_eq!(cfg.host, "192.0.2.1");
    let offset = opened.source.acquisition_start_offset();
    assert!(
        offset >= discovery && offset <= took,
        "offset {offset:?}, discovery {discovery:?}, open took {took:?}"
    );
}

#[test]
fn explicit_hpsdr_host_bypasses_discovery_and_hostless_reopens_discover() {
    let mut options = CliOverrides::none().hpsdr;
    options.host = Some("radio.local".into());
    let host = options
        .resolve_host_with(|_, _| panic!("explicit host must not discover"))
        .unwrap();
    assert_eq!(host, "radio.local");
    options.host = None;
    options.mac = Some([2, 17, 34, 51, 68, 85]);
    options.port = Some(12345);
    for address in ["192.0.2.1", "192.0.2.2"] {
        assert_eq!(
            options
                .resolve_host_with(|port, mac| {
                    assert_eq!(port, 12345);
                    assert_eq!(mac, options.mac);
                    Ok(address.into())
                })
                .unwrap(),
            address
        );
    }
}

#[test]
fn every_hpsdr_selector_selects_the_hpsdr_source() {
    for option in ["host", "mac", "freq", "rate", "port"] {
        let mut cli = CliOverrides::none();
        match option {
            "host" => cli.hpsdr.host = Some("radio.local".into()),
            "mac" => cli.hpsdr.mac = Some([2, 17, 34, 51, 68, 85]),
            "freq" => cli.hpsdr.freq = Some(7030000.0),
            "rate" => cli.hpsdr.rate = Some(192000.0),
            "port" => cli.hpsdr.port = Some(12345),
            _ => unreachable!(),
        }
        assert!(cli.source_selector().unwrap().starts_with("--hpsdr-"));
        assert!(matches!(cli.into_spec(), LiveSourceSpec::Hpsdr(_)));
    }
}

#[test]
fn a_maritime_or_aeronautical_mobile_callsign_is_counted_as_unresolved() {
    // /MM and /AM resolve through the base prefix, so the entity-number
    // test alone reads them as resolved -- but `SpotMessage::from_spot`
    // emits UNKNOWN_CONTINENT/UNKNOWN_CQ_ZONE and null lat/lon for them,
    // so the counter must not sit at zero while those go out.
    let cty =
        manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
    assert_eq!(
        cty.lookup("W1AW/MM").and_then(|e| e.dxcc),
        Some(291),
        "test premise: the base prefix still resolves"
    );
    assert!(geography_is_unresolved(&cty, "W1AW/MM"));
    assert!(geography_is_unresolved(&cty, "W1AW/AM"));
    assert!(!geography_is_unresolved(&cty, "W1AW/P"));
}

// ---- MAN-13: several [[input]] entries ----

const THREE_KA9Q: &str = "[[input]]\nname = \"40m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\n\
        ssrc = 7030000\n\n[[input]]\nname = \"20m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\n\
        ssrc = 14030000\nfreq_correction_ppm = -1.5\ncapture_rate_hz = 48000.0\n\n\
        [[input]]\nname = \"15m\"\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\n\
        freq_hz = 21030000.0\ncenter_freq_hz = 21000000.0\nfreq_correction_ppm = 0.8\n";

// ---- MAN-261: CLI > env > file precedence (`resolve`) and coverage

#[test]
fn resolve_returns_one_resolved_input_per_entry_with_its_own_ppm_dial_and_capture_rate() {
    let loaded = loaded_from(THREE_KA9Q);
    let r = resolve(cli_overrides(), &loaded).unwrap();
    let names: Vec<&str> = r.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["40m", "20m", "15m"]);
    assert!(matches!(r.inputs[0].spec, LiveSourceSpec::Ka9q(_)));
    assert!(matches!(r.inputs[2].spec, LiveSourceSpec::Kiwi(_)));
    assert_eq!(r.inputs[0].freq_correction_ppm, 0.0);
    assert_eq!(r.inputs[1].freq_correction_ppm, -1.5);
    assert_eq!(r.inputs[1].capture_rate_hz, Some(48_000.0));
    assert_eq!(r.inputs[2].freq_correction_ppm, 0.8);
    assert_eq!(r.inputs[2].dial_freq_hz, Some(21_000_000.0));
    assert_eq!(r.inputs[0].dial_freq_hz, None);
}

#[test]
fn a_cli_source_flag_replaces_the_input_list_with_a_note() {
    let loaded = loaded_from(THREE_KA9Q);
    let cli = CliOverrides {
        prior: None,
        kiwi: KiwiOpts {
            host: Some("kiwi.example.net".into()),
            port: 8073,
            freq: Some(14_030_000.0),
            password: String::new(),
        },
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert!(r.inputs.is_empty());
    assert!(matches!(r.spec, LiveSourceSpec::Kiwi(_)));
    assert_eq!(r.freq_correction_ppm, 0.0);
    assert_eq!(
        r.notes,
        vec![format!(
            "note: --kiwi-host selects the source; ignoring the 3 [[input]] entries from {}",
            loaded.origin
        )]
    );
}

#[test]
fn per_receiver_cli_flags_are_refused_with_two_or_more_inputs() {
    let loaded = loaded_from(THREE_KA9Q);
    for (cli, flag, key) in [
        (
            CliOverrides {
                prior: None,
                freq_correction_ppm: Some(1.0),
                ..cli_overrides()
            },
            "--freq-correction-ppm",
            "freq_correction_ppm",
        ),
        (
            CliOverrides {
                prior: None,
                dial_freq_hz: Some(7_000_000.0),
                ..cli_overrides()
            },
            "--dial-freq-hz",
            "center_freq_hz",
        ),
        (
            CliOverrides {
                prior: None,
                capture_rate_hz: Some(48_000.0),
                ..cli_overrides()
            },
            "--capture-rate-hz",
            "capture_rate_hz",
        ),
    ] {
        let err = resolve(cli, &loaded).err().unwrap().to_string();
        assert_eq!(
            err,
            format!(
                "{flag} applies to one receiver and {} has 3 [[input]] entries; set {key} in \
                     the entry, or name one receiver with a source flag",
                loaded.origin
            )
        );
    }
}

#[test]
fn realtime_loop_and_replay_epoch_flags_apply_to_every_file_input() {
    let loaded = loaded_from(
        "[[input]]\nname = \"a\"\ntype = \"file\"\npath = \"/x/a.wav\"\n\n\
             [[input]]\nname = \"b\"\ntype = \"file\"\npath = \"/x/b.wav\"\n",
    );
    let cli = CliOverrides {
        prior: None,
        realtime: true,
        replay_epoch: Some(1_700_000_000),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert_eq!(r.inputs.len(), 2);
    assert!(r.replay.paced);
    assert_eq!(r.replay_epoch, Some(1_700_000_000));
}

#[test]
fn single_input_resolution_is_unchanged() {
    let loaded = loaded_from(KIWI_INPUT);
    let r = resolve(cli_overrides(), &loaded).unwrap();
    assert!(r.inputs.is_empty());
    assert!(matches!(r.spec, LiveSourceSpec::Kiwi(_)));
    assert_eq!(r.freq_correction_ppm, 2.5);
    assert_eq!(r.dial_freq_hz, Some(14_000_000.0));
}

#[test]
fn manta_input_env_reaches_run_through_a_one_entry_input_array() {
    use std::ffi::OsString;
    let f = write_temp_file(
        b"[[input]]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nfreq_hz = 14025000.0\n\
              freq_correction_ppm = 0.5\n",
    );
    let vars = [(
        OsString::from("MANTA_INPUT_FREQ_CORRECTION_PPM"),
        OsString::from("1.25"),
    )];
    let loaded = config::load(Some(f.path()), config::Env::Read(&vars)).unwrap();
    let r = resolve(cli_overrides(), &loaded).unwrap();
    assert!(r.inputs.is_empty());
    assert_eq!(r.freq_correction_ppm, 1.25);
}

#[test]
fn select_input_requires_a_name_with_two_or_more_inputs() {
    let mut loaded = loaded_from(THREE_KA9Q);
    let err = select_input(&mut loaded, None, "doctor")
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        format!(
            "manta doctor checks one receiver at a time and {} has 3 inputs: pass --input \
                 40m, --input 20m or --input 15m",
            loaded.origin
        )
    );
}

#[test]
fn select_input_refuses_an_unknown_name() {
    let mut loaded = loaded_from(THREE_KA9Q);
    let err = select_input(&mut loaded, Some("nope"), "check")
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        format!(
            "--input \"nope\" is not one of {}'s inputs (40m, 20m, 15m)",
            loaded.origin
        )
    );
    select_input(&mut loaded, Some("20m"), "check").unwrap();
    assert_eq!(loaded.inputs.len(), 1);
    assert_eq!(loaded.input.shared.freq_correction_ppm, Some(-1.5));
    let r = resolve(cli_overrides(), &loaded).unwrap();
    assert!(r.inputs.is_empty());
    assert_eq!(r.freq_correction_ppm, -1.5);
    assert_eq!(r.capture_rate_hz, Some(48_000.0));
}

#[test]
fn select_input_names_the_streams_of_a_band_plan() {
    let body = "[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"60m\"]\n";
    let mut loaded = loaded_from(body);
    let err = select_input(&mut loaded, None, "doctor")
        .unwrap_err()
        .to_string();
    assert!(
        err.ends_with("has 2 inputs: pass --input kiwi-60m-1 or --input kiwi-60m-2"),
        "{err}"
    );
    let err = select_input(&mut loaded, Some("kiwi"), "doctor")
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        "--input \"kiwi\" is one receiver with 2 streams (kiwi-60m-1, kiwi-60m-2); pass \
             --input kiwi-60m-1 or --input kiwi-60m-2"
    );
    // A named `[input]`'s streams are found by its name, not its type.
    let mut named = loaded_from(
        "[input]\nname = \"mykiwi\"\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"60m\"]\n",
    );
    let err = select_input(&mut named, Some("mykiwi"), "doctor")
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("--input \"mykiwi\" is one receiver with 2 streams"),
        "{err}"
    );
    let err = select_input(&mut named, Some("kiwi"), "doctor")
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not one of"), "{err}");
    let mut loaded = loaded_from(body);
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("v1.wav")),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("ignoring the 2 streams [input].bands plans in")),
        "{:?}",
        r.notes
    );
    select_input(&mut loaded, Some("kiwi-60m-2"), "doctor").unwrap();
    assert_eq!(loaded.inputs.len(), 1);
    assert_eq!(
        loaded.input.source.as_ref().and_then(|s| s.freq_hz()),
        Some(5_362_750.0)
    );
}

#[test]
fn select_input_accepts_the_single_inputs_name_or_none() {
    let mut loaded = loaded_from(KIWI_INPUT);
    select_input(&mut loaded, None, "soak").unwrap();
    select_input(&mut loaded, Some("kiwi"), "soak").unwrap();
    assert!(select_input(&mut loaded, Some("other"), "soak").is_err());
}

#[test]
fn cli_source_flag_discards_a_typed_input_layer_with_a_note() {
    let loaded = loaded_from(KIWI_INPUT);
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("v1.wav")),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert!(matches!(r.spec, LiveSourceSpec::File { .. }));
    assert_eq!(r.freq_correction_ppm, 0.0);
    assert_eq!(r.dial_freq_hz, None);
    assert_eq!(r.notes.len(), 1);
    assert!(r.notes[0].contains("--source selects the source; ignoring [input] (type = \"kiwi\")"));
}

#[test]
fn untyped_input_shared_keys_survive_a_cli_source() {
    let loaded = loaded_from("[input]\nfreq_correction_ppm = 2.5\n");
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("v1.wav")),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert_eq!(r.freq_correction_ppm, 2.5);
    assert!(r.notes.is_empty());
}

#[test]
fn config_source_is_used_when_no_cli_source_is_given() {
    let r = resolve(cli_overrides(), &loaded_from(KIWI_INPUT)).unwrap();
    let LiveSourceSpec::Kiwi(kiwi) = &r.spec else {
        panic!("expected the kiwi source from [input]");
    };
    assert_eq!(kiwi.host.as_deref(), Some("127.0.0.1"));
    assert_eq!(kiwi.freq, Some(14_025_000.0));
    assert_eq!(r.freq_correction_ppm, 2.5);
    assert_eq!(r.dial_freq_hz, Some(14_000_000.0));
}

#[test]
fn cli_ppm_beats_file_ppm() {
    let cli = CliOverrides {
        prior: None,
        freq_correction_ppm: Some(4.0),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from(KIWI_INPUT)).unwrap();
    assert_eq!(r.freq_correction_ppm, 4.0);
}

#[test]
fn explicit_cli_ppm_zero_beats_file_ppm() {
    let cli = CliOverrides {
        prior: None,
        freq_correction_ppm: Some(0.0),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from(KIWI_INPUT)).unwrap();
    assert_eq!(r.freq_correction_ppm, 0.0);
}

#[test]
fn source_iq_flag_sets_iq_on_a_config_file_source() {
    let cli = CliOverrides {
        prior: None,
        source_iq: true,
        ..cli_overrides()
    };
    // Absolute on every OS: "/x/v1.wav" has no drive prefix, so Windows
    // treats it as relative and joins it onto the config dir.
    let wav = std::env::temp_dir().join("x").join("v1.wav");
    let r = resolve(
        cli,
        &loaded_from(&format!(
            "[input]\ntype = \"file\"\npath = '{}'\n",
            wav.display()
        )),
    )
    .unwrap();
    let LiveSourceSpec::File {
        path, source_iq, ..
    } = &r.spec
    else {
        panic!("expected the file source from [input]");
    };
    assert_eq!(path, &wav);
    assert!(*source_iq);
}

// ---- MAN-269: paced and looping file replay

const FILE_INPUT: &str = "[input]\ntype = \"file\"\npath = \"/x/v1.wav\"\niq = true\n";

#[test]
fn realtime_flag_paces_a_cli_file_source() {
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("x.wav")),
        realtime: true,
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from("")).unwrap();
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: false
        }
    );
}

#[test]
fn loop_flag_implies_pacing() {
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("x.wav")),
        loop_replay: true,
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from("")).unwrap();
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: true
        }
    );
}

#[test]
fn input_realtime_and_loop_keys_apply_to_a_config_file_source() {
    let loaded = loaded_from(&format!("{FILE_INPUT}realtime = true\nloop = true\n"));
    let r = resolve(cli_overrides(), &loaded).unwrap();
    assert!(matches!(r.spec, LiveSourceSpec::File { .. }));
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: true
        }
    );
    let r = resolve(
        cli_overrides(),
        &loaded_from(&format!("{FILE_INPUT}realtime = true\n")),
    )
    .unwrap();
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: false
        }
    );
}

#[test]
fn cli_realtime_adds_to_a_config_file_source() {
    let cli = CliOverrides {
        prior: None,
        realtime: true,
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from(FILE_INPUT)).unwrap();
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: false
        }
    );
    let cli = CliOverrides {
        prior: None,
        loop_replay: true,
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded_from(&format!("{FILE_INPUT}realtime = true\n"))).unwrap();
    assert_eq!(
        r.replay,
        ReplayMode {
            paced: true,
            looped: true
        }
    );
}

#[test]
fn a_cli_source_selector_discards_the_files_realtime_and_loop() {
    let loaded = loaded_from(&format!("{FILE_INPUT}realtime = true\nloop = true\n"));
    let cli = CliOverrides {
        prior: None,
        source: Some(PathBuf::from("y.wav")),
        ..cli_overrides()
    };
    let r = resolve(cli, &loaded).unwrap();
    assert_eq!(r.replay, ReplayMode::default());
    let LiveSourceSpec::File { path, .. } = &r.spec else {
        panic!("expected the CLI file source");
    };
    assert_eq!(path, &PathBuf::from("y.wav"));
}

#[test]
fn realtime_with_a_live_source_is_an_error() {
    let cli = CliOverrides {
        prior: None,
        kiwi: KiwiOpts {
            host: Some("192.0.2.1".into()),
            port: 8073,
            freq: Some(7_030_000.0),
            password: String::new(),
        },
        realtime: true,
        loop_replay: true,
        ..cli_overrides()
    };
    let err = resolve(cli, &loaded_from("")).err().unwrap().to_string();
    assert_eq!(
        err,
        "--realtime only applies to a file replay (--source, or input.type = \"file\"); a \
             live source already runs in real time"
    );
}

#[test]
fn loop_with_the_default_audio_device_is_an_error() {
    let cli = CliOverrides {
        prior: None,
        loop_replay: true,
        ..cli_overrides()
    };
    let err = resolve(cli, &loaded_from("")).err().unwrap().to_string();
    assert_eq!(
        err,
        "--loop only applies to a file replay (--source, or input.type = \"file\"); a live \
             source never reaches an end to loop from"
    );
    // The same for a typed non-file [input] the CLI adds --loop to.
    let cli = CliOverrides {
        prior: None,
        loop_replay: true,
        ..cli_overrides()
    };
    let err = resolve(cli, &loaded_from(KIWI_INPUT))
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.starts_with("--loop only applies to a file replay"),
        "{err}"
    );
}

#[test]
fn replay_mode_note_names_the_ignored_keys() {
    let mode = ReplayMode::from_flags(true, false);
    assert_eq!(
        replay_mode_ignored_note("soak", mode, "demo.toml").as_deref(),
        Some(
            "note: soak replays files as fast as it can; ignoring input.realtime/input.loop \
                 from demo.toml"
        )
    );
    assert_eq!(
        replay_mode_ignored_note("doctor", ReplayMode::default(), "demo.toml"),
        None
    );
}

/// `n` samples of a ramp at `fs`, for the `wrap_file_replay` tests.
struct RampSource {
    n: usize,
    cursor: usize,
    fs: f64,
}

impl IqSource for RampSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        0.0
    }
    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        let k = buf.len().min(self.n - self.cursor);
        for (i, s) in buf[..k].iter_mut().enumerate() {
            *s = num_complex::Complex32::new((self.cursor + i) as f32, 0.0);
        }
        self.cursor += k;
        Ok(k)
    }
}

fn ramp(n: usize, fs: f64) -> Box<dyn IqSource> {
    Box::new(RampSource { n, cursor: 0, fs })
}

#[test]
fn wrap_file_replay_is_identity_for_the_default_mode() {
    let first = ramp(10, 1000.0);
    let before = &*first as *const dyn IqSource as *const ();
    let wrapped = wrap_file_replay(
        first,
        ReplayMode::default(),
        "x.wav".into(),
        Box::new(|| bail!("the default mode never reopens")),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(&*wrapped as *const dyn IqSource as *const (), before);
}

/// MAN-453: the paced wrapper gets the run's stop flag, so a stop ends
/// its wait.
#[test]
fn wrap_file_replay_hands_the_stop_flag_to_the_pacing() {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut src = wrap_file_replay(
        ramp(16_000, 8000.0),
        ReplayMode::from_flags(true, false),
        "x.wav".into(),
        Box::new(|| bail!("not looped")),
        stop,
    )
    .unwrap();
    let mut buf = vec![num_complex::Complex32::new(0.0, 0.0); 16_000];
    let start = std::time::Instant::now();
    assert_eq!(src.read(&mut buf).unwrap(), 16_000);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn wrap_file_replay_loops_inside_pacing() {
    // 100 samples at 1 kS/s, read 300 times over: three passes of one
    // paced clock take >= 300 ms. Lower bound only.
    let mut src = wrap_file_replay(
        ramp(100, 1000.0),
        ReplayMode::from_flags(false, true),
        "x.wav".into(),
        Box::new(|| Ok(ramp(100, 1000.0))),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let mut buf = vec![num_complex::Complex32::new(0.0, 0.0); 300];
    let start = std::time::Instant::now();
    let mut filled = 0;
    while filled < buf.len() {
        let n = src.read(&mut buf[filled..]).unwrap();
        assert!(n > 0, "the loop ended after {filled} samples");
        filled += n;
    }
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(290),
        "300 paced samples at 1 kS/s took only {:?}",
        start.elapsed()
    );
    assert_eq!(buf[99].re, 99.0);
    assert_eq!(
        buf[100].re, 0.0,
        "the second pass starts from the first sample"
    );
    assert_eq!(buf[299].re, 99.0);
}

#[test]
fn cli_allowlist_replaces_file_allowlist() {
    let spot = config::SpotFile {
        allowlist: vec!["W1AW".into()],
        ..Default::default()
    };
    let r = resolve_spot(vec!["K1ABC".into()], None, None, None, None, None, &spot);
    assert_eq!(r.allowlist, vec!["K1ABC".to_string()]);
}

#[test]
fn empty_cli_allowlist_keeps_file_allowlist() {
    let spot = config::SpotFile {
        allowlist: vec!["W1AW".into()],
        ..Default::default()
    };
    let r = resolve_spot(Vec::new(), None, None, None, None, None, &spot);
    assert_eq!(r.allowlist, vec!["W1AW".to_string()]);
}

#[test]
fn cli_blocklist_beats_blocklist_path() {
    let spot = config::SpotFile {
        blocklist_path: Some(PathBuf::from("/cfg/bad.txt")),
        notch_path: Some(PathBuf::from("/cfg/n.txt")),
        ..Default::default()
    };
    let r = resolve_spot(
        Vec::new(),
        Some(PathBuf::from("cli.txt")),
        None,
        None,
        None,
        None,
        &spot,
    );
    assert_eq!(r.blocklist, Some(PathBuf::from("cli.txt")));
    assert_eq!(r.notch, Some(PathBuf::from("/cfg/n.txt")));
}

#[cfg(not(feature = "soapy"))]
#[test]
fn soapy_input_type_needs_the_soapy_feature() {
    let loaded = loaded_from(
        "[input]\ntype = \"soapy\"\ndriver = \"d\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
    );
    let err = resolve(cli_overrides(), &loaded).err().unwrap().to_string();
    assert!(
        err.contains("input.type = \"soapy\" needs a manta built with --features soapy"),
        "{err}"
    );
}

#[test]
fn spec_from_file_maps_an_hpsdr_input_without_opening_it() {
    let loaded = loaded_from(
            "[input]\ntype = \"hpsdr\"\nhost = \"receiver.invalid\"\nfreq_hz = 7030000.0\nrate_hz = 48000.0\n",
        );
    let r = resolve(cli_overrides(), &loaded).unwrap();
    let LiveSourceSpec::Hpsdr(h) = &r.spec else {
        panic!("expected the HPSDR source from [input]");
    };
    assert_eq!(h.host.as_deref(), Some("receiver.invalid"));
    assert_eq!(h.port, Some(1024));
    assert_eq!(h.freq, Some(7_030_000.0));
    assert_eq!(h.rate, Some(48_000.0));
    assert_eq!(r.replaced_file_source, None);
}

#[test]
fn spec_from_file_carries_an_attenuation_handle_only_when_asked() {
    let body = "[input]\ntype = \"hpsdr\"\nhost = \"receiver.invalid\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\n";
    let loaded = loaded_from(&format!("{body}auto_attenuation = true\n"));
    let r = resolve(cli_overrides(), &loaded).unwrap();
    let handle = r.spec.auto_attenuation().expect("a handle with the key");
    let reopened = r.spec.clone();
    assert!(std::sync::Arc::ptr_eq(
        &handle,
        &reopened.auto_attenuation().expect("the clone shares it")
    ));
    let r = resolve(cli_overrides(), &loaded_from(body)).unwrap();
    assert!(r.spec.auto_attenuation().is_none());
    // A `--hpsdr-host` spec replaces [input] whole, key included.
    let mut cli = cli_overrides();
    cli.hpsdr.host = Some("192.0.2.10".into());
    cli.hpsdr.freq = Some(7_030_000.0);
    cli.hpsdr.rate = Some(192_000.0);
    assert!(cli.into_spec().auto_attenuation().is_none());
}

#[test]
fn absent_hpsdr_selection_preserves_audio_and_file_sources() {
    let cli = CliOverrides::none();
    assert_eq!(cli.source_selector(), None);
    assert!(matches!(cli.into_spec(), LiveSourceSpec::AudioDevice(None)));

    let mut cli = CliOverrides::none();
    cli.device = Some("test audio".into());
    assert_eq!(cli.source_selector(), Some("--device"));
    assert!(
        matches!(cli.into_spec(), LiveSourceSpec::AudioDevice(Some(name)) if name == "test audio")
    );

    let mut cli = CliOverrides::none();
    cli.source = Some(PathBuf::from("capture.wav"));
    cli.source_iq = true;
    assert_eq!(cli.source_selector(), Some("--source"));
    assert!(
        matches!(cli.into_spec(), LiveSourceSpec::File { path, source_iq: true, .. } if path == std::path::Path::new("capture.wav"))
    );
}

#[test]
fn spec_from_file_maps_a_ka9q_input() {
    let loaded = loaded_from(
        "[input]\ntype = \"ka9q\"\ngroup = \"239.101.6.1\"\nport = 5010\nssrc = 14100000\n\
             interface = \"192.168.1.20\"\n",
    );
    let r = resolve(cli_overrides(), &loaded).unwrap();
    let LiveSourceSpec::Ka9q(k) = &r.spec else {
        panic!("expected the ka9q source from [input]");
    };
    assert_eq!(k.group.as_deref(), Some("239.101.6.1"));
    assert_eq!(k.port, 5010);
    assert_eq!(k.ssrc, Some(14_100_000));
    assert_eq!(k.interface, Some(std::net::Ipv4Addr::new(192, 168, 1, 20)));
    assert_eq!(r.replaced_file_source, None);

    let loaded = loaded_from("[input]\ntype = \"ka9q\"\ngroup = \"cw-20m.local\"\n");
    let LiveSourceSpec::Ka9q(k) = resolve(cli_overrides(), &loaded).unwrap().spec else {
        panic!("expected the ka9q source from [input]");
    };
    assert_eq!(k.group.as_deref(), Some("cw-20m.local"));
    assert_eq!(k.port, 5004);
    assert_eq!(k.ssrc, None);
    assert_eq!(k.interface, None);
}

#[test]
fn ka9q_spec_is_rf_aware_and_reconnectable_and_named_ka9q() {
    let spec = LiveSourceSpec::Ka9q(ka9q_opts(None));
    // radiod reports the channel's frequency, so --dial-freq-hz is an
    // override, not a requirement.
    assert!(spec.is_rf_aware());
    assert!(!needs_dial_freq(true, spec.is_rf_aware(), None));
    assert!(spec.is_reconnectable());
    // The `source` label of manta_source_health and manta_input_*.
    assert_eq!(spec.name(), "ka9q");
}
