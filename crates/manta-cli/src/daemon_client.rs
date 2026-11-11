//! `manta status`'s client: resolving the daemon's address, fetching and
//! parsing its `/status` document, and the tokio `http_get` that
//! `manta health` shares. MAN-548 moved it out of `main.rs`.

use crate::config;
use crate::fmt;
use anyhow::{anyhow, bail, Context, Result};

/// Resolves the address(es) `manta status` should DIAL to reach a running
/// daemon's metrics/status listener (MAN-44). An explicit `--addr` always
/// wins; otherwise a `--config` file's `[server]` table supplies the
/// port, with its `metrics_bind_addr` (MAN-132: the metrics listener's own
/// address, loopback by default -- never `bind_addr`, which only moves
/// telnet/JSON) translated to a real dialable address --
/// `0.0.0.0`/`::` mean "listening on every interface," which isn't itself
/// something a client can connect TO, so those collapse to loopback (the
/// one address guaranteed to reach a same-host daemon). With neither,
/// falls back to the documented default metrics port on loopback.
///
/// Returns every address a hostname resolves to, not just the first
/// (code-review fix): `ToSocketAddrs` on a hostname can return several
/// candidates in resolver-dependent order -- e.g. `localhost` resolving
/// `::1` before `127.0.0.1` on a dual-stack host -- and a daemon bound to
/// `0.0.0.0` (or the default `127.0.0.1`) only listens on IPv4. Keeping just
/// `.next()` picked whichever candidate the resolver happened to list
/// first, reporting a healthy daemon as unreachable whenever that guess
/// was wrong. `fetch_status` tries every returned address in turn (same
/// precedent as `uplink::connect_first_reachable`).
fn resolve_status_addr(
    addr: Option<&str>,
    server: Option<&manta_server::config::ServerConfig>,
) -> Result<Vec<std::net::SocketAddr>> {
    if let Some(addr) = addr {
        if let Ok(sock) = addr.parse() {
            return Ok(vec![sock]);
        }
        // CR-B applies equally here: the daemon accepts a hostname in its
        // own `bind_addr` (resolved via `ToSocketAddrs` in
        // `start_spot_server`), and the runbook tells operators to reach a
        // remote daemon with `--addr <host>:<metrics_port>` -- rejecting a
        // literal-IP-only `--addr` would contradict both.
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .with_context(|| format!("invalid --addr {}", fmt::quoted(addr)))?
            .collect();
        if addrs.is_empty() {
            bail!("--addr {} resolved to no addresses", fmt::quoted(addr));
        }
        return Ok(addrs);
    }
    let Some(server) = server else {
        return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], 7302))]);
    };
    match server.metrics_bind_addr.as_str() {
        "0.0.0.0" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv4Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        "::" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv6Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        other => match other.parse::<std::net::IpAddr>() {
            Ok(ip) => Ok(vec![std::net::SocketAddr::new(ip, server.metrics_port)]),
            // CR-B: the daemon itself binds `metrics_bind_addr` through
            // `TcpListener::bind((host, port))`, which resolves a
            // hostname via `ToSocketAddrs` (`spot_server::start_spot_server`)
            // rather than requiring a literal IP -- so `metrics_bind_addr =
            // "localhost"` is a config the daemon happily runs on. `manta
            // status` must resolve the same way instead of rejecting a
            // config the daemon itself accepts.
            Err(_) => {
                use std::net::ToSocketAddrs;
                let addrs: Vec<_> = (other, server.metrics_port)
                    .to_socket_addrs()
                    .with_context(|| {
                        format!("resolving server.metrics_bind_addr {}", fmt::quoted(other))
                    })?
                    .collect();
                if addrs.is_empty() {
                    bail!(
                        "server.metrics_bind_addr {} resolved to no addresses",
                        fmt::quoted(other)
                    );
                }
                Ok(addrs)
            }
        },
    }
}

