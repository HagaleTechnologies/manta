//! MAN-285: an older CLI preserves additive fields from a newer daemon.

mod common;

use common::{fake_server, http_reply};
use manta_server::status_doc::{render_human, StatusDoc};
use serde_json::{json, Value};
use std::process::{Command, Output};
use std::time::Duration;

fn status_body(health: &str) -> Value {
    json!({
        "schema_version": 1,
        "version": "9.9.9-newer-daemon",
        "uptime_seconds": 123,
        "spots_total": 456,
        "telnet_clients": 1,
        "json_clients": 0,
        "ws_clients": 0,
        "active_tracks": 2,
        "foo": 1,
        "uplink": {
            "health": health,
            "connected_targets": 0,
            "enabled_targets": 0,
            "sent_total": 0,
            "suppressed_total": 0,
            "reconnects_total": 0,
            "reconnect_window_seconds": 300,
            "flapping_threshold": 3,
            "targets": [],
            "uplink_extra": "x"
        }
    })
}

fn status(body: &Value, json: bool) -> Output {
    let (addr, requests) = fake_server(http_reply("200 OK", &body.to_string()), 1);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // Keep the test runner's config and overrides out of the child.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(["status", "--addr", &addr.to_string()]);
    if json {
        cmd.arg("--json");
    }
    let output = cmd.output().unwrap();
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(request.starts_with("GET /status HTTP/1.1"), "{request:?}");
    output
}

#[test]
fn status_json_prints_a_field_this_build_does_not_know() {
    let body = status_body("down");
    let output = status(&body, true);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"foo\""), "{stdout}");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["foo"], 1);
    assert_eq!(value["uplink"]["uplink_extra"], "x");
    assert_eq!(value, body);
}

#[test]
fn status_summary_is_unchanged_by_a_field_this_build_does_not_know() {
    let body = status_body("ok");
    let doc: StatusDoc = serde_json::from_value(body.clone()).unwrap();
    let output = status(&body, false);
    let mut without_extra = body;
    without_extra.as_object_mut().unwrap().remove("foo");
    without_extra["uplink"]
        .as_object_mut()
        .unwrap()
        .remove("uplink_extra");
    let baseline = status(&without_extra, false);
    for result in [&output, &baseline] {
        assert_eq!(result.status.code(), Some(0), "{result:?}");
        assert!(result.stderr.is_empty(), "{result:?}");
    }
    assert_eq!(output.stdout, baseline.stdout);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        render_human(&doc)
    );
}
