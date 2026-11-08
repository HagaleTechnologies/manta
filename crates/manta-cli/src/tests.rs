use super::*;
use crate::daemon_client::*;
use crate::recording::*;
use crate::resolve::*;
use crate::session_epoch::*;
use crate::source_spec::*;
use crate::spot_server::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn cli_definition_passes_clap_debug_assert() {
    Cli::command().debug_assert();
}

/// MAN-133: clap_complete 4.6 writes hidden subcommands and flags into
/// every shell's script, so hiding one would still offer it on <TAB>.
/// Filter them in cli_docs before hiding anything.
#[test]
fn no_command_or_flag_is_hidden() {
    fn walk(cmd: &clap::Command, path: &str, hidden: &mut Vec<String>) {
        for arg in cmd.get_arguments().filter(|a| a.is_hide_set()) {
            hidden.push(format!("{path} --{}", arg.get_id()));
        }
        for sub in cmd.get_subcommands() {
            let p = format!("{path} {}", sub.get_name());
            if sub.is_hide_set() {
                hidden.push(p.clone());
            }
            walk(sub, &p, hidden);
        }
    }
    let mut hidden = Vec::new();
    let mut cmd = Cli::command();
    cmd.build();
    walk(&cmd, "manta", &mut hidden);
    assert!(hidden.is_empty(), "hidden: {hidden:?}");
}

fn rtty_sig() -> manta_engine::RttySignalReport {
    manta_engine::RttySignalReport {
        mark_hz: 14_080_002.4,
        space_hz: 14_079_832.6,
        shift_hz: 170.0,
        measured_shift_hz: 169.8,
        decoded_text: String::new(),
        marked_sample_ts: 2_016_000,
        cleared_sample_ts: None,
        id: 1,
    }
}

#[test]
fn rtty_log_lines_match_the_design() {
    assert_eq!(
            format_rtty_marked(&rtty_sig()),
            "RTTY marked: 14080.002 kHz shift 170 Hz (measured 169.8 Hz); no CW tracks there while it lasts"
        );
    assert_eq!(
        format_rtty_cleared(&rtty_sig(), 446),
        "RTTY cleared: 14080.002 kHz shift 170 Hz, marked for 446 s"
    );
}

/// Codex review on PR #272: the duration is the stream time between
/// the engine's mark and clear timestamps, whatever the replay pace.
#[test]
fn rtty_marked_for_counts_stream_seconds() {
    // Marked at 21.0 s, cleared 446.9 s later at 96 kHz.
    let cleared = 2_016_000 + 42_902_400;
    assert_eq!(rtty_marked_for_secs(&rtty_sig(), cleared, 96_000.0), 446);
    assert_eq!(rtty_marked_for_secs(&rtty_sig(), 2_016_000, 96_000.0), 0);
    assert_eq!(rtty_marked_for_secs(&rtty_sig(), 0, 96_000.0), 0);
}

#[test]
fn rtty_decode_line_lists_each_signal() {
    assert_eq!(rtty_line(&[], 96_000.0), "rtty: 0");
    assert_eq!(
        rtty_line(&[rtty_sig()], 96_000.0),
        "rtty: 1 (14080.002 kHz shift 170 Hz from 21.0 s)"
    );
}
// Derived from manta_spot's constant, never hardcoded, so a cty.dat refresh
// does not break these tests.
fn at_day(d: u64) -> SystemTime {
    manta_spot::vintage::cty_dat_retrieved_at().unwrap() + Duration::from_secs(d * 86_400)
}

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
fn bundled_cty_warning_names_the_age_and_the_fix() {
    let w = bundled_cty_warning(false, at_day(200)).expect("stale");
    let head = format!(
        "warning: the built-in cty.dat is 200 days old (retrieved {})",
        manta_spot::vintage::CTY_DAT_RETRIEVED
    );
    assert!(w.starts_with(&head), "{w}");
    for needle in [
        "https://www.country-files.com/cty/cty.dat",
        "--cty",
        "spot.cty_path",
    ] {
        assert!(w.contains(needle), "{needle}: {w}");
    }
    assert!(!w.contains('\n'), "one line: {w}");
    // Other suites assert these never appear on these commands' stderr.
    for absent in [
        "deprecated",
        "--dial-freq-hz",
        "telnet=",
        "listening:",
        "station_callsign",
        "(os error 2)",
    ] {
        assert!(!w.contains(absent), "{absent}: {w}");
    }
}

#[test]
fn no_warning_at_or_below_the_threshold() {
    assert_eq!(bundled_cty_warning(false, at_day(180)), None);
    assert_eq!(bundled_cty_warning(false, at_day(0)), None);
}

#[test]
fn no_warning_with_an_override() {
    assert_eq!(bundled_cty_warning(true, at_day(10_000)), None);
}

#[test]
fn no_warning_when_the_clock_reads_before_the_retrieval_date() {
    assert_eq!(bundled_cty_warning(false, UNIX_EPOCH), None);
}

#[test]
fn the_stale_cty_warning_is_judged_on_the_cty_in_use() {
    // MAN-316: a live `run` with refresh on may start from the cache's
    // copy; the built-in copy's age alone must not warn then.
    let built_in = list_refresh::InUse::bundled(ListKind::Cty);
    assert!(stale_cty_warning(&built_in, at_day(200)).is_some());
    let cached = list_refresh::InUse::new(
        ListKind::Cty,
        built_in.sha256.clone(),
        list_refresh::Origin::Cache {
            downloaded_at: at_day(190),
        },
    );
    assert_eq!(stale_cty_warning(&cached, at_day(200)), None);
    let pinned = list_refresh::InUse::new(
        ListKind::Cty,
        built_in.sha256.clone(),
        list_refresh::Origin::Pinned(list_refresh::PinSource::Bundled),
    );
    assert_eq!(stale_cty_warning(&pinned, at_day(200)), None);
}

const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

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
fn tables_config(cty: Option<PathBuf>, scp: Option<PathBuf>) -> Result<PipelineConfig> {
    let spot = resolve_spot(
        vec![],
        None,
        None,
        cty,
        scp,
        None,
        &config::SpotFile::default(),
    );
    build_pipeline_config(0.0, &spot, Default::default(), Engine::Legacy).map(|(cfg, _)| cfg)
}
#[test]
fn build_pipeline_config_loads_an_override_cty_and_scp() {
    let cty = write_temp_file(format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT).as_bytes());
    let scp = write_temp_file(b"QQ9ZZZ\nW1AW\n");
    let cfg = tables_config(Some(cty.path().into()), Some(scp.path().into())).unwrap();
    assert!(cfg.cty_table().is_allocated("QQ9ZZZ"));
    assert!(cfg.scp_set().contains("QQ9ZZZ"));
    assert_eq!(cfg.scp_set().len(), 2);
}
// ---- MAN-316: hash pins (Design §5)