/// Renders a list of candidate addresses for an error message.
pub(crate) fn format_addrs(addrs: &[std::net::SocketAddr]) -> String {
    addrs
        .iter()
        .map(std::net::SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The full pre-render `manta status` flow: read/parse an optional
/// `--config` (a.k.a. the deprecated `--server-config` alias), resolve
/// the dial address, and fetch+parse the daemon's `/status` document.
/// Extracted so every failure along this path -- not just
/// `fetch_status`'s -- goes through the same exit-2 handling (CR-A).
///
/// `resolve_status_addr` runs INSIDE the tokio runtime, on the blocking
/// pool (MAN-44 code review CR-2): the previous version called it before
/// the runtime -- and therefore before any timer -- existed, so
/// `--timeout-secs` bounded connect+read but not the blocking
/// `ToSocketAddrs` lookup a hostname `--addr` or `bind_addr` triggers. An
/// unreachable or slow resolver then blocked for the OS's own
/// `resolv.conf` budget (commonly 10-40s) regardless of what the operator
/// asked for.
///
/// Resolution and the fetch share ONE end-to-end deadline rather than a
/// timeout window each (see `deadline`/`remaining` in `query_daemon`): giving
/// each leg its own full `timeout` would let `--timeout-secs 5` take
/// nearly ten seconds, twice the give-up bound the flag advertises.
pub(crate) fn run_status(
    server_config: Option<&std::path::Path>,
    addr: Option<&str>,
    timeout_secs: u64,
) -> Result<StatusReply> {
    // An explicit `--addr` needs no config at all.
    let server = if addr.is_some() {
        None
    } else {
        daemon_server_config(server_config)?.1
    };
    query_daemon(
        addr,
        server,
        timeout_secs,
        |targets, remaining| async move {
            fetch_status(&targets, remaining)
                .await
                .with_context(|| format!("could not reach daemon at {}", format_addrs(&targets)))
        },
    )
}

/// The daemon's `[server]` table as `run` would load it: `--config`, else
/// `MANTA_CONFIG`, with the `MANTA_*` overlay. `(None, None)` when neither
/// names a file. Shared by `manta status` and `manta health` (MAN-81).
pub(crate) fn daemon_server_config(
    config: Option<&std::path::Path>,
) -> Result<(
    Option<std::path::PathBuf>,
    Option<manta_server::config::ServerConfig>,
)> {
    // MAN-261: the same loader and `MANTA_*` overlay `run` uses, so the
    // port a client dials is the port the daemon bound -- including a
    // `MANTA_SERVER_METRICS_PORT` override and the `MANTA_CONFIG` fallback.
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let config_path = config
        .map(std::path::Path::to_path_buf)
        .or_else(|| config::config_path_from_env(&vars));
    match config_path {
        Some(path) => {
            let server = config::load(Some(&path), config::Env::Read(&vars))?.server;
            Ok((Some(path), server))
        }
        None => Ok((None, None)),
    }
}

/// Resolves `addr`/`server` on the blocking pool and runs `fetch` against
/// the resolved candidates within ONE end-to-end deadline, then detaches
/// the runtime. The MAN-44 plumbing, moved here unchanged from
/// `run_status` so `manta health` (MAN-81) shares the hardened copy.
pub(crate) fn query_daemon<T, F, Fut>(
    addr: Option<&str>,
    server: Option<manta_server::config::ServerConfig>,
    timeout_secs: u64,
    fetch: F,
) -> Result<T>
where
    F: FnOnce(Vec<std::net::SocketAddr>, std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let addr_owned = addr.map(str::to_string);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome = rt.block_on(async move {
        // MAN-44 review: ONE end-to-end deadline spans resolution and the
        // fetch. Giving each leg its own full `timeout` let a lookup that
        // finished just under the wire be followed by a fresh, full-length
        // connect/read window, so `--timeout-secs 5` could take nearly ten
        // seconds -- twice the give-up bound the flag advertises, and twice
        // what a cron/Nagios check budgeted for.
        // MAN-282: a clap-valid but unrepresentable `--timeout-secs`
        // (u64::MAX overflows every platform's `Instant`) must fail
        // through the command's own error path, not panic here.
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow!("invalid --timeout-secs {timeout_secs}: too large"))?;
        let targets = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                resolve_status_addr(addr_owned.as_deref(), server.as_ref())
            }),
        )
        .await
        .map_err(|_| anyhow!("resolving the daemon's address timed out"))?
        .context("resolving the daemon's address panicked")??;
        // Only what's LEFT of the deadline goes to the fetch. A zero
        // remainder is not special-cased: `fetch` bounds itself with this
        // duration and reports the same "timed out talking to ..." error
        // it would for any other exhausted budget.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        fetch(targets, remaining).await
    });
    // MAN-44 review: the timeout above only DROPS the JoinHandle -- a
    // `spawn_blocking` task cannot be aborted once it is running, so a
    // wedged `ToSocketAddrs` keeps occupying a blocking-pool thread after
    // `--timeout-secs` has already elapsed. Letting `rt` drop here would
    // then block the caller a second time, for the resolver's own
    // `resolv.conf` budget, which is exactly the wait `--timeout-secs`
    // exists to bound: the timeout would fire and the command would still
    // hang, wedging a monitoring invocation. `shutdown_background`
    // detaches the runtime instead of joining it, so the bound the
    // operator asked for is the bound they get; the orphaned lookup is
    // pure-read, owns no caller-visible state, and dies with the process
    // (which `Command::Status` and `Command::Health` reach immediately
    // after this returns).
    rt.shutdown_background();
    outcome
}

