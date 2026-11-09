//! MAN-316, MAN-536: the blocking GET that downloads cty.dat and
//! MASTER.SCP, over `https://` or plain `http://`. It runs `ureq` (rustls,
//! the Mozilla roots built into manta) on the `manta-lists` thread, so no
//! tokio runtime is needed. The rules: 200 is the only success, a 304 only
//! answers an `If-Modified-Since`, no redirects are followed, no chunked or
//! compressed bodies, an 8 MiB body cap checked before the body is read, a
//! 16 KiB header cap, one deadline for the whole fetch with name resolution
//! and the TLS handshake inside it, and no proxy. Every failure is a
//! sentence an operator can act on. See the MAN-316 plan, D7, and
//! docs/DECISIONS/2026-11-09-man536-https-list-downloads.md.

use std::fmt;
use std::io::{self, ErrorKind, Read};
use std::net::{Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Mutex;
use std::time::Duration;

use ureq::http::{HeaderMap, Uri};
use ureq::tls::TlsConfig;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

const READ_CHUNK: usize = 16 * 1024;
/// The most addresses one name resolves to that a fetch tries.
const MAX_ADDRS: usize = 16;

/// The two schemes a list URL may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

impl Scheme {
    /// 80 or 443.
    pub(crate) fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    /// `http` or `https`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// An `https://` or `http://` URL: the kinds `[spot].cty_url`/`scp_url`
/// accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpUrl {
    pub(crate) scheme: Scheme,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The request target: the path plus any query, never empty.
    pub(crate) path: String,
    original: String,
}

impl HttpUrl {
    /// Parses `https://HOST[:PORT][/PATH]` or `http://...`; HOST may be
    /// `[IPv6]`. The `Err` is a short reason, phrased to follow the URL.
    pub(crate) fn parse(s: &str) -> Result<HttpUrl, String> {
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("it contains a space or control character".to_string());
        }
        let (scheme, rest) = match s.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => (Scheme::Https, rest),
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => (Scheme::Http, rest),
            Some((scheme, _)) => {
                return Err(format!(
                    "it is a {scheme}:// URL, and manta downloads over https:// or http:// only"
                ))
            }
            None => return Err("it does not start with https:// or http://".to_string()),
        };
        let default_port = scheme.default_port();
        let (authority, target) = match rest.find(['/', '?', '#']) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        // The fragment never goes on the wire.
        let target = target.split_once('#').map_or(target, |(t, _)| t);
        let path = if target.starts_with('/') {
            target.to_string()
        } else {
            format!("/{target}")
        };
        if authority.contains('@') {
            return Err("it carries a user name, which manta does not send".to_string());
        }
        let bad_port = |p: &str| format!("its port \"{p}\" is not a number from 1 to 65535");
        let port_of = |p: &str| {
            p.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| bad_port(p))
        };
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| "its IPv6 address has no closing ']'".to_string())?;
            host.parse::<Ipv6Addr>()
                .map_err(|_| format!("\"{host}\" is not an IPv6 address"))?;
            let port = match after {
                "" => default_port,
                _ => match after.strip_prefix(':') {
                    Some(p) => port_of(p)?,
                    None => return Err(format!("\"{after}\" follows its IPv6 address")),
                },
            };
            (host, port)
        } else if let Some((host, port)) = authority.split_once(':') {
            if host.contains(':') {
                return Err("an IPv6 host must be written in brackets, [like::this]".to_string());
            }
            (host, port_of(port)?)
        } else {
            (authority, default_port)
        };
        if host.is_empty() {
            return Err("it has no host".to_string());
        }
        let url = HttpUrl {
            scheme,
            host: host.to_string(),
            port,
            path,
            original: s.to_string(),
        };
        // A URL the client would refuse fails here, at load, rather than
        // at the first refresh days later.
        Uri::try_from(url.request_uri())
            .map_err(|e| format!("it contains a character a URL cannot carry ({e})"))?;
        Ok(url)
    }

    /// `host:port`, with an IPv6 host in brackets: what a connect error
    /// names.
    pub(crate) fn host_port(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The URL as the operator wrote it.
    pub(crate) fn as_str(&self) -> &str {
        &self.original
    }

    /// The URL the client requests: `scheme://host[:port]/path`, the port
    /// only when it is not the scheme's default (so the `host` header is
    /// the bare host then), the fragment dropped.
    pub(crate) fn request_uri(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let scheme = self.scheme.as_str();
        if self.port == self.scheme.default_port() {
            format!("{scheme}://{host}{}", self.path)
        } else {
            format!("{scheme}://{host}:{}{}", self.port, self.path)
        }
    }
}

impl fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

/// The bounds on one fetch. `Default` is what `run` uses; tests shorten
/// the times.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchLimits {
    /// For connecting, shared across a name's addresses.
    pub(crate) connect_timeout: Duration,
    /// For the whole fetch, name resolution, connect and the TLS handshake
    /// included.
    pub(crate) deadline: Duration,
    pub(crate) max_body: u64,
    pub(crate) max_head: usize,
}

