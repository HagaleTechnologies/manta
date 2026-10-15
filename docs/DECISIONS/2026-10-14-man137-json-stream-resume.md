# MAN-137: resumable JSON spot stream

**Status:** accepted, implemented.

## Context

A reconnecting JSON consumer previously lost every record published while
it was away. The stream had neither sequence numbers nor a session greeting.
The planning probe published three spots, then connected through raw TCP
and WebSocket with `?session=0&since=0`. Both received nothing until another
spot was published, despite the bus retaining those three spots for telnet.

The ticket adopts dispensa `contracts/spots/stream.v1.md`, ADR-0015 and
Q-0070, accepted 2026-10-13. The private contract text was unavailable to
the research and plan phases. This implementation follows the ticket's
settled list and the plan's explicit interpretations below. These are
manta's adopted semantics, not quotations from the private document.

## Decisions

1. **Scope.** Only the spot listener, normally :7301, gains framing and
   replay. The MAN-324 live-state listener keeps its existing bytes and
   ignores resume queries. Raw TCP receives a hello and numbered live
   records, with no replay.
2. **Hello.** Every connection receives `kind`, `schema`, `session`, `seq`,
   `historyFrom`, `resumed`, in that order. `kind` is `hello`; `schema` is
   `stream.v1`. There is no start time. The hello is the first TCP line or
   WebSocket text frame, sent after transport classification or upgrade.
3. **Session.** Reuse `SpotBus::session_nonce()` as a decimal string, avoiding
   JSON-number precision loss. Live starts get a fresh nonce. Replaying the
   same file reuses its content-derived nonce for deterministic output.
   Sequence numbers restart on each bus, so consumers also check `resumed`.
   An ahead-of-session cursor detects a restarted file replay; the session
   alone cannot distinguish two runs of identical file input.
4. **Sequence and snapshot.** A shared `u64` starts at 1 and advances for
   every spot or grade. Assignment, history push and nonblocking broadcast
   send share one lock. Subscription and its newest-seq snapshot use the
   same lock. The hello reports that snapshot, S0, and live records start
   after S0. `historyFrom` is the oldest retained seq, capped at S0 + 1;
   an empty replay history reports S0 + 1, never null. It is read after
   the handshake so eviction during the handshake is visible.
5. **Resume outcomes.** A query with `since` requests replay. With a matching
   session and `historyFrom - 1 <= since <= S0`, replay `(since, S0]` and
   set `resumed: true`. An older cursor gets the retained suffix with
   `resumed: false`. A missing or different session, or `since > S0`, gets
   all retained records through S0 with `resumed: false`. A plain connection
   gets no replay and `resumed: false`. Replay and live delivery therefore
   meet at one snapshot, without duplicates or a subscription gap.
6. **Wire fields.** A spot's `seq` is its last, 27th key. A streamed grade's
   `seq` is its second key, after `kind`. It is optional in the ecosystem
   spot schema; manta always sends it on the spot stream. `SpotGrade.seq`
   is optional in Rust so offline `manta grade` output stays byte-identical.
7. **Retention.** One generic heard-time history has two independent
   instances. Telnet keeps its newest-50-spot floor. JSON keeps spots and
   grades together under `[server].history_minutes`, with a 50-record
   floor and 10,000-record ceiling. Grades age from the newest spot heard
   time at publication. Zero minutes keeps only the floor. Eviction only
   removes a prefix, preserving a contiguous sequence interval. As in
   MAN-93, late timestamps behind a younger front may remain until that
   front ages out; retention follows publication order. No history persists.
8. **Query parsing.** `since` is decimal digits fitting `u64`; leading zeros
   are allowed. Empty, negative, nonnumeric, repeated and overflowing values
   get HTTP 400, `since must be a whole number`. Repeated `session` gets
   HTTP 400, `session given more than once`. A session without since is a
   plain connection. Unknown keys and paths are accepted. Values are not
   percent-decoded; an encoded session fails the equality check and resets.
9. **Resume budget.** One shared per-IP limiter allows 30 requests per 10 s,
   matching telnet's command budget because either can render 10,000 rows.
   Excess resumes receive HTTP 429, `too many resume requests from this
   address; retry later`. Plain connects do not consume it. Entries expire
   through the existing stale-entry reaper. Proxied clients share one IP.
10. **Delivery and shutdown.** Every hello and replay write has the existing
    10 s timeout. Hello writes race shutdown and count abandoned live
    backlog on that path. Replay checks shutdown before each record, counts
    its unsent remainder separately, and then drains the live backlog using
    the existing deadline. Failed replay writes count the failed record and
    remaining replay, plus the abandoned live backlog. Live broadcast lag
    still disconnects a slow client and is counted; replay never blocks the
    decode pipeline on socket I/O.
11. **Metrics.** `manta_json_stream_resumes_total{outcome}` pre-renders
    `resumed`, `gap`, `reset`, `refused`, `invalid` at zero.
    `manta_json_stream_replay_abandoned_total` counts unsent replay records
    separately from live loss. `manta_json_stream_history_truncated_total`
    counts in-window ceiling eviction, separate from telnet history loss.
    See [METRICS.md](../METRICS.md).
12. **Consumers.** The field recorder skips hello without archiving or
    counting it as invalid. The comparison script skips all `kind` records,
    counting only grades. `manta grade` likewise counts only `spotGrade`.
    Consumers wanting spots only must skip records with a `kind` key.

## Contract interpretations

The explicit readings of the unavailable text are `schema = stream.v1`,
non-null/clamped `historyFrom`, full retained replay on reset, field order
including the last spot key and second grade key, and HTTP 400/429 with
the limits above. These choices are localized in `STREAM_SCHEMA`, `Hello`,
`resume_outcome`, the serde structs, query parser and limiter constants.
The ticket explicitly settles the shared seq, hello fields, WebSocket-only
resume, grade retention and shared ceiling. No dispensa files were changed.

## Validation and limits

Bus tests cover shared sequence order, snapshot boundaries, history window,
floor, ceiling, grade ageing and telnet isolation. Socket tests cover both
ticket scenarios, plain connections, raw TCP without replay, reset/gap/no-op
cursors, malformed and rate-limited requests, shutdown accounting and the
live-state listener. Consumer and documentation tests pin the wire fields.

A container smoke check on 2026-10-14 ran `manta gen v1`, then
`manta run --realtime --loop` with an ephemeral-port server config. Raw TCP
opened with session `12534119743695398054`, seq 0, historyFrom 1 and
resumed false. A WebSocket resume from 0 reported resumed true and received
the live W1AW spot at seq 1, frequency 14012362 Hz. Reconnecting with that
session and since 0 reported seq 1/resumed true and replayed the identical
spot. `?since=abc` returned HTTP 400. SIGINT stopped the daemon with exit 0.
The nonempty-replay-to-live boundary is asserted by
`a_reconnecting_ws_client_receives_what_it_missed` against a real listener.

No new config keys, persistent history, filters or raw TCP replay are added.
The field recorder still uses raw TCP and cannot recover its disconnect gap.
This does not change decoder output or claim Pi 4 replay performance.
The consumer reference is [SPOT-SCHEMA.md](../SPOT-SCHEMA.md#transport).