/// Exit code contract for scripting (cron/Nagios-style): `0` when every
/// enabled uplink target is connected, or there's no uplink configured at
/// all; `1` when the daemon was reached but the uplink is unhealthy. (A
/// third case -- `2`, "could not reach or parse the daemon's status at
/// all" -- is returned directly by `Command::Status`'s handler, since it
/// never gets as far as a `StatusDoc` to pass here.)
pub(crate) fn status_exit_code(doc: &manta_server::status_doc::StatusDoc) -> u8 {
    use manta_server::metrics::OverallUplinkHealth;
    match doc.uplink.health {
        OverallUplinkHealth::Ok | OverallUplinkHealth::Disabled => 0,
        OverallUplinkHealth::Degraded | OverallUplinkHealth::Down => 1,
    }
}

/// `manta doctor --strict`'s exit code for a run whose checks all passed:
/// `0` for DECODING, `3` for every other verdict. No wildcard arm, so a new
/// verdict has to choose its code here (and in the `--strict` help).
pub(crate) fn strict_exit_code(verdict: manta_engine::Verdict) -> u8 {
    use manta_engine::Verdict;
    match verdict {
        Verdict::Decoding => 0,
        Verdict::NoSignal
        | Verdict::NotCw
        | Verdict::ActivityNoSnr
        | Verdict::NoisyNoDecode
        | Verdict::WeakNoDecode => 3,
    }
}

/// Status uses the application error style while retaining exit code 2.
pub(crate) fn status_failure_message(e: &anyhow::Error) -> String {
    fmt::render_error(e)
}

/// Bounds a fetched status document's body size (MAN-44): a real status
/// document is kilobytes at most, but a wrong or hostile endpoint
/// answering `GET /status` must not be able to make this allocate without
/// bound.
const MAX_STATUS_BODY_BYTES: u64 = 256 * 1024;

/// Bounds the status line plus header block the same way
/// `manta_server::metrics_http`'s own `MAX_HEADER_LINES` bounds its side of
/// the identical parsing job (CR-E) -- without this, an unbounded
/// `read_line`-per-line loop against a hostile or misbehaving peer that
/// never terminates a line, or never ends its header block, could grow a
/// buffer or spin without bound; the body already had `MAX_STATUS_BODY_BYTES`
/// but the status line and headers did not.
const MAX_STATUS_HEADER_LINES: usize = 100;

