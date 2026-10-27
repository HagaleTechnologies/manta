//! MAN-318: QRZ.com's XML callsign service, as `callsign_lookup`'s worker
//! uses it: log in once, reuse the session key until QRZ drops it, POST
//! everything (no secret in a URL), follow no redirect, read at most
//! 64 KiB. Blocking; runs on the `manta-lookup` thread only. Spec:
//! https://www.qrz.com/XML/current_spec.html (fetched 2026-10-27).

use manta_spot::Existence;
use std::time::Duration;

pub(crate) const DEFAULT_URL: &str = "https://xmldata.qrz.com/xml/current/";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of one reply read; QRZ's callsign records are a few KiB.
const BODY_LIMIT: u64 = 65_536;
/// The most of QRZ's own error text that reaches a log line.
const ERROR_TEXT_CHARS: usize = 200;

/// Why a login or lookup got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No reply within the request timeout.
    Timeout,
    /// QRZ refused the login, with its own (scrubbed) text. `day_long` is
    /// QRZ's `Connection refused`: no login for at least 24 hours.
    Refused { text: String, day_long: bool },
    /// A network, TLS or HTTP-status failure.
    Transport(String),
    /// A reply that is not the QRZ XML this client understands.
    Protocol(String),
}

/// Whether the account has a Logbook Data subscription (`<SubExp>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Subscription {
    /// Expires at QRZ's date text.
    Until(String),
    /// `non-subscriber`: QRZ limits access to testing.
    None,
    /// No `<SubExp>` in the reply.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoggedIn {
    pub(crate) subscription: Subscription,
}

/// One QRZ reply, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    Found,
    NotFound,
    SessionExpired,
    LoginOk {
        key: String,
        subscription: Subscription,
    },
    Error(String),
}

/// The text of the first `<name>…</name>` element in `xml`. QRZ's nodes
/// carry no attributes (only the root has `version`/`xmlns`), and unknown
/// elements are ignored, as the spec asks.
fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let len = xml[start..].find(&close)?;
    Some(xml[start..start + len].trim())
}

/// QRZ text, safe to log: control characters replaced, at most 200 chars.
fn scrub(text: &str) -> String {
    crate::list_http::printable(&text.chars().take(ERROR_TEXT_CHARS).collect::<String>())
}

fn parse(xml: &str) -> Result<Reply, Failure> {
    if !xml.contains("<QRZDatabase") {
        return Err(Failure::Protocol("the reply is not QRZ XML".into()));
    }
    if xml.contains("<Callsign>") {
        return Ok(Reply::Found);
    }
    let Some(session) = element(xml, "Session") else {
        return Err(Failure::Protocol("the reply has no Session node".into()));
    };
    let key = element(session, "Key").filter(|k| !k.is_empty());
    match (element(session, "Error"), key) {
        (Some(error), Some(_))
            if error
                .get(..9)
                .is_some_and(|p| p.eq_ignore_ascii_case("not found")) =>
        {
            Ok(Reply::NotFound)
        }
        (Some(error), Some(_)) => Ok(Reply::Error(scrub(error))),
        (Some(error), None) => {
            if is_login_error(error) {
                Ok(Reply::Error(scrub(error)))
            } else {
                Ok(Reply::SessionExpired)
            }
        }
        (None, Some(key)) => Ok(Reply::LoginOk {
            key: key.to_string(),
            subscription: match element(session, "SubExp") {
                Some(s) if s.eq_ignore_ascii_case("non-subscriber") => Subscription::None,
                Some(s) => Subscription::Until(scrub(s)),
                None => Subscription::Unknown,
            },
        }),
        (None, None) => Ok(Reply::SessionExpired),
    }
}

/// Errors QRZ gives a login (as opposed to a lookup on a dead session).
fn is_login_error(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("password") || e.contains("connection refused") || e.contains("username")
}

/// A blocking QRZ XML client.
pub(crate) struct QrzClient {
    agent: ureq::Agent,
    url: String,
    username: String,
    password: String,
    key: Option<String>,
    agent_name: String,
}