impl Default for FetchLimits {
    fn default() -> Self {
        FetchLimits {
            connect_timeout: Duration::from_secs(10),
            deadline: Duration::from_secs(60),
            max_body: 8 * 1024 * 1024,
            max_head: 16 * 1024,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// A 200 with its whole body.
    Ok {
        body: Vec<u8>,
        last_modified: Option<String>,
    },
    /// A 304 answer to `If-Modified-Since`.
    NotModified,
}

/// Why a fetch failed, as the operator-facing reason the refresh warning
/// quotes.
#[derive(Debug)]
pub(crate) struct FetchError(String);

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `GET`, with `If-Modified-Since` when given. Blocks for at most
/// `limits.deadline`, name resolution and the TLS handshake included. An
/// `https://` URL's certificate is checked against the Mozilla roots built
/// into manta, and its host name against the certificate.
pub(crate) fn get(
    url: &HttpUrl,
    if_modified_since: Option<&str>,
    limits: &FetchLimits,
) -> Result<Fetched, FetchError> {
    get_with(
        &agent(limits, TlsConfig::default()),
        url,
        if_modified_since,
        limits,
    )
}

/// A fresh agent per fetch: nothing is pooled across attempts, and
/// dropping it closes the connection.
fn agent(limits: &FetchLimits, tls: TlsConfig) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(limits.deadline))
        .timeout_connect(Some(limits.connect_timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(false)
        // No HTTPS_PROXY/HTTP_PROXY/ALL_PROXY: the egress rule stays "these
        // two hosts", and a LAN mirror is never routed to a proxy.
        .proxy(None)
        .accept_encoding("identity")
        .user_agent(format!("manta/{}", env!("CARGO_PKG_VERSION")).as_str())
        .max_response_header_size(limits.max_head)
        .tls_config(tls)
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), ListsResolver)
}

fn get_with(
    agent: &ureq::Agent,
    url: &HttpUrl,
    if_modified_since: Option<&str>,
    limits: &FetchLimits,
) -> Result<Fetched, FetchError> {
    // The value comes from an earlier response. Only visible ASCII can be
    // a header value, so anything else is dropped rather than sent.
    let if_modified_since =
        if_modified_since.filter(|v| v.bytes().all(|b| (0x20..0x7f).contains(&b)));
    let mut request = agent.get(url.request_uri());
    if let Some(ims) = if_modified_since {
        request = request.header("If-Modified-Since", ims);
    }
    let mut response = request
        .call()
        .map_err(|e| transport_error(e, url, limits))?;

    let code = response.status().as_u16();
    let status = match response.status().canonical_reason() {
        Some(reason) => format!("HTTP {code} {reason}"),
        None => format!("HTTP {code}"),
    };
    let headers = response.headers();
    match code {
        200 => {}
        304 if if_modified_since.is_some() => return Ok(Fetched::NotModified),
        // Nothing was asked of it, so it vouches for no copy.
        304 => {
            return Err(FetchError(format!(
                "{status} to a request that sent no If-Modified-Since"
            )))
        }
        300..=399 => {
            return Err(FetchError(match header(headers, "location") {
                Some(loc) => format!(
                    "{status} (Location: {loc}); manta does not follow redirects, so point the \
                     URL at the new location"
                ),
                None => format!("{status}; manta does not follow redirects"),
            }))
        }
        _ => return Err(FetchError(status)),
    }
    if let Some(te) = header(headers, "transfer-encoding") {
        if !te.eq_ignore_ascii_case("identity") {
            return Err(FetchError(format!(
                "the server sent Transfer-Encoding: {te}, which manta does not support"
            )));
        }
    }
    if let Some(ce) = header(headers, "content-encoding") {
        if !ce.is_empty() && !ce.eq_ignore_ascii_case("identity") {
            return Err(FetchError(format!(
                "the server sent Content-Encoding: {ce}, which manta does not support"
            )));
        }
    }
    let last_modified = headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let too_large = || FetchError(format!("it is larger than {}", size(limits.max_body)));
    let content_length = response.body().content_length();
    if content_length.is_some_and(|want| want > limits.max_body) {
        return Err(too_large());
    }
    // ureq ends a close-delimited body at a reset or at a TLS close without
    // close_notify as if it were whole, so over HTTPS a cut body could not
    // be told from a complete one.
    if content_length.is_none() && url.scheme == Scheme::Https {
        return Err(FetchError(
            "the server sent no Content-Length, which manta needs over HTTPS to tell a whole \
             body from a cut one"
                .to_string(),
        ));
    }

    let host_port = url.host_port();
    let mut reader = response.body_mut().as_reader();
    let mut body = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    match content_length {
        Some(want) => {
            let short = |got: usize| {
                FetchError(format!(
                    "it is shorter than its Content-Length (got {got} of {want} bytes)"
                ))
            };
            while (body.len() as u64) < want {
                let room = (want - body.len() as u64).min(READ_CHUNK as u64) as usize;
                let n = match read_body(&mut reader, &mut buf[..room]) {
                    Ok(n) => n,
                    Err(BodyError::Eof) => 0,
                    Err(e) => return Err(e.into_fetch_error(&host_port, limits)),
                };
                if n == 0 {
                    return Err(short(body.len()));
                }
                body.extend_from_slice(&buf[..n]);
            }
        }
        None => loop {
            if body.len() as u64 > limits.max_body {
                return Err(too_large());
            }
            let n = read_body(&mut reader, &mut buf)
                .map_err(|e| e.into_fetch_error(&host_port, limits))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        },
    }
    Ok(Fetched::Ok {
        body,
        last_modified,
    })
}

