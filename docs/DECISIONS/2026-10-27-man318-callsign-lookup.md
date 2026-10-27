# MAN-318: withhold a spot until QRZ.com confirms its callsign exists

Date: 2026-10-27. Ticket: MAN-318.

## Context

Operators asked to spot only callsigns that exist. The ticket fixes three
behaviours:

1. **A call QRZ.com reports as not found is held back.** It is not spotted,
   and the reason is recorded.
2. **A lookup outage does not stop spotting.** If QRZ.com does not answer
   within 10 seconds, the spot is emitted anyway and the failure is counted.
3. **Answers are cached.** After a not-found answer, the same call is not
   looked up again for 24 hours.

The feature is the opt-in `[callsign_lookup]` table
([CONFIG.md](../CONFIG.md#callsign_lookup)).

## Decisions

- **D1. The gate is validation's last step.** `manta_spot::Validator` asks
  after dedupe, at both spot-construction sites (`evaluate_candidate` and the
  beacon path at track close), so no lookup is spent on a call validation
  already rejects. A call without an answer yet is held and recorded as
  `deferred`/`lookupPending`; the later decision is `spotted` or
  `withheld`/`lookupNotFound`. Every side effect of a spot stays in the one
  existing `on_spot` path, so a withheld call reaches no surface (telnet,
  JSON/WebSocket, the RBN uplink, stdout, `manta_spots_total`). Rejected: a
  gate in `run`'s `on_spot` closures (duplicated across the single- and
  multi-input paths, and the live stream would say `spotted` for spots never
  published) and a gate in the uplink (it narrows only RBN forwarding).
  Dedupe has stored a spot by the time it is held or withheld; a withheld
  call stays withheld for 24 h, longer than dedupe's 10-minute window.
- **D2. manta-spot only asks.** It defines the non-blocking
  `CallsignLookup` trait (`check`, `settled`, `settle_all`) and does no I/O
  and reads no clock. `manta-cli`'s `callsign_lookup` implements it.
- **D3. Live sources only.** A file replay prints `note: callsign lookup is
  off for file replay, so the run is repeatable` and runs without the step,
  so SPEC §6's no-wall-clock rule holds for file input and replay output
  stays byte-identical.
- **D4. HTTPS via `ureq` 3.5 on a std thread.** The `manta-lookup` thread
  owns a blocking `ureq::Agent` (10 s global timeout, no redirects, HTTP
  status not an error, `manta/<version>` user agent). Login and lookup go as
  POST form bodies, so the password and the session key never sit in a URL.
  Bodies are read up to 64 KiB. Plain HTTP was rejected because it would send
  the password in cleartext, and `list_http.rs` cannot read QRZ.com's chunked
  replies. `reqwest` was rejected because it pulls hyper and tokio into the
  lookup path. `ureq`'s `rustls` feature brings rustls, ring and the bundled
  webpki-roots (`CDLA-Permissive-2.0`, now allowed in `deny.toml`). This
  supersedes the MAN-65 record's statement that the lockfile holds no `ring`:
  ring compiles C and assembly, which cc-rs builds with `/MT` under
  `+crt-static` on Windows; the release workflow's static-CRT assertion stays
  the guard.
- **D5. Mapping QRZ.com's reply.** A `<Callsign>` node is found. A
  `<Session><Error>` starting `Not found` with a `<Key>` is not found. An
  `<Error>` without a `<Key>` is an expired session: log in again and retry
  once. Anything else (another error, a non-200 status, a body without
  `<QRZDatabase`) is a failure. A login `<Error>` is a refused login;
  `Connection refused` means QRZ.com allows no login for 24 h.
- **D6. Two independent 10 s bounds.** Each held call has a deadline taken
  when it is first held, enforced on the decode side in `settled()`; each
  HTTP request has ureq's own 10 s timeout. An answer that arrives after its
  deadline still fills the cache. The deadline is checked once per decode
  batch, so it bounds a hold to 10 s while samples flow. During a
  single-input source outage `ReconnectingSource::read()` blocks and no batch
  runs: a held spot waits for decoding to resume (or the run to end) and is
  released then, with QRZ.com's answer if one arrived, else unverified. With
  several inputs, the other inputs' batches keep releasing. Accepted, not
  prevented: the spot keeps its decode timestamp, and the spot bus and the
  JSON `seq` already tolerate a late publish.
- **D7. The cache.** In memory, keyed by the base call (`W1AW/P` asks about
  `W1AW`). Found and not-found answers live 24 h from the answer; failures
  are never cached. At most 20,000 entries, oldest evicted first. Spots of
  one pending call share one request.
- **D8. Pauses after failure.** A refused login stops logins for 600 s
  (QRZ.com blocks the user name for 10 minutes after a wrong password,
  observed 2026-10-27); `Connection refused` stops them for 24 h. A timeout,
  network or HTTP error starts `manta_server::backoff`'s pauses: 1 s
  doubling to 60 s, reset by any answer. While paused, requests answer
  `failed` without touching the network. The request queue holds 256 calls;
  a spot whose call finds it full goes out unverified.
- **D9. Counting and logging.** `manta_callsign_lookups_total{outcome}`
  counts requests; `manta_callsign_lookup_withheld_total` and
  `manta_callsign_lookup_unverified_total` count spots
  ([METRICS.md](../METRICS.md#metric-families)). One stderr line per
  not-found answer from QRZ.com, not per cached hit; one warning per failure
  episode (every refused login, at most one per pause) and one line on
  recovery.
- **D10. Exemptions.** Only `[spot].allowlist` calls skip the lookup.
  MASTER.SCP calls are not exempt: that would spot an SCP call QRZ.com does
  not list.
- **D11. Config.** `enabled`, `qrz_username`, `qrz_password`, `qrz_url`;
  restart-only; settable from `MANTA_CALLSIGN_LOOKUP_*` (strings verbatim).
  Enabled needs both credentials; `qrz_url` must be `https://`, or `http://`
  to a loopback host. Works without `[server]` (no metrics then).
- **D12. End of decoding.** When decoding ends (end of stream, stop, or a
  read error), held spots are settled before `listen` returns, each waiting
  at most its own remaining deadline, and go through `on_spot` like any
  other. A stop with lookups in flight takes at most 10 s longer.

## Threat notes

The password sits only in a `0600` config file or the environment, and in
TLS POST bodies. `config check` prints it as `set`/`none`, and the config's
`Debug` form hides it. No redirect is followed. QRZ.com's error text is
scrubbed of control characters and capped at 200 characters before it is
logged; neither the password nor the session key appears in any log line.

## Evidence

- QRZ.com answers `200 OK`, `Content-Type: text/xml`,
  `Transfer-Encoding: chunked` over HTTPS and plain HTTP; a wrong password
  returned `Username/password incorrect - blocked until …`, ten minutes on;
  POST works for login and lookup.
- `ureq` 3.5 with `timeout_global(Some(10 s))` ended a request to a silent
  loopback server after 10.45 s, hence D6's decode-side deadline.
- `cargo-deny 0.20.2`: with `ureq` and `CDLA-Permissive-2.0` allowed,
  advisories, bans, licenses and sources pass; new duplicate warnings for
  `getrandom` and `windows-sys` only.
- Tests: `manta-spot` validator tests for hold, release, withhold, the
  allowlist, portable calls and beacons; `manta-engine` `listen_spots.rs`
  and `listen_inputs.rs` lookup tests; `manta-cli` `qrz::tests` (parsing and
  a loopback mock) and `callsign_lookup::tests` (Scenario 2's 10 s deadline,
  Scenario 3's 24 h cache, pauses, the password kept out of logs).

## Follow-ups

- A `doctor` QRZ.com login check (a wrong password costs a 10-minute block).
- Persisting the cache under `[spot].list_cache_dir` across restarts.
- HTTPS for MAN-316's list downloads through `ureq`.
- A `/status` field, if operators ask for one.