impl QrzClient {
    pub(crate) fn new(url: &str, username: &str, password: &str, timeout: Duration) -> Self {
        let agent_name = format!("manta/{}", env!("CARGO_PKG_VERSION"));
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(false)
            .user_agent(agent_name.as_str())
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            key: None,
            agent_name,
        }
    }

    fn post(&self, form: &[(&str, &str)]) -> Result<Reply, Failure> {
        let mut response = self
            .agent
            .post(&self.url)
            .send_form(form.iter().copied())
            .map_err(failure)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(Failure::Transport(format!("HTTP {status}")));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .map_err(failure)?;
        parse(&body)
    }

    /// Whether a session key is held (a login succeeded and QRZ has not
    /// dropped it since).
    pub(crate) fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// Logs in and keeps the session key.
    pub(crate) fn login(&mut self) -> Result<LoggedIn, Failure> {
        self.key = None;
        let reply = self.post(&[
            ("username", &self.username),
            ("password", &self.password),
            ("agent", &self.agent_name),
        ])?;
        match reply {
            Reply::LoginOk { key, subscription } => {
                self.key = Some(key);
                Ok(LoggedIn { subscription })
            }
            Reply::Error(text) => {
                let day_long = text.to_ascii_lowercase().contains("connection refused");
                Err(Failure::Refused { text, day_long })
            }
            Reply::SessionExpired => Err(Failure::Refused {
                text: "no session key in the login reply".into(),
                day_long: false,
            }),
            Reply::Found | Reply::NotFound => Err(Failure::Protocol(
                "a callsign record in reply to a login".into(),
            )),
        }
    }

    /// `Exists` or `NotFound`; logs in first when there is no key, and once
    /// more when QRZ reports the session gone.
    pub(crate) fn lookup(&mut self, call: &str) -> Result<Existence, Failure> {
        if self.key.is_none() {
            self.login()?;
        }
        for attempt in 0..2 {
            let key = self.key.clone().unwrap_or_default();
            match self.post(&[("s", &key), ("callsign", call)])? {
                Reply::Found => return Ok(Existence::Exists),
                Reply::NotFound => return Ok(Existence::NotFound),
                Reply::SessionExpired if attempt == 0 => {
                    self.login()?;
                }
                Reply::SessionExpired => {
                    self.key = None;
                    return Err(Failure::Protocol(
                        "QRZ.com dropped a fresh session at once".into(),
                    ));
                }
                Reply::Error(text) => return Err(Failure::Protocol(text)),
                Reply::LoginOk { .. } => {
                    return Err(Failure::Protocol("a login reply to a lookup".into()))
                }
            }
        }
        Err(Failure::Protocol("unreachable retry".into()))
    }
}