/// A header's value, trimmed and made printable; a value that is not
/// visible ASCII is read lossily.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| {
        let text = match v.to_str() {
            Ok(text) => text.to_string(),
            Err(_) => String::from_utf8_lossy(v.as_bytes()).into_owned(),
        };
        printable(text.trim())
    })
}

/// Why reading the body stopped short.
enum BodyError {
    /// The server closed before the body was whole.
    Eof,
    Timeout,
    Other(String),
}

impl BodyError {
    fn into_fetch_error(self, host_port: &str, limits: &FetchLimits) -> FetchError {
        match self {
            BodyError::Eof => FetchError(format!(
                "reading from {host_port}: the server closed the connection"
            )),
            BodyError::Timeout => timed_out(limits.deadline),
            BodyError::Other(e) => FetchError(printable(&format!("reading from {host_port}: {e}"))),
        }
    }
}

/// One read of the body. `Ok(0)` is its end.
fn read_body(reader: &mut impl Read, buf: &mut [u8]) -> Result<usize, BodyError> {
    loop {
        match reader.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                if e.kind() == ErrorKind::UnexpectedEof {
                    return Err(BodyError::Eof);
                }
                return Err(match ureq::Error::from(e) {
                    ureq::Error::Timeout(_) => BodyError::Timeout,
                    ureq::Error::Io(e) if e.kind() == ErrorKind::UnexpectedEof => BodyError::Eof,
                    e => BodyError::Other(e.to_string()),
                });
            }
        }
    }
}

/// A failure before the response head was read, as the operator's
/// sentence.
fn transport_error(e: ureq::Error, url: &HttpUrl, limits: &FetchLimits) -> FetchError {
    let host_port = url.host_port();
    let text = match e {
        ureq::Error::Timeout(ureq::Timeout::Connect) => {
            format!("connecting to {host_port}: timed out")
        }
        ureq::Error::Timeout(_) => return timed_out(limits.deadline),
        ureq::Error::Io(e) if e.kind() == ErrorKind::UnexpectedEof => {
            "the server closed the connection without answering".to_string()
        }
        // Refused, reset, unreachable, and every certificate failure
        // ("invalid peer certificate: ...").
        ureq::Error::Io(e) => format!("connecting to {host_port}: {e}"),
        ureq::Error::HostNotFound => {
            format!("connecting to {host_port}: the name has no addresses")
        }
        ureq::Error::ConnectionFailed => {
            format!("connecting to {host_port}: the connection failed")
        }
        ureq::Error::LargeResponseHeader(..) => format!(
            "the server's response headers are larger than {}",
            size(limits.max_head as u64)
        ),
        ureq::Error::Protocol(p) => format!("the server did not answer with valid HTTP ({p})"),
        other => other.to_string(),
    };
    FetchError(printable(&text))
}

/// `text` with every control character (an escape sequence's ESC, say)
/// replaced by `?`. Server text reaches the log, and over plain HTTP anyone
/// on the path can write it.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Resolves through [`resolve_within`], so a resolver that never answers
/// holds one thread per name and the lookup spends the fetch's deadline.
#[derive(Debug)]
struct ListsResolver;

impl Resolver for ListsResolver {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let key = match (uri.scheme(), uri.authority()) {
            (Some(scheme), Some(authority)) => DefaultResolver::host_and_port(scheme, authority),
            _ => None,
        }
        .ok_or_else(|| ureq::Error::BadUri(uri.to_string()))?;
        let name = key.clone();
        let lookup = move || name.to_socket_addrs().map(Iterator::collect);
        let addrs = match resolve_within(&key, lookup, *timeout.after) {
            None => return Err(ureq::Error::Timeout(timeout.reason)),
            Some(Err(e)) => return Err(ureq::Error::Io(e)),
            Some(Ok(addrs)) => addrs,
        };
        let mut resolved = self.empty();
        for addr in addrs.into_iter().take(MAX_ADDRS) {
            resolved.push(addr);
        }
        if resolved.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(resolved)
        }
    }
}

type Lookup = io::Result<Vec<SocketAddr>>;

/// Name lookups that outlived the fetch that started them, by `host:port`.
static PENDING_LOOKUPS: Mutex<Vec<(String, Receiver<Lookup>)>> = Mutex::new(Vec::new());

