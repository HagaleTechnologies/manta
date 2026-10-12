//! MAN-316: the blocking HTTP/1.1 GET that downloads cty.dat and
//! MASTER.SCP. std only, so the `manta-lists` thread needs no tokio runtime
//! and the crate no new dependency: plain `http://`, no TLS, no redirects,
//! no chunked or compressed bodies. Every failure is a sentence an operator
//! can act on. See the MAN-316 plan, D7.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv6Addr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 80;
const READ_CHUNK: usize = 16 * 1024;

/// An `http://` URL: the only kind `[spot].cty_url`/`scp_url` accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpUrl {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The request target: the path plus any query, never empty.
    pub(crate) path: String,
    original: String,
}

impl HttpUrl {
    /// Parses `http://HOST[:PORT][/PATH]`; HOST may be `[IPv6]`. The `Err`
    /// is a short reason, phrased to follow the URL.
    pub(crate) fn parse(s: &str) -> Result<HttpUrl, String> {
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("it contains a space or control character".to_string());
        }
        let rest = match s.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some((scheme, _)) => {
                return Err(format!(
                    "it is a {scheme}:// URL, and manta downloads over plain http:// only"
                ))
            }
            None => return Err("it does not start with http://".to_string()),
        };
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
                "" => DEFAULT_PORT,
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
            (authority, DEFAULT_PORT)
        };
        if host.is_empty() {
            return Err("it has no host".to_string());
        }
        Ok(HttpUrl {
            host: host.to_string(),
            port,
            path,
            original: s.to_string(),
        })
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

    /// The `Host:` header: the port only when it is not 80.
    fn host_header(&self) -> String {
        if self.port == DEFAULT_PORT {
            if self.host.contains(':') {
                format!("[{}]", self.host)
            } else {
                self.host.clone()
            }
        } else {
            self.host_port()
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
    /// Per resolved address.
    pub(crate) connect_timeout: Duration,
    /// For the whole fetch, connect included; checked between reads.
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
/// `limits.deadline` after name resolution.
pub(crate) fn get(
    url: &HttpUrl,
    if_modified_since: Option<&str>,
    limits: &FetchLimits,
) -> Result<Fetched, FetchError> {
    let mut conn = Conn::open(url, limits)?;
    conn.send(&request(url, if_modified_since))?;

    let (head, mut body) = conn.read_head(limits.max_head)?;
    let response = parse_head(&head)?;
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let status = match response.reason.as_str() {
        "" => format!("HTTP {}", response.code),
        reason => format!("HTTP {} {reason}", response.code),
    };
    match response.code {
        200 => {}
        304 => return Ok(Fetched::NotModified),
        300..=399 => {
            return Err(FetchError(match header("location") {
                Some(loc) if loc.to_ascii_lowercase().starts_with("http://") => format!(
                    "{status} (Location: {loc}); manta does not follow redirects, so point the \
                     URL at the new location"
                ),
                Some(loc) => format!(
                    "{status} (Location: {loc}); manta downloads over plain http:// only, so \
                     point the URL at an http:// mirror"
                ),
                None => format!("{status}; manta does not follow redirects"),
            }))
        }
        _ => return Err(FetchError(status)),
    }
    if let Some(te) = header("transfer-encoding") {
        if !te.eq_ignore_ascii_case("identity") {
            return Err(FetchError(format!(
                "the server sent Transfer-Encoding: {te}, which manta does not support"
            )));
        }
    }
    if let Some(ce) = header("content-encoding") {
        if !ce.is_empty() && !ce.eq_ignore_ascii_case("identity") {
            return Err(FetchError(format!(
                "the server sent Content-Encoding: {ce}, which manta does not support"
            )));
        }
    }
    let too_large = || FetchError(format!("it is larger than {}", size(limits.max_body)));
    let last_modified = header("last-modified")
        .filter(|v| !v.is_empty())
        .map(str::to_string);

    match header("content-length") {
        Some(text) => {
            let want: u64 = text.parse().map_err(|_| {
                FetchError(format!(
                    "the server sent an invalid Content-Length: \"{text}\""
                ))
            })?;
            if want > limits.max_body {
                return Err(too_large());
            }
            // `want` fits in memory: it is at most `max_body`.
            body.truncate(want as usize);
            let mut buf = vec![0u8; READ_CHUNK];
            while (body.len() as u64) < want {
                let room = (want - body.len() as u64).min(READ_CHUNK as u64) as usize;
                let n = conn.read(&mut buf[..room])?;
                if n == 0 {
                    return Err(FetchError(format!(
                        "it is shorter than its Content-Length (got {} of {want} bytes)",
                        body.len()
                    )));
                }
                body.extend_from_slice(&buf[..n]);
            }
        }
        None => {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                if body.len() as u64 > limits.max_body {
                    return Err(too_large());
                }
                let n = conn.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
        }
    }
    Ok(Fetched::Ok {
        body,
        last_modified,
    })
}

fn request(url: &HttpUrl, if_modified_since: Option<&str>) -> Vec<u8> {
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: manta/{}\r\nAccept-Encoding: identity\r\n\
         Connection: close\r\n",
        url.path,
        url.host_header(),
        env!("CARGO_PKG_VERSION"),
    );
    // The value comes from an earlier response; a CR or LF in it would
    // split the request, so it is dropped rather than sent.
    if let Some(ims) = if_modified_since.filter(|v| !v.chars().any(|c| c.is_control())) {
        req.push_str(&format!("If-Modified-Since: {ims}\r\n"));
    }
    req.push_str("\r\n");
    req.into_bytes()
}

/// A connected socket and the fetch's deadline.
struct Conn {
    stream: TcpStream,
    deadline: Instant,
    allowed: Duration,
    host_port: String,
}

impl Conn {
    fn open(url: &HttpUrl, limits: &FetchLimits) -> Result<Conn, FetchError> {
        let host_port = url.host_port();
        let connect_err =
            |e: &dyn fmt::Display| FetchError(format!("connecting to {host_port}: {e}"));
        let addrs = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| connect_err(&e))?;
        let deadline = Instant::now() + limits.deadline;
        let mut last_err = None;
        for addr in addrs {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timed_out(limits.deadline));
            }
            match TcpStream::connect_timeout(&addr, limits.connect_timeout.min(left)) {
                Ok(stream) => {
                    return Ok(Conn {
                        stream,
                        deadline,
                        allowed: limits.deadline,
                        host_port,
                    })
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(match last_err {
            Some(e) => connect_err(&e),
            None => connect_err(&"the name has no addresses"),
        })
    }

    /// Time left before the deadline, or the timeout failure.
    fn left(&self) -> Result<Duration, FetchError> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(timed_out(self.allowed))
        } else {
            Ok(left)
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), FetchError> {
        let left = self.left()?;
        let _ = self.stream.set_write_timeout(Some(left));
        self.stream.write_all(bytes).map_err(|e| match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => timed_out(self.allowed),
            _ => FetchError(format!("sending the request to {}: {e}", self.host_port)),
        })
    }

    /// One read, bounded by the deadline. `Ok(0)` is the server closing.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, FetchError> {
        loop {
            let left = self.left()?;
            let _ = self.stream.set_read_timeout(Some(left));
            match self.stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(FetchError(format!("reading from {}: {e}", self.host_port))),
            }
        }
    }

    /// Reads up to the blank line ending the response head. Returns the
    /// head (without the blank line) and whatever body bytes followed it.
    fn read_head(&mut self, max_head: usize) -> Result<(Vec<u8>, Vec<u8>), FetchError> {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            if let Some((end, sep)) = find_head_end(&buf) {
                if end > max_head {
                    break;
                }
                let body = buf.split_off(end + sep);
                buf.truncate(end);
                return Ok((buf, body));
            }
            if buf.len() > max_head {
                break;
            }
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Err(FetchError(if buf.is_empty() {
                    "the server closed the connection without answering".to_string()
                } else {
                    "the server closed the connection in the middle of its response headers"
                        .to_string()
                }));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Err(FetchError(format!(
            "the server's response headers are larger than {}",
            size(max_head as u64)
        )))
    }
}