/// Maps a ureq error without formatting the request, so neither the
/// password nor the session key can reach a log.
fn failure(e: ureq::Error) -> Failure {
    match e {
        ureq::Error::Timeout(_) => Failure::Timeout,
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => Failure::Timeout,
        ureq::Error::Io(io) => Failure::Transport(format!("network error ({})", io.kind())),
        ureq::Error::HostNotFound => Failure::Transport("host not found".into()),
        ureq::Error::ConnectionFailed => Failure::Transport("connection failed".into()),
        ureq::Error::BodyExceedsLimit(_) => Failure::Protocol("the reply is over 64 KiB".into()),
        other => Failure::Transport(scrub(&other.to_string())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    // ---- parsing: bodies from QRZ's spec and from live replies ----

    #[test]
    fn a_callsign_node_is_found() {
        let xml = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.34">
  <Callsign>
      <call>AA7BQ</call>
      <fname>FRED L</fname>
  </Callsign>
  <Session>
      <Key>2331uf894c4bd29f3923f3bacf02c532d7bd9</Key>
      <Count>123</Count>
  </Session>
</QRZDatabase>"#;
        assert_eq!(parse(xml), Ok(Reply::Found));
    }

    #[test]
    fn not_found_with_a_key_is_not_found() {
        let xml = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.34">
  <Session>
    <Error>Not found: g1srdd</Error>
    <Key>1232u4eaf13b8336d61982c1fd1099c9a38ac</Key>
    <GMTime>Sun Nov 16 05:13:36 2008</GMTime>
  </Session>
</QRZDatabase>"#;
        assert_eq!(parse(xml), Ok(Reply::NotFound));
    }

    #[test]
    fn an_error_without_a_key_means_the_session_expired() {
        let spec = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.34">
  <Session>
    <Error>Session Timeout</Error>
    <GMTime>Sun Nov 16 05:13:36 2008</GMTime>
  </Session>
</QRZDatabase>"#;
        assert_eq!(parse(spec), Ok(Reply::SessionExpired));
        let live = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session>
<Error>Invalid session key</Error>
<GMTime>Tue Oct 27 03:12:05 2026</GMTime>
<Remark>cpu: 0.009s</Remark>
</Session>
</QRZDatabase>"#;
        assert_eq!(parse(live), Ok(Reply::SessionExpired));
    }

    #[test]
    fn login_ok_keeps_the_key_and_subscription() {
        let xml = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.34">
  <Session>
    <Key>2331uf894c4bd29f3923f3bacf02c532d7bd9</Key>
    <Count>123</Count>
    <SubExp>Wed Jan 1 12:34:03 2013</SubExp>
    <GMTime>Sun Aug 16 03:51:47 2012</GMTime>
  </Session>
</QRZDatabase>"#;
        assert_eq!(
            parse(xml),
            Ok(Reply::LoginOk {
                key: "2331uf894c4bd29f3923f3bacf02c532d7bd9".into(),
                subscription: Subscription::Until("Wed Jan 1 12:34:03 2013".into()),
            })
        );
        let free = xml.replace("Wed Jan 1 12:34:03 2013", "non-subscriber");
        assert!(matches!(
            parse(&free),
            Ok(Reply::LoginOk {
                subscription: Subscription::None,
                ..
            })
        ));
    }

    const REFUSED: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session>
<Error>Username/password incorrect - blocked until 2026-10-27 03:22:05 UTC</Error>
<GMTime>Tue Oct 27 03:12:05 2026</GMTime>
</Session>
</QRZDatabase>"#;

    #[test]
    fn login_refused_carries_qrz_text() {
        let url = Mock::start(vec![Canned::ok(REFUSED)]).url();
        let mut client = QrzClient::new(&url, "N1TEST", "wrong", Duration::from_secs(5));
        assert_eq!(
            client.login(),
            Err(Failure::Refused {
                text: "Username/password incorrect - blocked until 2026-10-27 03:22:05 UTC".into(),
                day_long: false,
            })
        );
        let refused = REFUSED.replace(
            "Username/password incorrect - blocked until 2026-10-27 03:22:05 UTC",
            "Connection refused",
        );
        let url = Mock::start(vec![Canned::Ok(refused)]).url();
        let mut client = QrzClient::new(&url, "N1TEST", "wrong", Duration::from_secs(5));
        assert!(matches!(
            client.login(),
            Err(Failure::Refused { day_long: true, .. })
        ));
    }

    #[test]
    fn a_body_that_is_not_qrz_xml_is_a_protocol_error() {
        assert!(matches!(
            parse("<html><body>502 Bad Gateway</body></html>"),
            Err(Failure::Protocol(_))
        ));
    }

    // ---- the client against a loopback mock ----

    pub(crate) const LOGIN_OK: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session><Key>k3y0001</Key><Count>1</Count><SubExp>Wed Jan 1 12:34:03 2027</SubExp></Session>
</QRZDatabase>"#;
    pub(crate) const FOUND: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Callsign><call>W1AW</call></Callsign><Session><Key>k3y0001</Key></Session>
</QRZDatabase>"#;
    pub(crate) const NOT_FOUND: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session><Error>Not found: QQ9ZZZ</Error><Key>k3y0001</Key></Session>
</QRZDatabase>"#;
    const EXPIRED: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.34"><Session><Error>Session Timeout</Error></Session></QRZDatabase>"#;

    /// What the mock does with one request.
    #[derive(Clone)]
    pub(crate) enum Canned {
        /// `200 OK` with this body (`Content-Length`).
        Ok(String),
        /// `200 OK` with this body, chunked.
        Chunked(String),
        /// A bare status line and headers.
        Raw(String),
        /// `200 OK` with this body after a delay.
        Late(Duration, String),
        /// Read the request, answer nothing, hold the socket open.
        Silent,
    }

    impl Canned {
        pub(crate) fn ok(body: &str) -> Self {
            Canned::Ok(body.to_string())
        }
    }

    /// A loopback QRZ stand-in: replies to the n-th request with the n-th
    /// `Canned` (the last one repeats), one thread per connection, and
    /// records each request's line and body.
    pub(crate) struct Mock {
        port: u16,
        pub(crate) requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Mock {
        pub(crate) fn start(script: Vec<Canned>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let seen = seen.clone();
                    let script = script.clone();
                    thread::spawn(move || serve(stream, &script, &seen));
                }
            });
            Self { port, requests }
        }

        pub(crate) fn url(&self) -> String {
            format!("http://127.0.0.1:{}/xml/current/", self.port)
        }

        pub(crate) fn seen(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).ok()? == 0 {
                return None;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_string();
        let len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).ok()?;
        let line = head.lines().next().unwrap_or_default().to_string();
        Some((line, String::from_utf8_lossy(&body).to_string()))
    }

    fn serve(mut stream: TcpStream, script: &[Canned], seen: &Mutex<Vec<(String, String)>>) {
        while let Some(request) = read_request(&mut stream) {
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(request);
                seen.len() - 1
            };
            let reply = script[n.min(script.len() - 1)].clone();
            let bytes = match reply {
                Canned::Ok(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
                Canned::Chunked(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                    body.len()
                ),
                Canned::Raw(text) => text,
                Canned::Late(delay, body) => {
                    thread::sleep(delay);
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                }
                Canned::Silent => {
                    let mut sink = Vec::new();
                    let _ = stream.read_to_end(&mut sink);
                    return;
                }
            };
            if stream.write_all(bytes.as_bytes()).is_err() {
                return;
            }
        }
    }

    fn client(mock: &Mock, password: &str) -> QrzClient {
        QrzClient::new(&mock.url(), "N1TEST", password, Duration::from_secs(5))
    }

    #[test]
    fn login_sends_credentials_only_in_the_post_body() {
        let mock = Mock::start(vec![Canned::ok(LOGIN_OK)]);
        let logged_in = client(&mock, "p&ss;w=rd").login().unwrap();
        assert_eq!(
            logged_in.subscription,
            Subscription::Until("Wed Jan 1 12:34:03 2027".into())
        );
        let seen = mock.seen();
        assert_eq!(seen.len(), 1);
        let (line, body) = &seen[0];
        assert_eq!(line, "POST /xml/current/ HTTP/1.1");
        assert!(!line.contains("password"));
        assert!(body.contains("username=N1TEST"), "{body}");
        assert!(body.contains("password=p%26ss%3Bw%3Drd"), "{body}");
        assert!(body.contains("agent=manta%2F"), "{body}");
    }

    #[test]
    fn lookup_posts_the_session_key_and_call() {
        let mock = Mock::start(vec![Canned::ok(LOGIN_OK), Canned::ok(FOUND)]);
        let mut c = client(&mock, "pw");
        assert_eq!(c.lookup("W1AW"), Ok(Existence::Exists));
        let seen = mock.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].1, "s=k3y0001&callsign=W1AW");
        assert!(!seen[1].0.contains("s="));
    }

    #[test]
    fn an_expired_session_logs_in_again_and_retries_once() {
        let mock = Mock::start(vec![
            Canned::ok(LOGIN_OK),
            Canned::ok(EXPIRED),
            Canned::ok(LOGIN_OK),
            Canned::ok(FOUND),
        ]);
        let mut c = client(&mock, "pw");
        c.login().unwrap();
        assert_eq!(c.lookup("W1AW"), Ok(Existence::Exists));
        assert_eq!(mock.seen().len(), 4);

        let mock = Mock::start(vec![
            Canned::ok(LOGIN_OK),
            Canned::ok(EXPIRED),
            Canned::ok(LOGIN_OK),
            Canned::ok(EXPIRED),
            Canned::ok(FOUND),
        ]);
        let mut c = client(&mock, "pw");
        c.login().unwrap();
        assert!(matches!(c.lookup("W1AW"), Err(Failure::Protocol(_))));
        assert_eq!(mock.seen().len(), 4, "no loop");
    }

    #[test]
    fn a_chunked_reply_is_read() {
        let mock = Mock::start(vec![
            Canned::Chunked(LOGIN_OK.into()),
            Canned::Chunked(NOT_FOUND.into()),
        ]);
        assert_eq!(
            client(&mock, "pw").lookup("QQ9ZZZ"),
            Ok(Existence::NotFound)
        );
    }

    #[test]
    fn a_silent_server_times_out() {
        let mock = Mock::start(vec![Canned::Silent]);
        let mut c = QrzClient::new(&mock.url(), "N1TEST", "pw", Duration::from_millis(300));
        let started = std::time::Instant::now();
        assert_eq!(c.login(), Err(Failure::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        other.set_nonblocking(true).unwrap();
        let port = other.local_addr().unwrap().port();
        let mock = Mock::start(vec![Canned::Raw(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/\r\nContent-Length: 0\r\n\r\n"
        ))]);
        assert_eq!(
            client(&mock, "pw").login(),
            Err(Failure::Transport("HTTP 302".into()))
        );
        assert!(
            other.accept().is_err(),
            "the redirect target saw a connection"
        );
    }

    #[test]
    fn a_non_200_status_is_a_transport_failure() {
        let mock = Mock::start(vec![Canned::Raw(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".into(),
        )]);
        assert_eq!(
            client(&mock, "pw").login(),
            Err(Failure::Transport("HTTP 503".into()))
        );
    }

    /// Needs a real QRZ.com account: `QRZ_TEST_USERNAME`/`QRZ_TEST_PASSWORD`
    /// (not `MANTA_*`, which the binary rejects when unknown).
    #[test]
    #[ignore]
    fn live_qrz_round_trip() {
        let (Ok(user), Ok(password)) = (
            std::env::var("QRZ_TEST_USERNAME"),
            std::env::var("QRZ_TEST_PASSWORD"),
        ) else {
            eprintln!("QRZ_TEST_USERNAME/QRZ_TEST_PASSWORD unset; skipped");
            return;
        };
        let mut c = QrzClient::new(DEFAULT_URL, &user, &password, REQUEST_TIMEOUT);
        c.login().unwrap();
        assert_eq!(c.lookup("W1AW"), Ok(Existence::Exists));
        assert_eq!(c.lookup("QQ9ZZZ"), Ok(Existence::NotFound));
    }
}
