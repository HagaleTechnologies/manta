//! MAN-96: scripts/field-node.py reads these metric families from a live node's /metrics;
//! renaming one here would silently blind a 30-day field ledger.

use manta_server::metrics::{BuildInfo, LatencyHistogram, Metrics};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The `REQUIRED_METRICS = ( ... )` tuple literal in scripts/field-node.py.
fn required_metrics() -> Vec<String> {
    let src = read_repo_file("scripts/field-node.py");
    let marker = "REQUIRED_METRICS = (";
    let start = src
        .find(marker)
        .expect("scripts/field-node.py defines `REQUIRED_METRICS = (`")
        + marker.len();
    let len = src[start..]
        .find(')')
        .expect("REQUIRED_METRICS tuple is closed with `)`");
    let body = &src[start..start + len];

    let mut names = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            break;
        };
        let literal = &after[..close];
        if literal.starts_with("manta_") {
            names.push(literal.to_string());
        }
        rest = &after[close + 1..];
    }
    assert!(
        !names.is_empty(),
        "no \"manta_...\" literals found in REQUIRED_METRICS: {body:?}"
    );
    names
}

#[test]
fn every_metric_field_node_requires_is_rendered_by_a_daemon_shaped_registry() {
    let m = Metrics::new();
    // Populated as the daemon does at startup (manta-cli's main.rs): build info, one source,
    // three listeners, the decode watchdog, one latency snapshot.
    m.set_build_info(BuildInfo {
        version: "0.1.0".to_string(),
        git_sha: "0123456789ab".to_string(),
        features: "soapy".to_string(),
    });
    m.set_source_health("soapy", true);
    for listener in ["telnet", "json", "metrics"] {
        m.set_listener_up(listener, true);
    }
    m.arm_decode_watchdog();
    m.set_decode_latency(LatencyHistogram {
        bounds_seconds: vec![0.001],
        bucket_counts: vec![1, 0],
        sum_seconds: 0.0005,
    });
    let text = m.render_prometheus_text();
    for name in required_metrics() {
        assert!(
            text.contains(&format!("# TYPE {name} ")),
            "field-node.py requires {name}; /metrics no longer renders it:\n{text}"
        );
    }
}

#[test]
fn the_committed_live_fixture_carries_every_required_metric() {
    let fixture = read_repo_file("scripts/tests/fixtures/field-node/metrics-live.txt");
    for name in required_metrics() {
        assert!(
            fixture.contains(&format!("# TYPE {name} ")),
            "scripts/tests/fixtures/field-node/metrics-live.txt lacks {name}, which \
             field-node.py requires; recapture it (see that directory's README.md)"
        );
    }
}
