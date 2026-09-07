# MAN-87: telnet IAC option-negotiation policy, and login control-byte trimming

Found by the 2026-09-05 broad review (lens 1, items #5/#30): manta's telnet
listener drops any client that opens the connection with RFC 854 IAC option
negotiation, which is exactly what Windows `telnet.exe`, PuTTY's telnet
mode, and most DX-cluster client software do the instant the TCP connection
opens, before any application data. This directly blocked `ROADMAP.md`'s M3
acceptance gate ("a stock DX cluster client connects, logs in, and receives
well-formed spots") for any client that isn't a bare `nc` or piped `telnet`.

Reproduced against `main` at `e398d46` (see the MAN-87 research/plan
documents for the full transcripts): a client sending
`\xff\xfb\x18\xff\xfd\x03W5AU\r\n` (IAC WILL TERMINAL-TYPE, IAC DO
SUPPRESS-GO-AHEAD, then the callsign) was disconnected with `login read
rejected (oversized/malformed line or timeout), disconnecting
error=line contains invalid UTF-8` before the callsign was ever read.
Separately, a callsign terminated with CR NUL (RFC 854's NVT encoding of
Enter — what macOS `telnet(1)` with piped stdin sends) was logged with both
control bytes intact (`login="W5AU\r\0"`), because `str::trim` stops at the
first non-whitespace character from each end and NUL is not Unicode
whitespace, so the trailing NUL blocks the scan from ever reaching the CR
one position in.

## Root causes

1. `bounded_io::read_line_bounded` validates every raw chunk as UTF-8 before
   any telnet-protocol awareness exists in the codebase. IAC is `0xFF`,
   which is never valid UTF-8 in any position, so the first `fill_buf()`
   chunk containing IAC bytes fails unconditionally.
2. `str::trim`'s edge-inward semantics mean a trailing NUL (not whitespace)
   stops the scan before reaching a CR (whitespace) behind it — verified
   directly: `"W5AU\r\u{0}".trim() == "W5AU\r\u{0}"`.

## Options considered

1. **Implement real telnet option support** (ECHO, SGA, NAWS, etc.).
   Rejected: `ARCHITECTURE.md` §7 describes a line-oriented, read-mostly
   text protocol; no option earns the state or attack surface it would
   cost for a server that only ever emits spot lines and reads short
   commands.
2. **Pre-scan and strip IAC bytes from the raw socket buffer before the
   existing UTF-8-validating reader runs, refusing every option.** Chosen
   — see below.
3. **Silently ignore/drop bytes ≥ 0x80 in the login line instead of
   telnet-aware parsing.** Rejected: too blunt — it would also mangle a
   subnegotiation payload containing a literal newline (see "IAC-in-line
   trap" below) and gives no reply, so negotiating clients that DO wait for
   an answer before proceeding would stall instead of logging in.

## Decision: strip and refuse everything, telnet-only

A pure, synchronous, byte-at-a-time RFC 854 state machine
(`manta_server::iac::IacFilter`) sits in front of UTF-8 validation on the
telnet listener specifically. Every `WILL`/`DO` is answered `DONT`/`WONT`
(every option refused); every `WONT`/`DONT` from the client is left
unanswered, per RFC 854's own loop-avoidance rule — answering a refusal is
how negotiation loops start. `IAC IAC` (an escaped literal `0xFF` data
byte) is dropped, not emitted: emitting it would hand a `0xFF` straight
back to UTF-8 validation and recreate the exact disconnect this decision
exists to prevent.

**A separate reader entry point (`read_line_bounded_telnet`), not a flag on
the existing one.** `bounded_io::read_line_bounded` has two other call
sites — `metrics_http`'s HTTP request line and `uplink`'s inbound RBN
read — both ASCII-by-contract protocols where
`docs/DECISIONS/2026-09-02-man23-threat-model.md` finding 19 already
commits to rejecting non-UTF-8 as a deliberate hardening measure. Only the
telnet listener talks to clients that legitimately prepend binary framing;
a separate function makes that a type-level fact instead of a call-site
convention that could silently erode. **Their behavior is unchanged by
this decision.**

**The IAC-in-line trap.** Telnet framing can itself contain `0x0A`: option
10 is NAOCRD, and subnegotiation payloads are arbitrary bytes, so `IAC DO
10` or a subnegotiation carrying a stray newline byte both put a raw
`0x0A` in the chunk that is not a line terminator. The newline is searched
for in the FILTERED output, never the raw chunk — a reader scanning raw
bytes for `\n` would cut a line in the middle of a negotiation sequence.
Covered by a dedicated unit test
(`telnet_variant_does_not_end_the_line_on_an_option_byte_of_0x0a`).

**The line-length cap counts raw bytes consumed, not surviving text
bytes.** Otherwise a client streaming endless negotiation would never hit
`MAX_LINE_BYTES` and could hold a read open indefinitely — MAN-23's
hardening must not regress. Cost: a legitimate client's negotiation counts
against its 1024-byte line budget, irrelevant at the few dozen bytes real
negotiation occupies. The raw-byte counter lives on `IacFilter` itself
(`raw_line_bytes`), not in the read function, because it must survive
`tokio::select!` cancellation mid-line exactly like `cmd_line` does — it's
the one per-connection value that does. The budget is released ONLY when
a line completes or a read errors — this is D4, held as originally
decided.

**Round 2 (finding C2) tried releasing the budget on pure-framing reads
too** — a read that consumed nothing but protocol framing with no
application text pending yet — to keep a client sending only occasional
keepalive negotiation (e.g. `IAC NOP`) from having `raw_line_bytes`
accumulate across its entire session and eventually trip the cap despite
never sending anything resembling a long line. **Reverted in round 3**
(validation, code-review findings 2 and 3): that release made the cap
unreachable for a client that never lets a single read return anything but
framing (an `IAC SB` stream with no closing `IAC SE`, or a trickle of
`IAC NOP` pairs — the branch's own test proved a flood of either sailed
through uncapped), and made the cap's behavior depend on how the OS
happened to chunk the incoming bytes across `fill_buf` calls rather than
on what was sent. The C2 concern (a long-lived, keepalive-only client
eventually crossing 1024 raw bytes without ever completing a command line)
is real but rare — D4's own rationale already accepts the cost of counting
negotiation against the line budget, on the grounds that real negotiation
is a few dozen bytes; it did not anticipate a client sending nothing else
for an entire multi-hour session. If that is ever observed from a real
client, the fix needs a budget that is bounded (never fully releasable to
an attacker) but distinct from the ordinary line-content cap — not the
full release round 2 shipped. Not attempted in round 3: no real client has
been observed doing this, and D4's original tradeoff already covers the
common case (option negotiation at connect).

**Negotiation replies are batched and flushed by the caller after the read
returns, not written from inside `bounded_io`.** Two reasons: the
`select!` driving the command read already holds `&mut wr` in its
spot-writing branch, so the read future cannot borrow it too; and RFC 854
negotiation is asynchronous — no real client blocks waiting for a reply
before sending its callsign. Stated plainly: a hypothetical client that
*did* block after sending only negotiation would hit the existing 30 s
idle timeout, which is still strictly better than today's immediate
disconnect. Revisit only if a real client is observed doing that.

**Correction (round 3 validation, code-review finding 4, plausible/lower
severity):** the 30 s idle-timeout backstop above applies to the LOGIN
read only (`read_line_bounded_telnet_with_timeout`). The command read
(`telnet.rs`'s `select!` loop) deliberately uses the untimed variant
(round-5 review finding: an established, quietly-listening client must
never be disconnected just for staying quiet) — so a logged-in client that
sends only a mid-session negotiation probe (e.g. `IAC DO TIMING-MARK`) and
then waits for the reply has nothing bounding that wait at all on the
command path. No real client has been observed doing this; not fixed in
round 3 on that basis, per this decision's own "revisit only if observed"
stance — but the decision record above overstated the backstop's reach and
is corrected here rather than left standing as written.

**Reply buffering is bounded** (`MAX_NEGOTIATION_REPLY_BYTES = 192`, 64
refusals). An unauthenticated client must not be able to grow a
server-side buffer without limit (MAN-23 threat model). Past the cap,
options are still parsed and stripped, just no longer answered.

**The command path gets the filter too, not just login.** The ticket only
names login, but a mid-session IAC byte on an established connection would
disconnect it with `command read rejected` and break the same M3 gate one
layer later — real clients that send periodic keepalive negotiation would
be hit by this on a long session. The filter is already per-connection
state (owned by `handle_client`, passed to both the login and command
reads); reusing it costs one argument and one post-read flush, not a
redesign.

## Scenario 2: trim, don't validate

The reader recognizes `CR NUL` as a line terminator alongside `\n`
(`bounded_io::read_line_bounded_telnet`), not just `\n` followed by EOF.
RFC 854's NVT encodes Enter as `CR NUL` too, and that is exactly what
BSD/macOS `telnet(1)` sends by default (`crlf` toggle default FALSE)
*without* closing the connection afterwards — the piped-stdin case (EOF
right after `CR NUL`) is a different, easier shape of the same client
behavior. Recognizing only `\n` handled the piped-stdin shape but left the
interactive shape stalling forever on a line that never completes, dropped
only by the 30 s idle timeout (review round 2, finding C1).

`trim_login()` (`crates/manta-server/src/telnet.rs`) strips trailing (and
leading) CR/NUL/whitespace via
`raw.trim_matches(|c: char| c.is_whitespace() || c == '\u{0}')` — a strict
superset of `str::trim`'s predicate that also catches NUL. This decision
deliberately stops at trimming: **rejecting an implausible callsign is out
of scope here.** MAN-86 (same broad-review initiative, D1) owns login
*validation* — its `sanitize_login` uses this identical predicate plus
shape checks and subsumes `trim_login` when it lands. At the time this
decision was made, MAN-86 was not yet on `main`
(`grep -rn "sanitize_login\|SKIMMER\|StationProfile" crates/manta-server/src/`
returned nothing), so this ticket implements the trim itself rather than
leaving Scenario 2 open; whichever ticket lands second deletes its own
copy rather than redesigning anything, since the predicates are
character-identical by construction.

## What this does not do

- No telnet options are implemented (no ECHO, no SGA, no NAWS, no
  terminal-type subnegotiation state) — every option is refused,
  unconditionally.
- No implausible-callsign rejection — `trim_login` only strips framing;
  MAN-86 owns validation.
- No metrics counter for refused negotiations — nothing consumes it yet,
  and MAN-23's counters exist for loss accounting, not protocol noise.
- Does not close M3's acceptance gate by itself — that gate needs a real
  client (Windows `telnet.exe`, PuTTY) verified against real hardware,
  which is not reachable from an automated/CI environment. This decision
  removes the blocker; the manual verification step remains outstanding.
- `metrics_http` and `uplink` are untouched — they keep rejecting `0xFF`
  and other non-UTF-8 bytes exactly as before, per MAN-23 finding 19.

## Implementation

- `crates/manta-server/src/iac.rs` (new): `IacFilter`, a byte-at-a-time
  state machine (`push(&mut self, b: u8) -> Option<u8>`), plus
  `take_replies`/`has_replies`/`reset_line`.
- `crates/manta-server/src/bounded_io.rs`: `read_line_bounded_telnet` and
  `read_line_bounded_telnet_with_timeout`, siblings of the existing
  `read_line_bounded`/`read_line_bounded_with_timeout` which are left
  untouched.
- `crates/manta-server/src/telnet.rs`: one `IacFilter` per connection,
  used by both the login read and the command read inside `select!`;
  refusals flushed after each; `trim_login` applied to the stored/logged
  login value, and (round 3) to the command line too, immediately before
  `command::parse` — same predicate, same reason: `read_line_bounded_telnet`
  recognizes `CR NUL` as a line terminator (above) on both reads, and
  `command::parse`'s `str::trim` does not strip the trailing NUL, so an
  unrimmed `"sh/dx\r\0"` tokenized to `["SH", "DX", "\0"]` and matched no
  command arm — an interactive client could log in but every command it
  sent was silently ignored (round-3 validation, code-review finding 1).
  `command.rs` is outside this ticket's change scope, so the trim happens
  at the `telnet.rs` call site rather than inside `command::parse`.
