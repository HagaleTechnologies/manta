# MAN-536: cty.dat and MASTER.SCP download over HTTPS by default

Date: 2026-11-09
Status: implemented.

## Context

With `[spot].refresh_lists = true`, `manta run` from a live receiver
downloads `cty.dat` every 7 days and `MASTER.SCP` every day
([MAN-316](2026-10-12-man316-reference-list-refresh.md)). Both lists decide
which calls are spotted. Until this change the downloads were plain HTTP on
port 80. The client in `crates/manta-cli/src/list_http.rs` was a hand-rolled
HTTP/1.1 client on `std::net::TcpStream`, with no TLS. An `https://` list
URL was refused at load:
`[spot]: cty_url must be an http:// URL (manta has no TLS support), got "…"`.

MAN-316 left HTTPS as its follow-up 3, because it needed manta's first TLS
dependency. [MAN-318](2026-10-27-man318-callsign-lookup.md) then added
`ureq` 3.5 with `rustls`, `ring` and bundled `webpki-roots` as an
unconditional `manta-cli` dependency for the QRZ.com client. HTTPS no longer
needs a new dependency, and MAN-318's follow-ups list it.

This record partly supersedes the MAN-316 record: its D7 client, its
`http://` defaults, its `https://` refusal, the first sentence of its threat
note, and its follow-up 3. The rest of that record stands. It also closes
MAN-318's follow-up "HTTPS for MAN-316's list downloads through `ureq`".

## Decisions

- **A1. One `ureq` path for both schemes.** The socket client is gone
  (`Conn`, `request`, `find_head_end`, `Head`, `parse_head`). The helpers
  `printable`, `size`, `timed_out`, `resolve_within` and `PENDING_LOOKUPS`
  stay. The fetch still runs on the `manta-lists` std thread, with no tokio
  runtime.
- **A2. The public shape and the name stay.** `HttpUrl` keeps its name and
  gains a `scheme` (`Http` or `Https`) and a `request_uri()`. The URI leaves
  out the port when it is the scheme's default, so the `Host` header is the
  old client's. `get`, `Fetched`, `FetchError` and `FetchLimits` are
  unchanged. Not renaming avoids a conflict with MAN-542, which calls the
  same API.
- **A3. The DNS bound stays, through a custom resolver.** `ListsResolver`
  implements `ureq`'s `Resolver` trait and calls the existing
  `resolve_within`. A lookup that outlives its fetch is adopted by the next
  attempt for the same `host:port`, so a resolver that never answers holds
  one thread per name. `ureq`'s default resolver would start an untracked
  thread on every attempt. The trait lives in `ureq::unversioned`, outside
  `ureq`'s semver promise. A breaking change there shows up as a compile
  error on the Dependabot PR, so it cannot change behaviour silently.
- **A4. The old client's strictness stays.**
  - 200 is the only success. A 304 counts as "not modified" only when
    `If-Modified-Since` was sent.
  - A 3xx is a failure that names the status and `Location`. It is never
    followed.
  - A `Transfer-Encoding` or `Content-Encoding` other than `identity` is a
    failure.
  - A `Content-Length` over 8 MiB is refused before any body byte is read.
    A body of exactly 8 MiB is accepted. A short body names
    `got N of M bytes`.
  - Response headers over 16 KiB are a failure.
  - The whole fetch ends within 60 s, name resolution and TLS handshake
    included. The connect budget is 10 s.
  - An `If-Modified-Since` value that is not all visible ASCII is dropped,
    and the request goes out unconditional.
  - Every failure text has its control characters replaced.
- **A5. No proxy.** The agent is built with `proxy(None)`, so
  `HTTPS_PROXY`, `HTTP_PROXY` and `ALL_PROXY` are ignored. This keeps the
  old client's behaviour, and the egress rule stays "port 443 to two
  hosts". `ureq` uses one proxy for both schemes and does not bypass
  loopback, so honouring the variables would also send an `http://` LAN
  mirror through the proxy. The QRZ.com client still honours
  `HTTPS_PROXY`.
- **A6. The defaults change together.** `DEFAULT_CTY_URL` and
  `DEFAULT_SCP_URL`, the `config init` scaffold, `manta.example.toml`, the
  `docs/CONFIG.md` Default cells and the `config check` summary test all
  say `https://`.
- **A7. Exact operator wording.** The refusal sentence, the `config check`
  note and the failure reasons are fixed in "Operator surface" below.
- **A8. A new record.** This record, plus "Partly superseded" notes in the
  MAN-316 record. That record is not rewritten.
- **A9. Tests.** Hermetic TLS unit tests run a local `rustls` server with
  test-only DER fixtures in `crates/manta-cli/tests/fixtures/list_tls/`.
  They cover a TLS fetch, a 304, a redirect, the size cap, an unknown
  issuer and a wrong host name. An integration scenario proves TLS through
  the real binary by the untrusted-certificate path. An `#[ignore]` live
  test fetches both real hosts. `rustls` is a `manta-cli` dev-dependency at
  the version `ureq` already locks, so no package is added.
