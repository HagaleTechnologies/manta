//! Minimal HTTP server exposing `Metrics::render_prometheus_text` on
//! `GET /metrics`. ARCHITECTURE §8: "Prometheus text endpoint (feature
//! `metrics`)." Hand-rolled rather than pulling in a full HTTP framework --
//! one static text response to one path is the entire surface.

use crate::bounded_io::read_line_bounded;
use crate::metrics::Metrics;
use crate::rate_limit::IpRateLimiter;
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
/// Upper bound on concurrently in-flight `/metrics` requests. Each is
/// short-lived (one request, one response, connection closed), but with no
/// cap at all an unauthenticated client could still open connections
/// without bound, each holding a socket and a tracked task open for up to
/// `HEADER_READ_TIMEOUT` (round-15 review finding). A much smaller budget
/// than the spot-stream listeners -- legitimate traffic here is a
/// low-cardinality set of Prometheus scrapers, not end-user clients.
pub const MAX_METRICS_CONNECTIONS: usize = 64;
/// Upper bound on concurrently admitted `/metrics` connections from a
/// SINGLE source IP (MAN-61, `docs/DECISIONS/2026-09-03-man61-per-ip-
/// connection-quota.md`, scope-expanded from the telnet/JSON finding in
/// PR #76 review round 2): each permit is held for up to
/// `HEADER_READ_TIMEOUT` even for a client sending an incomplete request,
/// so one unauthenticated peer continuously opening and holding
/// connections just under that deadline could occupy all
/// `MAX_METRICS_CONNECTIONS` permits, denying every legitimate Prometheus
/// scrape. A smaller value than the spot-stream listeners' per-IP cap --
/// legitimate traffic here is a low-cardinality set of scrapers, not
/// end-user clients (matching `MAX_METRICS_CONNECTIONS`'s own,
/// proportionally smaller, total ceiling).
pub const MAX_METRICS_CONNECTIONS_PER_IP: usize = 8;

