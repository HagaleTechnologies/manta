//! `manta health` (MAN-81): one `GET /healthz` against a running daemon's
//! metrics listener, turned into the 0/1 exit code a container health check
//! reads. The image's `HEALTHCHECK` runs it against the same config the
//! daemon runs, so a `metrics_port` or `metrics_bind_addr` set in
//! `manta.toml` or through `MANTA_SERVER_*` is the one dialled -- which a
//! curl probe at a fixed port cannot follow.
//!
//! Exit codes: 0 when `/healthz` answers 200 with an `ok` report, 1 for
//! everything else -- a 503 `unhealthy` report, a peer that is not a manta
//! health endpoint, a connect failure, a timeout or a config error. Never 2:
//! Docker reserves it, and `manta status` uses it for "couldn't ask".
//! Address resolution, the end-to-end deadline and the bounded reads are
//! `manta status`'s (MAN-44), shared through `crate::query_daemon` and
//! `crate::http_get`.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// A real `/healthz` report is a few short lines; a wrong or hostile peer
/// must not make the probe allocate without bound.
const MAX_HEALTH_BODY_BYTES: u64 = 64 * 1024;

/// What `/healthz` said: `healthy` from the status code and first line,
/// `report` the body with every line escaped for the terminal.
#[derive(Debug)]
struct Verdict {
    healthy: bool,
    report: String,
}

/// The reply as read off the wire, before it is judged.
struct Reply {
    addr: std::net::SocketAddr,
    status_line: String,
    /// `None` when the status was neither 200 nor 503: the body is not
    /// read, since the reply is rejected on its status line alone.
    body: Option<String>,
}

/// Runs the probe and returns the process exit code. The report goes to
/// stdout (Docker keeps it in `.State.Health.Log`); a failure to get one
/// goes to stderr as one `manta health: ...` line.
pub(crate) fn run(config: Option<&Path>, addr: Option<&str>, timeout_secs: u64) -> i32 {
    match probe(config, addr, timeout_secs) {
        Ok(Verdict { healthy, report }) => {
            print!("{report}");
            if healthy {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("manta health: {e:#}");
            1
        }
    }
}

fn probe(config: Option<&Path>, addr: Option<&str>, timeout_secs: u64) -> Result<Verdict> {
    // An explicit `--addr` needs no config at all (as `manta status`).
    let server = if addr.is_some() {
        None
    } else {
        // Re-read on every probe: the address is the one the file names
        // now, which is the daemon's own only until `metrics_port` or
        // `metrics_bind_addr` is edited. Both are restart-only (MAN-78),
        // so such an edit can point the probe away from the running
        // listener until a restart, and an edit after which the file no
        // longer loads, or has no `[server]` table, fails the probe until
        // it is fixed. The operator docs say so.
        let (path, server) = crate::daemon_server_config(config)?;
        // A config with no `[server]` table is one `run` serves no
        // listener for; falling back to 127.0.0.1:7302 would probe
        // whatever else happens to own that port.
        if let (Some(path), None) = (&path, &server) {
            bail!(
                "{} has no [server] table, so `manta run` serves no health endpoint for it",
                path.display()
            );
        }
        server
    };
    let reply = crate::query_daemon(addr, server, timeout_secs, |targets, left| async move {
        let label = crate::format_addrs(&targets);
        tokio::time::timeout(left, fetch_healthz(&targets, left))
            .await
            .map_err(|_| anyhow::anyhow!("timed out talking to {label}"))
            .and_then(|r| r)
            .with_context(|| format!("could not reach daemon at {label}"))
    })?;
    judge(reply)
}

async fn fetch_healthz(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<Reply> {
    let (addr, status_line, reader) = crate::http_get(addrs, "/healthz", "health", timeout).await?;
    let body = if is_status(&status_line, 200) || is_status(&status_line, 503) {
        Some(crate::read_http_body(reader, MAX_HEALTH_BODY_BYTES, "health").await?)
    } else {
        None
    };
    Ok(Reply {
        addr,
        status_line,
        body,
    })
}

fn is_status(status_line: &str, code: u16) -> bool {
    let mut words = status_line.split_whitespace();
    words.next() == Some("HTTP/1.1") && words.next() == Some(code.to_string().as_str())
}

/// 200 with first line `ok` is healthy and 503 with first line `unhealthy`
/// is unhealthy -- exactly `manta_server::metrics_http`'s two `/healthz`
/// replies. Anything else is some other server on the port, and must not
/// pass as healthy just because it answered 200.
fn judge(reply: Reply) -> Result<Verdict> {
    let first = reply
        .body
        .as_deref()
        .and_then(|b| b.lines().next())
        .unwrap_or("");
    let healthy = match (
        is_status(&reply.status_line, 200),
        is_status(&reply.status_line, 503),
        first,
    ) {
        (true, _, "ok") => true,
        (_, true, "unhealthy") => false,
        _ => bail!(
            "{} did not answer like a manta health endpoint: {}",
            reply.addr,
            manta_server::status_doc::escape_for_terminal(reply.status_line.trim_end())
        ),
    };
    let report = reply
        .body
        .unwrap_or_default()
        .lines()
        .map(|l| manta_server::status_doc::escape_for_terminal(l) + "\n")
        .collect();
    Ok(Verdict { healthy, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status_line: &str, body: Option<&str>) -> Reply {
        Reply {
            addr: "127.0.0.1:7302".parse().unwrap(),
            status_line: status_line.to_string(),
            body: body.map(str::to_string),
        }
    }

    #[test]
    fn judge_accepts_only_manta_healthz_replies() {
        let v = judge(reply("HTTP/1.1 200 OK\r\n", Some("ok\ndecode: ok\n"))).unwrap();
        assert!(v.healthy);
        assert_eq!(v.report, "ok\ndecode: ok\n");
        let v = judge(reply(
            "HTTP/1.1 503 Service Unavailable\r\n",
            Some("unhealthy\ndecode: stalled 12s\n"),
        ))
        .unwrap();
        assert!(!v.healthy);
        assert_eq!(v.report, "unhealthy\ndecode: stalled 12s\n");
        for (status, body) in [
            ("HTTP/1.1 200 OK\r\n", Some("hello\n")),
            ("HTTP/1.1 200 OK\r\n", Some("")),
            ("HTTP/1.1 503 Service Unavailable\r\n", Some("ok\n")),
            ("HTTP/1.1 200 OK\r\n", Some("unhealthy\n")),
            ("HTTP/1.1 404 Not Found\r\n", None),
            ("HTTP/1.1 2000 OK\r\n", Some("ok\n")),
        ] {
            let e = judge(reply(status, body)).unwrap_err().to_string();
            assert!(
                e.contains("did not answer like a manta health endpoint"),
                "{status:?} {body:?}: {e}"
            );
        }
    }

    #[test]
    fn judge_escapes_peer_text() {
        let v = judge(reply(
            "HTTP/1.1 503 Service Unavailable\r\n",
            Some("unhealthy\nsource \x1b[31mkiwi\n"),
        ))
        .unwrap();
        assert!(!v.report.contains('\x1b'), "{:?}", v.report);
        let e = judge(reply("HTTP/1.1 418 \x1b[2J\r\n", None))
            .unwrap_err()
            .to_string();
        assert!(!e.contains('\x1b'), "{e:?}");
    }
}
