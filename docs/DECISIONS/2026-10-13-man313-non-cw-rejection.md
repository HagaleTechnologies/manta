# MAN-313: no CW track on speech, data or a swept carrier

## Decision

A newly promoted track is decoded only once it qualifies as keyed CW
(SPEC-decode-core.md §2.7). Before this change the detector promoted a CW
track on anything 12 dB above the floor for 19 hops (about 50 ms), leased a
decoder at once and emitted `TrackPromoted`. SSB voice produced 56–166
garbage tracks in 40 s, and a swept carrier up to 213.

With `[detector] non_cw_filter = true` (the default):

- **Lazy lease.** The lifecycle is unchanged. A promoted track queues its
  decoder input and emits nothing until it qualifies. It then leases a
  decoder, the queue is replayed, and `TrackPromoted` carries the original
  promotion `sample_ts` and `freq_hz`. Decoded output is unchanged; the
  event arrives 0.5–1.5 s of stream later.
- **Qualification** needs a keying cycle, frequency stationarity (rejects
  sweeps), a non-flat neighbourhood (rejects noise-like data), and a
  channel outside any co-modulation span.
- **Co-modulation classifier** (`manta-dsp::comod`). Speech raises many
  channels' power together. A wide span of co-modulated, comparably strong
  channels vetoes spawns and qualification for 5 s.
- **`NotCw` close.** A track not qualified within `unkeyed_close_ms`
  (10 s) closes silently.
- **`Unkeyed` close.** A qualified track key-down for `unkeyed_close_ms`
  with no re-rise, no decoded character and no callsign read closes as
  bookkeeping.
- **Surfaces.** `[detector] non_cw_filter` and `unkeyed_close_ms`
  (≥ 1000 ms); a doctor `NOT_CW` verdict with a `detector_confirmations`
  count, so a band of speech or sweeps no longer reads `NO_SIGNAL`; and
  `decode` reports `no signal found (no keyed CW signal in the input)`
  for a non-empty input with no keyed CW.

`non_cw_filter = false` builds no classifier and reproduces the previous
behaviour exactly, as MAN-314's `rtty_filter = false` does.

`manta_active_tracks` and `/healthz` `active_tracks` now count keyed CW
tracks being decoded, not every promoted track. MAN-324's planned
`activeTracks` reads the same `decoding_track_count`, so it inherits this
meaning.

## Evidence

The 2026-10-13 MAN-313 plan prototyped the design end to end on `main` at
`cb4fff9` (scratch hooks, not committed). Harness: `decode_samples`
mirrored with `PipelineConfig::default()`, 96 kHz, noise seed 11.

### Baseline reproduction (`cb4fff9`)

| Scenario | Result |
|---|---|
| SSB voice at 14040 kHz, 40 s | +15/+30/+45/+60 dB: 56/122/148/166 `TrackPromoted`, all in band, 0 spots |
| 2.8 kHz band noise | +15 dB: 0; +30 dB: 2 promotions |
| Carrier swept 7000→7040 kHz | 2000 Hz/s: 212; 1000 Hz/s: 213; 400 Hz/s (60 s): 53; 100 Hz/s (60 s): 2 |
| CW `K5ARH` at 14043 kHz beside SSB at 14040 kHz | already passed: `K5ARH@14043005` spotted, 120 promotions in total |
| `VVV VVV VVV TEST` then steady carrier | keying ends 8.88 s, track closes at 38.57 s (`Silent`) |
| `CQ CQ DE K5ARH K5ARH K` then steady carrier | keying ends 12.84 s, track closes at 42.92 s |

The testkit's shipped SSB voice (`manta-testkit::noncw`, seed 1) gives 81
in-band promotions with the filter off at +30 dB. The prototype measured
122 with a different voice render. The CW-next-to-SSB scene gives 82
promotions with the filter off.

### Final prototype results

