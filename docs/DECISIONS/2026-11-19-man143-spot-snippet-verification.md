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
the closest pointers back to the signal; `timestamp` and `frequency`
locate it only coarsely. The schema calls `id` opaque.

**V1 metrics route smoke.** With the same daemon running, `GET /status`
on the metrics port returned `HTTP/1.1 200 OK` and a JSON body.
`GET /snippets/verification-probe` returned `HTTP/1.1 404 Not Found`.
The probe path is only a probe, not a proposed route. The source agrees:
`route` in `crates/manta-server/src/metrics_http.rs` matches `/metrics`,
`/healthz` and `/status` and answers 404 to everything else. Its unit
test `route_serves_status_json_metrics_text_and_404s_everything_else`
asserts `/status` and `/metrics` and the 404s; `/healthz` is covered by
its own tests in the same file.

**WebSocket resume.** `cargo test -p manta-server --locked --test
json_stream_acceptance a_reconnecting_ws_client_receives_what_it_missed`
passed: a client that reconnects with `?session=&since=` gets the records
it missed, in `seq` order, then live ones.

The daemon exited 0 on SIGTERM. Nothing diverged from what was compared:
the hello's six keys and the spot's 30 keys, both in the order
`docs/SPOT-SCHEMA.md` gives, and the three metrics routes
`docs/RUNBOOKS/network-exposure.md` names.

## D2. What the source says a future clip has to carry (source findings)

These are read from the code, not observed at runtime.

- A `Spot` (`crates/manta-spot/src/validator.rs`, `pub struct Spot`)
  carries a track ID and one sample timestamp, `sample_ts`. It carries no
  word interval and no samples.
- The word's interval is held in the live validator, not only in the
  MAN-207 draft: each word keeps its `start_ts` and `end_ts` (the word's
  sample span) while it sits in its track's window of 16 recent words
  (`WORD_WINDOW`), and the spot path works out the input with
  `input_of(track_id)` just before it builds the `Spot`. The emitted
  spot keeps neither the span nor the input. The MAN-207 observation
  draft (`observation_draft` in the same file) is the only output that
  carries the span out of the validator, as a `SampleSpan` from the
  word's first character
  to its word boundary (`crates/manta-spot/src/observation.rs`), with
  the input identity and the observation epoch; it runs only under
  `manta decode --observations`, never `manta run`.
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
- With `[callsign_lookup]` on (MAN-318) a spot can publish up to about
  10 s after its `timestamp`. WebSocket resume replays retained records,
  by default 30 minutes of them. The schema says "Replay uses the original
  seq and record contents" (`docs/SPOT-SCHEMA.md`, stream section), and
  the bus keeps each spot's first rendering (`JsonItem::Spot` in
  `crates/manta-server/src/bus.rs`). The resume test in D1 checks order,
  not bytes. On that reading, a URL rendered into a record would be
  replayed after it expired.
- File replay must give byte-identical spot logs (CLAUDE.md,
  "Deterministic decode"), and its spot IDs are deterministic because the
  session nonce is a hash of the file (`docs/SPOT-SCHEMA.md`, the `id`
  row). An expiring URL or a wall-clock token in the record would break
  that unless file replay leaves it out.

A future clip therefore needs, per spot: the word's sample span, the input
it came from, and that input's clock, held across stalls and delayed
publication. The spot carries none of the three: `timestamp` and
`frequency` locate the signal only coarsely, and only a single-input
session's `sample_ts` is already on that input's own clock
(`SourceClock::IDENTITY`).

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
the ticket wants to remove, and it works now, within two limits:

- It is a fresh decode of the recording, not the live decode that made
  the spot.
- A recording replayed on its own puts the spans on that recording's
  sample clock. A span indexes the file directly until a `gap` record
  with `missing_samples` above 0 precedes it. After such a gap, the span
  sits ahead of its file index by the sum of the `missing_samples` of all
  earlier gaps. A zero-length gap, which a stalled stream leaves when it
  resumes with no reconnect (MAN-201), starts a new epoch but adds
  nothing to that sum. The file stores only the samples received, while
  replay timestamps continue past the missing ones (the same guide). A multi-input session records
  one file per `[[input]]` that sets `record_iq`; recording is opt-in
  per input.
  Replaying several files together starts every file at the first
  input's start time, not at each receiver's original start (MAN-13,
  the same guide). `--observations` takes one recording, so no current
  command writes observation spans from a joint replay; if one ever
  does, its spans need D2's clock mapping before they index a file.

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
