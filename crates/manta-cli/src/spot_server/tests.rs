use super::*;
use crate::config;
use crate::daemon_client::fetch_status;
use crate::daemon_client::status_exit_code;
use crate::shutdown_runtime_after_drain;
use crate::signal_spot_server_shutdown;
use crate::test_support::*;
use manta_server::spot_message::geography_is_unresolved;
use manta_server::spot_message::station_geography_unresolved;

#[test]
fn start_spot_server_uses_the_cty_table_it_is_given() {
    let table = std::sync::Arc::new(manta_spot::cty::Table::parse_with_dxcc(
        &GEOGRAPHY_CTY_FIXTURE.replace("K,W,N;", "K,W,N,QQ9;"),
        GEOGRAPHY_DXCC_FIXTURE,
    ));
    let loaded = loaded_from(
            "[server]\nstation_callsign = 'W1AW'\nbind_addr = '127.0.0.1'\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
        );
    let (test_runtime, server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: vec![],
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
        epoch: std::time::UNIX_EPOCH,
        session_nonce: 0,
        lists: std::sync::Arc::new(manta_spot::reference::ReferenceLists::new(
            table.clone(),
            std::sync::Arc::new(manta_spot::scp::Set::bundled()),
        )),
        pipeline: &PipelineConfig {
            cty: Some(table.clone()),
            ..Default::default()
        },
        relays: RelayConfigs::default(),
        outputs: spot_outputs::SpotOutputs::default(),
    })
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

// MAN-316: the daemon's per-spot geography check reads the table in use
// now, so a refreshed cty.dat changes it without a restart, and the
// station side is checked against the same (current) table.
#[test]
fn published_spot_geography_follows_a_swapped_cty_table() {
    let lists = bundled_reference_lists();
    let loaded = loaded_from(
            "[server]\nstation_callsign = 'QQ9ZZZ'\nbind_addr = '127.0.0.1'\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
        );
    let (test_runtime, server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: vec![],
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
        epoch: std::time::UNIX_EPOCH,
        session_nonce: 0,
        lists: lists.clone(),
        pipeline: &PipelineConfig::default(),
        relays: RelayConfigs::default(),
        outputs: spot_outputs::SpotOutputs::default(),
    })
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
    let (rt, server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
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
    let (rt, server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
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
    let (rt, _server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
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
    let (rt, _server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
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
    let result = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_040_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
            coverage: &[],
            inputs: None,
        },
        epoch: std::time::SystemTime::UNIX_EPOCH,
        session_nonce: 0,
        lists: bundled_reference_lists(),
        pipeline: &PipelineConfig::default(),
        relays: RelayConfigs::default(),
        outputs: spot_outputs::SpotOutputs::default(),
    });
    assert!(
        result.is_ok(),
        "a config with the operator-identity keys present must still start: {:?}",
        result.err()
    );
}

/// The belt-and-braces path for a caller that skips `config::load`: a
/// hand-built config with half a lat/lon pair never starts a listener.
#[test]
fn start_spot_server_rejects_an_inconsistent_station_location() {
    let cfg = manta_server::config::ServerConfig {
        operator_lat: Some(41.7),
        ..test_server_config("W1AW")
    };
    let Err(err) = start_spot_server(SpotServerParams {
        cfg,
        rbn_uplink_cfgs: Vec::new(),
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
    }) else {
        panic!("half a lat/lon pair must not start the servers");
    };
    let err = format!("{err:#}");
    assert!(err.contains("operator_lon"), "{err}");
    assert!(err.starts_with("[server]: "), "{err}");
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
    let (rt, _server) = start_spot_server(SpotServerParams {
        cfg: loaded.server.unwrap(),
        rbn_uplink_cfgs: loaded.rbn_uplink,
        rbn_compare_cfg: None,
        source: SourceInfo {
            name: "file",
            sample_rate_hz: 96_000.0,
            dial_freq_hz: 14_000_000.0,
            rf_passband_hz: (-48_000.0, 48_000.0), // no resampling source here
            freq_calibration: 1.0,                 // --freq-correction-ppm 0
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
    let (_rt, server) = start_spot_server(SpotServerParams {
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