| Scenario | Result |
|---|---|
| SSB +15/+20/+25/+30/+45/+60 dB | 7/2/0/0/0/0 `TrackPromoted` |
| Band noise +30/+45/+60 dB | 0/0/0 |
| CW beside SSB +15/+30/+45/+60 dB | `K5ARH` spotted at 14043004.99/14043005.04/14043007.05/14043017.19 Hz; 1 `TrackPromoted` at +30/+45/+60 dB, 8 at +15 dB (7 inside the SSB) |
| `VVV VVV VVV TEST` then carrier | `Unkeyed` close at 18.944 s, 10.06 s after the carrier began |
| `CQ CQ DE K5ARH K5ARH K` then carrier | callsign read; `Silent` close at 42.92 s kept |
| Sweeps, 5 ms ramp, 150–4000 Hz/s × +10/+20/+40/+60 dB | 0 `TrackPromoted` in all 28 cases |
| Sweeps, abrupt start, same rates | +10/+20 dB: 0. +40 dB: 2 at 1000 Hz/s. +60 dB: 1/9/18 at 400/600/1000 Hz/s. No spots, at most 15 characters |
| All 20 golden vectors | same `TrackPromoted` count, characters, spot callsign@Hz and text hash as filter off; 0 `not_cw` closes |
| Loud CW, 20/35 WPM × 45/55/65 dB × 5/1 ms rise | primary-track text and spots identical to filter off, 1 `TrackPromoted` each (filter off spawned 3–46 skirt tracks above +45 dB) |

Qualification delay on the golden vectors: 0.53 s for most, 1.53 s at most
(VR5).

### Ablation (final rules minus one component)

| Removed | Effect |
|---|---|
| Per-track taint | redundant (SSB +15 dB: 7 vs 6), so dropped |
| Span veto | SSB +15/+25/+30 dB: 11/2/1 tracks; one extra track beside SSB |
| Slope | sweeps at 2000/1000/400 Hz/s: 25/77/38 tracks |
| Isolation | band noise +60 dB: 29 tracks |
| Cycles | never-keyed carrier: 2 tracks |

### Cost

Prototype, single thread, scene rendering included (user seconds):

| Scene | Filter off | Filter on |
|---|---|---|
| V8, 120 s, 50 signals | 13.45 / 13.77 s | 14.49 / 15.73 s (about +0.008–0.016× realtime) |
| SSB +45 dB, 40 s | 3.43 s | 3.14 s (no garbage tracks to decode) |

Memory: the classifier ring is `N × 375` `f32`, 3 MB at N = 2048, the same
as MAN-314's FSK detector. An unqualified track queues one 32-byte demod
input per hop until it qualifies or times out: about 120 KB at the default
10 s. `unkeyed_close_ms` accepts up to 1 h, which would let one birdie hold
about 43 MB, so the queue is capped at `MAX_UNQUALIFIED_BACKLOG` (7500
hops, 20 s, about 240 KB), dropping the oldest entries. The default never
reaches the cap (implement-phase self-review finding).