/// Fetches and parses `GET /status` from a running daemon's metrics
/// listener (MAN-44) -- a small hand-rolled HTTP/1.1 GET, matching
/// `manta_server::metrics_http`'s own hand-rolled precedent rather than
/// adding an HTTP client dependency for one request. The whole operation
/// (connect, write, read) is bounded by `timeout` so a silent or
/// half-open peer can't hang `manta status` indefinitely.
pub(crate) async fn fetch_status(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<StatusReply> {
    tokio::time::timeout(timeout, fetch_status_inner(addrs, timeout))
        .await
        .map_err(|_| anyhow!("timed out talking to {}", format_addrs(addrs)))?
}

/// Tries every candidate in turn, returning the first that accepts a TCP
/// connection, or the last error if all of them fail (code-review fix --
/// see `resolve_status_addr`). Same precedent as
/// `uplink::connect_first_reachable`: a hostname resolving to more than
/// one address must not make the CLI give up after the first, resolver-
/// order-dependent candidate.
async fn connect_any(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    connect_any_bounded(addrs, overall_timeout, tokio::net::TcpStream::connect).await
}

/// `connect_any`'s real logic, with the per-candidate connect operation
/// injectable (MAN-44 code review CR-1) so the fall-through-past-a-stalled-
/// candidate behavior is unit-testable with a fake, instantly-controllable
/// "hangs forever" attempt under paused tokio time -- same precedent as
/// `uplink::connect_first_reachable_bounded`. Each candidate gets its own
/// slice of `overall_timeout` -- the time still left before the deadline,
/// split evenly across the candidates not yet tried, so a candidate that
/// fails fast passes its unused time on -- rather than a bare, unbounded
/// `TcpStream::connect`: without a per-candidate
/// bound, a first address that silently black-holes SYNs (a firewall drop,
/// not a refusal) consumed `fetch_status`'s ENTIRE outer timeout before a
/// later, live candidate was ever dialled -- the exact failure this
/// function exists to prevent, defeated by having no bound of its own.
async fn connect_any_bounded<T, F, Fut>(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
    connect: F,
) -> std::io::Result<(T, std::net::SocketAddr)>
where
    F: Fn(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + overall_timeout;
    let mut last_err = None;
    for (i, &addr) in addrs.iter().enumerate() {
        // Re-split what is LEFT of the deadline across the candidates not
        // yet tried (Codex review, PR #95): a fixed up-front even split let
        // fast refusals strand their unused time, so a live last candidate
        // could time out with most of the budget unspent.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let per_addr_timeout = remaining / ((addrs.len() - i) as u32);
        match tokio::time::timeout(per_addr_timeout, connect(addr)).await {
            Ok(Ok(stream)) => return Ok((stream, addr)),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no addresses to try")
    }))
}

