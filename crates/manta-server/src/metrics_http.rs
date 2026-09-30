//! Minimal HTTP server exposing `Metrics::render_prometheus_text` on
//! `GET /metrics` and `status::StatusDoc` (MAN-44) on `GET /status`.
//! ARCHITECTURE §8: "Prometheus text endpoint (feature `metrics`)... manta
//! status ... GET /status." Hand-rolled rather than pulling in a full HTTP
//! framework -- two static routes is the entire surface.

use crate::bounded_io::read_line_bounded;
use crate::metrics::Metrics;
use crate::rate_limit::IpRateLimiter;
use crate::status::StatusDoc;
use crate::tasks::{ConnectionLimiter, IpQuota};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Publicly bound (ARCHITECTURE §7), so both bounds below matter: an
/// unauthenticated client must not be able to hold this connection's task
/// open by trickling headers, one every few seconds, forever.
const MAX_HEADER_LINES: usize = 100;
/// One absolute deadline for reading the ENTIRE header block (request line
/// through the blank line that ends it) -- not restarted per line. A
/// per-line timeout (the prior behavior) let a client send one short
/// header roughly every (timeout - epsilon) seconds and hold the
/// connection's socket/task open for up to `MAX_HEADER_LINES *
/// HEADER_READ_TIMEOUT`, nearly 50 minutes at the old 30s-per-line/100-line
/// bound -- many such connections could exhaust file descriptors (round-7
/// review finding).
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the accept loop backs off after a failed `accept()` before
/// retrying -- a persistent resource error (e.g. `EMFILE`) makes
/// `accept()` return immediately, and retrying with no delay turns this
/// into a tight loop that starves other tasks on the same runtime.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
/// Upper bound on concurrently in-flight `/metrics`/`/status` requests.
/// Each is short-lived (one request, one response, connection closed), but
/// with no cap at all an unauthenticated client could still open
/// connections without bound, each holding a socket and a tracked task
/// open for up to `HEADER_READ_TIMEOUT` (round-15 review finding). A much
/// smaller budget than the spot-stream listeners -- legitimate traffic
/// here is a low-cardinality set of Prometheus scrapers and `manta status`
/// invocations, not end-user clients.
pub const MAX_METRICS_CONNECTIONS: usize = 64;
/// Upper bound on concurrently admitted `/metrics`/`/status` connections
/// from a SINGLE source IP (MAN-61, `docs/DECISIONS/2026-09-03-man61-per-ip-
/// connection-quota.md`, scope-expanded from the telnet/JSON finding in
/// PR #76 review round 2): each permit is held for up to
/// `HEADER_READ_TIMEOUT` even for a client sending an incomplete request,
/// so one unauthenticated peer continuously opening and holding
/// connections just under that deadline could occupy all
/// `MAX_METRICS_CONNECTIONS` permits, denying every legitimate scrape/
/// status check. A smaller value than the spot-stream listeners' per-IP
/// cap -- legitimate traffic here is a low-cardinality set of scrapers/
/// operators, not end-user clients (matching `MAX_METRICS_CONNECTIONS`'s
/// own, proportionally smaller, total ceiling).
pub const MAX_METRICS_CONNECTIONS_PER_IP: usize = 8;

/// MAN-59 review round 2: same rationale as `telnet::QUOTA_REJECT_LOG_MAX_PER_WINDOW`
/// -- this warning runs on every rejected socket, before any request rate
/// limiter, so a source completing repeated handshakes despite holding
/// its allotment could otherwise flood the log sink unbounded.
const QUOTA_REJECT_LOG_MAX_PER_WINDOW: u32 = 1;
const QUOTA_REJECT_LOG_WINDOW: Duration = Duration::from_secs(60);

/// MAN-59 review round 4: rounds 2-3 each found one more individually
/// un-gated audit-log call site in this file (the quota-reject warning,
/// then the 404-reject warning, and round 4 found the header-rejection
/// warning STILL wasn't wired to the round-3 limiter, plus this file's
/// own task-boundary catch-all was never covered either) -- see
/// `telnet::CONNECTION_LOG_MAX_PER_WINDOW`'s doc comment for the full
/// "reconsider the fix strategy" rationale. Replaces the narrower,
/// round-3-only `REJECTED_REQUEST_LOG_*`/`rejected_request_log_limiter`:
/// ONE budget covers header rejections, the 404 rejection, and the
/// task-boundary catch-all alike, so there's no longer a second limiter
/// to remember to wire a future call site into.
///
/// Round 5 correction: UNLIKE `telnet`/`json_stream` (where an admitted
/// connection always logs at least a connect event, so deciding the
/// budget once per connection never wastes it), most connections here
/// are successful scrapes that log NOTHING at all -- deciding this once
/// at admission time (the original round-4 shape) would silently spend a
/// budget slot on connections that were never going to produce a log
/// line, letting a source burn through its window on harmless scrapes
/// and then flood rejections for free for the rest of it. Consulted
/// LAZILY at each actual warning site in `handle_request` instead of
/// once per connection.
const CONNECTION_LOG_MAX_PER_WINDOW: u32 = 30;
const CONNECTION_LOG_WINDOW: Duration = Duration::from_secs(60);