/// A `[spot]` with `list_cache_dir`, plus `--cty`/`--cty-sha256`.
fn pinned_spot(
    cty: Option<PathBuf>,
    cty_sha256: Option<&str>,
    list_cache_dir: Option<PathBuf>,
) -> SpotResolved {
    let spot = config::SpotFile {
        list_cache_dir,
        ..Default::default()
    };
    let pins = CliPins {
        cty_sha256: cty_sha256.map(str::to_string),
        scp_sha256: None,
    };
    resolve_spot_pinned(vec![], None, None, cty, None, pins, None, &spot)
}
fn pinned(spot: &SpotResolved) -> Result<(PipelineConfig, [list_refresh::InUse; 2])> {
    build_pipeline_config(0.0, spot, Default::default(), Engine::Legacy)
}
/// Writes `bytes` as `dir`'s cty version file; returns its SHA-256.
fn cache_cty_version(dir: &std::path::Path, bytes: &[u8]) -> String {
    let sha = list_cache::sha256_hex(bytes);
    write_cty_version(dir, &sha, bytes);
    sha
}
/// Writes `bytes` under the cty version file name for `sha`, whatever
/// their own hash.
fn write_cty_version(dir: &std::path::Path, sha: &str, bytes: &[u8]) -> PathBuf {
    let path = list_cache::Cache::new(dir.to_path_buf()).version_path(ListKind::Cty, sha);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
fn bundled_cty_sha256() -> String {
    list_cache::sha256_hex(manta_spot::CTY_DAT.as_bytes())
}
#[test]
fn a_pin_matching_the_bundled_copy_uses_it() {
    let sha = bundled_cty_sha256();
    let (cfg, in_use) = pinned(&pinned_spot(None, Some(&sha), None)).unwrap();
    assert!(cfg.cty_table().is_allocated("K1ABC"));
    assert!(!cfg.cty_table().is_allocated("QQ9ZZZ"));
    assert_eq!(in_use[0].sha256, sha);
    assert!(matches!(
        in_use[0].origin,
        list_refresh::Origin::Pinned(list_refresh::PinSource::Bundled)
    ));
    assert_eq!(in_use[1], list_refresh::InUse::bundled(ListKind::Scp));
}
#[test]
fn a_pin_resolves_from_the_cache_dir() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT);
    let sha = cache_cty_version(dir.path(), bytes.as_bytes());
    let (cfg, in_use) = pinned(&pinned_spot(None, Some(&sha), Some(dir.path().into()))).unwrap();
    assert!(cfg.cty_table().is_allocated("QQ9ZZZ"));
    assert_eq!(in_use[0].sha256, sha);
    let expected = list_cache::Cache::new(dir.path().into()).version_path(ListKind::Cty, &sha);
    match &in_use[0].origin {
        list_refresh::Origin::Pinned(list_refresh::PinSource::Cache(path)) => {
            assert_eq!(path, &expected)
        }
        other => panic!("expected a cache pin, got {other:?}"),
    }
}
#[test]
fn a_pin_with_a_path_checks_the_files_hash() {
    let bytes = format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT);
    let f = write_temp_file(bytes.as_bytes());
    let sha = list_cache::sha256_hex(bytes.as_bytes());
    let (cfg, in_use) = pinned(&pinned_spot(Some(f.path().into()), Some(&sha), None)).unwrap();
    assert!(cfg.cty_table().is_allocated("QQ9ZZZ"));
    assert!(matches!(
        in_use[0].origin,
        list_refresh::Origin::Pinned(list_refresh::PinSource::File(_))
    ));
    let other = bundled_cty_sha256();
    let err = pinned(&pinned_spot(Some(f.path().into()), Some(&other), None))
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        format!(
            "cty.dat file {} has SHA-256 {sha}, not the pinned {other} (--cty-sha256)",
            f.path().display()
        )
    );
}
#[test]
fn a_pin_with_a_mismatched_path_is_not_looked_up_in_the_cache() {
    let f = write_temp_file(manta_spot::CTY_DAT.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let cached = format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT);
    let pin = cache_cty_version(dir.path(), cached.as_bytes());
    let spot = pinned_spot(Some(f.path().into()), Some(&pin), Some(dir.path().into()));
    let err = pinned(&spot).unwrap_err().to_string();
    assert_eq!(
        err,
        format!(
            "cty.dat file {} has SHA-256 {}, not the pinned {pin} (--cty-sha256)",
            f.path().display(),
            bundled_cty_sha256()
        )
    );
}
#[test]
fn an_unmatched_pin_fails_before_io_and_says_where_it_looked() {
    let pin = "0".repeat(64);
    let bundled = bundled_cty_sha256();
    let err = format!(
        "{:#}",
        pinned(&pinned_spot(None, Some(&pin), None)).unwrap_err()
    );
    assert_eq!(
        err,
        format!(
            "--cty-sha256 {pin}: no cty.dat has that SHA-256: the built-in copy is {bundled} \
                 and spot.list_cache_dir is not set; pass the file itself with --cty"
        )
    );
    let dir = tempfile::tempdir().unwrap();
    let missing = list_cache::Cache::new(dir.path().into()).version_path(ListKind::Cty, &pin);
    let err = format!(
        "{:#}",
        pinned(&pinned_spot(None, Some(&pin), Some(dir.path().into()))).unwrap_err()
    );
    assert_eq!(
        err,
        format!(
            "--cty-sha256 {pin}: no cty.dat has that SHA-256: the built-in copy is {bundled} \
                 and {} does not exist; pass the file itself with --cty",
            missing.display()
        )
    );
}
#[test]
fn a_pinned_cache_file_whose_bytes_do_not_match_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let pin = list_cache::sha256_hex(b"some other cty.dat");
    let path = write_cty_version(dir.path(), &pin, manta_spot::CTY_DAT.as_bytes());
    let err = format!(
        "{:#}",
        pinned(&pinned_spot(None, Some(&pin), Some(dir.path().into()))).unwrap_err()
    );
    assert_eq!(
        err,
        format!(
            "--cty-sha256 {pin}: {} has SHA-256 {}, not {pin}",
            path.display(),
            bundled_cty_sha256()
        )
    );
}
/// The pin is the operator's explicit choice of bytes, so a copy the
/// download checks would reject (one entity: no canaries, nowhere near
/// 90% of the prefixes) is still used.
#[test]
fn pins_bypass_download_acceptance() {
    let bundled = manta_spot::cty::Table::bundled();
    assert!(manta_spot::reference::accept_cty(QQ9_ENTITY, &bundled).is_err());
    let dir = tempfile::tempdir().unwrap();
    let sha = cache_cty_version(dir.path(), QQ9_ENTITY.as_bytes());
    let (cfg, _) = pinned(&pinned_spot(None, Some(&sha), Some(dir.path().into()))).unwrap();
    assert!(cfg.cty_table().is_allocated("QQ9ZZZ"));
    assert!(!cfg.cty_table().is_allocated("K1ABC"));
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
fn build_pipeline_config_rejects_a_cty_file_with_no_prefixes() {
    let cty = write_temp_file(b"1A,Sov Mil Order of Malta,246,EU,15,28,41.90,-12.43,-1.0,1A;\n");
    let err = tables_config(Some(cty.path().into()), None)
        .unwrap_err()
        .to_string();
    for needle in [
        "cty.dat file",
        cty.path().to_str().unwrap(),
        "lists no callsign prefixes",
    ] {
        assert!(err.contains(needle), "{err}");
    }
}
#[test]
fn build_pipeline_config_rejects_an_scp_file_with_no_callsigns() {
    let scp = write_temp_file(b"# comment\n!!header\n");
    let err = tables_config(None, Some(scp.path().into()))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("master.scp file") && err.contains("lists no callsigns"),
        "{err}"
    );
}
#[test]
fn build_pipeline_config_reports_a_missing_cty_file() {
    let dir = tempfile::tempdir().unwrap();
    let err = tables_config(Some(dir.path().join("missing.dat")), None)
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("reading cty.dat file"), "{err}");
}
#[test]
fn build_pipeline_config_strips_a_bom_from_cty() {
    let cty = write_temp_file(format!("\u{feff}{QQ9_ENTITY}").as_bytes());
    assert!(tables_config(Some(cty.path().into()), None)
        .unwrap()
        .cty_table()
        .is_allocated("QQ9ZZZ"));
}
#[test]
fn build_pipeline_config_rejects_non_utf8_tables() {
    let file = write_temp_file(&[0xff, 0xfe]);
    for (cty, scp, label) in [
        (Some(file.path().into()), None, "cty.dat"),
        (None, Some(file.path().into()), "master.scp"),
    ] {
        let err = tables_config(cty, scp).unwrap_err().to_string();
        assert!(err.contains(&format!("reading {label} file")), "{err}");
        assert!(err.contains(file.path().to_str().unwrap()), "{err}");
    }
}
#[test]
fn start_spot_server_uses_the_cty_table_it_is_given() {
    let table = std::sync::Arc::new(manta_spot::cty::Table::parse_with_dxcc(
        &GEOGRAPHY_CTY_FIXTURE.replace("K,W,N;", "K,W,N,QQ9;"),
        GEOGRAPHY_DXCC_FIXTURE,
    ));
    let loaded = loaded_from(
            "[server]\nstation_callsign = 'W1AW'\nbind_addr = '127.0.0.1'\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
        );
    let (test_runtime, server) = start_spot_server(
        loaded.server.unwrap(),
        vec![],
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::UNIX_EPOCH,
        0,
        std::sync::Arc::new(manta_spot::reference::ReferenceLists::new(
            table.clone(),
            std::sync::Arc::new(manta_spot::scp::Set::bundled()),
        )),
        &PipelineConfig {
            cty: Some(table.clone()),
            ..Default::default()
        },
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();
    let cty = server.lists.cty.load();
    assert!(std::sync::Arc::ptr_eq(&cty, &table));
    assert!(!geography_is_unresolved(&cty, "QQ9ZZZ"));
    let entry = cty.lookup("QQ9ZZZ").unwrap();
    assert_eq!(
        (&*entry.continent, entry.cq_zone, entry.lat, entry.lon),
        ("NA", 5, 40.0, -75.0)
    );
    let _ = server.shutdown_tx.send(true);
    test_runtime.shutdown_timeout(std::time::Duration::from_secs(1));
}
/// The built-in cty.dat and MASTER.SCP as the shared lists a test's
/// `start_spot_server` needs.
fn bundled_reference_lists() -> std::sync::Arc<manta_spot::reference::ReferenceLists> {
    std::sync::Arc::new(manta_spot::reference::ReferenceLists::new(
        std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        std::sync::Arc::new(manta_spot::scp::Set::bundled()),
    ))
}
// MAN-316: the daemon's per-spot geography check reads the table in use
// now, so a refreshed cty.dat changes it without a restart, and the
// station side is checked against the same (current) table.
#[test]
fn published_spot_geography_follows_a_swapped_cty_table() {
    let lists = bundled_reference_lists();
    let loaded = loaded_from(
            "[server]\nstation_callsign = 'QQ9ZZZ'\nbind_addr = '127.0.0.1'\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
        );
    let (test_runtime, server) = start_spot_server(
        loaded.server.unwrap(),
        vec![],
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::UNIX_EPOCH,
        0,
        lists.clone(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();
    let cty = server.lists.cty.load();
    assert!(geography_is_unresolved(&cty, "QQ9ZZZ"));
    assert!(station_geography_unresolved(&cty, &server.station_callsign));
    // The vendored dxcc.tsv has no QQ9 row, so the test entity gets one
    // here: resolved geography needs an ADIF number as well as an entry.
    lists.cty.store(std::sync::Arc::new(
        manta_spot::cty::Table::parse_with_dxcc(
            &format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT),
            &format!("{}QQ9\t999\tTest DXpedition\n", manta_spot::DXCC_TSV),
        ),
    ));
    let cty = server.lists.cty.load();
    assert!(!geography_is_unresolved(&cty, "QQ9ZZZ"));
    assert!(!station_geography_unresolved(
        &cty,
        &server.station_callsign
    ));
    let _ = server.shutdown_tx.send(true);
    test_runtime.shutdown_timeout(std::time::Duration::from_secs(1));
}
/// MAN-126: doctor's help names the clock check's server flag, the four
/// statuses, and the exit rule scripts rely on.
#[test]
fn doctor_help_lists_ntp_server_and_the_exit_rule() {
    use clap::CommandFactory;
    let mut cli = Cli::command();
    let doctor = cli.find_subcommand_mut("doctor").unwrap();
    let help = doctor.render_long_help().to_string();
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for needle in [
        "--ntp-server",
        "Setup checks",
        "PASS, WARN, FAIL or SKIP",
        "Exits 1 when a check fails",
        "pool.ntp.org",
    ] {
        assert!(
            flat.contains(needle),
            "doctor --help lacks {needle:?}:\n{help}"
        );
    }
}

/// MAN-235: `doctor --help` names every verdict with its `--strict`
/// code, and the code it names is the one `strict_exit_code` returns.
#[test]
fn doctor_help_names_no_json_key() {
    use clap::CommandFactory;
    let mut cli = Cli::command();
    let doctor = cli.find_subcommand_mut("doctor").unwrap();
    let help = doctor.render_long_help().to_string();
    // MAN-139: the report's JSON keys are camelCase now; the help names
    // the concept, not a key.
    assert!(!help.contains("center_freq_hz"), "{help}");
}

#[test]
fn doctor_help_lists_the_strict_code_for_every_verdict() {
    use clap::CommandFactory;
    use manta_engine::Verdict;
    let mut cli = Cli::command();
    let doctor = cli.find_subcommand_mut("doctor").unwrap();
    let strict_help = doctor
        .get_arguments()
        .find(|arg| arg.get_long() == Some("strict"))
        .and_then(|arg| arg.get_long_help())
        .expect("doctor has a --strict flag with long help")
        .to_string();
    let strict_flat = strict_help.split_whitespace().collect::<Vec<_>>().join(" ");
    let help = doctor.render_long_help().to_string();
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for needle in ["--strict", "exits 3", "A failed check still exits 1"] {
        assert!(
            flat.contains(needle),
            "doctor --help lacks {needle:?}:\n{help}"
        );
    }
    // "DECODING exits 0, and NO_SIGNAL, ... and WEAK_NO_DECODE exit 3."
    let exits_3 = strict_flat
        .split("DECODING exits 0, and ")
        .nth(1)
        .and_then(|rest| rest.split(" exit 3").next())
        .unwrap_or_else(|| panic!("--strict help lists no exit-3 verdicts:\n{strict_help}"));
    for (verdict, code) in [
        (Verdict::NoSignal, 3),
        (Verdict::NotCw, 3),
        (Verdict::ActivityNoSnr, 3),
        (Verdict::NoisyNoDecode, 3),
        (Verdict::WeakNoDecode, 3),
        (Verdict::Decoding, 0),
    ] {
        assert_eq!(strict_exit_code(verdict), code, "{verdict:?}");
        let label = verdict.summary().split(" -- ").next().unwrap();
        assert!(
            flat.contains(label) && strict_flat.contains(label),
            "doctor --help does not name {label}:\n{help}"
        );
        let listed = if code == 0 {
            strict_flat.contains(&format!("{label} exits 0"))
        } else {
            exits_3.contains(label)
        };
        assert!(
            listed,
            "--strict help does not give {label} code {code}:\n{strict_help}"
        );
    }
}

/// MAN-254: `manta_engine::soak` waits out the whole --duration even
/// after a --source recording ends, so soak's help must say so and
/// must not borrow run's and doctor's "finishes sooner" promise.
#[test]
fn soak_help_says_a_short_recording_does_not_end_the_run() {
    use clap::CommandFactory;
    let flat = |help: clap::builder::StyledStr| {
        help.to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut cli = Cli::command();
    let soak = cli.find_subcommand_mut("soak").unwrap();
    let short = flat(soak.render_help());
    let long = flat(soak.render_long_help());
    let duration = "The run lasts this long even when a --source recording ends sooner";
    assert!(
        short.contains(duration),
        "soak -h lacks {duration:?}:\n{short}"
    );
    for needle in [
        duration,
        "runs for the full --duration seconds",
        "A --source recording that ends sooner does not end the run",
        "the run still lasts the full --duration",
        "decoding nothing, until the hour is up",
    ] {
        assert!(
            long.contains(needle),
            "soak --help lacks {needle:?}:\n{long}"
        );
    }
    assert!(
        !long.contains("finishes sooner"),
        "soak --help promises an early finish:\n{long}"
    );
}

#[test]
fn help_lists_cty_and_scp_for_every_validating_command() {
    use clap::CommandFactory;
    let cli = Cli::command();
    for name in ["decode", "run", "listen", "soak", "doctor"] {
        let command = cli.find_subcommand(name).unwrap();
        for flag in ["cty", "scp", "cty-sha256", "scp-sha256"] {
            assert!(
                command
                    .get_arguments()
                    .any(|arg| arg.get_long() == Some(flag)),
                "{name} --{flag}"
            );
        }
    }
}

fn write_temp_file(contents: &[u8]) -> tempfile::NamedTempFile {
    use std::io::Write as _;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(contents).unwrap();
    f.flush().unwrap();
    f
}

/// Walk every command/subcommand and every argument, collecting
/// (path, flag-or-<command>, help text) for each help string clap would
/// actually show an operator. `Arg::get_id()` is the Rust field name, so
/// use `get_long()` for the spelling a user types.
fn help_strings(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String, String)>) {
    let mut push = |who: String, text: Option<&clap::builder::StyledStr>| {
        if let Some(t) = text {
            out.push((path.to_string(), who, t.to_string()));
        }
    };
    push("<command>".into(), cmd.get_about());
    push("<command-long>".into(), cmd.get_long_about());
    push("<after-help>".into(), cmd.get_after_help());
    push("<after-long-help>".into(), cmd.get_after_long_help());
    for a in cmd.get_arguments() {
        let name = a
            .get_long()
            .map(|l| format!("--{l}"))
            .unwrap_or_else(|| format!("<{}>", a.get_id()));
        push(name.clone(), a.get_help());
        push(name, a.get_long_help());
    }
    for sub in cmd.get_subcommands() {
        help_strings(sub, &format!("{path} {}", sub.get_name()), out);
    }
}

/// Words an operator who has never seen this repo's specs, roadmap or
/// ticket tracker must not meet. Case-insensitive and whole-word: `SPEC`
/// and `spec` are the same leak to an operator, but "special-event
/// call" is ordinary English and must not trip the guard.
const JARGON: &[&str] = &[
    r"(?i)\bspec\b",
    r"(?i)\barchitecture\b",
    r"(?i)\broadmap\b",
    r"(?i)\bappendix\b",
    r"\u{a7}",
    r"\bM[0-4]\b",
    r"(?i)\bMAN-\d+",
];

fn jargon_res() -> Vec<regex::Regex> {
    JARGON
        .iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect()
}

/// MAN-135: `--help` is read by operators who have never seen this
/// repo's specs, roadmap, or ticket tracker. Contributor-facing
/// provenance belongs in `//` comments, which clap never republishes;
/// `///` on a clap field or variant IS user-facing copy.
#[test]
fn help_text_is_free_of_internal_process_jargon() {
    use clap::CommandFactory as _;
    let res = jargon_res();

    let mut out = Vec::new();
    help_strings(&Cli::command(), "manta", &mut out);

    let mut bad = Vec::new();
    for (path, who, text) in out {
        for re in &res {
            if let Some(m) = re.find(&text) {
                bad.push(format!("{path} {who}: {:?} -- {text}", m.as_str()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "internal jargon in --help:\n{}",
        bad.join("\n")
    );
}

/// MAN-76: `manta config init` writes the scaffold into an operator's
/// own file, so it is operator-facing copy just as `--help` is.
#[test]
fn scaffold_is_free_of_internal_process_jargon() {
    let res = jargon_res();
    let mut bad = Vec::new();
    for (n, line) in config_cmd::SCAFFOLD.lines().enumerate() {
        for re in &res {
            if let Some(m) = re.find(line) {
                bad.push(format!("line {}: {:?} -- {line}", n + 1, m.as_str()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "internal jargon in the config scaffold:\n{}",
        bad.join("\n")
    );
}

/// `hsmm` was measured against `legacy` and failed its promotion gate
/// (docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md), so no
/// `--engine` help may tell an operator it is unmeasured.
#[test]
fn engine_help_does_not_call_hsmm_unmeasured() {
    use clap::CommandFactory as _;
    let re = regex::Regex::new(r"(?i)unmeasured|not\s+(yet\s+)?measured").unwrap();
    let mut out = Vec::new();
    help_strings(&Cli::command(), "manta", &mut out);
    let engine: Vec<_> = out.iter().filter(|(_, who, _)| who == "--engine").collect();
    // decode, oracle and run each take --engine; an empty walk would
    // pass vacuously. Count commands, not strings: each arg yields both
    // a short and a long help string.
    let commands: std::collections::BTreeSet<_> = engine.iter().map(|(path, _, _)| path).collect();
    assert!(
        commands.len() >= 3,
        "found --engine on {commands:?}, expected decode, oracle and run"
    );
    let bad: Vec<String> = engine
        .iter()
        .filter(|(_, _, text)| re.is_match(text))
        .map(|(path, _, text)| format!("{path}: {text}"))
        .collect();
    assert!(
        bad.is_empty(),
        "--engine help calls hsmm unmeasured:\n{}",
        bad.join("\n")
    );
}

/// MAN-135: a flag with no `///` comment renders as a blank line under
/// `--help`, which tells an operator nothing at all.
#[test]
fn every_flag_has_non_empty_help() {
    use clap::CommandFactory as _;
    fn walk(cmd: &clap::Command, path: &str, bad: &mut Vec<String>) {
        for a in cmd.get_arguments() {
            let text = a
                .get_help()
                .or_else(|| a.get_long_help())
                .map(|t| t.to_string())
                .unwrap_or_default();
            if text.trim().is_empty() {
                // `get_id()` is the Rust field name (`kiwi_freq_hz`);
                // report the spelling an operator actually types.
                let name = a
                    .get_long()
                    .map(|l| format!("--{l}"))
                    .unwrap_or_else(|| format!("<{}>", a.get_id()));
                bad.push(format!("{path} {name}"));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), bad);
        }
    }
    let mut bad = Vec::new();
    walk(&Cli::command(), "manta", &mut bad);
    assert!(bad.is_empty(), "flags with empty help:\n{}", bad.join("\n"));
}

/// MAN-135: frequencies and sample rates should end in `-hz`; ports and
/// gains should not. Catches the next `--foo-rate` before review does.
#[test]
fn every_hz_valued_flag_is_named_hz() {
    use clap::CommandFactory as _;
    // A frequency-ish name that already carries a DIFFERENT explicit unit
    // is self-describing and must not be forced to `-hz`:
    // `--freq-correction-ppm` is parts-per-million, not hertz. Only
    // unitless frequency/rate names are ambiguous to an operator.
    const OTHER_UNIT_SUFFIXES: [&str; 1] = ["-ppm"];
    fn walk(cmd: &clap::Command, path: &str, bad: &mut Vec<String>) {
        for a in cmd.get_arguments() {
            if let Some(long) = a.get_long() {
                let is_freq_or_rate = long.contains("freq") || long.contains("rate");
                let has_explicit_unit =
                    long.ends_with("-hz") || OTHER_UNIT_SUFFIXES.iter().any(|s| long.ends_with(s));
                if is_freq_or_rate && !has_explicit_unit {
                    bad.push(format!("{path} --{long}"));
                }
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), bad);
        }
    }
    let mut bad = Vec::new();
    walk(&Cli::command(), "manta", &mut bad);
    assert!(
        bad.is_empty(),
        "frequency/rate flags without a -hz suffix:\n{}",
        bad.join("\n")
    );
}

/// MAN-453: every recording waits out the rest of one 5 s deadline, then
/// the rest of one 2 s publication grace after it, not a grace each.
#[test]
fn recording_finish_budget_shares_one_grace_after_the_deadline() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let s = std::time::Duration::from_secs;
    assert_eq!(
        recording_finish_budget(deadline, deadline - s(3)),
        (s(3), RECORDING_PUBLICATION_GRACE)
    );
    assert_eq!(
        recording_finish_budget(deadline, deadline + s(1)),
        (s(0), RECORDING_PUBLICATION_GRACE - s(1))
    );
    assert_eq!(
        recording_finish_budget(deadline, deadline + s(9)),
        (s(0), s(0))
    );
}

/// MAN-453: recordings finish inside the client drain, so the grace adds
/// nothing to the stop budget `TimeoutStopSec=60` covers.
#[test]
fn recording_finalization_ends_inside_the_client_drain() {
    assert!(RECORDING_FINISH_TIMEOUT + RECORDING_PUBLICATION_GRACE <= SHUTDOWN_DRAIN_DEADLINE);
}

/// MAN-45 (PR #63 round-16 finding): the outer registry-wide deadline
/// must never fire before a handler's own drain deadline, or
/// `shutdown_timeout` aborts a drain that was still inside its budget
/// and the abandoned queue goes uncounted again -- the exact failure
/// rounds 15 and 16 both landed on from different directions. The
/// margin covers task scheduling, not another spot's write.
///
/// MAN-45 remediate (round-16 P1, finding 2): `CLIENT_DRAIN_DEADLINE`
/// alone under-counts the true worst case -- a handler already mid-
/// write when shutdown fires doesn't even START its own drain
/// deadline until that in-progress `select!` branch resolves. Asserts
/// the FULL relationship (`2 * telnet::WRITE_TIMEOUT +
/// CLIENT_DRAIN_DEADLINE`, telnet's live-spot write being the largest
/// such in-progress branch across all three handlers), not just the
/// drain deadline in isolation -- see `SHUTDOWN_DRAIN_DEADLINE`'s own
/// doc comment for the full argument.
#[test]
fn the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline() {
    let worst_case_before_drain_starts = 2 * manta_server::telnet::WRITE_TIMEOUT;
    let true_worst_case =
        worst_case_before_drain_starts + manta_server::tasks::CLIENT_DRAIN_DEADLINE;
    assert!(
        SHUTDOWN_DRAIN_DEADLINE > true_worst_case,
        "SHUTDOWN_DRAIN_DEADLINE ({SHUTDOWN_DRAIN_DEADLINE:?}) must exceed the true \
             worst case of {true_worst_case:?} (2 * telnet::WRITE_TIMEOUT = \
             {worst_case_before_drain_starts:?}, the largest in-progress `select!` branch \
             a handler can already be running when shutdown fires, plus \
             CLIENT_DRAIN_DEADLINE = {:?} for its own drain loop once it gets there)",
        manta_server::tasks::CLIENT_DRAIN_DEADLINE,
    );
}

/// The status task retains a joinable handle and exits promptly on its
/// own stop signal, even with an hour-long interval. The separate
/// broadcast-order test pins when that join happens relative to clients.
#[test]
fn the_status_line_task_is_retained_and_joins_promptly_on_shutdown() {
    let cfg_file = write_temp_file(
        br#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                status_interval_secs = 3600
                "#,
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let handle = server
        .status_line
        .lock()
        .unwrap()
        .take()
        .expect("a non-zero status_interval_secs must leave a joinable handle on SpotServer");

    let _ = server.status_shutdown_tx.send(true);
    // One hour of configured interval against a one-second budget: this
    // can only pass because the task's `shutdown.changed()` arm is
    // `biased`-first and returns without waiting for the next tick.
    let joined = rt
        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(1), handle).await });
    assert!(
        joined.is_ok(),
        "the status task must be joinable within a scheduler poll of shutdown, \
             not at the next status_interval_secs tick"
    );
    assert!(
        joined.unwrap().is_ok(),
        "the status task must exit cleanly, not panic"
    );
}

/// MAN-256: a held watch borrow blocks the client shutdown send. The
/// status task must finish while that borrow is held, proving that the
/// production helper joins it before any client can observe shutdown.
/// Checking only after a receiver wakes would race the status task.
#[test]
fn the_status_line_task_has_exited_before_client_shutdown_is_broadcast() {
    let cfg_file = write_temp_file(
        br#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                status_interval_secs = 3600
                "#,
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let status_task = server
        .status_line
        .lock()
        .unwrap()
        .as_ref()
        .expect("status task enabled")
        .abort_handle();
    // Exercise the optional comparator join too. Its timer must be
    // created inside the runtime, and its stop still follows the client
    // broadcast. No external RBN connection is needed for this check.
    let mut client_shutdown = server.shutdown_tx.subscribe();
    let comparator = rt.spawn(async move {
        client_shutdown
            .wait_for(|stopping| *stopping)
            .await
            .unwrap();
    });
    let comparator_task = comparator.abort_handle();
    *server.rbn_compare.lock().unwrap() = Some(comparator);
    let held = server.shutdown_tx.borrow();
    let finished = std::thread::scope(|scope| {
        let shutdown = scope.spawn(|| signal_spot_server_shutdown(&rt, &server));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !status_task.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let finished = status_task.is_finished();
        assert!(!*held, "clients must not have received shutdown yet");
        if !finished {
            // A miswired status receiver must fail rather than hang the join.
            status_task.abort();
        }
        drop(held);
        shutdown.join().unwrap();
        finished
    });
    assert!(comparator_task.is_finished(), "comparator must be joined");
    assert!(
        finished,
        "the status task must exit before client shutdown is broadcast"
    );
    shutdown_runtime_after_drain(rt, &server.tasks);
}

#[test]
fn merge_cli_engine_prefers_the_explicit_cli_flag_over_the_file() {
    // SPEC v2 §7: --engine overrides [decode]'s engine key when both
    // are given -- this is the exact precedence rule Command::Run's
    // (and, since MAN-166's final-review fix batch, Decode's/Oracle's)
    // handler relies on `merge_cli_engine` for.
    let file_decode = manta_decode::decoder::DecodeConfig {
        engine: Engine::Legacy,
        ..manta_decode::decoder::DecodeConfig::default()
    };
    let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
    assert_eq!(resolved.engine, Engine::EdgeLegacy);
}

#[test]
fn merge_cli_engine_falls_back_to_the_file_engine_when_the_flag_is_absent() {
    let file_decode = manta_decode::decoder::DecodeConfig {
        engine: Engine::EdgeLegacy,
        ..manta_decode::decoder::DecodeConfig::default()
    };
    let resolved = merge_cli_engine(None, file_decode);
    assert_eq!(resolved.engine, Engine::EdgeLegacy);
}

#[test]
fn merge_cli_engine_leaves_every_other_decode_field_from_the_file_untouched() {
    // The CLI has no flag for sigma_u/beam/etc. -- only `engine` may be
    // overridden; everything else must come through verbatim from the
    // file-derived DecodeConfig.
    let mut file_decode = manta_decode::decoder::DecodeConfig::default();
    file_decode.evidence.sigma_u = 0.31;
    file_decode.hsmm.beam = 10;
    let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
    assert_eq!(resolved.evidence.sigma_u, 0.31);
    assert_eq!(resolved.hsmm.beam, 10);
}

#[test]
fn load_decode_config_file_defaults_when_no_server_config_given() {
    let cfg = load_decode_config_file(None).unwrap();
    assert_eq!(cfg.engine, Engine::Legacy);
    assert_eq!(
        cfg.evidence.sigma_u,
        manta_decode::evidence::EvidenceConfig::default().sigma_u
    );
}

#[test]
fn load_decode_config_file_reads_the_decode_table_from_server_config() {
    let f = write_temp_file(
        br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "edge-legacy"
            sigma_u = 0.31
            beam = 10
            "#,
    );
    let cfg = load_decode_config_file(Some(f.path())).unwrap();
    assert_eq!(cfg.engine, Engine::EdgeLegacy);
    assert_eq!(cfg.evidence.sigma_u, 0.31);
    assert_eq!(cfg.hsmm.beam, 10);
}

#[test]
fn load_decode_config_file_rejects_zero_fallback_hops() {
    let f = write_temp_file(b"[decode]\nfallback_hops = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_inverted_tau_hi_bounds() {
    // Codex review, PR #161 round 2: this would otherwise panic in
    // f64::clamp on the first speed update instead of failing to load.
    let f = write_temp_file(b"[decode]\ntau_hi_bounds_ms = [400.0, 100.0]\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nonpositive_tau_lo_ms() {
    let f = write_temp_file(b"[decode]\ntau_lo_ms = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
    let f = write_temp_file(b"[decode]\ntau_lo_ms = -5.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_accepts_valid_tau_bounds() {
    let f = write_temp_file(b"[decode]\ntau_lo_ms = 450.0\ntau_hi_bounds_ms = [120.0, 380.0]\n");
    let cfg = load_decode_config_file(Some(f.path())).unwrap();
    assert_eq!(cfg.demod.tau_lo_ms, 450.0);
    assert_eq!(cfg.demod.tau_hi_bounds_ms, (120.0, 380.0));
}

#[test]
fn load_decode_config_file_rejects_nonpositive_timing_sigma() {
    // Codex review, PR #161 round 3: this would otherwise produce
    // infinite/NaN confidence scores instead of failing to load.
    let f = write_temp_file(b"[decode]\ntiming_sigma = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
    let f = write_temp_file(b"[decode]\ntiming_sigma = -0.5\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_timing_sigma_too_small_to_avoid_underflow() {
    // Codex review, PR #161 round 5: 1e-30 is finite and > 0.0 (so
    // round 3's original check alone would accept it), but
    // beam::log_likelihood's `2.0 * sigma * sigma` underflows to
    // exactly 0.0 in f32, giving NaN confidence for a perfectly-timed
    // candidate.
    let f = write_temp_file(b"[decode]\ntiming_sigma = 1e-30\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn prior_cli_override_wins_and_rejects_hsmm() {
    use manta_decode::config_file::PriorSwitch;
    let cfg = manta_decode::decoder::DecodeConfig {
        prior_enabled: true,
        ..Default::default()
    };
    let cfg = merge_cli_prior(Some(PriorSwitch::Off), cfg, "test").unwrap();
    assert!(!cfg.prior_enabled);
    let cfg = merge_cli_prior(Some(PriorSwitch::On), cfg, "test").unwrap();
    assert!(cfg.prior_enabled);
    let mut cfg = cfg;
    cfg.engine = Engine::Hsmm;
    assert!(merge_cli_prior(None, cfg, "test").is_err());
    assert!(Cli::try_parse_from(["manta", "decode", "test.wav", "--prior", "on"]).is_ok());
    assert!(Cli::try_parse_from(["manta", "run", "--prior", "off"]).is_ok());
    assert!(Cli::try_parse_from(["manta", "decode", "test.wav", "--prior", "maybe"]).is_err());
}

#[test]
fn config_defaults_and_validation() {
    let defaults = load_decode_config_file(None).unwrap();
    assert!(!defaults.prior_enabled);
    assert_eq!(defaults.prior_weight, 2.0);
    assert_eq!(defaults.reading_margin, 3.0);
    assert_eq!(defaults.held_readings, 4);
    let file = write_temp_file(
        b"[decode]\nprior = 'on'\nprior_weight = 1.5\nreading_margin = 2.0\nheld_readings = 8\n",
    );
    let cfg = load_decode_config_file(Some(file.path())).unwrap();
    assert!(cfg.prior_enabled);
    assert_eq!(
        (cfg.prior_weight, cfg.reading_margin, cfg.held_readings),
        (1.5, 2.0, 8)
    );
    for invalid in [
        "prior = 'maybe'",
        "prior_weight = -0.1",
        "prior_weight = nan",
        "reading_margin = 0.01",
        "reading_margin = inf",
        "held_readings = 0",
        "held_readings = 17",
    ] {
        let file = write_temp_file(format!("[decode]\n{invalid}\n").as_bytes());
        assert!(
            load_decode_config_file(Some(file.path())).is_err(),
            "{invalid}"
        );
    }
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
fn load_decode_config_file_rejects_zero_beam_width() {
    let f = write_temp_file(b"[decode]\nbeam_width = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_zero_hsmm_beam() {
    let f = write_temp_file(b"[decode]\nbeam = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_zero_sigma_u() {
    let f = write_temp_file(b"[decode]\nsigma_u = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nan_sigma_u() {
    let f = write_temp_file(b"[decode]\nsigma_u = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_sigma_u_too_small_to_avoid_underflow() {
    // Codex review, PR #161 round 20: 1e-30 is finite and > 0.0 (so
    // the original check alone passed it), but sigma_u * sigma_u
    // underflows to exactly 0.0 in f32.
    let f = write_temp_file(b"[decode]\nsigma_u = 1e-30\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_empty_seed_units_hops() {
    let f = write_temp_file(b"[decode]\nseed_units_hops = []\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_nonpositive_seed_unit() {
    let f = write_temp_file(b"[decode]\nseed_units_hops = [9.0, 0.0, 18.0]\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_seed_unit_below_u_min() {
    let f = write_temp_file(b"[decode]\nseed_units_hops = [1.0]\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_seed_unit_above_u_max() {
    let f = write_temp_file(b"[decode]\nseed_units_hops = [100.0]\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_zero_conf_kappa() {
    let f = write_temp_file(b"[decode]\nconf_kappa = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_zero_dur_sigma() {
    let f = write_temp_file(b"[decode]\ndur_sigma = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_dur_sigma_too_small_to_avoid_underflow() {
    // Codex review, PR #161 round 16: 1e-30 is finite and > 0.0 (so
    // round 5's original check alone passed it), but 2*dur_sigma^2
    // underflows to exactly 0.0 in f32.
    let f = write_temp_file(b"[decode]\ndur_sigma = 1e-30\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nonpositive_hold_dits() {
    let f = write_temp_file(b"[decode]\nhold_dits = 0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_hold_dits_that_overflows_max_retain() {
    // Codex review, PR #161 round 6: a large but individually-plausible
    // hold_dits (300) at the default u_max (56.0) computes h = 16800,
    // far past Evidence's MAX_RETAIN (4096).
    let f = write_temp_file(b"[decode]\nhold_dits = 300\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_a_hold_dits_that_only_overflows_after_rounding() {
    // Codex review, PR #161 round 20: hold_dits=73.14 at the default
    // u_max (56.0) computes an UNROUNDED product of 4095.84 -- under
    // MAX_RETAIN (4096) -- but Evidence::set_u_ref rounds before
    // enforcing the cap, and round(4095.84) = 4096, which is not
    // "well under" MAX_RETAIN.
    let f = write_temp_file(b"[decode]\nhold_dits = 73.14\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_accepts_the_default_hold_dits() {
    let f = write_temp_file(b"[decode]\n");
    assert!(load_decode_config_file(Some(f.path())).is_ok());
}

#[test]
fn load_decode_config_file_rejects_nan_speed_alpha() {
    let f = write_temp_file(b"[decode]\nspeed_alpha = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_negative_speed_alpha() {
    let f = write_temp_file(b"[decode]\nspeed_alpha = -0.1\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nan_mark_insert_penalty() {
    let f = write_temp_file(b"[decode]\nmark_insert_penalty = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_infinite_noise_min_bias_db() {
    let f = write_temp_file(b"[decode]\nnoise_min_bias_db = inf\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_infinite_spectral_min_bias_db() {
    let f = write_temp_file(b"[decode]\nspectral_min_bias_db = inf\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_infinite_spectral_beta() {
    let f = write_temp_file(b"[decode]\nspectral_beta = inf\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_negative_spectral_beta() {
    // Codex review, PR #178 round 4: a negative value makes
    // NoiseTracker's max() always discard the spectral term,
    // silently disabling the discount instead of reporting the
    // operator's sign-typo config error.
    let f = write_temp_file(b"[decode]\nspectral_beta = -0.5\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_accepts_zero_spectral_beta() {
    // 0.0 is a valid, explicit "no spectral discount" value, not a
    // sign error -- must stay legal.
    let f = write_temp_file(b"[decode]\nspectral_beta = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_ok());
}

#[test]
fn load_decode_config_file_rejects_nan_refine_bw_hz() {
    let f = write_temp_file(b"[decode]\nrefine_bw_hz = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_infinite_refine_bw_hz() {
    let f = write_temp_file(b"[decode]\nrefine_bw_hz = inf\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_accepts_zero_refine_bw_hz() {
    // 0.0 is the documented "disabled" sentinel, not an error.
    let f = write_temp_file(b"[decode]\nrefine_bw_hz = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_ok());
}

#[test]
fn load_decode_config_file_accepts_a_positive_refine_bw_hz() {
    let f = write_temp_file(b"[decode]\nrefine_bw_hz = 30.0\n");
    let cfg = load_decode_config_file(Some(f.path())).unwrap();
    assert_eq!(cfg.refine_bw_hz, 30.0);
}

#[test]
fn load_decode_config_file_rejects_negative_refine_bw_hz() {
    // Codex review, PR #178: a negative value (e.g. a `-30` sign
    // typo) isn't `> 0.0`, so decoder_input's own bypass check would
    // silently treat it as disabled instead of reporting the error --
    // the documented disabled sentinel is specifically 0.0.
    let f = write_temp_file(b"[decode]\nrefine_bw_hz = -30.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_negative_lookahead_dits() {
    let f = write_temp_file(b"[decode]\nlookahead_dits = -1.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nan_lookahead_dits() {
    let f = write_temp_file(b"[decode]\nlookahead_dits = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_accepts_zero_lookahead_dits() {
    let f = write_temp_file(b"[decode]\nlookahead_dits = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_ok());
}

#[test]
fn load_decode_config_file_rejects_nonpositive_noise_window_ms() {
    let f = write_temp_file(b"[decode]\nnoise_window_ms = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_infinite_noise_window_ms() {
    let f = write_temp_file(b"[decode]\nnoise_window_ms = inf\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nan_hysteresis() {
    // Codex review, PR #161 round 4: NaN makes every `Demod::step`
    // comparison false, silently disabling Legacy's decode entirely.
    let f = write_temp_file(b"[decode]\nhyst_frac = nan\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_out_of_range_hysteresis() {
    // hyst_frac >= 0.5 pushes the band's outer edges to or past the
    // rails; <= 0.0 collapses it to a single point (no hysteresis).
    let f = write_temp_file(b"[decode]\nhyst_frac = 0.5\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
    let f = write_temp_file(b"[decode]\nhyst_frac = -1.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nonpositive_debounce_ms() {
    let f = write_temp_file(b"[decode]\ndebounce_ms = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

#[test]
fn load_decode_config_file_rejects_nonpositive_flush_gap_dits() {
    // Codex review, PR #161 round 4: <= 0 forces an instant/premature
    // word flush on every hop.
    let f = write_temp_file(b"[decode]\nflush_gap_dits = 0.0\n");
    assert!(load_decode_config_file(Some(f.path())).is_err());
}

/// Regression: an earlier version of this task rejected `engine =
/// "hsmm"` at TOML-deserialize time (inside `load_decode_config_file`,
/// i.e. BEFORE `merge_cli_engine` ever runs), which meant an explicit
/// `--engine legacy` could never override a config file staging
/// `engine = "hsmm"` -- the file's parse error fired first, and the
/// override never got a chance to apply. As of Task 11, `hsmm` is no
/// longer rejected anywhere in this path, but the precedence rule this
/// test protects (an explicit `--engine` beats the file's `engine` key)
/// still matters, so it's kept with `hsmm` as the file-staged value to
/// prove `merge_cli_engine` reads the CLI override, not the file, when
/// both are given.
#[test]
fn cli_engine_override_beats_a_hsmm_staged_file() {
    let f = write_temp_file(
        br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "hsmm"
            "#,
    );
    let file_decode = load_decode_config_file(Some(f.path())).unwrap();
    assert_eq!(
        file_decode.engine,
        Engine::Hsmm,
        "the file's own value must still be hsmm going into the merge"
    );
    let result = merge_cli_engine(Some(Engine::Legacy), file_decode);
    assert_eq!(
        result.engine,
        Engine::Legacy,
        "an explicit --engine must override a hsmm-staged file"
    );
}

/// The other direction: with NO CLI override, a file staging `engine =
/// "hsmm"` is honored (not rejected) -- `hsmm` is a fully implemented,
/// reviewed engine (Task 8) with no CLI-level gate as of Task 11.
#[test]
fn hsmm_staged_file_is_honored_without_a_cli_override() {
    let f = write_temp_file(
        br#"
            [decode]
            engine = "hsmm"
            "#,
    );
    let file_decode = load_decode_config_file(Some(f.path())).unwrap();
    let result = merge_cli_engine(None, file_decode);
    assert_eq!(result.engine, Engine::Hsmm);
}

#[test]
fn deprecation_notices_fire_only_for_the_replaced_spellings() {
    fn notices(argv: &[&str]) -> Vec<Deprecation> {
        deprecations(argv.iter().map(|s| s.to_string()))
    }
    // D-3: the daemon-via-listen path is deprecated ...
    assert_eq!(
        notices(&["manta", "listen", "--server-config", "m.toml"]),
        vec![Deprecation::ListenVerb, Deprecation::ServerConfigFlag]
    );
    assert_eq!(
        notices(&["manta", "listen", "--config", "m.toml"]),
        vec![Deprecation::ListenVerb]
    );
    // ... the ad hoc audio path the ticket title preserves is NOT.
    assert_eq!(notices(&["manta", "listen", "--device", "hw:1"]), vec![]);
    assert_eq!(notices(&["manta", "listen"]), vec![]);
    // The flag is deprecated under either verb.
    assert_eq!(
        notices(&["manta", "run", "--server-config", "m.toml"]),
        vec![Deprecation::ServerConfigFlag]
    );
    // `--flag=value` form must be caught too.
    assert_eq!(
        notices(&["manta", "run", "--server-config=m.toml"]),
        vec![Deprecation::ServerConfigFlag]
    );
    // The canonical spelling is silent.
    assert_eq!(notices(&["manta", "run", "--config", "m.toml"]), vec![]);
    assert_eq!(notices(&["manta", "run", "--device", "hw:1"]), vec![]);
    // Other subcommands are never implicated.
    assert_eq!(notices(&["manta", "decode", "/tmp/v1.wav"]), vec![]);
    assert_eq!(notices(&["manta", "soak", "--duration", "10"]), vec![]);
    // `status` is scanned like any other verb -- which is only
    // honest because `status` now ACCEPTS the flag the notice tells
    // the operator to switch to (Codex review, PR #95); see
    // `status_accepts_the_canonical_config_flag_the_deprecation_notice_names`.
    assert_eq!(
        notices(&["manta", "status", "--server-config", "m.toml"]),
        vec![Deprecation::ServerConfigFlag]
    );
    assert_eq!(notices(&["manta", "status", "--config", "m.toml"]), vec![]);
}

/// Codex review, PR #95: `warn_deprecations` scans raw argv without
/// knowing the verb, so `manta status --server-config m.toml` printed
/// "use `--config` instead" while `status` accepted no `--config` at
/// all -- the notice pointed operators at a spelling clap rejected.
/// Both spellings must now parse to the same field.

#[test]
fn shutdown_runtime_after_drain_awaits_a_tracked_task_to_completion() {
    // Regression (round-9/round-10 review, verified against tokio
    // 1.53's own source): `Runtime::shutdown_timeout`'s `duration`
    // parameter only bounds the BLOCKING thread pool's shutdown -- the
    // async executor itself is torn down synchronously and
    // immediately via `self.handle.inner.shutdown()`. An earlier
    // version of this function papered over that with a fixed blind
    // `sleep` before `shutdown_timeout`: real scheduler time, but no
    // guarantee the task actually FINISHED before the sleep elapsed.
    // This test spawns a task INTO the tracked `ClientTasks` registry
    // that only sets a flag after being signaled AND doing a small
    // amount of real async work (standing in for a socket write) --
    // proving `shutdown_runtime_after_drain` genuinely awaits its
    // completion rather than guessing a duration.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let drained = Arc::new(AtomicBool::new(false));
    let drained_task = drained.clone();
    let tasks = manta_server::tasks::new_client_tasks();

    rt.block_on({
        let tasks = tasks.clone();
        async move {
            tasks.lock().await.spawn(async move {
                let _ = rx.changed().await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                drained_task.store(true, Ordering::SeqCst);
            });
        }
    });

    let _ = tx.send(true);
    shutdown_runtime_after_drain(rt, &tasks);

    assert!(
            drained.load(Ordering::SeqCst),
            "the tracked task must have been genuinely awaited to completion before the runtime shut down"
        );
}

/// MAN-64 (PR #76 review round 7): `manta_source_health` had no failure
/// transition at all -- a fatal source read tore the daemon down with
/// the gauge still reading 1. The metrics listener is spawned bare (it
/// takes neither `ClientTasks` nor the shutdown watch), so writing 0
/// here, BEFORE the drain signal, is observable only for whatever
/// fraction of the `SHUTDOWN_DRAIN_DEADLINE` window
/// `shutdown_runtime_after_drain`'s `await_all` happens to keep the
/// runtime alive -- the full window when a telnet/JSON/WS client is
/// genuinely still draining, but effectively zero time in the ordinary
/// scrape-only deployment, where `await_all` returns almost immediately.
/// See `record_terminal_source_health`'s own doc comment for the full
/// corrected claim.
#[test]
fn fatal_listen_error_flips_source_health_to_zero() {
    let metrics = manta_server::metrics::Metrics::new();
    metrics.set_source_health("hpsdr", true);
    record_terminal_source_health(&metrics, "hpsdr", &Err(anyhow!("source read failed")));
    assert!(
        metrics
            .render_prometheus_text()
            .contains("manta_source_health{source=\"hpsdr\"} 0"),
        "a fatal listen error must be reported as unhealthy"
    );
}

/// A clean end of stream is NOT a source failure: file replay reaching
/// EOF and a Ctrl-C stop both return `Ok(())`, and reporting those as
/// unhealthy would make the gauge lie in the opposite direction.
#[test]
fn clean_listen_completion_leaves_source_health_untouched() {
    let metrics = manta_server::metrics::Metrics::new();
    metrics.set_source_health("file", true);
    record_terminal_source_health(&metrics, "file", &Ok(()));
    assert!(metrics
        .render_prometheus_text()
        .contains("manta_source_health{source=\"file\"} 1"));
}

#[test]
fn epoch_for_replay_path_rejects_a_pre_unix_epoch_mtime() {
    // Regression (round-8 review): a Unix filesystem can represent an
    // mtime before 1970 (rare, but real). Left unvalidated, that
    // SystemTime flows all the way to SpotBus::unix_ts_for, whose
    // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very
    // first spot delivered to any client -- a crash discovered at
    // spot-delivery time instead of a clean error at startup.
    let f = write_temp_file(b"pre-epoch mtime fixture");
    let pre_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
    // Windows requires FILE_WRITE_ATTRIBUTES on the handle to call
    // SetFileTime; a read-only `File::open` handle (as used previously)
    // fails with PermissionDenied there regardless of the target time
    // value -- open with write access instead.
    std::fs::OpenOptions::new()
        .write(true)
        .open(f.path())
        .unwrap()
        .set_modified(pre_epoch)
        .expect("this platform must support setting mtime for the test to be meaningful");

    let result = epoch_for_replay_path(f.path());
    assert!(
        result.is_err(),
        "a pre-1970 mtime must be rejected at startup, not deferred to a later panic"
    );
}

#[test]
fn resolve_epoch_ignores_replay_epoch_for_a_live_session() {
    // Regression (round-8 review): --replay-epoch's own doc comment
    // says "ignored for a live source," but the old match applied it
    // unconditionally on `Some(secs)` regardless of `replay_path`. A
    // live session given the flag would then publish spots with a
    // fabricated historical timestamp AND derive its session_nonce
    // from that same fixed value instead of a fresh nanosecond-
    // precision now -- breaking the "two live sessions started within
    // the same wall-clock second don't collide" guarantee entirely,
    // since every live start with the same flag value would collide.
    let before = std::time::SystemTime::now();
    let epoch = resolve_epoch(None, Some(1_751_635_200)).unwrap();
    let after = std::time::SystemTime::now();
    assert!(
        epoch >= before && epoch <= after,
        "a live session (no replay_path) must ignore --replay-epoch and use now(), got {epoch:?}"
    );
}

#[test]
fn resolve_epoch_prefers_an_explicit_replay_epoch_over_file_mtime() {
    // --replay-epoch must win even when a replay path is also given --
    // it's the escape hatch for exactly the case where mtime isn't
    // trustworthy (a copy/download that didn't preserve it).
    let f = write_temp_file(b"resolve_epoch fixture");
    let explicit =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200);
    assert_eq!(
        resolve_epoch(Some(f.path()), Some(1_751_635_200)).unwrap(),
        explicit
    );
}

#[test]
fn resolve_epoch_falls_back_to_file_mtime_for_replay_without_an_explicit_epoch() {
    let f = write_temp_file(b"resolve_epoch fixture");
    assert_eq!(
        resolve_epoch(Some(f.path()), None).unwrap(),
        epoch_for_replay_path(f.path()).unwrap()
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

#[test]
fn soak_and_doctor_say_they_ignore_spectrum_udp() {
    let loaded = loaded_from("[input]\ntype = \"audio\"\nspectrum_udp = \"127.0.0.1:13064\"\n");
    let note = spectrum_feed::ignored_note("soak", &loaded).unwrap();
    assert!(
        note.starts_with("note: soak does not send spectrum; ignoring input.spectrum_udp from "),
        "{note}"
    );
    assert_eq!(
        spectrum_feed::ignored_note("doctor", &loaded_from("[input]\ntype = \"audio\"\n")),
        None
    );
}

/// MAN-309: a manta IQ recording pair in `dir`, `gap` its one gap.
fn native_pair(dir: &Path, name: &str, start_secs: i64, gap: u64) -> PathBuf {
    use manta_input::recording_format::MetadataRecord;
    let path = dir.join(name);
    std::fs::write(&path, [0u8; 80]).unwrap();
    let header = manta_input::RecordingHeader {
        schema: "manta-iq.v1".into(),
        sample_format: "cf32_le".into(),
        sample_rate_hz: 48_000.0,
        center_freq_hz: 14_000_000.0,
        start_unix_seconds: start_secs,
        start_nanoseconds: 500,
        rf_passband_hz: [-24_000.0, 24_000.0],
        trusted_band_hz: None,
        analytic_guard_hz: 0.0,
    };
    let meta = MetadataRecord::Header(header).to_line()
        + &MetadataRecord::Gap {
            before_sample: 5,
            missing_samples: gap,
        }
        .to_line()
        + &MetadataRecord::End {
            sample_count: 10,
            reason: manta_input::EndReason::Stopped,
        }
        .to_line();
    std::fs::write(manta_input::metadata_path(&path), meta).unwrap();
    path
}

#[test]
fn ftx_epoch_preserves_live_acquisition_before_startup_finishes() {
    let opened = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_792_407_602);
    let acquired = opened - std::time::Duration::from_millis(1500);
    assert_eq!(
        resolve_ftx_epoch(&LiveSourceSpec::AudioDevice(None), Some(123), acquired).unwrap(),
        acquired
    );
}

#[test]
fn ftx_epoch_uses_each_native_input_and_keeps_live_time_in_a_mixed_session() {
    let dir = tempfile::tempdir().unwrap();
    let acquired = std::time::SystemTime::now();
    for (name, seconds) in [("a.cf32", 1_792_407_600), ("b.cf32", 1_792_407_615)] {
        let path = native_pair(dir.path(), name, seconds, 0);
        let spec = LiveSourceSpec::File {
            path,
            source_iq: true,
            raw: None,
        };
        assert_eq!(
            resolve_ftx_epoch(&spec, None, acquired).unwrap(),
            std::time::UNIX_EPOCH + std::time::Duration::new(seconds as u64, 500)
        );
    }
    assert_eq!(
        resolve_ftx_epoch(&LiveSourceSpec::AudioDevice(None), None, acquired).unwrap(),
        acquired
    );
}

#[test]
fn a_native_recording_replays_at_its_recorded_start_not_its_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let rec = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let recorded = std::time::UNIX_EPOCH + std::time::Duration::new(1_792_407_600, 500);
    assert_eq!(resolve_epoch(Some(&rec), None).unwrap(), recorded);
    // --replay-epoch still wins.
    assert_eq!(
        resolve_epoch(Some(&rec), Some(1_751_635_200)).unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200)
    );
    // A pre-1970 recorded start is refused, as a pre-1970 mtime is.
    let old = native_pair(dir.path(), "old.cf32", -5, 7);
    assert!(resolve_epoch(Some(&old), None).is_err());
}

#[test]
fn a_native_recordings_nonce_covers_its_metadata_but_not_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let a = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let renamed = native_pair(dir.path(), "renamed.cf32", 1_792_407_600, 7);
    let other_gap = native_pair(dir.path(), "gap.cf32", 1_792_407_600, 8);
    let other_start = native_pair(dir.path(), "start.cf32", 1_792_407_601, 7);
    let nonce = session_nonce_for_replay_path(&a).unwrap();
    assert_eq!(session_nonce_for_replay_path(&renamed).unwrap(), nonce);
    assert_ne!(session_nonce_for_replay_path(&other_gap).unwrap(), nonce);
    assert_ne!(session_nonce_for_replay_path(&other_start).unwrap(), nonce);
    // A plain file's nonce is still its bytes' hash alone.
    let plain = dir.path().join("plain.cf32");
    std::fs::write(&plain, [0u8; 80]).unwrap();
    assert_eq!(
        session_nonce_for_replay_path(&plain).unwrap(),
        u128::from(fnv1a_file(0xcbf29ce484222325, &plain).unwrap())
    );
    assert_ne!(session_nonce_for_replay_path(&plain).unwrap(), nonce);
}

/// MAN-309: copying a recording pair and touching its mtime keeps the
/// server's epoch and spot identity; how the capture ended (the end
/// reason) and the metadata's formatting are not part of the identity.
#[test]
fn a_copied_recording_with_a_new_mtime_keeps_its_epoch_and_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let a = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let copy = dir.path().join("copy.cf32");
    std::fs::copy(&a, &copy).unwrap();
    std::fs::copy(
        manta_input::metadata_path(&a),
        manta_input::metadata_path(&copy),
    )
    .unwrap();
    let touched = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for p in [copy.clone(), manta_input::metadata_path(&copy)] {
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(touched)
            .unwrap();
    }
    assert_eq!(
        std::fs::metadata(&copy).unwrap().modified().unwrap(),
        touched
    );
    assert_eq!(
        resolve_epoch(Some(&copy), None).unwrap(),
        resolve_epoch(Some(&a), None).unwrap()
    );
    let nonce = session_nonce_for_replay_path(&a).unwrap();
    assert_eq!(session_nonce_for_replay_path(&copy).unwrap(), nonce);

    // Same samples, start and gaps, but ended `eof` and written with
    // the keys spaced out: the same recording.
    let meta = manta_input::metadata_path(&copy);
    let text = std::fs::read_to_string(&meta)
        .unwrap()
        .replace("\"stopped\"", "\"eof\"")
        .replace("\":", "\": ");
    std::fs::write(&meta, text).unwrap();
    assert_eq!(
        manta_input::read_recording_summary(&copy).unwrap().reason,
        manta_input::EndReason::Eof
    );
    assert_eq!(session_nonce_for_replay_path(&copy).unwrap(), nonce);
}

#[test]
fn resolve_epoch_is_now_for_a_live_session_with_no_replay_path() {
    let before = std::time::SystemTime::now();
    let epoch = resolve_epoch(None, None).unwrap();
    let after = std::time::SystemTime::now();
    assert!(epoch >= before && epoch <= after);
}

#[test]
fn parse_replay_epoch_accepts_a_unix_seconds_value() {
    assert_eq!(parse_replay_epoch("1751635200").unwrap(), 1_751_635_200);
}

#[test]
fn parse_replay_epoch_rejects_negative_and_non_numeric_values() {
    assert!(parse_replay_epoch("-1").is_err());
    assert!(parse_replay_epoch("not-a-number").is_err());
    assert!(parse_replay_epoch("2026-07-04T12:00:00Z").is_err());
}

#[test]
fn parse_replay_epoch_rejects_unrealistic_far_future_values() {
    // Regression (round-9 review): an unbounded upper end let
    // i64::MAX (or anything close to it) through, which later
    // overflows SystemTime arithmetic in SpotBus::unix_ts_for
    // (`epoch + elapsed`) and panics on the very first spot delivered
    // to any client -- reject it here, at CLI-parse time, with a
    // clear error instead.
    assert!(parse_replay_epoch(&i64::MAX.to_string()).is_err());
    // A plausible near-future value must still be accepted.
    assert!(parse_replay_epoch("2000000000").is_ok());
}

#[test]
fn epoch_for_replay_path_is_a_real_deterministic_timestamp() {
    // Regression (round-6 review): the epoch fed into SpotBus (and
    // from there into every JSON `timestamp`/RBN Zulu field) must be
    // BOTH a genuine wall-clock instant (not a content-hash reinterpreted
    // as nanoseconds, which produced dates spanning 1970-2554) AND
    // stable across reruns of the same replay file (a fresh
    // SystemTime::now() every run broke reproducible replay output,
    // the specific regression this round's finding flagged). A file's
    // own mtime satisfies both: it's a real filesystem fact, and it
    // doesn't change between two reads of the same untouched file.
    let f = write_temp_file(b"replay epoch fixture");
    let a = epoch_for_replay_path(f.path()).unwrap();
    let b = epoch_for_replay_path(f.path()).unwrap();
    assert_eq!(a, b, "must be stable across reruns of the same file");

    let now = std::time::SystemTime::now();
    let drift = now
        .duration_since(a)
        .or_else(|_| a.duration_since(now))
        .unwrap();
    assert!(
        drift < std::time::Duration::from_secs(60),
        "must be a genuine near-present timestamp, not a fabricated far date; drift was {drift:?}"
    );
}

#[test]
fn session_nonce_for_replay_path_matches_the_published_fnv_1a_algorithm() {
    // Regression (round-12 review): std::collections::hash_map::
    // DefaultHasher's algorithm is explicitly documented as
    // UNSPECIFIED across Rust releases, so the same replay file could
    // hash differently across builds/toolchains -- and this value
    // feeds every JSON spot `id`. FNV-1a-64 is a small, independently
    // published, versioned algorithm with no dependency on any std or
    // compiler internals: `hash = (hash XOR byte) * FNV_PRIME`,
    // starting from the published offset basis. Recomputed here from
    // the same published constants via a separate expression (not
    // just self-consistency) to pin the implementation against the
    // actual formula, catching e.g. an accidentally swapped
    // XOR/multiply order or wrong constant.
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let expected_empty = OFFSET_BASIS as u128;
    let expected_a = (OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) as u128;
    let expected_ab =
        (((OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) ^ 0x62u64).wrapping_mul(PRIME)) as u128;

    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"").path()).unwrap(),
        expected_empty
    );
    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"a").path()).unwrap(),
        expected_a
    );
    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"ab").path()).unwrap(),
        expected_ab
    );
}

#[test]
fn session_nonce_for_replay_path_is_deterministic_for_the_same_content() {
    let f = write_temp_file(b"same recording bytes");
    assert_eq!(
        session_nonce_for_replay_path(f.path()).unwrap(),
        session_nonce_for_replay_path(f.path()).unwrap()
    );
}

#[test]
fn session_nonce_for_replay_path_is_stable_across_different_paths_for_the_same_content() {
    // The exact bug this fix exists to prevent: the same recording,
    // re-read from a different path (a rename, a different mount, a
    // different checkout) must derive the SAME replay session nonce.
    let a = write_temp_file(b"identical recording bytes");
    let b = write_temp_file(b"identical recording bytes");
    assert_eq!(
        session_nonce_for_replay_path(a.path()).unwrap(),
        session_nonce_for_replay_path(b.path()).unwrap(),
        "the same content at two different paths must derive the same nonce"
    );
}

#[test]
fn session_nonce_for_replay_path_differs_across_different_recordings() {
    let a =
        session_nonce_for_replay_path(write_temp_file(b"contest-weekend bytes").path()).unwrap();
    let b =
        session_nonce_for_replay_path(write_temp_file(b"quiet-weeknight bytes").path()).unwrap();
    assert_ne!(
        a, b,
        "two different recordings must not collide on the same replay session nonce"
    );
}

// MAN-32/MAN-42: start_spot_server spawns one RBN uplink task per
// configured [[rbn_uplink]] target, only for those that are enabled.

#[test]
fn disabled_uplink_makes_no_connection_attempt_from_the_daemon() {
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let target_port = target.local_addr().unwrap().port();

    let cfg_file = write_temp_file(
        format!(
            r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = false
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
        )
        .as_bytes(),
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, _server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let accepted = rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_millis(300), async {
            loop {
                if target.accept().is_ok() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
    });
    assert!(
        accepted.is_err(),
        "enabled=false must never attempt a connection"
    );
}

#[test]
fn enabled_uplink_connects_to_its_configured_target() {
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let target_port = target.local_addr().unwrap().port();

    let cfg_file = write_temp_file(
        format!(
            r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
        )
        .as_bytes(),
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, _server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let accepted = rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if target.accept().is_ok() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
    });
    assert!(
        accepted.unwrap_or(false),
        "enabled=true must connect to the configured target"
    );
}

// MAN-86: the three new [server] operator-identity keys must load
// through the real config path (`config::load` + `start_spot_server`), not
// just through manta-server's own unit tests. Additive on a
// deny_unknown_fields struct, so a config WITHOUT them (every other
// test in this module) must keep loading too.
#[test]
fn server_config_accepts_the_operator_identity_keys() {
    let cfg_file = write_temp_file(
        r#"
            [server]
            station_callsign = "HB9H"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0
            operator_name = "Art"
            operator_qth = "Switzerland"
            operator_grid = "JN46la"
            "#
        .as_bytes(),
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore)
        .expect("a config with the operator-identity keys present must load");
    let result = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_040_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    );
    assert!(
        result.is_ok(),
        "a config with the operator-identity keys present must still start: {:?}",
        result.err()
    );
}

/// MAN-138: starts the daemon's servers from a real config file (through
/// `config::load` and `start_spot_server`, as `run` does), publishes one
/// spot and returns the line a raw TCP client reads from the JSON port.
fn json_spot_from_a_daemon(callsign: &str, location_keys: &str) -> serde_json::Value {
    let cfg_file = write_temp_file(
        format!(
            "[server]\nstation_callsign = \"{callsign}\"\nbind_addr = '127.0.0.1'\n\
                 telnet_port = 0\njson_port = 0\nmetrics_port = 0\n{location_keys}"
        )
        .as_bytes(),
    );
    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let line = rt.block_on(async {
        use tokio::io::AsyncBufReadExt;
        let stream = tokio::net::TcpStream::connect(server.json_addr)
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        // Let the listener accept and subscribe before the publish (the
        // json_stream_acceptance precedent).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        server.bus.publish(manta_spot::Spot {
            callsign: "JA1ABC".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 23.0,
            wpm: 28.0,
            spot_type: manta_spot::SpotType::Cq,
            confidence: 0.9,
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 7,
            sample_ts: 12_345,
            rst: None,
            qrl_query: false,
            mode: manta_spot::SpotMode::Cw,
            baud: None,
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if value.get("kind").is_none() {
                    break line;
                }
            }
        })
        .await
        .expect("timed out waiting for a JSON spot")
    });

    let _ = server.shutdown_tx.send(true);
    shutdown_runtime_after_drain(rt, &server.tasks);
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
}

/// MAN-138's Gherkin scenario, end to end: an operator configures their
/// station's grid square, and every JSON spot names that location as
/// deGrid/deLat/deLon -- not the cty.dat centroid (37.6, -91.87) that
/// every US-based spotter used to share.
#[test]
fn a_configured_station_grid_reaches_json_spots_as_the_receiver_position() {
    let mut positions = Vec::new();
    for (call, grid) in [("W1AW", "FN31pr"), ("W5AU", "EM12kp")] {
        let spot = json_spot_from_a_daemon(call, &format!("operator_grid = \"{grid}\"\n"));
        let (lat, lon) = manta_server::maidenhead::square(grid).unwrap().center();
        assert_eq!(spot["deCall"], call, "{spot}");
        assert_eq!(spot["deGrid"], grid, "{spot}");
        let de_lat = spot["deLat"].as_f64().expect("deLat is a number");
        let de_lon = spot["deLon"].as_f64().expect("deLon is a number");
        assert!(
            (de_lat - lat).abs() < 1e-9,
            "{call}: deLat {de_lat}, want {lat}"
        );
        assert!(
            (de_lon - lon).abs() < 1e-9,
            "{call}: deLon {de_lon}, want {lon}"
        );
        assert_ne!((de_lat, de_lon), (37.6, -91.87), "{call}: the US centroid");
        // Entity-level fields still follow the callsign.
        assert_eq!(spot["deDxcc"], 291, "{spot}");
        positions.push((de_lat, de_lon));
    }
    assert_ne!(
        positions[0], positions[1],
        "two stations in one country must not share a position"
    );
}

/// With no location configured, the daemon sends the contract-legal
/// "unknown" (`null`), never a country centroid in its place.
#[test]
fn a_station_without_a_configured_location_sends_null_position_not_a_country_centroid() {
    let spot = json_spot_from_a_daemon("W1AW", "");
    for key in ["deGrid", "deLat", "deLon"] {
        assert!(spot[key].is_null(), "{key}: {spot}");
    }
    assert_eq!(spot["deDxcc"], 291, "{spot}");
}

/// The belt-and-braces path for a caller that skips `config::load`: a
/// hand-built config with half a lat/lon pair never starts a listener.
#[test]
fn start_spot_server_rejects_an_inconsistent_station_location() {
    let cfg = manta_server::config::ServerConfig {
        operator_lat: Some(41.7),
        ..test_server_config("W1AW")
    };
    let Err(err) = start_spot_server(
        cfg,
        Vec::new(),
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    ) else {
        panic!("half a lat/lon pair must not start the servers");
    };
    let err = format!("{err:#}");
    assert!(err.contains("operator_lon"), "{err}");
    assert!(err.starts_with("[server]: "), "{err}");
}

/// MAN-320: the loaded relay blocks, as `run` hands them over.
fn relay_test_start(body: &str) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    let cfg_file = write_temp_file(body.as_bytes());
    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs {
            skimmer: loaded.skimmer_relay,
            wsjtx: loaded.wsjtx_relay,
        },
        spot_outputs::SpotOutputs::default(),
    )
}

const RELAY_SERVER: &str = "[server]\nstation_callsign = \"W3XYZ\"\nbind_addr = \"127.0.0.1\"\n\
         telnet_port = 0\njson_port = 0\nmetrics_port = 0\n\
         [[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\ntarget_port = 9\n";

#[test]
fn a_taken_unicast_port_is_a_startup_error_naming_the_relay() {
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    let Err(err) = relay_test_start(&format!(
        "{RELAY_SERVER}[[wsjtx_relay]]\nlisten = \"{addr}\"\ncallsign = \"W3XYZ\"\n"
    )) else {
        panic!("a taken UDP port must fail startup");
    };
    let err = format!("{err:#}");
    assert!(
        err.starts_with(&format!("[[wsjtx_relay]] cannot listen on {addr}: ")),
        "{err}"
    );
    assert!(err.contains("another program may own the port"), "{err}");
}

#[test]
fn start_spot_server_registers_one_relay_series_per_block() {
    let (_rt, server) = relay_test_start(&format!(
        "{RELAY_SERVER}[[skimmer_relay]]\nhost = \"127.0.0.1\"\nport = 9\n\
             callsign = \"W3XYZ-2\"\n\
             [[skimmer_relay]]\nhost = \"192.0.2.1\"\nport = 7300\ncallsign = \"W3XYZ-3\"\n\
             enabled = false\n\
             [[wsjtx_relay]]\nlisten = \"127.0.0.1:0\"\ncallsign = \"W3XYZ\"\n\
             [[wsjtx_relay]]\nlisten = \"127.0.0.1:2237\"\ncallsign = \"W3XYZ\"\n\
             enabled = false\n"
    ))
    .unwrap();
    let text = server.metrics.render_prometheus_text();
    for series in [
        "relay=\"127.0.0.1:9\",kind=\"skimmer\"",
        "relay=\"192.0.2.1:7300\",kind=\"skimmer\"",
        "relay=\"127.0.0.1:0\",kind=\"wsjtx\"",
        "relay=\"127.0.0.1:2237\",kind=\"wsjtx\"",
    ] {
        assert!(
            text.contains(&format!("manta_relay_spots_total{{{series}}} 0")),
            "{series} missing:\n{text}"
        );
    }
}

#[test]
fn two_enabled_uplink_targets_each_independently_connect() {
    let target1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target1.set_nonblocking(true).unwrap();
    let target1_port = target1.local_addr().unwrap().port();

    let target2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target2.set_nonblocking(true).unwrap();
    let target2_port = target2.local_addr().unwrap().port();

    let cfg_file = write_temp_file(
        format!(
            r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target1_port}

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target2_port}
                "#
        )
        .as_bytes(),
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, _server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    async fn wait_for_accept(listener: &std::net::TcpListener) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if listener.accept().is_ok() {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(false)
    }
    let (accepted1, accepted2) =
        rt.block_on(async { tokio::join!(wait_for_accept(&target1), wait_for_accept(&target2)) });
    assert!(accepted1, "first configured target must be connected to");
    assert!(accepted2, "second configured target must be connected to");
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

/// MAN-73 (PR #207 review): during an outage `listen()` is blocked in
/// the reconnecting `read()` and can't republish the active-track
/// count, so the unhealthy transition itself must zero the shared
/// gauge, or `manta_active_tracks` exports the pre-drop tracks until
/// samples resume. MAN-251 (PR #330 review): the transition must clear
/// the EXPORTED gauge synchronously too -- the poller's atomic load and
/// its `set_active_tracks` publish are separate steps, so without this
/// clear a tick that loaded the pre-drop count can republish it after
/// the transition and the zero waits for the next full poll interval.
#[test]
fn source_health_sink_zeroes_active_tracks_when_the_source_goes_unhealthy() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let metrics = Arc::new(manta_server::metrics::Metrics::new());
    let gauge = Arc::new(AtomicU64::new(3));
    let mut sink = source_health_sink("kiwi", Some(metrics.clone()), Some(gauge.clone()));
    // The poller's last pre-drop publish: the exported gauge holds the
    // live count when the source drops.
    metrics.set_active_tracks(3);

    sink(true);
    assert_eq!(
        gauge.load(Ordering::Relaxed),
        3,
        "a healthy report leaves the live count alone"
    );
    sink(false);
    assert_eq!(
        gauge.load(Ordering::Relaxed),
        0,
        "an unhealthy source has no active tracks"
    );
    assert_eq!(
        metrics.active_tracks(),
        0,
        "an unhealthy source exports no active tracks either, not only the shared handle"
    );
    assert!(metrics
        .render_prometheus_text()
        .contains(r#"manta_source_health{source="kiwi"} 0"#));
}

/// MAN-314: `manta_rtty_signals` is published only from the per-batch
/// callback, which cannot run while `listen()` is blocked in the
/// reconnecting `read()`, so the unhealthy transition must zero it or
/// it exports the pre-outage RTTY counts for the whole outage.
#[test]
fn source_health_sink_zeroes_rtty_signals_when_the_source_goes_unhealthy() {
    use std::sync::Arc;

    let metrics = Arc::new(manta_server::metrics::Metrics::new());
    metrics.set_rtty_signals(&[14_080_000.0, 14_086_000.0]);
    let mut sink = source_health_sink("kiwi", Some(metrics.clone()), None);

    sink(true);
    assert!(
        metrics
            .render_prometheus_text()
            .contains(r#"manta_rtty_signals{band="20m"} 2"#),
        "a healthy report leaves the marked RTTY set alone"
    );
    sink(false);
    let text = metrics.render_prometheus_text();
    assert!(
        text.contains(r#"manta_rtty_signals{band="20m"} 0"#),
        "an unhealthy source has no marked RTTY signals: {text}"
    );
}

/// MAN-251 (PR #207 review): a reconnecting read blocks the decode
/// loop throughout an outage. The real health sink must clear the
/// live count during backoff, and decoding must publish it again
/// after samples resume. PR #330 review: the poller here mirrors the
/// production bridge (atomic load, then `set_active_tracks` publish)
/// into a real `Metrics`, and the assertions check the EXPORTED
/// `manta_active_tracks` too -- synchronously zero at the unhealthy
/// transition, not only the shared handle.
#[test]
fn reconnecting_outage_zeroes_active_tracks_until_samples_resume() {
    use num_complex::Complex32;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    struct DropWhenExhaustedSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        stall_at_eof: bool,
    }

    impl IqSource for DropWhenExhaustedSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }

        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }

        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = (self.samples.len() - self.cursor).min(buf.len());
            if n == 0 && self.stall_at_eof {
                anyhow::bail!("simulated live source stall");
            }
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    let spec = manta_testkit::vectors::v1();
    let rendered = manta_testkit::vectors::render(&spec).unwrap();
    let first = DropWhenExhaustedSource {
        samples: rendered.samples.clone(),
        cursor: 0,
        fs: spec.fs,
        center_freq_hz: spec.center_freq_hz,
        stall_at_eof: true,
    };
    let opener: reconnect::Opener = Box::new(move || {
        Ok(Box::new(DropWhenExhaustedSource {
            samples: rendered.samples.clone(),
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
            stall_at_eof: false,
        }))
    });
    let metrics = Arc::new(manta_server::metrics::Metrics::new());
    let gauge = Arc::new(AtomicU64::new(0));
    let transitions = Arc::new(Mutex::new(Vec::new()));
    let sink: reconnect::HealthSink = {
        let transitions = transitions.clone();
        let metrics = metrics.clone();
        let mut inner = source_health_sink("man251", Some(metrics.clone()), Some(gauge.clone()));
        Box::new(move |healthy| {
            let mut transitions = transitions.lock().unwrap();
            inner(healthy);
            // Recorded after the production sink returns: this is the
            // exported value a dashboard scrape would see at the
            // transition instant (PR #330 review).
            transitions.push((Instant::now(), healthy, metrics.active_tracks()));
        })
    };
    let src = reconnect::ReconnectingSource::new(
        "man251",
        Box::new(first),
        opener,
        Arc::new(AtomicBool::new(false)),
        true,
        sink,
    );

    // IqSource is not Send: keep decoding on this thread and move
    // only the shared gauge to the polling thread.
    let done = Arc::new(AtomicBool::new(false));
    let poller = {
        let done = done.clone();
        let gauge = gauge.clone();
        let metrics = metrics.clone();
        let transitions = transitions.clone();
        std::thread::spawn(move || {
            let mut polls = Vec::new();
            while !done.load(Ordering::Relaxed) {
                {
                    // Serialize with health callbacks so a poll cannot
                    // straddle a transition and appear to belong to the
                    // wrong side of the outage. Inside that
                    // serialization this is the PRODUCTION poller's
                    // pair (PR #330 review): load the shared gauge,
                    // then publish it into the exported metric --
                    // recorded as (when, count, exported) so the
                    // bridge itself is what the assertions read.
                    let _transition = transitions.lock().unwrap();
                    let count = gauge.load(Ordering::Relaxed);
                    metrics.set_active_tracks(count);
                    polls.push((Instant::now(), count, metrics.active_tracks()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            polls
        })
    };
    let result = manta_engine::listen_with_observers(
        Box::new(src),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        manta_engine::ListenObservers {
            active_tracks: Some(gauge),
            ..Default::default()
        },
        |_ev| {},
        |_spot| {},
        |_n| {},
    );
    done.store(true, Ordering::Relaxed);
    let polls = poller.join().expect("active-track poller must finish");
    result.expect("the reconnected source must decode through clean EOF");

    let transitions = transitions.lock().unwrap();
    let (false_at, exported_at_drop) = transitions
        .iter()
        .find_map(|&(at, healthy, exported)| (!healthy).then_some((at, exported)))
        .expect("the live source must report its outage");
    let true_at = transitions
        .iter()
        .find_map(|&(at, healthy, _)| (healthy && at > false_at).then_some(at))
        .expect("the reconnected source must report resumed samples");
    assert!(
        polls
            .iter()
            .any(|&(at, count, _)| at < false_at && count > 0),
        "V1 must have active tracks before the source drops"
    );
    assert!(
        polls
            .iter()
            .any(|&(at, _, exported)| at < false_at && exported > 0),
        "the metrics bridge must export the live count before the source drops"
    );
    assert_eq!(
        exported_at_drop, 0,
        "MAN-251: the sink must clear the exported manta_active_tracks synchronously at \
             the unhealthy transition, not only the shared handle"
    );
    assert!(
        true_at.duration_since(false_at) >= manta_server::backoff::INITIAL_BACKOFF,
        "the unhealthy interval must include the real reconnect backoff"
    );
    assert!(
        polls
            .iter()
            .filter(|&&(at, _, _)| at > false_at && at < true_at)
            .all(|&(_, count, _)| count == 0),
        "MAN-251: no poll during the outage may report ghost active tracks"
    );
    assert!(
        polls
            .iter()
            .filter(|&&(at, _, _)| at > false_at && at < true_at)
            .all(|&(_, _, exported)| exported == 0),
        "MAN-251: no poll during the outage may export ghost active tracks"
    );
    let first_zero = polls
        .iter()
        .find_map(|&(at, count, _)| (at >= false_at && count == 0).then_some(at))
        .expect("active tracks must reach zero after the source drops");
    assert!(
        first_zero.duration_since(false_at) <= ACTIVE_TRACKS_POLL_INTERVAL,
        "MAN-251: active tracks must reach zero within one polling interval; lag: {:?}",
        first_zero.duration_since(false_at)
    );
    assert!(
        polls
            .iter()
            .any(|&(at, count, _)| at > true_at && count > 0),
        "decoding must republish a real active-track count after samples resume"
    );
    assert!(
        polls
            .iter()
            .any(|&(at, _, exported)| at > true_at && exported > 0),
        "the metrics bridge must export the resumed real count after samples resume"
    );
}

/// MAN-96: a reconnecting source's healthy -> unhealthy -> healthy
/// round trip through the real sink is one `manta_source_outages_total`
/// increment.
#[test]
fn source_health_sink_counts_a_source_outage() {
    use std::sync::Arc;

    let metrics = Arc::new(manta_server::metrics::Metrics::new());
    let mut sink = source_health_sink("kiwi", Some(metrics.clone()), None);
    sink(true);
    sink(false);
    sink(true);
    let text = metrics.render_prometheus_text();
    assert!(
        text.contains(r#"manta_source_outages_total{source="kiwi"} 1"#),
        "{text}"
    );
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
fn input_attenuation_of_translates_a_snapshot() {
    assert_eq!(
        input_attenuation_of(manta_input::hpsdr::AttenuationSnapshot { db: 12, changes: 2 }),
        manta_server::metrics::InputAttenuation { db: 12, changes: 2 }
    );
}

#[test]
fn publish_attenuation_skips_a_handle_no_protocol_2_open_engaged() {
    let metrics = manta_server::metrics::Metrics::new();
    let handle = manta_input::hpsdr::AutoAttenuation::new(Box::new(|_| {}));
    assert_eq!(handle.snapshot(), None);
    publish_attenuation(&metrics, "hpsdr", &handle);
    let text = metrics.render_prometheus_text();
    assert!(text.contains("# TYPE manta_input_attenuation_db gauge\n"));
    assert!(!text.contains("manta_input_attenuation_db{"), "{text}");
}

#[test]
fn input_health_of_snapshots_all_three_counters_without_transposing_them() {
    // Three same-typed u64s: a transposition would be invisible to any
    // test that used equal values (MAN-56 D7).
    let c = manta_input::InputHealthCounters::new();
    c.record_dropped(7);
    c.record_gap();
    c.record_gap();
    c.record_malformed();
    let h = input_health_of(&c);
    assert_eq!(h.dropped_packets, 7);
    assert_eq!(h.gaps_detected, 2);
    assert_eq!(h.malformed_packets, 1);
}

/// MAN-128: `daemon_build_info` must never fail or panic (it feeds a
/// gauge any scrape can trigger), and `version` is the crate's own
/// Cargo-reported version, not whatever happens to be in `Cargo.lock`.
#[test]
fn build_info_carries_the_crate_version_and_a_non_empty_git_sha() {
    let info = daemon_build_info();
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert!(!info.git_sha.is_empty());
    assert!(!info.features.is_empty());
}

#[test]
fn daemon_build_info_propagates_the_selected_cargo_features() {
    assert_eq!(daemon_build_info().features, build_info::FEATURES);
}

#[test]
fn latency_histogram_of_copies_the_engines_bucket_bounds() {
    let obs = manta_engine::DecodeLatencyObserver::new();
    obs.observe(std::time::Duration::from_millis(1));
    let h = latency_histogram_of(&obs.snapshot());
    assert_eq!(
        h.bounds_seconds,
        manta_engine::DECODE_LATENCY_BUCKETS_SECONDS
    );
    assert_eq!(h.bucket_counts.iter().sum::<u64>(), 1);
}

#[test]
fn decode_quality_of_attaches_the_engines_bucket_bounds() {
    let obs = manta_engine::DecodeQualityObserver::new();
    obs.add_chars(4);
    obs.add_garbles(1);
    obs.observe_rail_ratio(3.5);
    let q = decode_quality_of(&obs.snapshot());
    assert_eq!(q.chars_total, 4);
    assert_eq!(q.garbles_total, 1);
    assert_eq!(q.rail_ratio.bounds, manta_engine::RAIL_RATIO_BUCKETS);
    assert_eq!(
        q.rail_ratio.bucket_counts.len(),
        q.rail_ratio.bounds.len() + 1
    );
    assert_eq!(q.rail_ratio.bucket_counts.iter().sum::<u64>(), 1);
    assert_eq!(
        q.repetitions.bounds,
        manta_spot::REPETITION_BUCKETS.map(f64::from)
    );
    assert_eq!(
        q.repetitions.bucket_counts.len(),
        q.repetitions.bounds.len() + 1
    );
}

/// MAN-207: every valid decision/reason pair, labelled as LIVE-STATE.md.
#[test]
fn decode_quality_of_carries_the_decision_counts() {
    let obs = manta_engine::DecodeQualityObserver::new();
    let q = decode_quality_of(&obs.snapshot());
    assert_eq!(q.decisions.len(), 1 + manta_spot::Reason::ALL.len());
    assert_eq!(q.decisions[0], ("spotted", "none", 0));
    assert!(q.decisions.contains(&("withheld", "repetition", 0)));
    assert!(q.decisions.contains(&("deferred", "beaconPending", 0)));
    assert!(q.decisions.contains(&("deferred", "lookupPending", 0)));
    assert_eq!(
        q.decisions
            .iter()
            .filter(|&&(d, _, _)| d == "spotted")
            .count(),
        1
    );
    assert_eq!(
        q.not_candidates,
        vec![
            ("addressee", 0),
            ("relayed_mention", 0),
            ("self_id", 0),
            ("uncertain", 0),
        ]
    );
}

#[test]
fn a_source_without_counters_publishes_no_input_health_series() {
    struct StubSourceNoCounters;
    impl IqSource for StubSourceNoCounters {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
    }

    let m = manta_server::metrics::Metrics::new();
    // Mirrors the wiring above: `None` means we never call
    // set_input_health.
    let src: Box<dyn IqSource> = Box::new(StubSourceNoCounters);
    if let Some(c) = src.health_counters() {
        m.set_input_health("file", input_health_of(&c));
    }
    assert!(!m
        .render_prometheus_text()
        .contains("manta_input_malformed_packets_total{"));
}

/// MAN-128: pins the label path for a non-HPSDR source. The daemon's
/// `health_counters()` wiring (above `input_health_of`) is already
/// generic over `source_name` -- this confirms a Kiwi-shaped source's
/// counters reach `/metrics` under `source="kiwi"` with no CLI change.
#[test]
fn kiwi_like_source_counters_reach_metrics_under_kiwi_label() {
    let counters = manta_input::InputHealthCounters::new();
    counters.record_gap();
    counters.record_dropped(3);

    let m = manta_server::metrics::Metrics::new();
    m.set_input_health("kiwi", input_health_of(&counters));
    let text = m.render_prometheus_text();
    assert!(text.contains(r#"manta_input_gaps_detected_total{source="kiwi"} 1"#));
    assert!(text.contains(r#"manta_input_dropped_packets_total{source="kiwi"} 3"#));
}

// MAN-136 round-1 validate code-review finding 1: the increment
// condition for `manta_spots_unresolved_geography_total` must match the
// condition under which `SpotMessage::from_spot` emits the `UNKNOWN_*`
// sentinels -- the RESOLVED ADIF entity number, not merely whether
// `cty.lookup` returned an entry.

const GEOGRAPHY_CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N;
";
/// One `dxcc.tsv` row for the fixture above, in the vendored file's
/// `<primary-prefix>\t<adif-number>\t<name>` shape.
const GEOGRAPHY_DXCC_FIXTURE: &str = "K\t291\tUnited States\n";

#[test]
fn a_callsign_with_a_resolved_entity_number_is_not_counted_as_unresolved() {
    let cty =
        manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
    assert_eq!(cty.lookup("W1AW").and_then(|e| e.dxcc), Some(291));
    assert!(!geography_is_unresolved(&cty, "W1AW"));
}

#[test]
fn an_unresolvable_callsign_is_counted_as_unresolved() {
    let cty =
        manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
    assert!(cty.lookup("QQ1AAA").is_none(), "test premise");
    assert!(geography_is_unresolved(&cty, "QQ1AAA"));
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

#[test]
fn a_cty_resolvable_callsign_with_no_dxcc_row_is_still_counted_as_unresolved() {
    // The cty.dat/dxcc.tsv drift state: `cty.dat` was hand-refreshed
    // (data/SOURCES.md has no refresh automation) without regenerating
    // the TSV, so geography resolves -- non-null dxLat/dxLon -- while
    // the entity number does not, and the spot goes out with
    // `dxDxcc: -1`. Counting `lookup().is_none()` missed exactly this.
    let cty = manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, "");
    let entry = cty.lookup("W1AW").expect("geography still resolves");
    assert_eq!(entry.dxcc, None, "test premise: only the number is missing");
    assert_eq!(entry.continent, "NA");
    assert!(
        geography_is_unresolved(&cty, "W1AW"),
        "a spot emitted with UNKNOWN_DXCC must be counted, even though cty.dat resolved it"
    );
}

// ---- MAN-261: CLI > env > file precedence (`resolve`) and coverage

fn cli_overrides() -> CliOverrides {
    CliOverrides::none()
}

fn loaded_from(body: &str) -> config::Loaded {
    let f = write_temp_file(body.as_bytes());
    config::load(Some(f.path()), config::Env::Ignore).unwrap()
}

const KIWI_INPUT: &str = "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nfreq_hz = 14025000.0\n\
                              freq_correction_ppm = 2.5\ncenter_freq_hz = 14000000.0\n";

// ---- MAN-13: several [[input]] entries ----

const THREE_KA9Q: &str = "[[input]]\nname = \"40m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\n\
        ssrc = 7030000\n\n[[input]]\nname = \"20m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\n\
        ssrc = 14030000\nfreq_correction_ppm = -1.5\ncapture_rate_hz = 48000.0\n\n\
        [[input]]\nname = \"15m\"\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\n\
        freq_hz = 21030000.0\ncenter_freq_hz = 21000000.0\nfreq_correction_ppm = 0.8\n";

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

/// MAN-306 (PR #359 review): `--hpsdr-protocol` names how to speak to a
/// radio discovery finds, by MAC or alone, not only one given by host.
#[test]
fn hpsdr_protocol_accepts_hostless_selectors() {
    for command in ["run", "listen", "soak", "doctor", "calibrate", "check"] {
        let mut base = vec!["manta", command];
        if command == "soak" {
            base.extend(["--duration", "1"]);
        }
        let tuning = ["--hpsdr-freq-hz", "7030000", "--hpsdr-rate-hz", "192000"];
        for (selector, protocol) in [
            (&["--hpsdr-mac", "02:11:22:33:44:55"][..], "1"),
            (&["--hpsdr-mac", "02:11:22:33:44:55"][..], "2"),
            (&[][..], "1"),
            (&[][..], "2"),
        ] {
            let args: Vec<&str> = base
                .iter()
                .copied()
                .chain(selector.iter().copied())
                .chain(tuning)
                .chain(["--hpsdr-protocol", protocol])
                .collect();
            if let Err(error) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {error}");
            }
        }
        // Still meaningless with no HPSDR flag at all.
        let args: Vec<&str> = base
            .iter()
            .copied()
            .chain(["--hpsdr-protocol", "2"])
            .collect();
        let error = Cli::try_parse_from(&args).err().expect("refused");
        assert_eq!(error.exit_code(), 2, "{args:?}: {error}");
    }
}

// ---- MAN-140: the ka9q-radio source

#[test]
fn every_live_command_rejects_zero_ka9q_port() {
    let mut accepted_invalid = Vec::new();
    for command in ["run", "listen", "soak", "doctor", "calibrate", "check"] {
        let mut args = vec!["manta", command, "--ka9q-group", "239.101.6.1"];
        if command == "soak" {
            args.extend(["--duration", "1"]);
        }
        assert!(
            Cli::try_parse_from(&args).is_ok(),
            "{command}: default port"
        );
        for port in ["1", "5004", "65535"] {
            assert!(
                Cli::try_parse_from(args.iter().copied().chain(["--ka9q-port", port])).is_ok(),
                "{command}: valid port {port}"
            );
        }
        for port in ["0", "65536"] {
            match Cli::try_parse_from(args.iter().copied().chain(["--ka9q-port", port])) {
                Ok(_) => accepted_invalid.push(format!("{command}: {port}")),
                Err(error) => {
                    assert_eq!(error.exit_code(), 2);
                    assert!(error.to_string().contains("--ka9q-port"), "{error}");
                }
            }
        }
    }
    assert!(
        accepted_invalid.is_empty(),
        "accepted invalid ports: {accepted_invalid:?}"
    );
}

fn ka9q_opts(ssrc: Option<u32>) -> Ka9qOpts {
    Ka9qOpts {
        group: Some("239.101.6.1".into()),
        port: manta_input::ka9q::wire::DATA_PORT,
        ssrc,
        interface: None,
        ..Ka9qOpts::unset()
    }
}

#[test]
fn ka9q_flag_selects_the_source_and_replaces_a_typed_input() {
    let cli = CliOverrides {
        prior: None,
        ka9q: Ka9qOpts {
            interface: Some(std::net::Ipv4Addr::new(192, 168, 1, 20)),
            ..ka9q_opts(Some(14_100_000))
        },
        ..cli_overrides()
    };
    assert_eq!(cli.source_selector(), Some("--ka9q-group"));
    let LiveSourceSpec::Ka9q(k) = cli.clone().into_spec() else {
        panic!("expected the ka9q source from --ka9q-group");
    };
    assert_eq!(k.group.as_deref(), Some("239.101.6.1"));
    assert_eq!(k.port, 5004);
    assert_eq!(k.ssrc, Some(14_100_000));
    assert_eq!(k.interface, Some(std::net::Ipv4Addr::new(192, 168, 1, 20)));

    // A typed [input] is replaced whole, its shared keys included.
    let r = resolve(cli, &loaded_from(KIWI_INPUT)).unwrap();
    assert!(matches!(r.spec, LiveSourceSpec::Ka9q(_)));
    assert_eq!(r.replaced_file_source, Some("--ka9q-group"));
    assert_eq!(r.freq_correction_ppm, 0.0);
    assert_eq!(r.dial_freq_hz, None);
    assert!(
        r.notes[0].contains("--ka9q-group selects the source; ignoring [input] (type = \"kiwi\")"),
        "{:?}",
        r.notes
    );

    // The flags as clap parses them, defaults included.
    let parsed = Cli::try_parse_from([
        "manta",
        "run",
        "--ka9q-group",
        "239.101.6.1",
        "--ka9q-ssrc",
        "14100000",
        "--ka9q-interface",
        "192.168.1.20",
    ])
    .unwrap();
    let Command::Run {
        ka9q_group,
        ka9q_port,
        ka9q_ssrc,
        ka9q_interface,
        ..
    } = parsed.command
    else {
        unreachable!("parsed `run`");
    };
    assert_eq!(ka9q_group.as_deref(), Some("239.101.6.1"));
    assert_eq!(ka9q_port, 5004);
    assert_eq!(ka9q_ssrc, Some(14_100_000));
    assert_eq!(
        ka9q_interface,
        Some(std::net::Ipv4Addr::new(192, 168, 1, 20))
    );
    // --ka9q-interface takes an address, not an interface name.
    assert!(Cli::try_parse_from([
        "manta",
        "run",
        "--ka9q-group",
        "239.101.6.1",
        "--ka9q-interface",
        "eth0",
    ])
    .is_err());
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

#[test]
fn receiver_desc_names_the_ka9q_group() {
    let d = receiver_desc(&LiveSourceSpec::Ka9q(ka9q_opts(None)));
    assert_eq!(d.kind, doctor_checks::ReceiverKind::Ka9q);
    assert_eq!(d.description, "ka9q-radio group 239.101.6.1:5004");
    let d = receiver_desc(&LiveSourceSpec::Ka9q(ka9q_opts(Some(14_100_000))));
    assert_eq!(
        d.description,
        "ka9q-radio group 239.101.6.1:5004 ssrc 14100000"
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

/// D10: every config-backed flag maps to the key it overrides.
const FLAG_KEYS: &[(&str, &str)] = &[
    ("device", "input.device"),
    ("source", "input.path"),
    ("source_iq", "input.iq"),
    ("realtime", "input.realtime"),
    ("loop_replay", "input.loop"),
    ("kiwi_host", "input.host"),
    ("kiwi_port", "input.port"),
    ("kiwi_freq_hz", "input.freq_hz"),
    ("kiwi_password", "input.password"),
    ("soapy_driver", "input.driver"),
    ("soapy_freq_hz", "input.freq_hz"),
    ("soapy_rate_hz", "input.rate_hz"),
    ("soapy_gain", "input.gain_db"),
    ("hpsdr_host", "input.host"),
    ("hpsdr_mac", "input.mac"),
    ("hpsdr_port", "input.port"),
    ("hpsdr_protocol", "input.protocol"),
    ("hpsdr_freq_hz", "input.freq_hz"),
    ("hpsdr_rate_hz", "input.rate_hz"),
    ("ka9q_group", "input.group"),
    ("ka9q_port", "input.port"),
    ("ka9q_ssrc", "input.ssrc"),
    ("ka9q_interface", "input.interface"),
    ("freq_correction_ppm", "input.freq_correction_ppm"),
    ("dial_freq_hz", "input.center_freq_hz"),
    ("capture_rate_hz", "input.capture_rate_hz"),
    ("replay_epoch", "input.replay_epoch"),
    ("record_iq", "input.record_iq"),
    ("iq_format", "input.iq_format"),
    ("sample_rate_hz", "input.sample_rate_hz"),
    ("allowlist", "spot.allowlist"),
    ("blocklist", "spot.blocklist_path"),
    ("notch", "spot.notch_path"),
    ("cty", "spot.cty_path"),
    ("scp", "spot.scp_path"),
    ("cty_sha256", "spot.cty_sha256"),
    ("scp_sha256", "spot.scp_sha256"),
    ("patterns", "spot.pattern_path"),
    ("engine", "decode.engine"),
    ("prior", "decode.prior"),
    ("rbn_compare", "rbn_compare.enabled"),
    ("rbn_spotter", "rbn_compare.spotters"),
];
/// Flags with no config key, by design.
const CLI_ONLY: &[&str] = &[
    "json",
    "decoded_text",
    "duration",
    "config",
    "path",
    // `decode --center-freq-hz` (MAN-131 D8): a recording's centre, not
    // the live receiver's `input.center_freq_hz`.
    "center_freq_hz",
    // `decode --timing` (MAN-315): an output choice, like `json`.
    "timing",
    // `decode --observations`/`--observation-text` (MAN-207): an output
    // choice, like `json`.
    "observations",
    "observation_text",
    "help",
    "version",
    "reference_hz",
    "search_ppm",
    "tune_hz",
    "write",
    "ntp_server",
    // doctor --strict (MAN-235): an exit-code choice, like json.
    "strict",
    "verbose",
    "quiet",
    "log_level",
    "log_format",
    // `--input NAME` (MAN-13): picks one `[[input]]` entry by name.
    "input",
];

/// MAN-124: `run`'s `Logging` flags, parsed (or refused) by clap.
fn run_log_opts(extra: &[&str]) -> Result<logging::LogOpts, clap::Error> {
    let argv = ["manta", "run"].iter().chain(extra);
    match Cli::try_parse_from(argv)?.command {
        Command::Run { logging, .. } => Ok(logging),
        _ => unreachable!("parsed `run`"),
    }
}

#[test]
fn log_level_flags_are_mutually_exclusive() {
    use clap::error::ErrorKind;
    for argv in [
        &["-v", "-q"][..],
        &["-v", "--log-level", "warn"],
        &["-q", "--log-level", "warn"],
    ] {
        let err = run_log_opts(argv).expect_err("conflicting flags parsed");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{argv:?}: {err}");
    }
}

#[test]
fn log_flag_values_are_a_closed_set() {
    use clap::error::ErrorKind;
    let err = run_log_opts(&["--log-level", "loud"]).expect_err("`loud` parsed");
    assert_eq!(err.kind(), ErrorKind::InvalidValue, "{err}");
    let err = run_log_opts(&["--log-format", "yaml"]).expect_err("`yaml` parsed");
    assert_eq!(err.kind(), ErrorKind::InvalidValue, "{err}");
}

#[test]
fn log_flags_parse_on_run_and_its_listen_alias() {
    let o = run_log_opts(&["-vv"]).unwrap();
    assert_eq!(o.verbose, 2);
    assert_eq!(
        o.log_format,
        logging::LogFormat::Text,
        "text is the default"
    );
    let o = run_log_opts(&["--log-format", "json", "--log-level", "debug"]).unwrap();
    assert_eq!(o.log_format, logging::LogFormat::Json);
    assert_eq!(o.log_level, Some(logging::LogLevel::Debug));
    match Cli::try_parse_from(["manta", "listen", "-q"])
        .unwrap()
        .command
    {
        Command::Run { logging, .. } => assert_eq!(logging.quiet, 1),
        _ => panic!("`listen` is `run`'s alias"),
    }
}

#[test]
fn log_flags_are_scoped_to_run() {
    use clap::error::ErrorKind;
    for argv in [
        &["manta", "decode", "-v", "x.wav"][..],
        &["manta", "-v", "run"],
    ] {
        let err = match Cli::try_parse_from(argv) {
            Ok(_) => panic!("{argv:?} parsed"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{argv:?}: {err}");
    }
}

/// MAN-249: `oracle --spotter` has no default, because the reference
/// skimmer depends on the recording.
#[test]
fn oracle_requires_a_spotter() {
    use clap::error::ErrorKind;
    let argv = [
        "manta",
        "oracle",
        "a.wav",
        "b.csv",
        "--capture-start",
        "2025-11-29T00:00:00Z",
    ];
    let err = match Cli::try_parse_from(argv) {
        Ok(_) => panic!("oracle parsed without --spotter"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument, "{err}");
    assert!(err.to_string().contains("--spotter <SPOTTER>"), "{err}");
    let with = argv.iter().copied().chain(["--spotter", "N0ABC"]);
    match Cli::try_parse_from(with).unwrap().command {
        Command::Oracle { spotter, .. } => assert_eq!(spotter, "N0ABC"),
        _ => unreachable!("`manta oracle` parses as Command::Oracle"),
    }
}

#[test]
fn every_config_backed_flag_maps_to_a_key() {
    use clap::CommandFactory;
    let cli = Cli::command();
    for name in ["run", "soak", "doctor", "decode", "check", "calibrate"] {
        let sub = cli.find_subcommand(name).unwrap();
        for arg in sub.get_arguments() {
            let id = arg.get_id().as_str();
            assert!(
                FLAG_KEYS.iter().any(|(flag, _)| *flag == id) || CLI_ONLY.contains(&id),
                "`manta {name} --{id}` has no config key: add it to FLAG_KEYS (and the \
                     loader) or to CLI_ONLY"
            );
        }
    }
}

#[test]
fn every_flag_key_is_accepted_by_the_loader() {
    const TYPED: &[(&str, &str)] = &[
            ("audio", "type = \"audio\"\ndevice = \"d\"\n"),
            (
                "file",
                "type = \"file\"\npath = \"x.wav\"\niq = true\nrealtime = true\nloop = true\n\
                 iq_format = \"cf32\"\nsample_rate_hz = 96000\n",
            ),
            (
                "kiwi",
                "type = \"kiwi\"\nhost = \"h\"\nport = 8073\nfreq_hz = 7e6\npassword = \"\"\n",
            ),
            (
                "soapy",
                "type = \"soapy\"\ndriver = \"d\"\nfreq_hz = 7e6\nrate_hz = 48000.0\ngain_db = 10.0\n",
            ),
            (
                "hpsdr",
                "type = \"hpsdr\"\nhost = \"h\"\nport = 1024\nprotocol = \"2\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
            ),
            (
                "hpsdr_mac",
                "type = \"hpsdr\"\nmac = \"02:11:22:33:44:55\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
            ),
            (
                "ka9q",
                "type = \"ka9q\"\ngroup = \"239.1.2.3\"\nport = 5004\nssrc = 1\n\
                 interface = \"127.0.0.1\"\n",
            ),
        ];
    const SHARED: &[(&str, &str)] = &[
            (
                "input.freq_correction_ppm",
                "[input]\nfreq_correction_ppm = 1.0\n",
            ),
            ("input.center_freq_hz", "[input]\ncenter_freq_hz = 7e6\n"),
            (
                "input.capture_rate_hz",
                "[input]\ncapture_rate_hz = 6000.0\n",
            ),
            ("input.replay_epoch", "[input]\nreplay_epoch = 0\n"),
            ("input.record_iq", "[input]\nrecord_iq = \"s.cf32\"\n"),
            ("spot.allowlist", "[spot]\nallowlist = [\"W1AW\"]\n"),
            (
                "spot.blocklist_path",
                "[spot]\nblocklist_path = \"b.txt\"\n",
            ),
            ("spot.notch_path", "[spot]\nnotch_path = \"n.txt\"\n"),
            ("spot.cty_path", "[spot]\ncty_path = 'cty.dat'\n"),
            ("spot.scp_path", "[spot]\nscp_path = 'MASTER.SCP'\n"),
            (
                "spot.cty_sha256",
                "[spot]\ncty_sha256 = 'e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1'\n",
            ),
            (
                "spot.scp_sha256",
                "[spot]\nscp_sha256 = '00692208fa86b1498dcd6fc0f88e14d2a1a0462ba5d481855da5de69be4e3cf0'\n",
            ),
            (
                "spot.pattern_path",
                "[spot]\npattern_path = 'patterns.lst'\n",
            ),
            ("decode.engine", "[decode]\nengine = \"legacy\"\n"),
            ("decode.prior", "[decode]\nprior = \"on\"\n"),
            (
                "rbn_compare.enabled",
                "[server]\nstation_callsign = \"W1AW\"\n[rbn_compare]\nenabled = true\n",
            ),
            (
                "rbn_compare.spotters",
                "[server]\nstation_callsign = \"W1AW\"\n[rbn_compare]\n\
                 spotters = [\"N0ABC\"]\n",
            ),
        ];
    for (_, key) in FLAG_KEYS {
        let body = match SHARED.iter().find(|(k, _)| k == key) {
            Some((_, body)) => body.to_string(),
            None => {
                let field = key.strip_prefix("input.").unwrap();
                let (_, table) = TYPED
                    .iter()
                    .find(|(_, t)| t.lines().any(|l| l.starts_with(&format!("{field} ="))))
                    .unwrap_or_else(|| panic!("no [input] type takes {key}"));
                format!("[input]\n{table}")
            }
        };
        let f = write_temp_file(body.as_bytes());
        if let Err(e) = config::load(Some(f.path()), config::Env::Ignore) {
            panic!("{key}: {body:?} was rejected: {e:#}");
        }
    }
}

// MAN-86 review: the two ways the source-to-SETT wiring can advertise
// coverage manta cannot hear. Both go through `station_profile`, the
// exact code path `start_spot_server` uses, rather than calling
// `segments_for_passband` directly.

fn test_server_config(callsign: &str) -> manta_server::config::ServerConfig {
    let cfg_file = write_temp_file(
        format!(
            r#"
                [server]
                station_callsign = "{callsign}"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                "#
        )
        .as_bytes(),
    );
    let text = std::fs::read_to_string(cfg_file.path()).unwrap();
    let file: manta_server::config::DaemonConfigFile = toml::from_str(&text).unwrap();
    file.server
}

#[test]
fn sett_advertises_the_configured_minimal_validation_level() {
    let cfg = test_server_config("W3XYZ");
    let profile = station_profile(
        &cfg,
        14_040_000.0,
        (-5_000.0, 5_000.0),
        1.0,
        manta_spot::ValidationLevel::Minimal,
    );
    assert_eq!(profile.sett.to_string(), "SETT: vlMinimal 14035.0-14045.0");
}

#[test]
fn sett_advertises_a_rig_audio_source_only_above_its_dial_frequency() {
    // `--source` WAV / an audio device with --dial-freq-hz: analytic
    // audio carries spectrum ONLY above the dial, over the rig's ~3 kHz
    // AF passband -- not the +/-24 kHz its 48 kS/s stream could hold.
    let cfg = test_server_config("W3XYZ");
    let profile = station_profile(
        &cfg,
        14_027_000.0,
        (
            manta_input::AUDIO_PASSBAND_LO_HZ,
            manta_input::AUDIO_PASSBAND_HI_HZ,
        ),
        1.0,
        manta_spot::ValidationLevel::Normal,
    );
    assert_eq!(
        profile.sett.to_string(),
        "SETT: vlNormal 14027.3-14030.0",
        "audio coverage must be dial+0.3..dial+3.0 kHz"
    );
}

#[test]
fn sett_advertises_the_frequency_corrected_passband_not_the_raw_one() {
    // --freq-correction-ppm moves every emitted spot; the advertised
    // bounds have to move with them.
    let cfg = test_server_config("W3XYZ");
    let factor = manta_spot::calibration_factor_from_ppm(1_000.0).unwrap();
    let corrected = station_profile(
        &cfg,
        14_040_000.0,
        (-5_000.0, 5_000.0),
        factor,
        manta_spot::ValidationLevel::Normal,
    );
    let raw = station_profile(
        &cfg,
        14_040_000.0,
        (-5_000.0, 5_000.0),
        1.0,
        manta_spot::ValidationLevel::Normal,
    );
    assert_eq!(raw.sett.to_string(), "SETT: vlNormal 14035.0-14045.0");
    assert_eq!(corrected.sett.to_string(), "SETT: vlNormal 14049.0-14059.1");
}

// MAN-89 (PR #131 review, rounds 6 and 7): `station_geography_unresolved`
// is precomputed from the operator's configured `station_callsign`, which
// may carry an RBN `-N` per-band SSID. It must be classified through the
// SAME string `SpotMessage::from_spot` resolves -- the SSID-stripped one
// -- or a mobile node's spots go out with the de-side sentinels while
// `manta_spots_unresolved_geography_total` stays at zero. These drive the
// production entry point directly rather than stripping in the test, so
// the strip cannot silently move back out to the call sites.

#[test]
fn an_ssid_bearing_mobile_station_callsign_is_counted_as_unresolved() {
    let cty =
        manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
    for call in ["K5ARH/MM-1", "K5ARH/AM-1", "K5ARH/MM-99"] {
        assert!(
            !geography_is_unresolved(&cty, call),
            "test premise: unstripped, {call} reads as resolved -- this is the bug"
        );
        assert!(
            station_geography_unresolved(&cty, call),
            "{call} carries the de-side sentinels and must be counted"
        );
    }
}

#[test]
fn an_ssid_bearing_ordinary_station_callsign_is_not_counted_as_unresolved() {
    // The other direction: stripping must not turn a perfectly resolvable
    // node identity into a counted one.
    let cty =
        manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
    assert!(!station_geography_unresolved(&cty, "W1AW-1"));
    assert!(!station_geography_unresolved(&cty, "W1AW/P-2"));
    // An SSID-free identity is classified exactly as before.
    assert!(!station_geography_unresolved(&cty, "W1AW"));
    assert!(station_geography_unresolved(&cty, "W1AW/MM"));
}

// MAN-44: uplink health at a glance -- `manta status` + `GET /status`.

#[test]
fn every_configured_uplink_target_is_registered_even_when_disabled() {
    // Two [[rbn_uplink]] entries, one enabled=false: the daemon's
    // Metrics must report BOTH, the disabled one as
    // UplinkHealth::Disabled -- an operator must be able to see
    // "configured but off", not an empty list.
    let cfg_file = write_temp_file(
        br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1

            [[rbn_uplink]]
            enabled = false
            target_host = "127.0.0.1"
            target_port = 2
            "#,
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (_rt, server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let snap = server.metrics.uplink_snapshot();
    assert_eq!(
        snap.len(),
        2,
        "both configured targets must be registered, including the disabled one"
    );
    assert!(snap.iter().any(|t| t.label == "127.0.0.1:1" && t.enabled));
    assert!(snap.iter().any(|t| t.label == "127.0.0.1:2" && !t.enabled));
}

/// MAN-44 end-to-end: a real daemon (port 0, discovered via
/// SpotServer::metrics_addr) answers `manta status`'s own fetch path.
#[test]
fn status_reports_a_configured_uplink_target_from_a_live_daemon() {
    let cfg_file = write_temp_file(
        br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1
            "#,
    );

    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    let (rt, server) = start_spot_server(
        loaded.server.unwrap(),
        loaded.rbn_uplink,
        None,
        SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        std::time::SystemTime::UNIX_EPOCH,
        0,
        bundled_reference_lists(),
        &PipelineConfig::default(),
        RelayConfigs::default(),
        spot_outputs::SpotOutputs::default(),
    )
    .unwrap();

    let doc = rt
        .block_on(fetch_status(
            &[server.metrics_addr],
            std::time::Duration::from_secs(5),
        ))
        .unwrap()
        .doc;

    assert!(
        doc.uplink.targets.iter().any(|t| t.label == "127.0.0.1:1"),
        "expected the configured target to be visible, got: {doc:?}"
    );
    assert_eq!(
        status_exit_code(&doc),
        1,
        "a never-yet-connected enabled target must not read as healthy"
    );

    let _ = server.shutdown_tx.send(true);
    shutdown_runtime_after_drain(rt, &server.tasks);
}

fn cfg_with(metrics_bind_addr: &str, metrics_port: u16) -> manta_server::config::ServerConfig {
    manta_server::config::ServerConfig {
        metrics_bind_addr: metrics_bind_addr.to_string(),
        metrics_port,
        ..test_server_config("W3XYZ")
    }
}

/// MAN-132 made the metrics listener bind its own `metrics_bind_addr`
/// (loopback by default) instead of `bind_addr`, so `manta status`
/// must dial that address: a public `bind_addr` says nothing about
/// where `/status` listens.
#[test]
fn status_address_follows_metrics_bind_addr_not_bind_addr() {
    let server = manta_server::config::ServerConfig {
        bind_addr: "10.0.0.5".to_string(),
        metrics_port: 17302,
        ..test_server_config("W3XYZ")
    };
    assert_eq!(server.metrics_bind_addr, "127.0.0.1", "MAN-132 default");
    assert_eq!(
        resolve_status_addr(None, Some(&server)).unwrap(),
        vec!["127.0.0.1:17302".parse().unwrap()]
    );
}

#[test]
fn status_address_prefers_explicit_addr_then_config_then_default() {
    assert_eq!(
        resolve_status_addr(Some("1.2.3.4:9999"), None).unwrap(),
        vec!["1.2.3.4:9999".parse().unwrap()]
    );
    // bind_addr 0.0.0.0 in config means "listening everywhere"; the
    // CLI still has to DIAL something, and loopback is the only
    // address guaranteed to reach the local daemon.
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("0.0.0.0", 17302))).unwrap(),
        vec!["127.0.0.1:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("::", 17302))).unwrap(),
        vec!["[::1]:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, Some(&cfg_with("10.0.0.5", 17302))).unwrap(),
        vec!["10.0.0.5:17302".parse().unwrap()]
    );
    assert_eq!(
        resolve_status_addr(None, None).unwrap(),
        vec!["127.0.0.1:7302".parse().unwrap()]
    );
}

/// MAN-44 remediate regression: `--addr` must accept a hostname too,
/// the same way the `--server-config`/`bind_addr` path already does
/// (`status_address_resolves_a_hostname_bind_addr_like_the_daemon_does`
/// below) -- an explicit `--addr localhost:PORT` was being rejected
/// outright by a literal `SocketAddr` parse, contradicting
/// `docs/RUNBOOKS/uplink-health.md`'s documented cross-host invocation.
#[test]
fn status_address_resolves_a_hostname_passed_via_addr() {
    let addrs = resolve_status_addr(Some("localhost:17302"), None).unwrap();
    assert!(!addrs.is_empty(), "expected at least one resolved address");
    for addr in &addrs {
        assert!(
            addr.ip().is_loopback(),
            "expected localhost to resolve to a loopback address, got {addr}"
        );
        assert_eq!(addr.port(), 17302);
    }
}

/// MAN-44 CR-B regression: the daemon binds `bind_addr` via
/// `TcpListener::bind((host, port))`, which resolves a hostname (not
/// just a literal IP) through `ToSocketAddrs` -- so `bind_addr =
/// "localhost"` is a config the daemon runs on happily. `manta status
/// --server-config` must resolve it the same way instead of rejecting
/// a config the daemon itself accepts.
#[test]
fn status_address_resolves_a_hostname_bind_addr_like_the_daemon_does() {
    let addrs = resolve_status_addr(None, Some(&cfg_with("localhost", 17302))).unwrap();
    assert!(!addrs.is_empty(), "expected at least one resolved address");
    for addr in &addrs {
        assert!(
            addr.ip().is_loopback(),
            "expected localhost to resolve to a loopback address, got {addr}"
        );
        assert_eq!(addr.port(), 17302);
    }
}

/// Reads the client's request through its blank line before a fake
/// `/status` server answers it, as the real `metrics_http::read_headers`
/// does (without that function's line-count and line-length bounds --
/// the peer here is always this file's own `fetch_status`). A fake
/// that closes with the request still unread makes the kernel send RST
/// instead of FIN: macOS then fails the client's pending read with
/// "Connection reset by peer (os error 54)" and drops the response it
/// had not consumed yet, while Linux delivers the response first and
/// hides the reset -- why these tests were green on ubuntu-latest and
/// red on macos-latest.
async fn read_request_head(socket: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
}

/// Code-review regression (finding 1): a hostname that resolves to
/// several addresses -- e.g. `localhost` returning `::1` before
/// `127.0.0.1` on a dual-stack host -- must not make `manta status`
/// give up after dialing only the FIRST candidate. The previous
/// `resolve_status_addr`/`fetch_status_inner` kept only
/// `to_socket_addrs().next()`, so a real daemon bound IPv4-only (the
/// project's own default `bind_addr = "0.0.0.0"`) was reported as
/// unreachable whenever the resolver listed an unreachable address
/// first. This reproduces that shape directly -- a dead IPv6 loopback
/// candidate followed by a live IPv4-only listener -- so it fails on
/// any implementation that dials only the first address, regardless
/// of what a given machine's real resolver happens to return for
/// "localhost".
#[tokio::test]
async fn fetch_status_falls_back_past_an_unreachable_first_address() {
    // A closed port on ::1: nothing is listening, so connecting here
    // fails immediately (connection refused) rather than hanging.
    let dead = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 1);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let live = listener.local_addr().unwrap();
    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        let _ = socket.shutdown().await;
    });

    let doc = fetch_status(&[dead, live], std::time::Duration::from_secs(5))
        .await
        .expect("must fall back to the second address after the first refuses")
        .doc;
    assert_eq!(doc.schema_version, 1);
}

/// MAN-44 code review CR-1 regression: a black-holed first candidate
/// (SYNs silently dropped, not refused) must not consume the WHOLE
/// overall timeout budget before a later, live candidate is ever
/// tried -- same shape and same reasoning as
/// `uplink::connect_first_reachable_bounded_stops_at_the_overall_deadline`:
/// a real SYN black hole depends on undocumented host/network behavior
/// (a host with no route to a given block gets an immediate
/// `NetworkUnreachable` instead of a hang), so this fakes an
/// unconditionally hanging first attempt under paused tokio time
/// instead of dialing a real address. `connect_any`'s previous bare
/// `TcpStream::connect` with no per-candidate bound would have let
/// address 1 alone eat the entire `overall_timeout` here, never
/// reaching address 2.
#[tokio::test(start_paused = true)]
async fn connect_any_bounded_falls_through_a_stalled_first_candidate() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let overall_timeout = std::time::Duration::from_secs(10);
    let addrs: Vec<std::net::SocketAddr> = vec![
        "127.0.0.1:1".parse().unwrap(),
        "127.0.0.1:2".parse().unwrap(),
    ]; // never actually dialed -- `connect` below is faked

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_connect = attempts.clone();
    let connect = move |addr: std::net::SocketAddr| {
        let attempts = attempts_for_connect.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            if addr.port() == 1 {
                std::future::pending::<std::io::Result<()>>().await
            } else {
                Ok(())
            }
        }
    };

    let started = tokio::time::Instant::now();
    let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
        .await
        .expect("must fall through to the live second candidate");
    assert_eq!(addr.port(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() < overall_timeout,
        "the stalled first candidate must not consume the whole overall budget: elapsed {:?}",
        started.elapsed()
    );
}

/// Codex review, PR #95: a candidate that fails fast must hand its
/// unused slice of the budget to the candidates after it. Splitting
/// `overall_timeout` evenly up front gave the live fourth address here
/// only 5s / 4 = 1.25s, so a daemon that takes 3s to accept timed out
/// (exit 2, "could not reach") with nearly the whole deadline unspent.
#[tokio::test(start_paused = true)]
async fn connect_any_bounded_carries_unused_time_forward_to_later_candidates() {
    let overall_timeout = std::time::Duration::from_secs(5);
    let addrs: Vec<std::net::SocketAddr> = (1..=4)
        .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
        .collect(); // never actually dialed -- `connect` below is faked
    let connect = |addr: std::net::SocketAddr| async move {
        if addr.port() < 4 {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            ))
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            Ok(())
        }
    };

    let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
        .await
        .expect("three instant refusals must leave the fourth candidate the rest of the deadline");
    assert_eq!(addr.port(), 4);
}

/// Same terminal-injection concern as `status::render_human`'s label
/// escaping (Codex review, PR #95), on the failure path: a spoofed
/// endpoint's status line is quoted in the error `manta status` prints
/// to stderr.
#[tokio::test]
async fn fetch_status_escapes_control_characters_in_a_peer_status_line() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 500 \x1b]0;pwned\x07Oops\r\n\r\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });

    let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect_err("a 500 status line must be an error");
    let message = status_failure_message(&err);
    assert!(
        !message.chars().any(char::is_control),
        "no peer-supplied control character may reach the terminal: {message:?}"
    );
    assert!(message.contains(r"\u{1b}]0;pwned\u{7}Oops"));
}

/// serde_json's data errors quote the offending value ("unknown variant
/// `...`"), and that value comes from the peer -- same escaping as the
/// status line above (Codex review, PR #95).
#[test]
fn parse_status_doc_escapes_control_characters_quoted_in_a_serde_error() {
    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled)
        .to_json()
        .replace(r#""health": "disabled""#, r#""health": "\u001b[2J\npwned""#);
    let err = parse_status_doc(&body).expect_err("an unknown health variant must not parse");
    let message = status_failure_message(&err);
    assert!(
        message.contains("not a valid status document"),
        "{message:?}"
    );
    assert!(
        !message.chars().any(char::is_control),
        "no peer-supplied control character may reach the terminal: {message:?}"
    );
    assert!(message.contains(r"\u{1b}[2J\npwned"), "{message:?}");
}

/// Local snippets keep their source and caret, with visible line breaks.
#[test]
fn status_config_parse_errors_keep_their_escaped_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "[server]\nbind_addr = \n").unwrap();
    let err = run_status(Some(&path), None, 5).expect_err("a broken config must not parse");
    let message = status_failure_message(&err);
    assert!(message.contains("TOML parse error"), "{message:?}");
    assert!(message.contains("bind_addr = ") && message.contains('^'));
    assert!(
        !message.chars().any(char::is_control) && message.contains(r"\n"),
        "the TOML snippet must keep visible line breaks: {message:?}"
    );
}

fn doc_with(
    health: manta_server::metrics::OverallUplinkHealth,
) -> manta_server::status_doc::StatusDoc {
    manta_server::status_doc::StatusDoc {
        schema_version: 1,
        version: "test".to_string(),
        uptime_seconds: 0,
        spots_total: 0,
        telnet_clients: 0,
        json_clients: 0,
        ws_clients: 0,
        active_tracks: None,
        uplink: manta_server::status_doc::UplinkStatus {
            health,
            connected_targets: 0,
            enabled_targets: 0,
            sent_total: 0,
            suppressed_total: 0,
            reconnects_total: 0,
            reconnect_window_seconds: 300,
            flapping_threshold: 3,
            targets: vec![],
        },
        inputs: vec![],
    }
}

#[test]
fn parse_status_doc_keeps_fields_this_build_does_not_declare() {
    use manta_server::metrics::OverallUplinkHealth;
    let doc = doc_with(OverallUplinkHealth::Down);
    let mut value = serde_json::to_value(&doc).unwrap();
    value["foo"] = serde_json::json!(1);
    value["uplink"]["uplink_extra"] = serde_json::json!("x");

    let reply = parse_status_doc(&value.to_string()).unwrap();
    assert_eq!(reply.raw["foo"], 1);
    assert_eq!(reply.raw["uplink"]["uplink_extra"], "x");
    assert_eq!(reply.doc, doc);
    assert_eq!(status_exit_code(&reply.doc), 1);
}

#[test]
fn status_exit_code_is_zero_when_healthy_one_when_degraded() {
    use manta_server::metrics::OverallUplinkHealth;
    assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Ok)), 0);
    assert_eq!(
        status_exit_code(&doc_with(OverallUplinkHealth::Disabled)),
        0
    );
    assert_eq!(
        status_exit_code(&doc_with(OverallUplinkHealth::Degraded)),
        1
    );
    assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Down)), 1);
}

#[tokio::test]
async fn fetch_status_parses_a_chunk_split_http_response() {
    // A body assembled from one read() would pass trivially and hide
    // a real framing bug -- the status line, headers, and body are
    // deliberately written in three separate writes here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        socket
                .write_all(
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        socket.write_all(body.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    });

    let doc = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect("must parse a response split across several writes")
        .doc;
    assert_eq!(doc.schema_version, 1);
}

#[tokio::test]
async fn fetch_status_errors_cleanly_on_a_404_and_on_a_non_json_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });
    let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
        .await
        .expect_err("a 404 must be a clean error, not a panic");
    assert!(format!("{err:#}").contains("404"));

    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut socket, _) = listener2.accept().await.unwrap();
        read_request_head(&mut socket).await;
        let body = "not json";
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let _ = socket.shutdown().await;
    });
    let err = fetch_status(&[addr2], std::time::Duration::from_secs(5))
        .await
        .expect_err("a non-JSON body must be a clean error, not a panic");
    assert!(format!("{err:#}").contains("status document"));
}

#[tokio::test]
async fn fetch_status_times_out_instead_of_hanging_on_a_silent_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _peer) = listener.accept().await.unwrap();
        // Accepts, then never writes anything -- the socket stays open.
        std::future::pending::<()>().await
    });

    let started = std::time::Instant::now();
    let err = fetch_status(&[addr], std::time::Duration::from_millis(200))
        .await
        .expect_err("a silent server must time out, not hang forever");
    assert!(err.to_string().contains("timed out"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "must not have hung past the configured timeout"
    );
}

// ---- MAN-301: `run --rbn-compare` / `--rbn-spotter`

/// `run`'s parsed RBN comparison flags.
fn run_rbn_opts(extra: &[&str]) -> Result<RbnCompareOpts, clap::Error> {
    let argv = ["manta", "run"].iter().chain(extra);
    match Cli::try_parse_from(argv)?.command {
        Command::Run { rbn, .. } => Ok(rbn),
        _ => unreachable!("`manta run` parses as Command::Run"),
    }
}

const RBN_SERVER: &str = "[server]\nstation_callsign = \"W3XYZ-1\"\n";

/// `prepare_config` over a config file `body` and the given flags, with
/// no environment (the same path `run` and `config check` take).
fn prepare_rbn(body: &str, rbn: RbnCompareOpts, source: Option<&str>) -> Result<Prepared> {
    let f = write_temp_file(body.as_bytes());
    let cli = CliOverrides {
        prior: None,
        source: source.map(PathBuf::from),
        rbn,
        ..CliOverrides::none()
    };
    prepare_config(cli, Some(f.path().to_path_buf()), None, &[])
}

/// `prepare_rbn` followed by `run`'s replay refusal.
fn run_rbn(body: &str, rbn: RbnCompareOpts, source: Option<&str>) -> Result<Prepared> {
    let prepared = prepare_rbn(body, rbn, source)?;
    refuse_rbn_compare_replay(&prepared.resolved, source.is_some())?;
    Ok(prepared)
}

#[test]
fn rbn_compare_flag_enables_comparison() {
    let rbn = run_rbn_opts(&["--rbn-compare"]).unwrap();
    assert!(rbn.rbn_compare);
    assert!(run_rbn_opts(&[]).unwrap().rbn_spotter.is_empty());
    // `listen` is `run`'s alias and takes the same flags.
    assert!(Cli::try_parse_from(["manta", "listen", "--rbn-compare"]).is_ok());
    // The flag alone (no [rbn_compare] table) enables with defaults.
    let p = prepare_rbn(RBN_SERVER, rbn.clone(), None).unwrap();
    let rc = p.resolved.rbn_compare.expect("the flag creates the table");
    assert!(rc.enabled);
    assert_eq!(rc.window_secs, 180);
    // Over a disabled table, the flag enables and keeps its keys.
    let body = format!("{RBN_SERVER}[rbn_compare]\nenabled = false\nwindow_secs = 300\n");
    let rc = prepare_rbn(&body, rbn.clone(), None)
        .unwrap()
        .resolved
        .rbn_compare
        .unwrap();
    assert!(rc.enabled);
    assert_eq!(rc.window_secs, 300);
    // Without the flag, the file decides.
    let rc = prepare_rbn(&body, RbnCompareOpts::default(), None)
        .unwrap()
        .resolved
        .rbn_compare
        .unwrap();
    assert!(!rc.enabled);
    assert!(prepare_rbn(RBN_SERVER, RbnCompareOpts::default(), None)
        .unwrap()
        .resolved
        .rbn_compare
        .is_none());
    // The flag without [server] is the table's own needs-[server] refusal.
    let err = format!(
        "{:#}",
        prepare_rbn("[decode]\nengine = \"legacy\"\n", rbn, None)
            .err()
            .unwrap()
    );
    assert!(
        err.ends_with(
            ": [rbn_compare] needs a [server] table (grades are published on the JSON \
                 stream and /metrics, which only the daemon runs)"
        ),
        "{err}"
    );
}

#[test]
fn rbn_spotter_flag_replaces_file_spotters() {
    let rbn = run_rbn_opts(&["--rbn-spotter", "n0abc", "--rbn-spotter", "N0XYZ"]).unwrap();
    assert_eq!(
        rbn.rbn_spotter,
        vec!["N0ABC".to_string(), "N0XYZ".to_string()]
    );
    assert!(run_rbn_opts(&["--rbn-spotter", "N0ABC-#"]).is_err());
    assert!(run_rbn_opts(&["--rbn-spotter", "not a call"]).is_err());
    let body = format!("{RBN_SERVER}[rbn_compare]\nenabled = true\nspotters = [\"W1AW\"]\n");
    let rc = prepare_rbn(&body, rbn, None)
        .unwrap()
        .resolved
        .rbn_compare
        .unwrap();
    assert_eq!(rc.spotters, vec!["N0ABC".to_string(), "N0XYZ".to_string()]);
    let rc = prepare_rbn(&body, RbnCompareOpts::default(), None)
        .unwrap()
        .resolved
        .rbn_compare
        .unwrap();
    assert_eq!(rc.spotters, vec!["W1AW".to_string()]);
    // --rbn-spotter naming this station is refused like the file's list.
    let own = run_rbn_opts(&["--rbn-spotter", "w3xyz"]).unwrap();
    let err = format!("{:#}", prepare_rbn(&body, own, None).err().unwrap());
    assert_eq!(
        err,
        "--rbn-spotter W3XYZ is this station's own callsign: its reports are always \
             excluded, so every spot would grade none"
    );
    // So is one the file's exclude_spotters names.
    let body = format!("{body}exclude_spotters = [\"N0XYZ\"]\n");
    let excluded = run_rbn_opts(&["--rbn-spotter", "N0ABC", "--rbn-spotter", "n0xyz"]).unwrap();
    let err = format!("{:#}", prepare_rbn(&body, excluded, None).err().unwrap());
    assert_eq!(
        err,
        "--rbn-spotter N0XYZ is in [rbn_compare].exclude_spotters: its reports would never \
             count"
    );
}

#[test]
fn rbn_spotter_without_comparison_is_refused() {
    let rbn = run_rbn_opts(&["--rbn-spotter", "N0ABC"]).unwrap();
    for body in [
        RBN_SERVER.to_string(),
        format!("{RBN_SERVER}[rbn_compare]\nenabled = false\n"),
    ] {
        let err = format!("{:#}", prepare_rbn(&body, rbn.clone(), None).err().unwrap());
        assert_eq!(
            err,
            "--rbn-spotter needs RBN comparison: add --rbn-compare or set \
                 [rbn_compare].enabled = true"
        );
    }
    let with_flag = run_rbn_opts(&["--rbn-compare", "--rbn-spotter", "N0ABC"]).unwrap();
    assert!(prepare_rbn(RBN_SERVER, with_flag, None).is_ok());
}

#[test]
fn rbn_compare_with_file_replay_is_refused() {
    let enabled = format!("{RBN_SERVER}[rbn_compare]\nenabled = true\n");
    let err = format!(
        "{:#}",
        run_rbn(&enabled, RbnCompareOpts::default(), Some("x.wav"))
            .err()
            .unwrap()
    );
    assert_eq!(
        err,
        "RBN comparison grades spots against the live RBN feed, but this run replays a \
             recording (--source). Grade a recorded node's spot log with `manta grade` instead."
    );
    let flag = run_rbn_opts(&["--rbn-compare"]).unwrap();
    assert!(run_rbn(RBN_SERVER, flag, Some("x.wav")).is_err());
    let file_input = format!("{enabled}[input]\ntype = \"file\"\npath = \"/x.wav\"\n");
    let err = format!(
        "{:#}",
        run_rbn(&file_input, RbnCompareOpts::default(), None)
            .err()
            .unwrap()
    );
    assert!(
        err.starts_with(
            "RBN comparison grades spots against the live RBN feed, but this run replays a \
                 recording ([input] type = \"file\")."
        ),
        "{err}"
    );
    // A replay with comparison off is unaffected.
    assert!(run_rbn(RBN_SERVER, RbnCompareOpts::default(), Some("x.wav")).is_ok());
}

/// `soak` and `doctor` share `prepare_config` with `run` but never start
/// the comparator, so a node config with `[rbn_compare]` enabled still
/// prepares over a replayed recording; only `run` refuses it.
#[test]
fn soak_and_doctor_prepare_a_replay_with_rbn_compare_enabled() {
    let enabled = format!("{RBN_SERVER}[rbn_compare]\nenabled = true\n");
    let p = prepare_rbn(&enabled, RbnCompareOpts::default(), Some("x.wav")).unwrap();
    assert!(p.resolved.rbn_compare.is_some_and(|c| c.enabled));
    let file_input = format!("{enabled}[input]\ntype = \"file\"\npath = \"/x.wav\"\n");
    assert!(prepare_rbn(&file_input, RbnCompareOpts::default(), None).is_ok());
    assert!(run_rbn(&file_input, RbnCompareOpts::default(), None).is_err());
}

#[test]
fn spot_bus_for_uses_configured_history_or_the_default() {
    let mut cfg = test_server_config("W1AW");
    let bus = spot_bus_for(&cfg, 1.0, std::time::SystemTime::UNIX_EPOCH, 0);
    assert_eq!(
        bus.history_minutes(),
        manta_server::bus::DEFAULT_HISTORY_MINUTES
    );
    cfg.history_minutes = Some(90);
    let bus = spot_bus_for(&cfg, 1.0, std::time::SystemTime::UNIX_EPOCH, 0);
    assert_eq!(bus.history_minutes(), 90);
}
