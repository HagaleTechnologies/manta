# 2026-10-17 — MAN-275 structured threat model: KiwiSDR client input

**Status:** accepted (investigation plus one bounded code change and a
committed regression-lock suite, all landed in this ticket's own PR; every
other finding is dispositioned below, exactly one disposition each).

## Decision

A structured, STRIDE-organized adversarial pass over the KiwiSDR client
input (`crates/manta-input/src/kiwi.rs`) — the outbound surface MAN-23's
own threat model (`2026-09-02-man23-threat-model.md`, Scope section and
its filed follow-up table, originally ticketed as MAN-60) explicitly
carved out and never covered. This closes the KiwiSDR carve-out on
current main.

Method: manual STRIDE walk over `kiwi.rs` as it stands on this branch,
cross-checked against (a) the research phase's four adversarial
binary-level reproduction tests that drove the real `manta` binary against
a scripted fake receiver (clean `Close` mid-stream, raw protocol violation
mid-stream, unknown-tag/truncated frames mid-stream, `Close` during the
initial handshake — 4/4 survived or failed fast cleanly), (b) this
ticket's committed daemon-level regression suite
(`crates/manta-cli/tests/kiwi_adversarial.rs`, five scenarios including
the over-64-KiB frame), and (c) the vendored tungstenite 0.30.0 source
(defaults and handshake bounds re-derived, not assumed). Every finding
below gets exactly one disposition: **fixed** (landed, cites where),
**filed** (a follow-up ticket whose scope is specified precisely enough to
open verbatim — "filed" here means *recommend-file*: no tracker credential
was available in the environment that wrote this record, so the row's
scope text is the ticket), or **accepted risk** (recorded here with
rationale, no ticket). Nothing is left as unfiled prose.

**Supersession note.** This record replaces the closed MAN-60 / PR #109
(its branch survives as `archive/MAN-60-pr109`; 11 repair rounds, closed
unmerged by the owner-approved 2026-10-05 audit triage). MAN-60's
implementation predates MAN-73's generic `ReconnectingSource` and its
bespoke reconnect/zero-fill design is **superseded and must not be
revived**: zero-filling an outage longer than ~2.5 s pins
`manta-dsp::floor`'s noise-floor estimate and floods false tracks on
resume — see `2026-10-05-man73-source-reconnect.md`'s "Why not zero-fill
the gap" section. This ticket re-did the work from current main, reusing
only the archived branch's still-valid ideas (the WebSocket size bound,
the STRIDE-table dispositions, the loopback fake-receiver test harness
patterns). The archived branch's own decision doc wrote "Fixed"
dispositions for code that never merged; the findings below describe
current main, not that branch's intentions.

## STRIDE findings

