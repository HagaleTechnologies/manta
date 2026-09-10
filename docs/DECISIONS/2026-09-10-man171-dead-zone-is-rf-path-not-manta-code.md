# MAN-171's "dead zone" traces to the RF/antenna path on the test rig, not manta's channelizer/detector/track-manager; the startup-transient comb is a real, absolute-frequency-locked hardware artifact

Investigates issue #171 (`CONFIRMED manta-side: synchronized RBN test proves
real detection gap, not propagation/clipping/tuning`) and
`2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md`.
Per `superpowers:systematic-debugging`: before touching channelizer/
detector code, this session built a repro. It found one -- but the repro
points at a different root cause than the issue's own title claims, and a
first attempt at fixing the secondary finding turned out to be unsafe and
was reverted. Both are reported here in full rather than forcing a fix
that doesn't match the evidence.

## Method

This dev environment has a real RSP1B attached (`SoapySDRUtil --find`
confirms it). Rather than continue guessing with synthetic AWGN test
vectors (three hypotheses tried and ruled out below), this session did the
exact thing the issue recommended: a fresh, synchronized live capture,
cross-checked against real-time RBN telnet spots, using manta's own
`SoapySdrIqSource` this time (not an external ad-hoc probe) via a new
scratch tool, `crates/manta-input/examples/iq_probe.rs`.

- 90 s capture, 40 m (7021 kHz center, 192 kHz passband, `--soapy-gain
  15`), 2026-09-10 04:12:01-04:13:31 UTC.
- Simultaneous RBN telnet capture (`telnet.reversebeacon.net:7000`) for
  the same window.
- Decoded via `manta decode --json` (`decode_wav` -> `decode_samples`,
  the same path `manta run` uses) -- exercising the real channelizer/
  floor/gate/track-manager end to end, not a synthetic scene.
- A second scratch tool, `crates/manta-engine/examples/man171_power_map.rs`,
  runs *only* the channelizer (bypassing floor/gate/track-manager
  entirely) and dumps average per-channel power -- to separate "the
  channelizer isn't seeing the signal" from "the channelizer sees it but
  the detector logic doesn't act on it."

## Result 1: real, RBN-confirmed signals are not detectably present in the raw channelizer power -- this is upstream of manta's DSP

RBN, live, for the exact capture window:

- **VE7CA @ 7007.50 kHz**, up to 42 dB, ~30 distinct skimmers.
- **LZ1NP @ 7011.00 kHz**, up to 61 dB (!), ~35 distinct skimmers.
- **VA7VB @ 7022.50 kHz**, up to 13 dB, several skimmers.

`manta decode --json` on the synchronized capture: **zero validated
spots**, matching the original finding. But this session went one step
further and looked at the *raw channelizer power*, independent of any
floor/gate/track logic:

| target | channel | avg power (whole 90 s) | nearest ambient channel |
|---|---|---|---|
| VE7CA (7007.50) | k=1904 | -53.35 dB | -54.3 to -54.8 dB |
| LZ1NP (7011.00) | k=1941 | -54.86 dB | -54.3 to -54.8 dB |
| VA7VB (7022.50) | k=16 | -54.21 dB | -54.3 to -54.8 dB |

A signal RBN puts at up to 61 dB is, in the raw channelizer output,
**indistinguishable from the ambient noise floor** (≤1.5 dB above
neighboring quiet channels, no visible peak at all). The channelizer
itself is proven correct elsewhere in this repo (unit tests, and this
session's own multi-signal tests below) -- it faithfully reports whatever
is in the samples. If it reports no signal, no signal reached it at a
meaningful level. **This is not a detector/track-manager question; it's
upstream of the channelizer, in the RF/antenna signal path.**

A same-session sanity check confirms this further: 8 s at 10.000 MHz
(WWV, one of the strongest, most reliable HF signals receivable almost
anywhere in North America on nearly any antenna) shows **no distinguishable
carrier or AM-sideband structure** -- the one elevated bin at exactly
10.000 MHz is a single, isolated channel (immediate neighbors back to
ambient within 1 channel), the same narrow-single-bin signature as the
comb artifact below, not the broader structure real AM/CW modulation
sidebands would leave. The most likely explanation: **no functioning
antenna is actually connected to this RSP1B** (disconnected, wrong port,
or a dummy load) -- not a manta code defect. This directly corroborates
the not-yet-merged `docs/man166-167-20m-detection-gap-and-gain-scale`
branch's own conclusion ("most plausibly the physical antenna/feedline
path specifically feeding the RSP1B... not resolvable from software
alone"), now with a concrete, repeatable, hardware-verified confirmation
that goes further: **the detector/DSP side is ruled out**, not just left
open.

**Recommendation:** physically check the antenna connection on this test
rig before any further live-hardware software investigation. Issue #171's
title ("CONFIRMED manta-side") should be revisited -- this session's
evidence points the other way.

### Hypotheses ruled out first (for the record)

Before the live capture, this session tried to reproduce the dead zone
synthetically against `manta_engine::decode_samples` (`crates/manta-engine/
tests/man171_repro.rs`), matching JN1THL/VK2GR's exact offsets and SNRs:

1. Plain two-tone + AWGN at the exact real offsets/SNRs -- both promote fine.
2. A heavy simulated ADC/USB DC bias -- both still promote fine; the
   channelizer isolates it cleanly to its own channel (no leakage), as its
   own unit tests predict.
3. A busy-band scene (49 concurrent signals across the passband) -- both
   still promote fine; no block/floor poisoning from crowding.
4. Realistic HF fading (Watterson "Poor" 2-path/1 Hz Doppler on both
   targets, peak SNR = the RBN-reported values) -- both still promote fine.

None of these reproduced the symptom, which is why this session moved to
a real hardware capture rather than trying a fifth synthetic guess.

## Result 2: the ~8 kHz-spaced "startup transient" comb is real, reproduced live, and is a genuine RF/hardware artifact locked to absolute frequency

The original doc's "spurious `TrackPromoted` events on a near-8 kHz grid"
finding reproduces exactly on this session's fresh 40 m capture -- 61 of
116 `TrackPromoted` events (53%) land within 100 Hz of an *exact* multiple
of 8000 Hz (6936000, 6944000, ..., 7112000 Hz), most grid points promoting
2-3 times across the 90 s.

Critically, **the same grid appears in the original 20 m session (center
14030 kHz) and this session's 40 m capture (center 7021 kHz)** -- e.g.
14024000.3 Hz and 7024000.0 Hz are both within measurement noise of exact
8000 Hz multiples, on two completely different bands and dial settings.
This proves the artifact is **locked to absolute RF frequency, not to the
tuned center/baseband offset** -- ruling out a manta-side channelizer/
rotation bug (which would move with the tuning) and pointing at something
in the RF chain itself: almost certainly an SDR/USB clock-harmonic comb
(a well-documented category of SDR RFI). The direct channelizer power-map
confirms each tooth is a genuine, persistent (constant across the full 90
s average), single-channel-wide spike, 15-20 dB above the ambient floor --
not spectral leakage (immediate neighbor channels sit right back at
ambient, matching `manta-dsp::proto`'s own -78 dB stopband).

**Mechanism for why this promotes as a "startup transient":** a
persistent, unmodulated interferer keeps `Gate::update`'s `rise[k]` true
forever -- SPEC §2.2's neighborhood-floor clamp (`effective_floor_db =
min(own_floor, block_floor + 3 dB)`) exists specifically so a channel
can never "learn" to ignore its own parked carrier, by design. Every
comb tooth: spawns a CANDIDATE, promotes, decodes nothing (it's not
Morse), and -- per the churn evidence in Result 3 below -- almost
certainly free-runs a persistent 30 s `Silent`-close/immediate-respawn
cycle for the entire session, not just once at the `warmup_hops`
boundary. `TrackManager::step_hop`'s `past_warmup` gate is a single,
global, fixed-hop-count switch (`crates/manta-engine/src/track.rs`
~line 758) -- every channel that's already been rising throughout the
2 s warmup window becomes spawn-eligible in the exact same hop, which is
why the *first* burst looks synchronized; it recurs continuously after
that (not analyzed in the original doc, which only looked at the first
15 events).

## Result 3 (new): a real, clean, synthetic 20 dB signal can spuriously promote and Silent-close up to 3 times, taking ~90 s to lock on -- unrelated to hardware, a distinct bug worth its own ticket

While investigating a fix for Result 2, this session discovered the
existing `process_hops_orders_track_promoted_before_same_batch_decoder_
updates` test (`crates/manta-engine/src/track.rs`) already exhibits
multi-cycle churn on **V1**, the plainest golden vector (continuous,
non-fading, 20 dB, "CQ CQ DE W1AW W1AW K" looped) -- no birdies, no real
hardware involved at all:

| track_id | promoted at | gap from previous |
|---|---|---|
| 1 | 2.06 s | -- |
| 9 | 32.16 s | 30.10 s |
| 10 | 62.29 s | 30.13 s |
| 11 | 92.34 s | 30.05 s (**first char decoded here**) |

Tracks 1, 9, and 10 each ran with **zero** decoder-pool events of any
kind (`has_emitted` never set -- not even a `TrackMeta`) before closing;
the ~30.1 s cadence matches `gc_hops` (30 s) almost exactly, strongly
implying all three closed `CloseReason::Silent` and immediately
respawned. Only the 4th attempt (track 11) actually locks on and starts
decoding real characters. This means: **even a textbook-clean, always-on
20 dB CW signal can take up to ~90 seconds and multiple full promote/
Silent-close cycles before manta produces its first character** -- a real
robustness gap, unrelated to hardware RFI, likely compounding with it on
a live, busy band. Root cause not yet investigated (candidate area:
channel/centroid selection or decoder envelope handoff immediately after
promotion, before centroid tracking has converged) -- **out of scope for
this session; a new ticket should track it separately** since it's a
distinct symptom from both MAN-171 results above.

## An attempted fix for Result 2 was reverted as unsafe

Initial fix: bar a channel from spawning a new CANDIDATE for
`silent_respawn_cooldown_hops` (30 s) after any `CloseReason::Silent`
closure, on the theory that a channel silent for a full GC window is
very likely a stationary artifact. Implemented, compiled clean, and
*appeared* to work -- until it was checked against the existing engine
test suite: it broke `process_hops_orders_track_promoted_before_same_
batch_decoder_updates` by suppressing track 9's respawn (the fix can't
distinguish "this channel is a permanent birdie" from "this is track
11's real signal on its 2nd of 4 required attempts to converge," because
Result 3 above means that distinction genuinely doesn't exist from a
single Silent closure's perspective). **Reverted rather than shipped** --
per `superpowers:verification-before-completion`, a fix that regresses a
passing golden-vector test is not a fix. A real fix needs Result 3
understood first (e.g. exponential backoff keyed off *consecutive*
Silent closures with zero decoder output on the same channel, distinguishing
"never once emitted anything, cycle after cycle" from "eventually
converges" -- but that needs its own investigation and test coverage, not
a rushed patch here).

## Next steps

1. **Physically check the antenna connection on this RSP1B test rig**
   before any further live-hardware software investigation -- this is now
   the most direct, concrete lead from this session.
2. Re-run this exact synchronized-capture method once a working antenna
   is confirmed, to see whether the dead zone persists (if manta's own
   detector genuinely still can't see a properly-fed strong signal, *that*
   would be the real manta-side reproduction to chase).
3. Open a new ticket for Result 3 (slow first-lock / repeated Silent-close
   churn on a real, decodable signal) -- distinct from both MAN-171
   findings, deserves its own investigation and fix.
4. If a Result-2 fix is attempted again, it needs: (a) understanding of
   Result 3 first, since they share the same closure/respawn machinery,
   and (b) the full engine test suite green, including
   `process_hops_orders_track_promoted_before_same_batch_decoder_updates`.
5. `crates/manta-input/examples/iq_probe.rs` and `crates/manta-engine/
   examples/man171_power_map.rs` (this session's scratch tools) are kept
   as reusable diagnostics for the next live-hardware session: capture via
   manta's own `SoapySdrIqSource` straight to `WavIqSource`'s expected
   format, and inspect raw channelizer power independent of the detector.
