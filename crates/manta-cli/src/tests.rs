use super::*;
use crate::resolve::resolve_spot;
use crate::run_cmd::RunArgs;
use crate::spot_server::start_spot_server;
use crate::spot_server::RelayConfigs;
use crate::spot_server::SourceInfo;
use crate::spot_server::SpotServerParams;
use crate::test_support::*;
use manta_server::spot_message::geography_is_unresolved;
use manta_server::spot_message::station_geography_unresolved;
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
fn pinned(spot: &SpotResolved) -> Result<(PipelineConfig, [list_refresh::InUse; 3])> {
    build_pipeline_config(0.0, spot, Default::default(), Engine::Legacy)
}
// ---- MAN-542: the pattern list in use

/// A `[spot]` with refresh on and `list_cache_dir`, and the given
/// `pattern_url` and `pattern_path`, resolved as `run` does.
fn pattern_spot(
    cache: &std::path::Path,
    pattern_url: Option<&str>,
    pattern_path: Option<PathBuf>,
) -> SpotResolved {
    let spot = config::SpotFile {
        refresh_lists: true,
        list_cache_dir: Some(cache.to_path_buf()),
        pattern_url: pattern_url.map(str::to_string),
        pattern_path,
        ..Default::default()
    };
    resolve_spot(vec![], None, None, None, None, None, &spot)
}

/// Seeds `cache` with a current pattern version holding `+QQ9@@@`;
/// returns its SHA-256.
fn cache_pattern_version(cache: &std::path::Path) -> String {
    let text = format!("{}+QQ9@@@\n", manta_spot::PATTERNS_LST);
    let sha = list_cache::sha256_hex(text.as_bytes());
    let now = list_cache::unix_secs(SystemTime::now());
    list_cache::Cache::new(cache.to_path_buf())
        .store(
            ListKind::Patterns,
            text.as_bytes(),
            &list_cache::State {
                sha256: sha.clone(),
                url: "http://mirror.lan/Patt3Ch.lst".into(),
                downloaded_at: now,
                checked_at: now,
                last_modified: None,
            },
        )
        .unwrap();
    sha
}

#[test]
fn build_pipeline_config_reports_the_pattern_list_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let spot = pattern_spot(dir.path(), None, None);
    let (_, in_use) = pinned(&spot).unwrap();
    assert_eq!(in_use[2], list_refresh::InUse::bundled(ListKind::Patterns));
    let path = dir.path().join("patterns.lst");
    let bytes = "\u{feff}+QQ9@@@\n".as_bytes();
    std::fs::write(&path, bytes).unwrap();
    let spot = pattern_spot(dir.path(), None, Some(path.clone()));
    let (cfg, in_use) = pinned(&spot).unwrap();
    assert_eq!(
        in_use[2],
        list_refresh::InUse::new(
            ListKind::Patterns,
            list_cache::sha256_hex(bytes),
            list_refresh::Origin::File(path),
        )
    );
    assert_eq!(
        cfg.pattern_table().tier("QQ9ZZZ"),
        manta_spot::PatternTier::Common
    );
}

#[test]
fn prepare_run_lists_shares_the_configured_pattern_table() {
    let dir = tempfile::tempdir().unwrap();
    cache_pattern_version(dir.path());
    let path = dir.path().join("patterns.lst");
    std::fs::write(&path, "+K5@@@\n").unwrap();
    let url = Some("http://mirror.lan/Patt3Ch.lst");
    let spot = pattern_spot(dir.path(), url, Some(path));
    let (cfg, in_use) = pinned(&spot).unwrap();
    // Refresh off (a file source): the very table `cfg` holds.
    let off = prepare_run_lists(false, &spot, in_use.clone(), &cfg).unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &off.lists.patterns.load(),
        &cfg.pattern_table()
    ));
    // Refresh on with a cached version: `pattern_path` still wins.
    let on = prepare_run_lists(true, &spot, in_use, &cfg).unwrap();
    let table = on.lists.patterns.load();
    assert_eq!(
        (table.len(), table.tier("QQ9ZZZ")),
        (1, manta_spot::PatternTier::Unlisted)
    );
    assert!(matches!(on.in_use[2].origin, list_refresh::Origin::File(_)));
}