- **A10. Scope.** Everything in "Not done here" stays out.
- **A11. A URI check at load.** `HttpUrl::parse` also requires `ureq`'s URI
  parser to accept the URL. A URL `ureq` would refuse fails `config check`
  instead of the first refresh, days later.
- **A12. A fresh agent per fetch.** Fetches are at most hourly per list, so
  building the TLS config each time is cheap. Dropping the agent closes the
  connection, which replaces the old `Connection: close` header. No pooled
  state carries over between attempts.

## Operator surface

- `[spot].cty_url` and `scp_url`, from the file or `MANTA_SPOT_CTY_URL` and
  `MANTA_SPOT_SCP_URL`, accept `https://` and `http://` URLs, in any case.
  The default ports are 443 and 80.
- The defaults are `https://www.country-files.com/cty/cty.dat` and
  `https://www.supercheckpartial.com/MASTER.SCP`.
- An `https://` certificate is checked against the Mozilla roots built into
  manta (`webpki-roots`), not the system trust store, and against the host
  name. `http://` stays accepted, for a local mirror.
- A refused URL reads
  `[spot]: scp_url must be an https:// or http:// URL, got "ftp://mirror.lan/MASTER.SCP"`.
  When the value starts with `https://` or `http://`, the parser's reason
  follows, for example `: it carries a user name, which manta does not
  send`. A URL with a character a URL cannot carry is now refused at load
  too: `: it contains a character a URL cannot carry (…)`.
- The `config check` note names the scheme and port of each host:
  ``note: list refresh downloads over HTTPS from www.country-files.com and www.supercheckpartial.com (TCP port 443) while `run` reads a live receiver; file replay never downloads``.
  With an `http://` mirror for one list, the note has one clause per scheme:
  `over plain HTTP from mirror.lan (TCP port 8080) and over HTTPS from www.supercheckpartial.com (TCP port 443)`.
- The `config init` scaffold and `manta.example.toml` say downloads are
  HTTPS on TCP port 443, checked against the certificate authorities built
  into manta. They say redirects are not followed and the proxy variables
  are not used.
- `run`'s startup line keeps its format and names the new URLs. A failure
  keeps the shape `warning: list refresh: <list> failed: <reason>; still
  using sha256 <sha> (<origin>); next try in 1 hour`. A certificate failure
  reads `connecting to <host>:<port>: invalid peer certificate: <rustls
  reason>`, for example `UnknownIssuer`. A connect timeout reads
  `connecting to <host>:<port>: timed out`. Every redirect, to `https://` or
  `http://`, reads `HTTP 301 Moved Permanently (Location: …); manta does not
  follow redirects, so point the URL at the new location`.
- Firewalls allow outbound TCP port 443 to `www.country-files.com` and
  `www.supercheckpartial.com`, or the port an `http://` mirror URL names.
  `docs/CONFIG.md`, `packaging/README.md`, the systemd unit, the Compose
  file, `docs/RUNBOOKS/network-exposure.md` and `SECURITY.md` say so.

## Threat note

HTTPS stops an on-path attacker from substituting a list fetched from the
default hosts. An `http://` mirror keeps the MAN-316 risk: an on-path
attacker can substitute a list that passes the acceptance checks. The impact
stays bounded as the MAN-316 note says. A list decides only whether a prefix
counts as allocated and the SCP confidence boost. It cannot inject spots
that no signal produced. The hash pin (`cty_sha256`/`scp_sha256`) stays the
strongest mitigation: it fixes the bytes in use and turns refresh off for
that list. Trust is the `webpki-roots` set compiled into manta and nothing
else. A node whose clock is wrong fails the certificate validity check, keeps
the list in use and retries an hour later. Proxy variables are ignored:
manta connects to the list host directly.

## Behaviour differences from the socket client

- The 10 s connect budget is shared across the addresses of one name. The
  old client gave each resolved address its own 10 s.
- A server's own reason phrase is replaced by the standard one.
  `HTTP 404 Not Found` stays, and a non-standard code prints only its
  number, for example `HTTP 599`.
- A reply that is not HTTP is reported with the parser's reason:
  `the server did not answer with valid HTTP (<reason>)`. No server bytes
  are echoed.
- The `Connection: close` header is no longer sent. `ureq` also sends
  `accept: */*`, and header names go out in lower case.
- `ureq` ends a body that has no `Content-Length` at a connection reset,
  or at a TLS close without `close_notify`, as if the body were whole. The
  old client reported a reset as a failure. Over HTTPS, manta therefore
  refuses a 200 that has no `Content-Length`, so a cut body cannot pass as
  a whole one. Both default hosts send `Content-Length`. Over plain HTTP a
  close-delimited body is still accepted. A reset in the middle of it now
  reads as the end of the body, and the parse and completeness checks are
  what catch a cut list. Plain HTTP has no integrity anyway (the MAN-316
  threat note).