async fn fetch_status_inner(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<StatusReply> {
    let (_, status_line, reader) = http_get(addrs, "/status", "status", timeout).await?;
    if !status_line.starts_with("HTTP/1.1 200") {
        bail!(
            "daemon returned {}",
            manta_server::status_doc::escape_for_terminal(status_line.trim_end())
        );
    }
    let body = read_http_body(reader, MAX_STATUS_BODY_BYTES, "status").await?;
    parse_status_doc(&body)
}

/// One hand-rolled `GET {path}` against the first of `addrs` that accepts
/// a connection within `timeout`: writes the request and reads the HTTP
/// status line (bounded). Returns the address dialled, the status line and
/// the reader, positioned at the header block for `read_http_body`. `what`
/// names the request in errors ("status", "health"). Shared by
/// `manta status` and `manta health` (MAN-81).
pub(crate) async fn http_get(
    addrs: &[std::net::SocketAddr],
    path: &str,
    what: &str,
    timeout: std::time::Duration,
) -> Result<(
    std::net::SocketAddr,
    String,
    tokio::io::BufReader<tokio::net::TcpStream>,
)> {
    use manta_server::bounded_io::{read_line_bounded, Utf8Tail};
    use tokio::io::{AsyncWriteExt, BufReader};

    let (mut stream, addr) = connect_any(addrs, timeout)
        .await
        .with_context(|| format!("connecting to {}", format_addrs(addrs)))?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .with_context(|| format!("writing the {what} request"))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    read_line_bounded(&mut reader, &mut status_line, &mut Utf8Tail::default())
        .await
        .context("reading the status line")?;
    Ok((addr, status_line, reader))
}

/// Skips the header block `http_get` left unread (at most
/// `MAX_STATUS_HEADER_LINES` lines) and reads the body, capped at
/// `max_body` bytes, as UTF-8.
pub(crate) async fn read_http_body(
    mut reader: tokio::io::BufReader<tokio::net::TcpStream>,
    max_body: u64,
    what: &str,
) -> Result<String> {
    use manta_server::bounded_io::{read_line_bounded, Utf8Tail};
    use tokio::io::AsyncReadExt;

    let mut tail = Utf8Tail::default();
    for _ in 0..MAX_STATUS_HEADER_LINES {
        let mut line = String::new();
        let n = read_line_bounded(&mut reader, &mut line, &mut tail)
            .await
            .context("reading response headers")?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    let mut body = Vec::new();
    reader
        .take(max_body)
        .read_to_end(&mut body)
        .await
        .with_context(|| format!("reading the {what} body"))?;
    String::from_utf8(body).with_context(|| format!("{what} body was not valid UTF-8"))
}

/// A parsed `/status` reply (MAN-285): `doc` drives the summary and the
/// exit code; `raw` is the same body as parsed, so `--json` keeps fields
/// this build's `StatusDoc` does not declare.
#[derive(Debug)]
pub(crate) struct StatusReply {
    pub(crate) doc: manta_server::status_doc::StatusDoc,
    pub(crate) raw: serde_json::Value,
}

/// Parses a `/status` body AND enforces its `schema_version` (Codex
/// review, PR #95).
///
/// `schema_version` only earns its place in the document if someone
/// checks it: a newer daemon that removed or re-meaning'd a field can
/// still deserialize structurally into this build's `StatusDoc` -- serde
/// ignores unknown keys and every field this build requires may well
/// still be present -- and `manta status` would then render it, and pick
/// an exit code from it, under semantics that no longer hold. An operator
/// running a monitoring one-liner would get a confident `0`/`1` computed
/// from a document this build cannot actually interpret.
///
/// So a version this build does not understand is a hard error, which
/// `Command::Status` turns into the documented exit 2 ("could not reach
/// or parse the daemon's status at all", `docs/RUNBOOKS/uplink-health.md`)
/// -- deliberately NOT exit 1, which means "asked, and the uplink is
/// unhealthy". Version skew is a tooling problem, not an uplink problem.
///
/// The version is read BEFORE the document is deserialized into
/// `StatusDoc`, not after: an incompatible future schema is exactly the
/// one that may have dropped or renamed a field this build requires, and
/// deserializing first would then fail with "not a valid status document"
/// -- reporting a malformed daemon when the real cause is version skew,
/// and hiding the one message that tells an operator which side to
/// upgrade. Reading the version off a `serde_json::Value` first makes the
/// check hold for EVERY document carrying a version this build does not
/// understand, structurally compatible or not. (The extra intermediate
/// parse costs nothing worth counting: one kilobytes-sized document, once,
/// per `manta status` invocation.)
///
/// A body with no `schema_version` at all -- or a non-integer one -- is
/// deliberately NOT a version error: it is a wrong endpoint answering
/// `GET /status`, and it falls through to the structural parse so it
/// fails on "not a valid status document" instead.
///
/// The same `Value` is kept for `--json` (MAN-285), preserving additive
/// fields at every depth. The typed document is deserialized from it, so
/// output and exit code agree on one parse, including duplicate keys.
///
/// Separated from `fetch_status_inner` so this is testable without a
/// socket; `fetch_status_inner` has no other post-read logic to keep.
pub(crate) fn parse_status_doc(body: &str) -> Result<StatusReply> {
    use manta_server::status_doc::{StatusDoc, STATUS_SCHEMA_VERSION};
    use serde::Deserialize;

    let value: serde_json::Value = serde_json::from_str(body).map_err(invalid_status_doc)?;
    if let Some(version) = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        if version != u64::from(STATUS_SCHEMA_VERSION) {
            bail!(
                "daemon reports status schema_version {} but this manta build understands only {} \
                 -- upgrade whichever of the daemon and the CLI is older",
                version,
                STATUS_SCHEMA_VERSION
            );
        }
    }
    let doc = StatusDoc::deserialize(&value).map_err(invalid_status_doc)?;
    Ok(StatusReply { doc, raw: value })
}

/// serde_json's data errors quote the offending value ("unknown variant
/// `...`", "invalid type: string ..."), and that value comes from the
/// peer, so it is escaped before it can reach the operator's terminal
/// (Codex review, PR #95).
fn invalid_status_doc(e: serde_json::Error) -> anyhow::Error {
    anyhow!(
        "status body was not a valid status document: {}",
        manta_server::status_doc::escape_for_terminal(&e.to_string())
    )
}

#[cfg(test)]
mod tests;