pub async fn serve(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    limiter: ConnectionLimiter,
    ip_quota: IpQuota,
) {
    let quota_reject_log_limiter =
        IpRateLimiter::new(QUOTA_REJECT_LOG_MAX_PER_WINDOW, QUOTA_REJECT_LOG_WINDOW);
    crate::rate_limit::spawn_stale_entry_reaper(quota_reject_log_limiter.clone());
    let connection_log_limiter =
        IpRateLimiter::new(CONNECTION_LOG_MAX_PER_WINDOW, CONNECTION_LOG_WINDOW);
    crate::rate_limit::spawn_stale_entry_reaper(connection_log_limiter.clone());
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => {
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        // Checked BEFORE the shared limiter, not after (MAN-61): a source
        // already at its own per-IP cap is declined without consuming a
        // `ConnectionLimiter` permit at all -- the socket is simply
        // dropped here, closing the connection, leaving that shared
        // capacity for other sources.
        let Some(ip_guard) = ip_quota.try_acquire(peer.ip()) else {
            if quota_reject_log_limiter.allow(peer.ip()) {
                tracing::warn!(ip = %peer.ip(), "metrics_http: per-IP connection quota exceeded, declining");
            }
            continue;
        };
        // Blocks the accept loop itself until capacity is available -- a
        // flood beyond `MAX_METRICS_CONNECTIONS` is left waiting in the
        // OS's own connection backlog rather than ever being admitted or
        // tracked at all (round-15 review finding).
        let Ok(permit) = limiter.clone().acquire_owned().await else {
            continue; // limiter closed: unreachable in practice, never panics
        };
        let metrics = metrics.clone();
        // MAN-59 review round 5: unlike telnet/json_stream (where an
        // admitted connection ALWAYS logs at least a connect event), most
        // admitted connections here are successful scrapes that
        // deliberately emit NOTHING -- deciding this once per connection
        // (the round-4 pattern) would silently spend budget on connections
        // that were never going to log anything, letting a source use up
        // its whole window on harmless scrapes and then flood rejections
        // for free. `connection_log_limiter` is consulted LAZILY at each
        // actual warn site below instead, right when there's a real event
        // to charge for.
        let connection_log_limiter = connection_log_limiter.clone();
        tokio::spawn(async move {
            let _permit = permit; // held for the connection's lifetime
            let _ip_guard = ip_guard; // held for the connection's lifetime
            let result =
                handle_request(socket, metrics, peer, connection_log_limiter.clone()).await;
            // MAN-59 review round 3: successful requests are deliberately
            // not logged (Prometheus scrapes this every 10-30s -- pure
            // noise), so a connection that resets mid-response or times
            // out on write would otherwise produce no audit event at
            // all, unlike telnet/json_stream's equivalent catch-all.
            if let Err(e) = &result {
                if connection_log_limiter.allow(peer.ip()) {
                    tracing::warn!(peer = %peer, error = %e, "metrics_http: request task ended with an error");
                }
            }
        });
    }
}

/// Reads the request line plus up to `MAX_HEADER_LINES` header lines (we
/// don't need the headers themselves, just to consume them off the wire).
/// Returns `true` if the connection hit EOF before a request line arrived.
/// Deliberately takes no timeout itself -- the caller wraps the whole call
/// in ONE `tokio::time::timeout`, so this reads with no per-line deadline
/// and the outer timeout is what actually bounds total time spent here.
async fn read_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    request_line: &mut String,
) -> std::io::Result<bool> {
    if read_line_bounded(reader, request_line).await? == 0 {
        return Ok(true);
    }
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let n = read_line_bounded(reader, &mut line).await?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }
    Ok(false)
}

/// One routed response: status line, `Content-Type`, and body.
pub(crate) struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: String,
}

/// Extracts the request path from an HTTP request line
/// (`"GET /status?x=1 HTTP/1.1\r\n"` -> `Some("/status")`), requiring GET
/// and tolerating a trailing query string -- the pre-MAN-44 route check
/// (`starts_with("GET /metrics ")`) rejected `GET /metrics?x=1`, and some
/// scrapers append a query string.
fn parse_get_path(request_line: &str) -> Option<&str> {
    let rest = request_line.strip_prefix("GET ")?;
    let raw_path = rest.split(' ').next()?;
    Some(raw_path.split('?').next().unwrap_or(raw_path))
}

/// Pure routing: request line in, response out (MAN-44). Split from
/// `handle_request` so both routes are unit-testable without a socket.
pub(crate) fn route(request_line: &str, metrics: &Metrics) -> Response {
    match parse_get_path(request_line) {
        Some("/metrics") => Response {
            status: "200 OK",
            content_type: "text/plain; version=0.0.4",
            body: metrics.render_prometheus_text(),
        },
        Some("/status") => Response {
            status: "200 OK",
            content_type: "application/json",
            body: StatusDoc::from_metrics(metrics).to_json(),
        },
        _ => Response {
            status: "404 Not Found",
            content_type: "text/plain; version=0.0.4",
            body: String::new(),
        },
    }
}