/// The end of the head and the length of the blank line that ends it
/// (`\r\n\r\n`, or a bare `\n\n` from a sloppy server).
fn find_head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

struct Head {
    code: u16,
    reason: String,
    /// In order, names as sent, values trimmed.
    headers: Vec<(String, String)>,
}

/// `text` with every control character (an escape sequence's ESC, say)
/// replaced by `?`. The status line and header values reach the log, and
/// over plain HTTP anyone on the path can write them.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn parse_head(head: &[u8]) -> Result<Head, FetchError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let status_line = lines.next().unwrap_or_default();
    let not_http = || {
        FetchError(format!(
            "the server did not answer with HTTP (it sent \"{}\")",
            printable(&status_line.chars().take(80).collect::<String>())
        ))
    };
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") || code.len() != 3 {
        return Err(not_http());
    }
    let code: u16 = code.parse().map_err(|_| not_http())?;
    let reason = printable(parts.next().unwrap_or_default().trim());
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (printable(n.trim()), printable(v.trim())))
        .collect();
    Ok(Head {
        code,
        reason,
        headers,
    })
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
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

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
        assert!(
            req.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")),
            "{req}"
        );
        assert!(
            req.contains(&format!(
                "\r\nUser-Agent: manta/{}\r\n",
                env!("CARGO_PKG_VERSION")
            )),
            "{req}"
        );
        assert!(req.contains("\r\nAccept-Encoding: identity\r\n"), "{req}");
        assert!(req.contains("\r\nConnection: close\r\n"), "{req}");
        assert!(!req.contains("If-Modified-Since"), "{req}");
    }

    #[test]
    fn sends_if_modified_since_and_reports_not_modified_on_304() {
        const LM: &str = "Tue, 15 Sep 2026 20:52:28 GMT";
        let (url, server) = serve("/MASTER.SCP", Then::Close, |req| {
            if req.contains(&format!("\r\nIf-Modified-Since: {LM}\r\n")) {
                b"HTTP/1.1 304 Not Modified\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nW1".to_vec()
            }
        });
        assert_eq!(get(&url, Some(LM), &quick()).unwrap(), Fetched::NotModified);
        let req = server.join().unwrap();
        assert!(
            req.contains(&format!("\r\nIf-Modified-Since: {LM}\r\n")),
            "{req}"
        );
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
    fn server_text_in_a_failure_has_its_control_characters_replaced() {
        let (url, _server) = serve(
            "/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 302 Found\u{1b}]0;x\u{7}\r\nLocation: http://m/\u{1b}[2J\r\n\
                 Content-Length: 0\r\n\r\n",
            ),
        );
        assert_eq!(
            fetch_err(&url, &quick()),
            "HTTP 302 Found?]0;x? (Location: http://m/?[2J); manta does not follow \
             redirects, so point the URL at the new location"
        );
    }

    #[test]
    fn a_301_is_a_failure_naming_the_location_and_the_http_only_rule() {
        let (url, _server) = serve(
            "/cty/cty.dat",
            Then::Close,
            canned(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://www.country-files.com/cty/cty.dat\r\n\
                 Content-Length: 0\r\n\r\n",
            ),
        );
        assert_eq!(
            fetch_err(&url, &quick()),
            "HTTP 301 Moved Permanently (Location: https://www.country-files.com/cty/cty.dat); \
             manta downloads over plain http:// only, so point the URL at an http:// mirror"
        );
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
    fn url_parsing_accepts_http_host_port_path_and_refuses_others() {
        let u = HttpUrl::parse("http://www.country-files.com/cty/cty.dat").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("www.country-files.com", 80, "/cty/cty.dat")
        );
        assert_eq!(u.host_port(), "www.country-files.com:80");
        assert_eq!(u.as_str(), "http://www.country-files.com/cty/cty.dat");
        assert_eq!(u.to_string(), "http://www.country-files.com/cty/cty.dat");

        let u = HttpUrl::parse("http://mirror.lan:8080/lists/MASTER.SCP?v=1#top").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("mirror.lan", 8080, "/lists/MASTER.SCP?v=1")
        );
        assert_eq!(u.host_header(), "mirror.lan:8080");

        let u = HttpUrl::parse("http://[::1]:80/cty.dat").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("::1", 80, "/cty.dat")
        );
        assert_eq!(u.host_port(), "[::1]:80");
        assert_eq!(u.host_header(), "[::1]");

        let u = HttpUrl::parse("HTTP://example.org").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("example.org", 80, "/")
        );

        for bad in [
            "https://www.country-files.com/cty/cty.dat",
            "ftp://example.org/cty.dat",
            "http:///cty.dat",
            "http://",
            "http://:8080/cty.dat",
            "http://h:0/p",
            "http://h:65536/p",
            "http://h:port/p",
            "http://h:/p",
            "http://[::1/p",
            "http://[nothex]/p",
            "http://::1/p",
            "http://user@h/p",
            "http://h/a b",
            "www.country-files.com/cty/cty.dat",
            "",
        ] {
            assert!(HttpUrl::parse(bad).is_err(), "{bad} was accepted");
        }
        assert_eq!(
            HttpUrl::parse("https://h/p").unwrap_err(),
            "it is a https:// URL, and manta downloads over plain http:// only"
        );
        assert_eq!(HttpUrl::parse("http:///p").unwrap_err(), "it has no host");
    }
}