/// Runs `lookup` for `key` (`host:port`) on its own thread and waits at
/// most `within` for it. `None` when it has not answered by then; the
/// thread is kept as `key`'s pending lookup. The next call for `key` takes
/// it over instead of starting another: it waits on it while it runs, uses
/// its addresses if they came meanwhile, and starts afresh only after a
/// failed or lost one. With calls made one at a time, as the `manta-lists`
/// thread makes them, a resolver that never answers holds one thread per
/// name, not one per attempt.
fn resolve_within<F>(key: &str, lookup: F, within: Duration) -> Option<Lookup>
where
    F: FnOnce() -> Lookup + Send + 'static,
{
    let pending = {
        let mut pending = PENDING_LOOKUPS.lock().unwrap_or_else(|e| e.into_inner());
        let found = pending.iter().position(|(k, _)| k == key);
        found.map(|i| pending.swap_remove(i).1)
    };
    let mut running = None;
    if let Some(rx) = pending {
        match rx.try_recv() {
            Err(TryRecvError::Empty) => running = Some(rx),
            Ok(Ok(addrs)) => return Some(Ok(addrs)),
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {}
        }
    }
    let rx = match running {
        Some(rx) => rx,
        None => {
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("manta-lists-dns".into())
                .spawn(move || {
                    let _ = tx.send(lookup());
                });
            if let Err(e) = spawned {
                return Some(Err(e));
            }
            rx
        }
    };
    match rx.recv_timeout(within) {
        Ok(result) => Some(result),
        Err(RecvTimeoutError::Timeout) => {
            PENDING_LOOKUPS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((key.to_string(), rx));
            None
        }
        Err(RecvTimeoutError::Disconnected) => Some(Err(io::Error::other(
            "the name lookup ended without an answer",
        ))),
    }
}

fn timed_out(allowed: Duration) -> FetchError {
    let secs = if allowed.subsec_nanos() == 0 {
        allowed.as_secs().to_string()
    } else {
        format!("{:.1}", allowed.as_secs_f64())
    };
    FetchError(format!("no answer within {secs} s"))
}