/// Aggregate per-source-IP budget on COMPLETED requests (MAN-64, PR #76
/// review round 7). `ConnectionLimiter`/`IpQuota` above bound how many
/// connections one source holds AT ONCE, and `HEADER_READ_TIMEOUT` bounds
/// how long an INCOMPLETE request may hold one -- neither bounds how many
/// complete requests a fast, COOPERATIVE client drives through this
/// listener over time: `handle_request` releases its permit the instant
/// the response closes, so a peer that reopens, scrapes, and closes as
/// fast as the network allows never occupies a permit long enough to be
/// declined by either of those, while still costing one full
/// `render_prometheus_text()` plus response write per round trip. The
/// opposite failure mode to MAN-61's quiet, permit-holding client on this
/// same listener.
///
/// Unlike telnet/JSON (MAN-57) there is deliberately NO per-connection
/// `RateLimiter` tier here: this endpoint answers exactly one request per
/// connection (`Connection: close`, see `handle_request`), so a
/// per-connection budget would be a budget of one and bound nothing
/// `IpQuota` doesn't already bound. The shared, IP-keyed tier below is the
/// entire mechanism.
///
/// 60 per 60s is roughly 5x the tightest realistic scrape load (a 5s-
/// interval Prometheus scraper is 12/min), leaving room for several
/// independent scrapers or a federation setup behind one IP while cutting
/// a flood by orders of magnitude. A 60s window rather than telnet's 10s
/// because scrape traffic is periodic at minute granularity -- a short
/// window with a small count would false-positive whenever two scrapers'
/// intervals happen to align.
pub const MAX_METRICS_REQUESTS_PER_IP: u32 = 60;
pub const METRICS_REQUEST_RATE_WINDOW: Duration = Duration::from_secs(60);

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
    ip_request_limiter: IpRateLimiter,
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
        let ip_request_limiter = ip_request_limiter.clone();
        tokio::spawn(async move {
            let _permit = permit; // held for the connection's lifetime
            let _ip_guard = ip_guard; // held for the connection's lifetime
            let result = handle_request(
                socket,
                metrics,
                peer,
                connection_log_limiter.clone(),
                ip_request_limiter,
            )
            .await;
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

async fn handle_request(
    socket: tokio::net::TcpStream,
    metrics: Arc<Metrics>,
    peer: std::net::SocketAddr,
    connection_log_limiter: IpRateLimiter,
    ip_request_limiter: IpRateLimiter,
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

    // MAN-64: charged on every COMPLETE request, including one that goes
    // on to get a 404 below -- the finding names "task, formatting, TCP,
    // and bandwidth work" as the cost being driven, and the unit of that
    // work is a request, not a successful scrape. Metering only
    // `GET /metrics` would hand a prober the same task/socket/write cost
    // for free. Matches `telnet.rs`'s command budget, which charges a
    // line whether or not it parses. Header-read failures above are
    // deliberately NOT charged: they never reach this point at all, and
    // `IpQuota` + `HEADER_READ_TIMEOUT` (MAN-61) already bound them.
    if !ip_request_limiter.allow(peer.ip()) {
        // Lazily, at the warn site, on the single connection-level log
        // budget -- MAN-59 rounds 4/5: one budget for every warn site in
        // this file, spent only when there is a real event to charge for.
        if connection_log_limiter.allow(peer.ip()) {
            tracing::warn!(peer = %peer, "metrics_http: per-IP request rate exceeded, returning 429");
        }
        write_response(
            &mut wr,
            "429 Too Many Requests",
            &format!("Retry-After: {}\r\n", METRICS_REQUEST_RATE_WINDOW.as_secs()),
            "",
        )
        .await?;
        // `Ok(())`, not `Err`: this rejection has already been logged
        // above, and `serve`'s task-boundary catch-all logs every `Err` --
        // returning `Err` here would print two lines per rejection, the
        // exact double-logging pattern 5b9e747 removed from telnet/WS. A
        // failed WRITE below still propagates as `Err`, which is a
        // genuinely different (and still-unlogged-elsewhere) event.
        return Ok(());
    }

    let body = if request_line.starts_with("GET /metrics ") {
        metrics.render_prometheus_text()
    } else {
        String::new()
    };
    let status = if request_line.starts_with("GET /metrics ") {
        "200 OK"
    } else {
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
        "404 Not Found"
    };

    write_response(&mut wr, status, "", &body).await
}

/// One place that knows this endpoint's response shape, shared by the 200,
/// 404, and 429 paths above so the three don't drift out of sync with each
/// other (MAN-64). `extra_headers` must be empty or a CRLF-terminated
/// header block. Generic over `W` (rather than the concrete write half)
/// purely so tests can drive it over an in-memory buffer the same way
/// `read_headers` above is driven over `tokio::io::duplex`.
async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    wr: &mut W,
    status: &str,
    extra_headers: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::time::timeout(WRITE_TIMEOUT, async {
        wr.write_all(response.as_bytes()).await?;
        wr.shutdown().await
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// MAN-64: the 429 path's `Retry-After` header must reach the client
    /// intact, and the response must still be a complete, well-formed HTTP
    /// response an operator's scraper can parse and act on -- not a bare
    /// socket close, which reads as a network fault rather than "you are
    /// over budget, retry later."
    #[tokio::test]
    async fn write_response_carries_extra_headers_and_closes_cleanly() {
        use tokio::io::AsyncReadExt;

        let (mut write_half, read_half) = tokio::io::duplex(4096);
        write_response(
            &mut write_half,
            "429 Too Many Requests",
            "Retry-After: 60\r\n",
            "",
        )
        .await
        .unwrap();

        let mut reader = BufReader::new(read_half);
        let mut out = String::new();
        reader.read_to_string(&mut out).await.unwrap();
        assert!(
            out.starts_with("HTTP/1.1 429 Too Many Requests\r\n"),
            "response: {out:?}"
        );
        assert!(out.contains("Retry-After: 60\r\n"), "response: {out:?}");
        assert!(out.contains("Content-Length: 0\r\n"), "response: {out:?}");
        assert!(out.contains("Connection: close\r\n"), "response: {out:?}");
    }

    /// Charging only `GET /metrics` would let a prober drive the same
    /// task, socket, and response-write cost for free; `telnet.rs`'s
    /// command budget charges a line whether or not it parses, and this
    /// matches it. Budget of 1/window: the first request (an unmatched
    /// path, itself a 404) consumes the single slot, so the SECOND
    /// request -- a well-formed `GET /metrics` -- must be refused too.
    #[tokio::test]
    async fn unmatched_paths_are_charged_against_the_request_budget() {
        use tokio::io::AsyncBufReadExt;

        let metrics = Arc::new(Metrics::new());
        let ip_request_limiter = IpRateLimiter::new(1, Duration::from_secs(60));
        let connection_log_limiter =
            IpRateLimiter::new(CONNECTION_LOG_MAX_PER_WINDOW, CONNECTION_LOG_WINDOW);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (socket, peer) = listener.accept().await.unwrap();
                let metrics = metrics.clone();
                let ip_request_limiter = ip_request_limiter.clone();
                let connection_log_limiter = connection_log_limiter.clone();
                tokio::spawn(async move {
                    let _ = handle_request(
                        socket,
                        metrics,
                        peer,
                        connection_log_limiter,
                        ip_request_limiter,
                    )
                    .await;
                });
            }
        });

        for (path, expected) in [("/nope", "HTTP/1.1 404"), ("/metrics", "HTTP/1.1 429")] {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut status_line = String::new();
            reader.read_line(&mut status_line).await.unwrap();
            assert!(
                status_line.starts_with(expected),
                "expected {expected}, got {status_line:?}"
            );
        }
    }
}