| # | Category | Finding (current main) | Disposition |
|---|---|---|---|
| 1 | Tampering / DoS | A single malformed/adversarial WebSocket, MSG or SND frame, a raw WS protocol violation, or a clean `Close` from the receiver — MAN-60's original unconditionally-fatal-daemon finding | **Fixed** — not by Kiwi-specific code but by MAN-73's generic `ReconnectingSource` (reconnect forever, 1 s→60 s backoff, outage reported via `take_discontinuity()`), which wraps every non-File live source at the composition root; within one connection, `kiwi.rs`'s `read()` discards unusable frames in place (too-short SND body → counted malformed, unknown 3-byte tag → ignored, `Text`/`Ping`/`Pong` → ignored, invalid UTF-8 → `from_utf8_lossy`, scrambled `seq` → silent re-baseline). Regression-locked by this ticket's `kiwi_adversarial.rs` (clean Close, raw violation, unknown-tag/truncated, handshake-time Close, oversized frame); independently corroborated in operator prose (`docs/GUIDES/kiwisdr.md` "It reconnects after 1 s, backing off to 60 s, forever") |
| 2 | DoS | No explicit client-side WebSocket frame/message size bound: `KiwiIqSource::connect` called `tungstenite::client` with no `WebSocketConfig`, so tungstenite 0.30.0's defaults (64 MiB message / 16 MiB frame) applied, while this repo's own server-side convention bounds the same thing to 16 KiB (`json_stream.rs`'s `MAX_INBOUND_WS_MESSAGE_BYTES`) | **Fixed** — in this ticket: `MAX_WS_FRAME_BYTES = 64 * 1024` applied via `client_with_config` to both `max_message_size` and `max_frame_size` (`kiwi.rs`); ~30x headroom over the constant 2068-byte real SND capture. Red-then-Green: `crates/manta-input/tests/kiwi_ws_size_bound.rs` proved a 68 KiB SND-tagged frame was accepted and decoded on main (`Ok(8192)`) and errors after the bound; `kiwi_adversarial.rs`'s `oversized_frame_midstream_is_survived` locks the daemon-level outcome (logged reconnect, daemon keeps running) |
| 3 | Information Disclosure / Spoofing | The optional password travels in cleartext over `ws://` (`SET auth t=kiwi p=…` as a `Message::Text`, `kiwi.rs`) — **the ticket's scenario-2 item** | **Accepted risk** — no `wss://` variant exists in the Kiwi protocol as this client speaks it; reaching arbitrary operator-chosen public hosts *is the feature's stated purpose* (ARCHITECTURE.md §3), so there is no LAN-trust fallback the way HPSDR has one. Already disclosed to operators (`docs/GUIDES/kiwisdr.md` "The password travels unencrypted (`ws://`). Use one that isn't used anywhere else."). Mitigations recorded: use a unique password, deliver it via the mode-0600 config file (`[input] password`, docs/CONFIG.md) rather than the CLI flag |
| 4 | Information Disclosure | Password visible in `argv`/`ps`/shell history when `--kiwi-password` is used | **Accepted risk (mitigated)** — the `[input] password` config-file key plus the operator guide's explicit instruction to use it instead of the CLI flag (`docs/GUIDES/kiwisdr.md` "Use the config file instead of putting the password in shell history", with the `chmod 0600` example) already close the practical exposure. A `MANTA_KIWI_PASSWORD` env var was considered and declined: it adds a second secret store, readable via `/proc/<pid>/environ` by the same trust principal that could read `argv`, and removes nothing |
| 5 | Spoofing | The IQ stream is trusted at face value from an arbitrary third party: spoofed or imagined "RF" becomes decodable input and, potentially, spots | **Accepted risk** — same reasoning as MAN-23's HPSDR UDP-spoofing finding: inherent to an unauthenticated receiver protocol; every Kiwi client (including the reference `jks-prv/kiwiclient`) has the identical property. The downstream consequence is bounded by MAN-62's occurrence-count cap (`2026-09-03-man62-bounded-occurrence-counts.md`), which was filed by MAN-23 for exactly this class of injection |
| 6 | Tampering | Parser memory safety: `parse_snd_frame`, `parse_kv_f64`, `handle_msg`, `SndSeqTracker` all run on server-controlled bytes | **No finding** — no `unsafe`, no `unwrap`, no unguarded indexing reachable from server-controlled bytes (length-checked header slicing; `String::from_utf8_lossy` for MSG text; `wrapping` arithmetic on `seq`); verified by direct walk in this pass, and pinned by the existing `--lib kiwi` unit tests |
| 7 | DoS | WebSocket handshake buffering (pre-upgrade) | **No finding** — tungstenite 0.30.0 wraps every handshake role, client and server alike, in `AttackCheck` (`MAX_BYTES = 65536` / `MAX_PACKETS = 512`, `handshake/machine.rs`), unconditional, with no config knob; re-derived against the vendored source in this pass |
| 8 | DoS | DNS resolution / TCP connect unbounded (MAN-60's finding 10: "`connect()` blocks unboundedly on a stalled resolver") | **Fixed** (pre-empted by MAN-73-era work, before this ticket) — `OVERALL_CONNECT_TIMEOUT` (30 s) races resolution plus every per-address `connect_timeout` on a detached thread, and `OUTSTANDING_CONNECTS` keeps at most one abandoned worker per `(host, port)` (`kiwi.rs`); supersedes the archived MAN-60 doc's "filed" disposition for the same finding |
| 9 | DoS | A flood of well-formed-but-useless frames (unknown tags, keepalive-only MSG) resets the silence bound and never trips it: `MAX_CONSECUTIVE_TIMEOUTS` only advances on a read timeout, so a receiver that keeps sending garbage-but-parseable frames stalls the decode forever without a reconnect | **Filed (recommend-file)** — scope, verbatim-openable: *a generic in-connection stall detector at the `ReconnectingSource` layer (`crates/manta-cli/src/reconnect.rs`): count bounded-useless reads across a rolling N-second window for **all** source kinds, reconnect on breach. Deliberately NOT a Kiwi-local `MAX_CONSECUTIVE_UNUSABLE_FRAMES`: a per-source inner bound would have to be duplicated in every driver (Kiwi, HPSDR, Soapy, ka9q, audio) and would still not cover the others. Estimated P3: a hostile receiver can already be neutralized by the operator (point manta elsewhere); the gap is availability hygiene, not a crash* |
| 10 | Information Disclosure (diagnostics) | `kiwi.rs`'s own error strings omit host:port (e.g. "KiwiSDR closed the connection"); unknown-tag frames are discarded without any counter | **Accepted risk (low, observability only)** — `ReconnectingSource`'s log lines carry the operator-assigned input name, which disambiguates multi-input setups (`multi_run.rs` passes `input.name` into the wrapper), and `malformed_packets` already counts too-short SND bodies; the residue is per-receiver garbage accounting no tooling consumes today. Revisit if node-health tooling ever needs it |

## Non-outcomes

- The archived branch's reconnect/zero-fill code, `KiwiStats`, and
  `MAX_CONSECUTIVE_UNUSABLE_FRAMES` are **not** revived (see the
  supersession note above).
- No TLS/`wss://` support is added: no `wss://` variant exists in the Kiwi
  protocol as this client speaks it (finding 3 records the exposure
  instead).
- No `MANTA_KIWI_PASSWORD` env var is added (finding 4 records the
  rationale for declining it).
- `soapy.rs`/`audio.rs` remain uncovered by any threat-model pass — the
  same carve-out MAN-23 left for Kiwi. Future work: re-run this pass's
  method there separately.
- The telnet/RBN surfaces and everything under the `hpsdr` feature are
  untouched.