/// `8 MiB`, `16 KiB`, or a byte count.
fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;
    use ureq::tls::{Certificate, RootCerts};

    const CA: &[u8] = include_bytes!("../tests/fixtures/list_tls/ca.der");
    const LOCALHOST: &[u8] = include_bytes!("../tests/fixtures/list_tls/localhost.der");
    const LOCALHOST_KEY: &[u8] = include_bytes!("../tests/fixtures/list_tls/localhost.key.der");
    const WRONG_NAME: &[u8] = include_bytes!("../tests/fixtures/list_tls/wrong-name.der");
    /// What `tls_serve`'s server returns when no request came over TLS.
    const NO_REQUEST: &str = "no request: ";

    /// How the fake server ends the exchange after writing its reply.
    #[derive(Clone, Copy)]
    enum Then {
        /// Close at once.
        Close,
        /// Hold the connection open until the client hangs up.
        WaitForClient,
    }

    /// A one-shot server on 127.0.0.1 that reads the request head, writes
    /// `reply` (built from that head) and ends as `then` says. Returns the
    /// URL of `path` on it and the request it received.
    fn serve(
        path: &str,
        then: Then,
        reply: impl FnOnce(&str) -> Vec<u8> + Send + 'static,
    ) -> (HttpUrl, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                req.push(byte[0]);
            }
            let req = String::from_utf8(req).unwrap();
            stream.write_all(&reply(&req)).unwrap();
            if let Then::WaitForClient = then {
                let mut sink = Vec::new();
                let _ = stream.read_to_end(&mut sink);
            }
            req
        });
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}{path}")).unwrap();
        (url, handle)
    }

    /// As `serve`, over TLS with `cert` (signed by the test CA, with the
    /// `localhost` key). The server returns [`NO_REQUEST`] and the reason
    /// when the handshake or the request read failed.
    fn tls_serve(
        cert: &'static [u8],
        path: &str,
        then: Then,
        reply: impl FnOnce(&str) -> Vec<u8> + Send + 'static,
    ) -> (HttpUrl, JoinHandle<String>) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(LOCALHOST_KEY.to_vec())),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                match tls.read(&mut byte) {
                    Ok(0) => return format!("{NO_REQUEST}closed after {} bytes", req.len()),
                    Ok(_) => req.push(byte[0]),
                    Err(e) => return format!("{NO_REQUEST}{e}"),
                }
            }
            let req = String::from_utf8(req).unwrap();
            tls.write_all(&reply(&req)).unwrap();
            match then {
                Then::WaitForClient => {
                    let mut sink = Vec::new();
                    let _ = tls.read_to_end(&mut sink);
                }
                Then::Close => {
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                }
            }
            req
        });
        let url = HttpUrl::parse(&format!("https://127.0.0.1:{port}{path}")).unwrap();
        (url, handle)
    }

    /// A client that trusts the test CA alone.
    fn trusting_test_ca() -> TlsConfig {
        TlsConfig::builder()
            .root_certs(RootCerts::new_with_certs(&[
                Certificate::from_der(CA).to_owned()
            ]))
            .build()
    }

    fn canned(text: &'static str) -> impl FnOnce(&str) -> Vec<u8> + Send + 'static {
        move |_| text.as_bytes().to_vec()
    }

    /// Short enough that a test that wrongly waits fails fast.
    fn quick() -> FetchLimits {
        FetchLimits {
            connect_timeout: Duration::from_secs(5),
            deadline: Duration::from_secs(5),
            ..FetchLimits::default()
        }
    }

    fn fetch_err(url: &HttpUrl, limits: &FetchLimits) -> String {
        get(url, None, limits).unwrap_err().to_string()
    }

    #[test]
    fn fetches_a_200_body_with_content_length() {
        let (url, server) = serve(
            "/cty/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\ncontent-length: 11\r\n\
                 Last-Modified: Tue, 15 Sep 2026 20:52:28 GMT\r\n\r\nhello world",
            ),
        );
        let got = get(&url, None, &quick()).unwrap();
        assert_eq!(
            got,
            Fetched::Ok {
                body: b"hello world".to_vec(),
                last_modified: Some("Tue, 15 Sep 2026 20:52:28 GMT".to_string()),
            }
        );
        let req = server.join().unwrap();
        let port = url.port;
        assert!(req.starts_with("GET /cty/cty.dat HTTP/1.1\r\n"), "{req}");
        // Header names are case-insensitive, and ureq sends them lower case.
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains(&format!("\r\nhost: 127.0.0.1:{port}\r\n")),
            "{req}"
        );
        assert!(
            lower.contains(&format!(
                "\r\nuser-agent: manta/{}\r\n",
                env!("CARGO_PKG_VERSION")
            )),
            "{req}"
        );
        assert!(lower.contains("\r\naccept-encoding: identity\r\n"), "{req}");
        assert!(!lower.contains("if-modified-since"), "{req}");
    }

    #[test]
    fn sends_if_modified_since_and_reports_not_modified_on_304() {
        const LM: &str = "Tue, 15 Sep 2026 20:52:28 GMT";
        let (url, server) = serve("/MASTER.SCP", Then::Close, |req| {
            if req
                .to_ascii_lowercase()
                .contains(&format!("\r\nif-modified-since: {LM}\r\n").to_ascii_lowercase())
            {
                b"HTTP/1.1 304 Not Modified\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1".to_vec()
            }
        });
        assert_eq!(get(&url, Some(LM), &quick()).unwrap(), Fetched::NotModified);
        let req = server.join().unwrap();
        assert!(
            req.to_ascii_lowercase()
                .contains(&format!("\r\nif-modified-since: {LM}\r\n").to_ascii_lowercase()),
            "{req}"
        );
    }

    #[test]
    fn a_304_to_a_request_without_if_modified_since_is_a_failure() {
        // No value, and a value `get` drops for its control character:
        // neither request is conditional, so neither 304 vouches for a copy.
        for ims in [None, Some("Tue, 15 Sep 2026\n20:52:28 GMT")] {
            let (url, server) = serve(
                "/MASTER.SCP",
                Then::Close,
                canned("HTTP/1.1 304 Not Modified\r\n\r\n"),
            );
            assert_eq!(
                get(&url, ims, &quick()).unwrap_err().to_string(),
                "HTTP 304 Not Modified to a request that sent no If-Modified-Since"
            );
            let req = server.join().unwrap();
            assert!(
                !req.to_ascii_lowercase().contains("if-modified-since"),
                "{req}"
            );
        }
    }

    #[test]
    fn a_slow_name_lookup_spends_the_fetch_deadline() {
        let started = Instant::now();
        let slow = || {
            std::thread::sleep(Duration::from_secs(3));
            Ok(Vec::new())
        };
        assert!(resolve_within("slow.invalid:80", slow, Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));

        let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let quick_lookup = move || Ok(vec![addr]);
        let got = resolve_within("quick.invalid:80", quick_lookup, Duration::from_secs(5));
        assert_eq!(got.unwrap().unwrap(), vec![addr]);
    }

    #[test]
    fn a_lookup_still_running_is_taken_over_not_started_again() {
        const KEY: &str = "wedged.invalid:80";
        let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let (release, wedged) = mpsc::channel::<()>();
        let hung = move || {
            let _ = wedged.recv();
            Ok(vec![addr])
        };
        let within = Duration::from_millis(100);
        assert!(resolve_within(KEY, hung, within).is_none());
        // Started, this one would answer at once; it is not started.
        assert!(resolve_within(KEY, || Ok(Vec::new()), within).is_none());
        // Released, the hung lookup's answer is the next call's, whether it
        // came before that call or during it.
        release.send(()).unwrap();
        let got = resolve_within(KEY, || Ok(Vec::new()), Duration::from_secs(5));
        assert_eq!(got.unwrap().unwrap(), vec![addr]);
    }

    #[test]
    fn a_lookup_that_failed_while_nobody_waited_is_tried_again() {
        const KEY: &str = "failed.invalid:80";
        let (tx, rx) = mpsc::channel();
        tx.send(Err(io::Error::other("no answer from the name server")))
            .unwrap();
        PENDING_LOOKUPS.lock().unwrap().push((KEY.to_string(), rx));
        let addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let got = resolve_within(KEY, move || Ok(vec![addr]), Duration::from_secs(5));
        assert_eq!(got.unwrap().unwrap(), vec![addr]);
    }

    #[test]
    fn a_404_is_a_failure_naming_the_status() {
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found"),
        );
        assert_eq!(fetch_err(&url, &quick()), "HTTP 404 Not Found");
    }

    #[test]
    fn server_text_with_control_characters_never_reaches_the_failure() {
        // ESC and BEL in the status line: the parser refuses the line, so
        // none of the server's bytes are echoed.
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 302 Found\u{1b}]0;x\u{7}\r\nLocation: http://m/\u{1b}[2J\r\n\
                 Content-Length: 0\r\n\r\n",
            ),
        );
        let err = fetch_err(&url, &quick());
        assert!(!err.chars().any(char::is_control), "{err:?}");
        assert!(
            err.starts_with("the server did not answer with valid HTTP ("),
            "{err:?}"
        );

        // A clean status line and a TAB inside a header value: the value
        // reaches the failure with the TAB replaced.
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 302 Found\r\nLocation: http://m/a\tb\r\nContent-Length: 0\r\n\r\n"),
        );
        let err = fetch_err(&url, &quick());
        assert!(!err.chars().any(char::is_control), "{err:?}");
        assert!(
            err.starts_with("HTTP 302 Found (Location: http://m/a?b); "),
            "{err:?}"
        );
    }

    #[test]
    fn a_301_is_a_failure_naming_the_location() {
        for location in [
            "https://www.country-files.com/cty/cty.dat",
            "http://mirror.lan/cty/cty.dat",
        ] {
            let (url, _server) = serve("/cty/cty.dat", Then::Close, move |_| {
                format!(
                    "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\n\
                     Content-Length: 0\r\n\r\n"
                )
                .into_bytes()
            });
            assert_eq!(
                fetch_err(&url, &quick()),
                format!(
                    "HTTP 301 Moved Permanently (Location: {location}); manta does not follow \
                     redirects, so point the URL at the new location"
                )
            );
        }
    }

    #[test]
    fn a_body_shorter_than_content_length_is_truncated() {
        let (url, _server) = serve(
            "/MASTER.SCP",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nW1AW\nK1ABC\n"),
        );
        assert_eq!(
            fetch_err(&url, &quick()),
            "it is shorter than its Content-Length (got 11 of 100 bytes)"
        );
    }

    #[test]
    fn a_content_length_over_8_mib_is_refused_before_reading() {
        // The server sends no body and holds the connection open: a client
        // that tried to read it would run into the deadline instead.
        let (url, _server) = serve(
            "/cty.dat",
            Then::WaitForClient,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 8388609\r\n\r\n"),
        );
        let started = Instant::now();
        assert_eq!(fetch_err(&url, &quick()), "it is larger than 8 MiB");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn chunked_transfer_encoding_is_a_failure() {
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nW1\r\n0\r\n\r\n"),
        );
        assert_eq!(
            fetch_err(&url, &quick()),
            "the server sent Transfer-Encoding: chunked, which manta does not support"
        );
    }

    #[test]
    fn a_content_encoding_other_than_identity_is_a_failure() {
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nxx"),
        );
        assert_eq!(
            fetch_err(&url, &quick()),
            "the server sent Content-Encoding: gzip, which manta does not support"
        );
    }

    #[test]
    fn a_closed_port_is_connection_refused() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/cty.dat")).unwrap();
        let err = fetch_err(&url, &quick());
        assert!(
            err.starts_with(&format!("connecting to 127.0.0.1:{port}: ")),
            "{err}"
        );
        assert!(err.to_lowercase().contains("refused"), "{err}");
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        // Bound but never accepted: the kernel completes the handshake and
        // the request sits unread.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/cty.dat")).unwrap();
        let limits = FetchLimits {
            connect_timeout: Duration::from_secs(1),
            deadline: Duration::from_millis(300),
            ..FetchLimits::default()
        };
        let started = Instant::now();
        assert_eq!(fetch_err(&url, &limits), "no answer within 0.3 s");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(listener);
        assert_eq!(
            timed_out(FetchLimits::default().deadline).to_string(),
            "no answer within 60 s"
        );
    }

    #[test]
    fn url_parsing_accepts_https_and_http_and_refuses_others() {
        let u = HttpUrl::parse("https://www.country-files.com/cty/cty.dat").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port, u.path.as_str()),
            (Scheme::Https, "www.country-files.com", 443, "/cty/cty.dat")
        );
        assert_eq!(u.host_port(), "www.country-files.com:443");
        assert_eq!(u.request_uri(), "https://www.country-files.com/cty/cty.dat");
        assert_eq!(u.as_str(), "https://www.country-files.com/cty/cty.dat");
        assert_eq!(u.to_string(), "https://www.country-files.com/cty/cty.dat");

        let u = HttpUrl::parse("HTTPS://example.org").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port, u.path.as_str()),
            (Scheme::Https, "example.org", 443, "/")
        );
        let u = HttpUrl::parse("https://[::1]:8443/x").unwrap();
        assert_eq!(u.request_uri(), "https://[::1]:8443/x");

        let u = HttpUrl::parse("http://www.country-files.com/cty/cty.dat").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port, u.path.as_str()),
            (Scheme::Http, "www.country-files.com", 80, "/cty/cty.dat")
        );
        assert_eq!(u.host_port(), "www.country-files.com:80");
        assert_eq!(u.request_uri(), "http://www.country-files.com/cty/cty.dat");

        let u = HttpUrl::parse("http://mirror.lan:8080/lists/MASTER.SCP?v=1#top").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("mirror.lan", 8080, "/lists/MASTER.SCP?v=1")
        );
        assert_eq!(
            u.request_uri(),
            "http://mirror.lan:8080/lists/MASTER.SCP?v=1"
        );

        let u = HttpUrl::parse("http://[::1]:80/cty.dat").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("::1", 80, "/cty.dat")
        );
        assert_eq!(u.host_port(), "[::1]:80");
        assert_eq!(u.request_uri(), "http://[::1]/cty.dat");

        let u = HttpUrl::parse("HTTP://example.org").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port, u.path.as_str()),
            (Scheme::Http, "example.org", 80, "/")
        );

        for bad in [
            "ftp://example.org/cty.dat",
            "http:///cty.dat",
            "http://",
            "https://",
            "http://:8080/cty.dat",
            "http://h:0/p",
            "https://h:0/p",
            "http://h:65536/p",
            "http://h:port/p",
            "http://h:/p",
            "http://[::1/p",
            "http://[nothex]/p",
            "http://::1/p",
            "http://user@h/p",
            "https://user@h/p",
            "http://h/a b",
            "http://mirror.lan/a<b",
            "https://h/a>b",
            "www.country-files.com/cty/cty.dat",
            "",
        ] {
            assert!(HttpUrl::parse(bad).is_err(), "{bad} was accepted");
        }
        assert_eq!(
            HttpUrl::parse("ftp://h/p").unwrap_err(),
            "it is a ftp:// URL, and manta downloads over https:// or http:// only"
        );
        assert_eq!(
            HttpUrl::parse("h/p").unwrap_err(),
            "it does not start with https:// or http://"
        );
        assert_eq!(HttpUrl::parse("http:///p").unwrap_err(), "it has no host");
        assert_eq!(HttpUrl::parse("https:///p").unwrap_err(), "it has no host");
        let err = HttpUrl::parse("http://h/a<b").unwrap_err();
        assert!(
            err.starts_with("it contains a character a URL cannot carry ("),
            "{err}"
        );
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_accepted_with_or_without_content_length() {
        let limits = FetchLimits {
            max_body: 1024,
            ..quick()
        };
        for with_length in [true, false] {
            let (url, _server) = serve("/cty.dat", Then::Close, move |_| {
                let mut reply = if with_length {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n".to_vec()
                } else {
                    b"HTTP/1.1 200 OK\r\n\r\n".to_vec()
                };
                reply.extend_from_slice(&[b'W'; 1024]);
                reply
            });
            match get(&url, None, &limits).unwrap() {
                Fetched::Ok { body, .. } => assert_eq!(body.len(), 1024, "{with_length}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_close_delimited_body_over_the_cap_is_refused() {
        let limits = FetchLimits {
            max_body: 1024,
            ..quick()
        };
        let (url, _server) = serve("/cty.dat", Then::Close, |_| {
            let mut reply = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
            reply.extend_from_slice(&[b'W'; 1025]);
            reply
        });
        assert_eq!(fetch_err(&url, &limits), "it is larger than 1 KiB");
    }

    #[test]
    fn an_if_modified_since_that_is_not_visible_ascii_is_not_sent() {
        let (url, server) = serve(
            "/MASTER.SCP",
            Then::Close,
            canned("HTTP/1.1 304 Not Modified\r\n\r\n"),
        );
        assert_eq!(
            get(&url, Some("Tue, 15 Sep 2026 20:52:28 GMT\u{e9}"), &quick())
                .unwrap_err()
                .to_string(),
            "HTTP 304 Not Modified to a request that sent no If-Modified-Since"
        );
        let req = server.join().unwrap();
        assert!(
            !req.to_ascii_lowercase().contains("if-modified-since"),
            "{req}"
        );
    }

    #[test]
    fn a_localhost_url_resolves_through_the_lists_resolver() {
        let (url, _server) = serve(
            "/x",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1"),
        );
        let url = HttpUrl::parse(&format!("http://localhost:{}/x", url.port)).unwrap();
        assert_eq!(
            get(&url, None, &quick()).unwrap(),
            Fetched::Ok {
                body: b"W1".to_vec(),
                last_modified: None,
            }
        );
    }

    #[test]
    fn the_agent_uses_no_proxy_and_follows_no_redirect() {
        let a = agent(&FetchLimits::default(), TlsConfig::default());
        assert!(a.config().proxy().is_none());
        assert_eq!(a.config().max_redirects(), 0);
        assert!(!a.config().http_status_as_error());
        assert_eq!(a.config().max_response_header_size(), 16 * 1024);
    }

    #[test]
    fn an_https_url_is_fetched_over_tls_with_its_last_modified() {
        const LM: &str = "Tue, 15 Sep 2026 20:52:28 GMT";
        let (url, server) = tls_serve(
            LOCALHOST,
            "/cty/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\
                 Last-Modified: Tue, 15 Sep 2026 20:52:28 GMT\r\n\r\nhello world",
            ),
        );
        let got = get_with(&agent(&quick(), trusting_test_ca()), &url, None, &quick()).unwrap();
        assert_eq!(
            got,
            Fetched::Ok {
                body: b"hello world".to_vec(),
                last_modified: Some(LM.to_string()),
            }
        );
        let req = server.join().unwrap();
        assert!(req.starts_with("GET /cty/cty.dat HTTP/1.1\r\n"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains(&format!("\r\nhost: 127.0.0.1:{}\r\n", url.port)),
            "{req}"
        );
    }

    #[test]
    fn an_https_304_answers_if_modified_since() {
        const LM: &str = "Tue, 15 Sep 2026 20:52:28 GMT";
        let (url, _server) = tls_serve(LOCALHOST, "/MASTER.SCP", Then::Close, |req| {
            if req
                .to_ascii_lowercase()
                .contains(&format!("\r\nif-modified-since: {LM}\r\n").to_ascii_lowercase())
            {
                b"HTTP/1.1 304 Not Modified\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1".to_vec()
            }
        });
        assert_eq!(
            get_with(
                &agent(&quick(), trusting_test_ca()),
                &url,
                Some(LM),
                &quick()
            )
            .unwrap(),
            Fetched::NotModified
        );
    }

    #[test]
    fn a_certificate_from_an_unknown_authority_is_refused_before_any_request() {
        let (url, server) = tls_serve(
            LOCALHOST,
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1"),
        );
        // The roots `run` uses: nothing there signed the test CA.
        let err = fetch_err(&url, &quick());
        assert!(
            err.starts_with(&format!(
                "connecting to 127.0.0.1:{}: invalid peer certificate: ",
                url.port
            )),
            "{err}"
        );
        assert!(err.contains("UnknownIssuer"), "{err}");
        let req = server.join().unwrap();
        assert!(req.starts_with(NO_REQUEST), "{req}");
    }

    #[test]
    fn a_certificate_for_another_name_is_refused() {
        let (url, server) = tls_serve(
            WRONG_NAME,
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1"),
        );
        let err = get_with(&agent(&quick(), trusting_test_ca()), &url, None, &quick())
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(&format!(
                "connecting to 127.0.0.1:{}: invalid peer certificate: ",
                url.port
            )),
            "{err}"
        );
        assert!(err.contains("not valid for name"), "{err}");
        let req = server.join().unwrap();
        assert!(req.starts_with(NO_REQUEST), "{req}");
    }

    #[test]
    fn an_https_redirect_is_not_followed() {
        let (url, _server) = tls_serve(
            LOCALHOST,
            "/cty/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://www.country-files.com/cty/cty.dat\r\n\
                 Content-Length: 0\r\n\r\n",
            ),
        );
        assert_eq!(
            get_with(&agent(&quick(), trusting_test_ca()), &url, None, &quick())
                .unwrap_err()
                .to_string(),
            "HTTP 301 Moved Permanently (Location: https://www.country-files.com/cty/cty.dat); \
             manta does not follow redirects, so point the URL at the new location"
        );
    }

    #[test]
    fn an_https_content_length_over_the_cap_is_refused_before_reading() {
        let (url, _server) = tls_serve(
            LOCALHOST,
            "/cty.dat",
            Then::WaitForClient,
            canned("HTTP/1.1 200 OK\r\nContent-Length: 8388609\r\n\r\n"),
        );
        let started = Instant::now();
        assert_eq!(
            get_with(&agent(&quick(), trusting_test_ca()), &url, None, &quick())
                .unwrap_err()
                .to_string(),
            "it is larger than 8 MiB"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn an_https_body_without_content_length_is_refused() {
        let (url, _server) = tls_serve(
            LOCALHOST,
            "/cty.dat",
            Then::Close,
            canned("HTTP/1.1 200 OK\r\n\r\nW1"),
        );
        assert_eq!(
            get_with(&agent(&quick(), trusting_test_ca()), &url, None, &quick())
                .unwrap_err()
                .to_string(),
            "the server sent no Content-Length, which manta needs over HTTPS to tell a whole \
             body from a cut one"
        );
    }

    #[test]
    #[ignore = "network: fetches the two real list hosts over HTTPS"]
    fn live_https_list_hosts_answer_over_tls() {
        for url in [
            "https://www.country-files.com/cty/cty.dat",
            "https://www.supercheckpartial.com/MASTER.SCP",
        ] {
            let url = HttpUrl::parse(url).unwrap();
            match get(&url, None, &FetchLimits::default()).unwrap() {
                Fetched::Ok {
                    body,
                    last_modified,
                } => {
                    assert!(!body.is_empty(), "{url}");
                    assert!(last_modified.is_some(), "{url}");
                }
                other => panic!("{url}: {other:?}"),
            }
        }
    }
}
