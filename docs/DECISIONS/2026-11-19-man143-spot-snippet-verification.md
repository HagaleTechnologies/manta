# MAN-143: a spot carries no signal snippet today, and clip work stays parked

Date: 2026-11-19
Ticket: MAN-143 (2026-09-05 broad review, lens 4, #16)
Status: verified; no code change. The snippet feature stays parked.
Audience: maintainers.

## Context

MAN-143 asks that an operator who opts into local audio-snippet retention
gets, on each spot in the JSON stream, a short-lived local URL to the audio
or IQ the decode came from. The ticket's technical note limits it to an
opt-in, local-only HTTP endpoint. Nothing is uploaded.

The owner parked the ticket twice:

- 2026-10-13: "Parked in Backlog (owner-approved)." "An agent can verify
  this, but its value depends on RBN admin interest, and the owner's
  2026-10-11 decision was no RBN outreach yet." The same note lists clip
  retention, disk budget and privacy design as open. "Revisit when
  outreach starts."
- 2026-11-13: "Decision (Tony, 2026-11-13 decision packet): RBN outreach
  stays closed for now (verify-only, ka9q input first)." "Keep parked."

On 2026-11-19 the owner moved the card from Backlog to Todo. The ticket
has no later description edit and no decision that reopens outreach or
starts the feature. This record treats the move as permission for the
verification the 2026-10-13 note allows, and nothing more.

## D1. What a spot gives an operator today (observed)

All checks ran on a local checkout of `d555c72` with synthetic input and
loopback listeners only. No recording, relay or uplink was configured.

**V1 TCP schema smoke.** `manta gen v1 --out <scratch>` wrote `v1.wav`
(96 kS/s IQ, W1AW in CW at 14012.3 kHz). A scratch `manta.toml` set
`[input]` to `type = "file"`, `path = "v1.wav"`, `iq = true`,
`realtime = true`, `replay_epoch = 1700000000`, and `[server]` to
`station_callsign = "W3XYZ"`, `bind_addr` and `metrics_bind_addr` both
`127.0.0.1`, and port 0 for the telnet, JSON and metrics listeners.
`manta run --config <scratch>/manta.toml` ran with no `MANTA_*`
variables in its environment. A raw TCP client read the ports from the
`listening:` banner and connected to the JSON port before the fixture
played.

- The first record was the hello:
  `{"kind":"hello","schema":"stream.v1","session":"12534119743695398054","seq":0,"historyFrom":1,"resumed":false}`.
- The next was a CW spot for W1AW, `id`
  `W3XYZ:12534119743695398054:1:2006272:W1AW`, `timestamp` 1700000020,
  `frequency` 14012342.
- Its keys were exactly the 30 rows of `docs/SPOT-SCHEMA.md`'s Fields
  table, in the same order, `id` through `seq`. No key and no string
  value named a URL, snippet, clip or audio.

The track ID (`1`) and the sample timestamp (`2006272`) in the `id` are
the only pointers back to the signal. The schema calls `id` opaque.

**V1 metrics route smoke.** With the same daemon running, `GET /status`
on the metrics port returned `HTTP/1.1 200 OK` and a JSON body.
`GET /snippets/verification-probe` returned `HTTP/1.1 404 Not Found`.
The probe path is only a probe, not a proposed route. The source agrees:
`route` in `crates/manta-server/src/metrics_http.rs` matches `/metrics`,
`/healthz` and `/status` and answers 404 to everything else.

**WebSocket resume.** `cargo test -p manta-server --locked --test
json_stream_acceptance a_reconnecting_ws_client_receives_what_it_missed`
passed: a client that reconnects with `?session=&since=` gets the records
it missed, in `seq` order, then live ones. The test checks order, not
bytes. That a retained record keeps its original contents comes from the
schema ("Replay uses the original seq and record contents") and from the
bus, which keeps each spot's first rendering (`JsonItem::Spot` in
`crates/manta-server/src/bus.rs`). D2 relies on it.

The daemon exited 0 on SIGTERM. Nothing diverged from the documented
baseline.

## D2. What the source says a future clip has to carry (source findings)

These are read from the code, not observed at runtime.

- A `Spot` (`crates/manta-spot/src/validator.rs`, `pub struct Spot`)
  carries a track ID and one sample timestamp, `sample_ts`. It carries no
  word interval and no samples.
- The word's interval exists only in the MAN-207 observation draft
  (`observation_draft` in the same file): a `SampleSpan` from the word's
  first character to its word boundary
  (`crates/manta-spot/src/observation.rs`), with the input identity and
  the observation epoch. The emitted spot does not keep it.
- The live engine reads into one reused chunk buffer
  (`crates/manta-engine/src/listen.rs`, the read loop in
  `listen_with_observers`). Apart from MAN-309's opt-in session recording
  (D3), nothing keeps samples once the engine has consumed them.
- A stall or a discontinuity closes the segment's tracks
  (`take_stalled` and `finish_for_discontinuity` in the same loop) and
  emits their pending spots then, after the samples that produced them
  have passed.
- Multi-input sessions map each input's sample clock onto a common clock
  (`SourceClock` in `crates/manta-engine/src/multi.rs`). A common-clock
  timestamp alone does not name the input or its own sample index.
- With `[callsign_lookup]` on (MAN-318) a spot can publish about 10 s after
  its `timestamp`. WebSocket resume replays retained records, by default
  30 minutes of them, with their original rendering
  (`docs/SPOT-SCHEMA.md`, stream section). A URL rendered into a record
  would be replayed after it expired.
- File replay gives byte-identical spot logs and deterministic spot IDs
  (CLAUDE.md, "Deterministic decode"). An expiring URL or a wall-clock
  token in the record would break that unless file replay leaves it out.

A future clip therefore needs, per spot: the word's sample span, the input
it came from, and that input's clock, held across stalls and delayed
publication. The spot alone does not give an operator any of the three.

## D3. The workaround an operator has today

MAN-309 records a whole session to a raw IQ file and a metadata sidecar
when the operator opts in (`--record-iq`, `docs/GUIDES/iq-recording.md`).
It has no length cap and no rotation; the guide gives the disk cost
(1.38 GB per hour at 48 kS/s). The same guide shows how to replay the
recording with `--observations` (MAN-207) to see why a callsign was or was
not spotted. That replay is how an operator adjudicates a questionable
spot today: by re-running the decoder on the recorded signal. Each
observation record carries the input, the track, and the word's
`startSampleTs` and `endSampleTs`, which are `null` once the word has
left its track's 16-word window (`docs/OBSERVATIONS.md`). It is the step
the ticket wants to remove, and it works now.

## D4. Disposition

- The snippet feature stays parked, per the 2026-11-13 decision. No
  capture, retention, clip format, expiring URL, HTTP route or JSON field
  is added.
- Clip duration, expiry, byte or disk budget, and privacy stay product
  decisions for an explicit reopening. This record chooses none of them.
- Reopening needs an owner decision that starts RBN outreach or otherwise
  asks for the feature. Any launch keeps the ticket's opt-in, local-only
  constraint. The metrics listener's bind is operator-widenable
  (`docs/RUNBOOKS/network-exposure.md`), so a clip route there would not be
  local-only by construction.
- RBN admin demand was not validated here. That waits on the owner's
  outreach decision, after the ka9q input work.