## One re-download after the upgrade

A conditional request goes out only when the cached `state.json` names the
configured URL. The default URL changes from `http://` to `https://`, so an
upgraded node with a cache makes one unconditional GET per list at its next
due check. That is about 100 KB for `cty.dat` and 360 KB for `MASTER.SCP`,
once. If the bytes match the list in use, the check counts as `unchanged`
and writes a new state with the `https://` URL. Later checks are
conditional again. The operator has nothing to do.

An operator who set `cty_url` or `scp_url` to the old `http://` upstream URL
keeps plain HTTP until they change the value. `config check` says "plain
HTTP" for those hosts.

## Evidence

Measured in the planning container on 2026-11-09 against `7020ede`, before
the change:

- `manta config check` with
  `cty_url = "https://www.country-files.com/cty/cty.dat"` exited 1 with the
  refusal quoted in "Context". `MANTA_SPOT_SCP_URL=https://x/MASTER.SCP`
  gave the same refusal, ending `(environment: MANTA_SPOT_SCP_URL)`.
- `manta config init --out` output was byte-identical to
  `crates/manta-cli/src/config_init.toml`.
- The existing `list_http`, `list_refresh` and list-URL config tests
  passed: 34 passed, 0 failed.
- A scratch `ureq` 3.5.0 agent with A4's and A5's settings and a custom
  resolver compiled. Against live servers:
  - `https://www.country-files.com/cty/cty.dat` answered 200 with
    `Content-Length: 106951` and `Last-Modified: Sat, 07 Nov 2026 19:21:41
    GMT` in 263 ms. The same request with that `If-Modified-Since` answered
    304.
  - `https://www.supercheckpartial.com/MASTER.SCP` answered 200 with
    359,507 bytes.
  - Both hosts sent `Content-Length` and no `Transfer-Encoding`
    (`curl -I` and the probe).
  - A 301 from `http://www.reversebeacon.net/Patt3Ch.lst` was returned, not
    followed, with its `Location` readable.
  - `expired.badssl.com` and `wrong.host.badssl.com` were refused with
    rustls certificate errors.
  - A resolver that slept 30 s under a 2 s global timeout ended at 2.000 s.
- Against local plain-TCP fake servers, with the body read by manta's own
  loop: a short body reported 11 of 100 bytes. A body of exactly the cap was
  accepted with and without `Content-Length`. One byte over, close-delimited,
  was refused. A `Content-Length` over the cap was refused at once. A closed
  port gave `Connection refused` with no host and port in `ureq`'s text, so
  manta adds `connecting to <host>:<port>: `.
- A local rustls 0.23.46 server with a throwaway CA: a client trusting the
  CA got 200, and a 301 over TLS was not followed. A client with the
  built-in roots got `invalid peer certificate: UnknownIssuer`, and the
  server read no request bytes.
- Adding `rustls` as a `manta-cli` dev-dependency added one line to
  `Cargo.lock` and no package.
- `ureq` 3.5.1 (Dependabot #549) has the same resolver, timing, body limit,
  agent, TLS and connector code as 3.5.0.
- `ureq` 3.5.0's sources: the `Resolver` trait is public in
  `ureq::unversioned::resolver`. The default resolver starts one untracked
  thread per lookup. The proxy comes from `ALL_PROXY`, then `HTTPS_PROXY`,
  then `HTTP_PROXY`, unless `proxy(None)` is set. Several addresses of one
  name share the connect budget.

Not run in the planning phase: the full workspace suite, the CI script
suites, `cargo deny`, and the macOS and Windows legs. The PR's CI runs them.

## Not done here

- Honouring `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` for list downloads.
- Following redirects, or accepting chunked or compressed bodies (MAN-316
  follow-up 7).
- The system trust store, a custom CA option (`ureq`'s `platform-verifier`
  feature, `SSL_CERT_FILE`) and certificate pinning. A node behind a
  TLS-inspecting proxy uses an `http://` mirror or a hash pin.
- Restricting `http://` to loopback, as `qrz_url` is. The ticket requires
  `http://` mirrors to keep working.
- A `config check` warning for an `http://` URL to a host that is not
  loopback, beyond the note's "plain HTTP".
- A `manta doctor` reachability check for the list hosts (MAN-316
  follow-up 4).
- Any change to the refresh schedule, the cache format, the acceptance
  checks, the metrics, the JSON spot schema, file replay or decoder output.
  File replay never downloads, so a replay stays byte-identical.
- Renaming `HttpUrl` or changing `get`'s signature.

## Follow-ups

1. Proxy support for list downloads, keeping loopback and LAN mirrors
   direct.
2. Accepting chunked responses from mirrors (MAN-316 follow-up 7).
3. The system trust store or a custom CA, for a site with its own CA or a
   TLS-inspecting proxy.
