//! MAN-159 acceptance scenario:
//!   Scenario: Enabling the uplink without an explicit dry_run setting does not transmit
//!     ...
//!     And a startup log line tells the operator dry_run is on by default
//!     and how to disable it
//!
//! `uplink.rs` had no `tracing` output at all before MAN-159, so this is
//! also the first log-assertion harness in the workspace.

use manta_server::bus::SpotBus;
use manta_server::config::DaemonConfigFile;
use manta_server::metrics::Metrics;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

/// Parses `toml_src`, runs `serve()` to its first shutdown check with a
/// capturing subscriber installed, and returns everything it logged.
///
/// Shutdown is signalled BEFORE the call: `serve()` emits the startup line
/// and then returns at the top of its reconnect loop without opening a
/// socket, so this test needs no listener and never races. `serve()` is
/// awaited directly rather than `tokio::spawn`ed on purpose -- the
/// `tracing` dispatcher installed by `set_default` is thread-local and does
/// NOT propagate into a spawned task, so a spawned variant captures nothing.
async fn logs_from_serve(toml_src: &str) -> String {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();

    let file: DaemonConfigFile = toml::from_str(toml_src).unwrap();
    let cfg = file.rbn_uplink[0].clone();

    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(96_000.0, epoch, 0));
    // MAN-44 registered per-target uplink state: `serve()` now takes the
    // target handle its own counters live on, not the whole `Metrics`.
    let metrics = Arc::new(Metrics::new());
    let target = metrics.register_uplink_target(
        manta_server::uplink::target_specs(std::slice::from_ref(&cfg)).remove(0),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let _ = shutdown_tx.send(true);

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        manta_server::uplink::serve(cfg, "W3XYZ".to_string(), bus, target, shutdown_rx).await;
    }

    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

const NO_DRY_RUN_KEY: &str = r#"
    [server]
    station_callsign = "W3XYZ"
    [[rbn_uplink]]
    enabled = true
    target_host = "127.0.0.1"
    target_port = 1
"#;

#[tokio::test]
async fn startup_log_announces_the_dry_run_default_and_how_to_disable_it() {
    let out = logs_from_serve(NO_DRY_RUN_KEY).await;
    assert!(out.contains("uplink:"), "module prefix missing: {out}");
    assert!(out.contains("dry_run is ON"), "state not announced: {out}");
    assert!(
        out.contains("dry_run = false"),
        "no instruction for how to disable it: {out}"
    );
    assert!(out.contains("INFO"), "expected info level: {out}");
}

#[tokio::test]
async fn startup_log_warns_when_the_operator_has_opted_into_transmitting() {
    let out = logs_from_serve(&format!("{NO_DRY_RUN_KEY}dry_run = false\n")).await;
    assert!(out.contains("WARN"), "live transmit must warn: {out}");
    assert!(out.contains("transmitting real spots"), "{out}");
}

#[tokio::test]
async fn a_disabled_uplink_logs_nothing() {
    let out = logs_from_serve(&NO_DRY_RUN_KEY.replace("enabled = true", "enabled = false")).await;
    assert!(out.is_empty(), "disabled uplink should be silent: {out}");
}
