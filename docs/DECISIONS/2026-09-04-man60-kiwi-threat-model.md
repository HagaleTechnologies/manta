# 2026-09-04 — MAN-60 structured threat model: KiwiSDR client input

**Status:** accepted (code changes for the fixed findings landed in this
same PR; findings left open are dispositioned as **filed** (recommend-file
— see note) or **accepted risk**, no unfiled prose).

## Decision

A STRIDE-organized adversarial pass over `crates/manta-input/src/kiwi.rs`
(`KiwiIqSource`), the one network-facing input surface
`docs/DECISIONS/2026-09-02-man23-threat-model.md` explicitly carved out of
its own scope and filed as this ticket (MAN-23 Scope section, and its
follow-up-tickets table). Every finding below is given exactly one
disposition: **fixed** (landed in this PR, cited by file/line), **filed**
(a new ticket's exact scope, specified precisely enough to open verbatim —
see the note below on why these aren't live ticket numbers yet), or
**accepted risk** (recorded here with its rationale, no ticket). Nothing is
left as unfiled prose, matching MAN-23's own invariant.

**Note on "filed" items in this document:** this pass ran in an environment
with no Linear/GitHub write credential (a constraint of the session, not a
policy choice) — the same constraint MAN-23's own originating research
noted for some of its findings. Every item dispositioned "filed" below has
its exact scope specified precisely enough to open the ticket verbatim;
none are live ticket numbers the way MAN-23's MAN-57..MAN-64 are. Treat
"filed" here as "recommend-file, scope below" until a maintainer with
tracker access opens them.

Scope: `KiwiIqSource::connect`/`read` and their helpers (`handshake`,
`reconnect`, `classify_and_apply`, `parse_snd_frame`, `parse_kv_f64`,
`ack_audio_rate_if_present`) in `crates/manta-input/src/kiwi.rs`, plus every
place that consumes or is affected by its `IqSource` implementation: the
`IqSource` trait (`crates/manta-input/src/lib.rs`), `manta_engine::listen`'s
read loop (`crates/manta-engine/src/listen.rs`), and the CLI wiring/exit
path (`crates/manta-cli/src/main.rs`). This mirrors MAN-23's own scope shape
(protocol driver → engine consumption → process-level consequence).
**Explicitly NOT covered by this pass**, matching MAN-23's own boundaries:
`crates/manta-input/src/soapy.rs` and `crates/manta-input/src/audio.rs` (the
other pre-existing input drivers, still uncovered by any pass), and MAN-13
multi-source orchestration (doesn't exist yet — re-run this pass, and
MAN-23's, once it lands, the same caveat MAN-23 already carries forward for
MAN-11/MAN-12).

Method: manual STRIDE walk over the actual code (not the diff-oriented
`security-review` skill, since most of this was already-shipped code before
this pass began), cross-checked against `ARCHITECTURE.md` §3/§7/§8's stated
design intent, plus direct verification against `tungstenite 0.30.0`'s
actual source (vendored in this environment's local registry cache) rather
than assumed defaults — the same discipline
`docs/DECISIONS/2026-09-03-man63-ws-handshake-size-bound-verification.md`
established for the server-side WS listener. A full `manta-input` `unsafe`
grep (zero hits) reconfirms MAN-23's own crate-wide finding still holds.

## STRIDE findings

| # | Category | Finding | Disposition |
|---|---|---|---|
| 1 | Tampering / DoS | A single WebSocket protocol violation (a tungstenite-rejected frame) or a single clean `Close` from the connected receiver was unconditionally, unboundedly fatal to the whole daemon — the pre-fix shape of MAN-22's HPSDR finding, never given the equivalent fix here | **Fixed** — `KiwiIqSource::reconnect` (`kiwi.rs`), a bounded (`MAX_RECONNECT_ATTEMPTS = 4`, doubling backoff capped at 2s), zero-fill-compensated reconnect. Unlike HPSDR's fix, this reconnects rather than discards-and-continues: RFC 6455 requires *failing the connection* on a protocol violation, and tungstenite's state machine does so, so there is no usable socket left to keep reading from. A clean `Close` is treated identically to a protocol error (a receiver reboot or an operator-initiated kick is a realistic non-adversarial event, indistinguishable from an attack without this fix). Worst-case time from "receiver goes silent" to "daemon exits with a host-named error": ~10s stall detection + 4×5s connect timeouts + (0.5+1+2+2)s backoff ≈ 35s, all bounded and logged (`tracing::warn!`/`info!`) |
| 2 | DoS | An unbounded stream of well-formed-but-useless MSG/SND frames could stall `read()` indefinitely without ever tripping the silence-timeout bound (`consecutive_timeouts` reset on *any* successful read, useful or not) | **Fixed** — `MAX_CONSECUTIVE_UNUSABLE_FRAMES = 10_000`, mirroring HPSDR's `MAX_CONSECUTIVE_MALFORMED` exactly (a count bound, not wall-clock, since unusable-frame arrival rate is peer-controlled). `KiwiStats::unusable_frames` makes this observable rather than silent. Regression-tested against a live loopback fake receiver flooding unknown-tag frames (`kiwi.rs` tests) |
| 3 | DoS | The client-side WebSocket handshake set no explicit frame/message size limit, unlike this repo's own server-side 16 KiB convention (`json_stream.rs`) | **Fixed** — `MAX_WS_FRAME_BYTES = 64 * 1024` via an explicit `WebSocketConfig` on `tungstenite::client_with_config`. Verified directly against tungstenite 0.30.0's source (not assumed): the un-overridden default is `max_message_size: Some(64 << 20)` / `max_frame_size: Some(16 << 20)` (`tungstenite-0.30.0/src/protocol/mod.rs`) — 64 KiB is ~30x headroom over real 2068-byte SND frames and ~256x below the frame default. Regression-tested: a fake receiver sending a frame larger than the bound is rejected by the client's own configured limit and recovered via reconnect (finding 1), not left as an unbounded allocation |
| 4a | Information Disclosure | The optional password is sent as a plaintext WebSocket **text** frame over `ws://` — legible to any on-path observer between the manta host and the configured receiver | **Accepted risk** — narrower rationale than MAN-23's structurally similar accepted risks for RBN uplink/telnet-WS (findings 18/20 there): the KiwiSDR wire protocol as this client speaks it has no `wss://` variant at all (live-verified during original M2 implementation against several public receivers and against the reference `jks-prv/kiwiclient` client — both speak plain `ws://` only). Unlike HPSDR's LAN-trust fallback, there's no topological mitigation available: reaching arbitrary internet hosts is this feature's entire value proposition (`ARCHITECTURE.md` §3). Narrowed in practice: public KiwiSDR nodes are overwhelmingly password-less (the CLI flag's own help text says so), so this specifically affects only the less-common private/password-protected node case. Follow-up filed below for optional `wss://` support |
| 4b | Information Disclosure | The password was additionally visible in `argv`/`/proc/<pid>/cmdline`/shell history via `--kiwi-password` | **Fixed** — `MANTA_KIWI_PASSWORD` env var (`crates/manta-cli/src/main.rs`, `resolve_kiwi_password`), precedence: non-empty flag wins, else the env var, else anonymous. Deliberately a manual env read, not clap's `env` feature: with that feature, merely *exporting* the variable would make clap treat `--kiwi-password` as present, firing its existing `requires = "kiwi_host"` and breaking unrelated invocations like `manta listen --source foo.wav` run with the variable set — regression-tested (`exporting_the_kiwi_password_env_var_does_not_break_a_non_kiwi_listen`, `manta-cli/tests/cli.rs`). Not zeroized in memory: the value is in `argv` regardless of what this process does with its own copy, so zeroizing the `String` would be theater, not a real mitigation |
| 5 | Spoofing / Tampering | The entire IQ stream is trusted at face value from an operator-chosen third party, with no content-integrity check | **Accepted risk** — same category as MAN-23 finding 2 (HPSDR UDP source-IP spoofing), but the trust model is structurally different: HPSDR's finding assumed an attacker exploiting the *absence* of authentication on an otherwise-trusted local device, whereas here the "attacker" *is*, by design, an arbitrary third party the operator explicitly chose (`ARCHITECTURE.md` §3's whole rationale). No LAN-trust fallback is available or meaningful. The downstream consequence (fabricated decoded callsigns growing `SpotBus::occurrence_counts` without bound) is already covered generically by MAN-62 regardless of which `IqSource` originated the data — no KiwiSDR-specific duplicate needed |
| 6 | Tampering (memory safety) | Panic/out-of-bounds surface reachable from server-controlled frame bytes across `parse_snd_frame`/`parse_kv_f64`/`ack_audio_rate_if_present`/`handle_msg` | **No action needed** — verified clean by direct walk against every input length/content: `parse_snd_frame`'s `HEADER_LEN` guard returns empty before any indexing for a short body, `n_pairs = payload.len() / 4` (integer division) keeps every computed offset in-bounds for any length including non-multiples-of-4, `parse_kv_f64` uses only `split_whitespace`/`strip_prefix`/`.parse().ok()` (no indexing, no `unwrap`), and every UTF-8 decode uses `from_utf8_lossy` (never panics on invalid input). Zero `unsafe` in `manta-input`, matching MAN-23's own crate-wide finding |
| 7 | Repudiation | `manta-input` (including `kiwi.rs`) carried no logging at all, unlike `manta-server`'s now-landed audit trail (MAN-59); a crash's only diagnostic was a single unleveled stderr line that didn't even name the host/port | **Partially fixed** — `kiwi.rs` now emits `tracing::warn!`/`tracing::info!` on every connection loss, reconnect attempt, and give-up, and every error message in this file now names host:port. The generic `manta-input`/`manta-engine` logging gap for the *rest* of those crates stays exactly where `ARCHITECTURE.md` §8 already records it — this fix is scoped to `kiwi.rs` only, because this PR is what introduces automatic (and otherwise invisible) reconnect behavior here; it would be a bigger, separate decision to add logging to the crates generically |
| 8 | (liveness correctness) | `confirmed_live_handle()`'s default (`None`) is the correct choice for KiwiSDR | **No action needed** — verified correct, unchanged by this pass: KiwiSDR's WebSocket handshake (a real TCP connect, a real WS upgrade, and reading a real `MSG sample_rate=` response) cannot succeed at all without a genuine live response from the far end, which is exactly the condition under which the trait's default is documented to be right (MAN-55) |
| 9 | DoS | Candidate finding, considered and rejected: does the client-side WS handshake (before `MAX_WS_FRAME_BYTES`, finding 3, ever applies) have an unbounded buffering surface, the client-side analogue of the server-side gap MAN-63 investigated? | **No action needed — verified clean**, same rigor MAN-63 applied to the server side, not assumed. Traced `tungstenite::client_with_config` → `ClientHandshake::start` → `HandshakeMachine::new`, which unconditionally wraps *every* handshake role (client and server alike) in `HandshakeState::Reading(ReadBuffer::new(), AttackCheck::new())`. `AttackCheck::check_incoming_packet_size` (`tungstenite-0.30.0/src/handshake/machine.rs`) enforces `MAX_BYTES = 65536` and `MAX_PACKETS = 512` unconditionally, with no config knob, on every handshake round-trip regardless of role — an oversized or dribbling handshake response aborts with `Error::AttackAttempt` well before this module's own `MAX_WS_FRAME_BYTES` bound would even apply. Confirmed against the vendored `tungstenite-0.30.0` source directly (registry cache in this environment), the same class of verification MAN-63 performed for `manta-server`'s side of the same library |
| 10 | DoS | `handshake()`'s DNS resolution (`ToSocketAddrs::to_socket_addrs()`) is a blocking, unbounded standard-library call; `CONNECT_TIMEOUT` (added by this PR) only bounds the TCP connect phase that follows it | **Filed** (recommend-file, see note above) — a hostile or slow resolver can still block a reconnect attempt (and therefore the whole recovery budget) for longer than the stated ~35s worst case. `std` has no bounded synchronous resolver; a real fix needs either an async resolver crate (a new dependency decision for `manta-input`) or a resolve-time deadline enforced via a helper thread — out of scope for a threat-model pass to decide unilaterally |

## Follow-up tickets to file (exact scope, so they can be opened verbatim)

- *"KiwiSDR input should support `wss://` for receivers behind TLS
  proxies"* (finding 4a) — needs a TLS dependency decision for
  `manta-input` (`rustls` vs `native-tls`, matching how `manta-server`/
  `tungstenite`'s own `native-tls`/`__rustls-tls` features are gated), a
  URL-scheme flag or autodetect, and a certificate-verification policy. P3.
- *"`KiwiIqSource::connect`/`reconnect` block unboundedly on DNS
  resolution"* (finding 10) — `CONNECT_TIMEOUT` bounds only the TCP phase;
  the resolve step itself has no timeout. P3.

## Accepted risks (recorded here, no ticket)

1. Plaintext password over `ws://` (finding 4a) — the KiwiSDR protocol has
   no encrypted variant in the form this client speaks it; no LAN-trust
   fallback exists the way it does for HPSDR, since reaching arbitrary
   internet hosts is this feature's design goal. Narrowed by the
   overwhelming real-world prevalence of password-less public nodes.
2. No content-integrity check on the IQ stream (finding 5) — inseparable
   from the feature's own design goal of using arbitrary, operator-chosen
   third-party receivers (`ARCHITECTURE.md` §3). Downstream consequence
   already covered generically by MAN-62.
3. Not zeroizing the password in process memory (finding 4b) — it is in
   `argv` for the process's lifetime regardless, so zeroizing this
   module's own copy provides no real protection.

## Non-outcomes

- No changes were made to `soapy.rs`/`audio.rs` — same carve-out MAN-23
  made; they remain unreviewed by any pass.
- No changes were made to HPSDR, telnet, JSON/WS, or the RBN uplink;
  MAN-23's dispositions for those stand unmodified.
- MAN-13 (multi-source orchestration) does not exist yet and is therefore
  not covered by this pass — re-run this STRIDE walk (and MAN-23's) once
  it lands, the same caveat both documents already carry forward.
- TLS/`wss://` support was deliberately not added in this PR — it's a real
  dependency decision, not a threat-model-pass-sized change; filed as a
  follow-up above instead.

## References

- Originating pass: `docs/DECISIONS/2026-09-02-man23-threat-model.md`
  (Scope carve-out naming this ticket; HPSDR finding 1's pre/post-fix
  shape, the direct precedent for this doc's finding 1; accepted risks
  18/20 for the TLS comparison; MAN-62 for finding 5's downstream
  consequence)
- Verification method precedent:
  `docs/DECISIONS/2026-09-03-man63-ws-handshake-size-bound-verification.md`
  (finding 9 above applies the same "verify against the library's actual
  source, don't assume" discipline to the client-side handshake)
- Fix pattern mirrored: `crates/manta-input/src/hpsdr.rs`
  (`MAX_CONSECUTIVE_MALFORMED`, `pump_one_packet`, loopback adversarial
  tests)
- Size-bound convention mirrored: `crates/manta-server/src/json_stream.rs`
  (`MAX_INBOUND_WS_MESSAGE_BYTES`, explicit `WebSocketConfig`)
- Code changed: `crates/manta-input/src/kiwi.rs` (`handshake`, `reconnect`,
  `classify_and_apply`, `pump_until_samples`, `KiwiStats`),
  `crates/manta-input/Cargo.toml` (added `tracing`),
  `crates/manta-cli/src/main.rs` (`resolve_kiwi_password`)
- Protocol geometry: `docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md`
- Timestamp semantics motivating the zero-fill design (finding 1):
  `crates/manta-spot/src/validator.rs`, `crates/manta-server/src/bus.rs`
  (`SpotBus::unix_ts_for`)