Bench: the comod criterion bench is
`cargo bench -p manta-dsp --bench comod`. Re-measured with `--quick` on
the 32-core dev container at load average 25 (2026-10-13): scene (a), 10
co-modulated 30-channel blocks plus 300 keyed tones at n = 2048, 1.013 s
per 60 s (0.017× realtime, inside the plan's 0.02× target); scene (b),
every channel hot, 3.805 s per 60 s (0.063×, over the plan's 0.05×
target); noise only 0.402 s (0.0067×). An earlier run at load average
31–36 measured 3.50 / 12.12 / 1.05 s, so these are upper bounds on a
shared machine. Scene (b)'s time is the scalar f64 Pearson loop over
every hot pair plus the channel-major ring write every hop; a hop-major
ring or running sums with f32 centred rows in a reused buffer is a
follow-up, and a prerequisite for the Pi4 CPU-budget leg.

The Pi4 CPU-budget leg was **not run**: the hardware is not reachable from
this environment.

## The isolation rule that shipped: flat neighbourhood

The prototype counted a key-down hop as isolated when
`PdB[k] − max(PdB[k±2], PdB[k±3]) ≥ 6 dB`. That failed
`cw_starting_after_mark_near_rtty_still_decodes`: CW at +10 dB, 300 Hz
above a +30 dB RTTY mark, had an isolation fraction of 0.13–0.17 and never
qualified, though its cycles, slope, span and comod count all passed. The
same rule rejects any weak CW 200–300 Hz from a much louder signal.

The shipped rule counts a hop as non-flat when
`|PdB[k] − max(PdB[k±2], PdB[k±3])| ≥ 6 dB`. A neighbour at least 6 dB
louder is a different signal, not a flat noise-like block. Every hop that
passed the prototype's rule still passes, so everything that qualified
before still qualifies.

Its acceptance gates are the tests in `crates/manta-engine/tests/non_cw.rs`:
band noise at +30/+45/+60 dB starts no track in its band, and weak CW
300 Hz from a +30 dB CW, RTTY or carrier signal still decodes and spots.

## Rejected alternatives

- **Per-track taint** (close a track once the comod count at its channel
  reaches 4). Key clicks co-modulate channels 30–60 dB below a loud
  carrier, so loud CW (+50 dB with 1 ms edges, +55 dB with 5 ms, +70 dB)
  was tainted to 0 tracks.
- **Mean-level span strength.** With window means, the span's strong test
  landed on a loud carrier itself. Window peaks keep the click skirt out.
- **Isolation against any louder neighbour.** It killed weak CW beside
  RTTY (above).
- **Two cycles, or cycles matched in frequency.** More latency, and the
  same +60 dB sweep residual.
- **Holding decoder events instead of a lazy lease.** The decoder's output
  is a pure function of its queued inputs, so queueing input and leasing
  at qualification reproduces the decode without leasing decoders to
  garbage tracks or buffering their output.

## Known limits

- **SSB residual with the testkit voice model.** The production testkit
  voice (`manta_testkit::noncw::add_ssb_voice`, seed 1) is not the
  prototype's. At +30 dB it starts 81 in-band tracks with the filter off.
  With the plan's rules alone, 12 qualified: the co-modulation span formed
  late (no span at the first evaluations, because partner counts sit at
  2–3, short of `COMOD_COUNT` over an 8-channel run) and covered only the
  strong lower harmonics (k 429–442 of 429–458). Adding the
  `comodulated_with_span` qualification check (SPEC §2.7 D) cut it to 7:
  4 tracks promoted in the first 0.1 s that qualified before any span
  existed, and 3 later tracks next to the span that did not correlate
  with it. None is spotted. `non_cw.rs` keeps the strict zero-track test
  as `#[ignore]` and gates the delivered property instead (at least 80 %
  fewer in-band tracks, no spot). The plan's prototype measured 0 at
  +25 dB and above with its own voice; closing this gap needs the span
  constants re-measured against this voice model. Recommended follow-up.
  Diagnosis at +30 dB (implement phase, 2026-10-13): 6 of the 7 tracks
  that qualify have key-down frequency slopes of 38–70 Hz/s (the voice's
  F0 declination and vibrato on a harmonic), inside `MAX_SWEEP_HZ_PER_S`
  (75), which real CW also needs (V8w reaches 54 Hz/s); the seventh
  (−3.8 Hz/s) qualified at 2.3 s, before any span existed; and once the
  span forms it covers channels 431–442 of the voice's 429–458, with a
  co-modulation count of 0–6 at the qualifying tracks. Requiring a
  full 375-hop classifier window of track age before qualification cut 7
  to 6 only, so it was not adopted.
- **Weak SSB.** +15 dB: 7 tracks per 40 s; +20 dB: 2 (56 and more before).
  Only formant peaks are hot at those levels, so co-modulation counts stay
  low.
- **Abrupt switch-on sweeps.** Prototype: +40 dB: 2 tracks at 1000 Hz/s;
  +60 dB: 1/9/18 at 400/600/1000 Hz/s. This build
  (`sweeps_across_rates_and_levels_start_no_cw_track`, 22 s scenes): 0 at
  +40 dB at every rate; +60 dB: 1/1/16/2 at 400/600/2000/4000 Hz/s, 0
  elsewhere; 0 with a 5 ms ramp at every rate and level. Each qualifies
  just after the carrier's abrupt start or end step, a broadband click.
  No spots.
- **CW inside an SSB passband is not protected.** The span vetoes spawn
  and qualification there for its 5 s hold. CW 3 kHz away is unaffected.
- **Not measured:** off-air recordings, FT8, PSK31 and Olivia/MFSK. No
  generator or recording exists in the repo. A steady-envelope FT8-like
  tone never cycles, so by construction it closes `not_cw` after 10 s.
- **Pi4 CPU budget not run** (above).
- A qualified track is never closed because speech or wideband energy
  appears on it later; only the existing closes and `Unkeyed` apply.

## Follow-up

**Recommended:** remove the abrupt-switch-on click residual with a
broadband click gate, coordinated with MAN-312's crash detector, which owns
impulsive broadband energy. The ticket is left for a maintainer to file.