async fn handle_request(
    socket: tokio::net::TcpStream,
    metrics: Arc<Metrics>,
    peer: std::net::SocketAddr,
    connection_log_limiter: IpRateLimiter,
) -> std::io::Result<()> {
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    let mut request_line = String::new();
    let headers_result = tokio::time::timeout(
        HEADER_READ_TIMEOUT,
        read_headers(&mut reader, &mut request_line),
    )
    .await;
    let eof = match headers_result {
        Ok(Ok(eof)) => eof,
        Ok(Err(e)) => {
            // MAN-59 review round 4: was previously ungated, unlike the
            // sibling 404-rejection warning below -- both now share the
            // one connection-level budget. Round 5: charged lazily, right
            // here, not pre-decided at connection-admission time -- see
            // `serve`'s own comment for why.
            if connection_log_limiter.allow(peer.ip()) {
                tracing::warn!(peer = %peer, error = %e, "metrics_http: header read rejected (oversized/malformed line), disconnecting");
            }
            return Err(e);
        }
        Err(_) => {
            if connection_log_limiter.allow(peer.ip()) {
                tracing::warn!(peer = %peer, "metrics_http: header read timed out, disconnecting");
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "header read timed out",
            ));
        }
    };
    if eof {
        return Ok(());
    }

    let response = route(&request_line, &metrics);
    if response.status == "404 Not Found" {
        // MAN-59 review: ordinary probing of this unauthenticated,
        // internet-facing endpoint (a wrong method, an unknown path) was
        // otherwise absent from the audit trail entirely -- only header
        // read failures were logged, never a request that completed but
        // didn't match. Debug (`?`), not Display, for the same reason the
        // telnet login field uses it: `request_line` is client-supplied
        // and unvalidated.
        if connection_log_limiter.allow(peer.ip()) {
            tracing::warn!(peer = %peer, request_line = ?request_line.trim_end(), "metrics_http: rejected request, returning 404");
        }
    }

    let wire = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.content_type,
        response.body.len(),
        response.body
    );
    tokio::time::timeout(WRITE_TIMEOUT, async {
        wr.write_all(wire.as_bytes()).await?;
        wr.shutdown().await
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_serves_status_json_metrics_text_and_404s_everything_else() {
        let m = Metrics::new();
        assert_eq!(
            route("GET /status HTTP/1.1", &m).content_type,
            "application/json"
        );
        assert_eq!(route("GET /metrics HTTP/1.1", &m).status, "200 OK");
        assert_eq!(route("GET /statuses HTTP/1.1", &m).status, "404 Not Found");
        assert_eq!(route("POST /status HTTP/1.1", &m).status, "404 Not Found");
        assert_eq!(route("GET /status?x=1 HTTP/1.1", &m).status, "200 OK");
    }

    #[test]
    fn status_route_body_parses_as_the_status_doc_json() {
        let m = Metrics::new();
        let response = route("GET /status HTTP/1.1", &m);
        let doc: crate::status::StatusDoc = serde_json::from_str(&response.body).unwrap();
        assert_eq!(doc.schema_version, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn header_block_honors_one_absolute_deadline_not_a_per_line_reset() {
        // Regression (round-7 review): the old code re-armed a fresh 30s
        // timeout on every individual header-line read, so a client
        // trickling one short line every ~29s could hold the connection
        // open for MAX_HEADER_LINES * 30s (~50 minutes). Two lines, each
        // preceded by a 20s clock advance (comfortably under the OLD
        // per-line 30s budget, but summing to 40s -- past the NEW single
        // HEADER_READ_TIMEOUT) must now time out; the old per-line logic
        // would never have timed out on this exact sequence. An in-memory
        // duplex stream (not a real TCP socket) is used because paused
        // virtual time doesn't reliably coexist with real socket I/O on
        // this runtime (confirmed separately in telnet_acceptance.rs).
        let (mut write_half, read_half) = tokio::io::duplex(256);
        let mut reader = BufReader::new(read_half);

        // Runs concurrently with the read below, so the two 20s sleeps
        // actually race against the single outer deadline instead of
        // executing sequentially before it starts.
        tokio::spawn(async move {
            write_half
                .write_all(b"GET /metrics HTTP/1.1\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(20)).await;
            write_half.write_all(b"Host: localhost\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(20)).await;
            let _ = write_half.write_all(b"\r\n").await;
        });

        let mut request_line = String::new();
        let result = tokio::time::timeout(
            HEADER_READ_TIMEOUT,
            read_headers(&mut reader, &mut request_line),
        )
        .await;
        assert!(
            result.is_err(),
            "40s of cumulative header-read time (two 20s gaps) must exceed the single 30s deadline"
        );
    }
}
