//! MAN-285: an older CLI preserves additive fields from a newer daemon.

mod common;

use common::{fake_server, http_reply};
use manta_server::status_doc::{render_human, StatusDoc};
use serde_json::{json, Value};
use std::process::Output;
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
    let mut cmd = common::manta();
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

/// MAN-541: the design's `rbn_compare` object, with the 20m warning held.
fn rbn_compare_body() -> Value {
    let band = |band: &str, hz: f64, n: u64, ppm: Value, err: Value, over: Value, warning: bool| {
        json!({"band": band, "median_offset_hz": hz, "samples": n, "median_ppm": ppm,
               "ppm_error": err, "over_limit_seconds": over, "warning": warning})
    };
    let reading = |band: &str, hz: f64, n: u64, ppm: Value, err: Value| {
        json!({"band": band, "median_offset_hz": hz, "samples": n, "median_ppm": ppm,
               "ppm_error": err})
    };
    json!({
        "window_seconds": 3600,
        "min_samples": 20,
        "skew_warn_ppm": 2.0,
        "skew_hold_seconds": 1800,
        "bands": [
            band("80m", 13.5, 41, json!(3.803), json!(2.2), Value::Null, false),
            band("40m", 21.0, 8, Value::Null, Value::Null, Value::Null, false),
            band("20m", 42.0, 40, json!(2.995), json!(0.564), json!(2100), true),
            band("15m", 63.0, 24, json!(2.996), json!(0.485), json!(720), false),
        ],
        "inputs": [{
            "entry": null,
            "names": ["kiwi"],
            "applied_ppm": 0.0,
            "replaced_by_flag": false,
            "bands": [
                reading("80m", 13.5, 41, json!(3.803), json!(2.2)),
                reading("40m", 21.0, 8, Value::Null, Value::Null),
                reading("20m", 42.0, 40, json!(2.995), json!(0.564)),
                reading("15m", 63.0, 24, json!(2.996), json!(0.485)),
            ],
            "observed_ppm": 3.017,
            "ppm_error": 0.363,
            "suggested_ppm": -3.02
        }]
    })
}

#[test]
fn status_prints_the_rbn_skew_block_from_a_daemon() {
    let mut body = status_body("ok");
    body["rbn_compare"] = rbn_compare_body();
    let output = status(&body, false);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("RBN skew, last hour"), "{stdout}");
    assert!(
        stdout.contains("  [input]                            0.00   -3.02  0.36\n"),
        "{stdout}"
    );
    assert!(stdout.contains("WARN for 35 min"), "{stdout}");
}

#[test]
fn status_exit_code_ignores_a_skew_warning() {
    let mut body = status_body("ok");
    body["rbn_compare"] = rbn_compare_body();
    let output = status(&body, false);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}