#[test]
fn prepare_run_lists_manages_patterns_only_with_a_url() {
    let managed_kinds = |run_lists: &RunLists| -> Vec<ListKind> {
        run_lists
            .refresh
            .as_ref()
            .unwrap()
            .lists
            .iter()
            .map(|l| l.in_use.kind)
            .collect()
    };
    let dir = tempfile::tempdir().unwrap();
    let cached_sha = cache_pattern_version(dir.path());
    // `pattern_url` set: three lists, the cache's pattern table.
    let spot = pattern_spot(dir.path(), Some("http://mirror.lan/Patt3Ch.lst"), None);
    let (cfg, in_use) = pinned(&spot).unwrap();
    let run_lists = prepare_run_lists(true, &spot, in_use, &cfg).unwrap();
    assert_eq!(managed_kinds(&run_lists), ListKind::ALL.to_vec());
    assert_eq!(run_lists.in_use[2].sha256, cached_sha);
    assert_eq!(
        run_lists.lists.patterns.load().tier("QQ9ZZZ"),
        manta_spot::PatternTier::Common
    );
    // Unset: two lists and the built-in table, cache or not (D11).
    let spot = pattern_spot(dir.path(), None, None);
    let (cfg, in_use) = pinned(&spot).unwrap();
    let run_lists = prepare_run_lists(true, &spot, in_use, &cfg).unwrap();
    assert_eq!(
        managed_kinds(&run_lists),
        vec![ListKind::Cty, ListKind::Scp]
    );
    assert_eq!(
        run_lists.in_use[2],
        list_refresh::InUse::bundled(ListKind::Patterns)
    );
    assert_eq!(
        run_lists.lists.patterns.load().tier("QQ9ZZZ"),
        manta_spot::PatternTier::Unlisted
    );
    // `pattern_path` set: two lists.
    let path = dir.path().join("patterns.lst");
    std::fs::write(&path, "+K5@@@\n").unwrap();
    let spot = pattern_spot(
        dir.path(),
        Some("http://mirror.lan/Patt3Ch.lst"),
        Some(path),
    );
    let (cfg, in_use) = pinned(&spot).unwrap();
    let run_lists = prepare_run_lists(true, &spot, in_use, &cfg).unwrap();
    assert_eq!(
        managed_kinds(&run_lists),
        vec![ListKind::Cty, ListKind::Scp]
    );
}

#[test]
fn adopt_run_lists_copies_all_three_tables() {
    let dir = tempfile::tempdir().unwrap();
    cache_pattern_version(dir.path());
    let spot = pattern_spot(dir.path(), Some("http://mirror.lan/Patt3Ch.lst"), None);
    let (cfg, in_use) = pinned(&spot).unwrap();
    let run_lists = prepare_run_lists(true, &spot, in_use.clone(), &cfg).unwrap();
    let mut adopted = cfg.clone();
    adopt_run_lists(&mut adopted, &run_lists);
    assert!(std::sync::Arc::ptr_eq(
        &adopted.pattern_table(),
        &run_lists.lists.patterns.load()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &adopted.cty_table(),
        &run_lists.lists.cty.load()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &adopted.scp_set(),
        &run_lists.lists.scp.load()
    ));
    // Refresh off: `cfg` is untouched.
    let off = prepare_run_lists(false, &spot, in_use, &cfg).unwrap();
    let mut untouched = cfg.clone();
    adopt_run_lists(&mut untouched, &off);
    assert!(untouched.patterns.is_none() && untouched.cty.is_none() && untouched.scp.is_none());
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
    let (rt, server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        epoch: std::time::SystemTime::UNIX_EPOCH,
        session_nonce: 0,
        lists: bundled_reference_lists(),
        pipeline: &PipelineConfig::default(),
        relays: RelayConfigs::default(),
        outputs: spot_outputs::SpotOutputs::default(),
    })
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

/// MAN-320: the loaded relay blocks, as `run` hands them over.
fn relay_test_start(body: &str) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    let cfg_file = write_temp_file(body.as_bytes());
    let loaded = config::load(Some(cfg_file.path()), config::Env::Ignore).unwrap();
    start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0),
            freq_calibration: 1.0,
            coverage: &[],
            inputs: None,
        },
        epoch: std::time::SystemTime::UNIX_EPOCH,
        session_nonce: 0,
        lists: bundled_reference_lists(),
        pipeline: &PipelineConfig::default(),
        relays: RelayConfigs {
            skimmer: loaded.skimmer_relay,
            wsjtx: loaded.wsjtx_relay,
        },
        outputs: spot_outputs::SpotOutputs::default(),
    })
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
    let Command::Run(RunArgs {
        ka9q_group,
        ka9q_port,
        ka9q_ssrc,
        ka9q_interface,
        ..
    }) = parsed.command
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
        Command::Run(RunArgs { logging, .. }) => Ok(logging),
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
        Command::Run(RunArgs { logging, .. }) => assert_eq!(logging.quiet, 1),
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

// ---- MAN-301: `run --rbn-compare` / `--rbn-spotter`

/// `run`'s parsed RBN comparison flags.
fn run_rbn_opts(extra: &[&str]) -> Result<RbnCompareOpts, clap::Error> {
    let argv = ["manta", "run"].iter().chain(extra);
    match Cli::try_parse_from(argv)?.command {
        Command::Run(RunArgs { rbn, .. }) => Ok(rbn),
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

/// Test-only entry point over the production loader (`config::load`), kept
/// so the `[decode]` validation tests exercise the real path unmodified.
fn load_decode_config_file(config: Option<&Path>) -> Result<manta_decode::decoder::DecodeConfig> {
    Ok(config::load(config, config::Env::Ignore)?.decode)
}
