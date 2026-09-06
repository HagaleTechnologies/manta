# MAN-63: WebSocket handshake buffering size bound — verification

MAN-63's premise: "WebSocket handshake buffering has no size bound, unlike
every post-upgrade frame/message limit" — `MAX_INBOUND_WS_MESSAGE_BYTES`
(16 KiB) governs post-upgrade WebSocket frames via `WebSocketConfig`, but
that config has no field for the HTTP handshake request itself, and
`HANDSHAKE_TIMEOUT` (10s, `json_stream.rs`) only bounds *time*, not bytes
buffered during that window. The ticket's own technical note required
verifying this against tungstenite's actual API surface rather than
assuming a gap exists.

## What was actually verified

`manta-server` resolves `tokio-tungstenite 0.26.2` → `tungstenite 0.26.2`
(`cargo tree -p manta-server -i tungstenite`; the workspace `Cargo.toml`
declares `"0.30"` but that isn't what this crate's dependency graph
resolves to).

- `WebSocketConfig` (`tungstenite::protocol::WebSocketConfig`) confirmed
  to expose only `read_buffer_size`, `write_buffer_size`,
  `max_write_buffer_size`, `max_message_size`, `max_frame_size` — all
  post-upgrade WebSocket-protocol-level, matching the ticket's premise so
  far.
- Traced the actual handshake code path:
  `accept_async_with_config` → `accept_hdr_async_with_config` →
  `handshake::server_handshake` (`tokio-tungstenite/src/handshake.rs`) →
  `handshake()` → `HandshakeMachine::single_round`
  (`tungstenite/src/handshake/machine.rs`).
- `HandshakeMachine::single_round`'s `Reading` branch calls
  `AttackCheck::check_incoming_packet_size` on **every** read, before
  attempting to parse the buffered bytes as an HTTP request. `AttackCheck`
  (same file) enforces, unconditionally and with no config knob:
  - `MAX_BYTES = 65536` — total cumulative bytes read for this handshake;
    exceeding it returns `Error::AttackAttempt` and aborts the handshake.
  - `MAX_PACKETS = 512` — total `read()` calls; same abort.
  - A slow-trickle heuristic once packet count exceeds 64: if the running
    average packet size drops below 128 bytes, aborts as an attack
    attempt (guards against a client dribbling single bytes to hold the
    connection open cheaply).
  - This runs inside `ServerHandshake`'s `HandshakeRole`, which
    `server_handshake` always drives — there is no way to reach
    `accept_async_with_config` without it.

Added a regression test,
`a_websocket_handshake_flooded_past_65536_bytes_is_rejected_not_buffered_forever`
(`crates/manta-server/tests/json_stream_acceptance.rs`), that floods a
handshake with >65536 bytes of header padding and no terminating blank
line: the server rejects it in the same tokio tick (0.00s), not after
waiting out `HANDSHAKE_TIMEOUT`, and the client is never counted in
`manta_ws_clients_connected`.

## Decision: no code change — ticket premise disproven, closing as verified-mitigated

The handshake is already bounded to 65536 cumulative bytes per attempt
(tungstenite's own hardcoded, non-configurable `AttackCheck`), on top of
manta's own 10s `HANDSHAKE_TIMEOUT`. Per-listener IP quotas already landed
in MAN-61 (`MAX_JSON_STREAM_CONNECTIONS_PER_IP = 16`), so worst case per
malicious source IP is `16 × 64 KiB = 1 MiB` transiently buffered — not a
meaningful resource-exhaustion vector, and not worth adding a parallel
`bounded_io`-style wrapper in front of a check that already exists one
layer down. A wrapper would only duplicate tungstenite's own protection
with a different threshold, adding a second thing to keep in sync for no
security benefit.

If tungstenite's dependency is ever bumped past a version that changes or
removes `AttackCheck` (its own doc comment marks it a `TODO`-flagged
stopgap: "instead of making them configurable, rework the way HTTP header
is parsed to remove this check at all"), this verification should be
redone rather than assumed to still hold.
