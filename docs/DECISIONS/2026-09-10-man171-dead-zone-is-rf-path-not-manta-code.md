# MAN-171's "dead zone" traces to the RF/antenna path on the test rig, not manta's channelizer/detector/track-manager; the startup-transient comb is a real, absolute-frequency-locked hardware artifact, and it now has a real fix

Investigates issue #171 (`CONFIRMED manta-side: synchronized RBN test proves
real detection gap, not propagation/clipping/tuning`) and
`2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md`.
Per `superpowers:systematic-debugging`: before touching channelizer/
detector code, this session built a repro. It found one -- but the repro
points at a different root cause than the issue's own title claims.

This doc went through two rounds of hostile review from Codex on PR #174.
Round 1 found real methodological gaps (a dB-averaging bug that could have
hidden the very signal this was trying to find; a fading-scaling gap; weak
test assertions) -- all checked and fixed below, and the corrected
measurements confirm the original conclusion held. Round 1 also correctly
identified that "Result 3" (below) was a testing artifact, not a real bug
-- retracted here, and the fix for Result 2 that this artifact had blocked
is now implemented, verified against production's real code path, and
shipped.

## Method

This dev environment has a real RSP1B attached (`SoapySDRUtil --find`
confirms it). Rather than continue guessing with synthetic AWGN test
vectors (four hypotheses tried and ruled out below), this session did the
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
  entirely) and reports per-channel power statistics -- to separate "the
  channelizer isn't seeing the signal" from "the channelizer sees it but
  the detector logic doesn't act on it." **Reports mean of LINEAR power,
  plus max and 99th-percentile per-hop power** (Codex review, PR #174
  round 1, P1, correct: an earlier version averaged per-hop dB values,
  which computes a *geometric* mean of linear power -- systematically
  understating any signal not present 100% of the time, exactly the kind
  of bug that could produce a false "nothing here" on real keyed CW. Fixed
  and re-verified against the same capture below; the conclusion held.)

## Result 1: real, RBN-confirmed signals are not detectably present in the raw channelizer power -- this is upstream of manta's DSP

RBN, live, for the exact capture window:

- **VE7CA @ 7007.50 kHz**, up to 42 dB, ~30 distinct skimmers.
- **LZ1NP @ 7011.00 kHz**, up to 61 dB (!), ~35 distinct skimmers.
- **VA7VB @ 7022.50 kHz**, up to 13 dB, several skimmers.

`manta decode --json` on the synchronized capture: **zero validated
spots**, matching the original finding. But this session went one step
further and looked at the *raw channelizer power*, independent of any
floor/gate/track logic, using the corrected linear-power statistics:

| target | channel | mean (linear) | p99 | max | nearest ambient (mean / p99 / max) |
|---|---|---|---|---|---|
| VE7CA (7007.50) | k=1904 | -50.72 dB | -43.81 dB | -41.05 dB | -51.9 / -45.3 / -41.4 dB |
| LZ1NP (7011.00) | k=1941 | -52.30 dB | -45.69 dB | -42.18 dB | -52.2 / -45.6 / -42.0 dB |
| VA7VB (7022.50) | k=16 | -51.69 dB | -44.99 dB | -41.97 dB | -51.7 / -45.0 / -42.0 dB |

Even at **max** -- the statistic least affected by duty cycle, since it
reflects only the single strongest hop across all 90 s -- each target sits
within ~1 dB of its own immediate neighbors, sometimes below a neighbor's
own max. A real signal present even briefly at anything near RBN's
reported peaks (up to 61 dB) would show a large, unambiguous max-power
spike regardless of duty cycle; there isn't one. **The corrected,
duty-cycle-robust statistics confirm the original finding**: nothing
resembling the RBN-reported signals reaches the channelizer at any point
in the 90 s. The channelizer itself is proven correct elsewhere in this
repo (unit tests, and this session's own multi-signal tests below) -- it
faithfully reports whatever is in the samples. If it reports no signal, no
signal reached it at a meaningful level. **This is not a detector/
track-manager question; it's upstream of the channelizer, in the RF/
antenna signal path.**

**A real caveat (Codex review, PR #174 round 2, P1, correct and addressed
below, not dismissed): RBN's reported SNR is what the *remote skimmers*
heard, not what this RSP1B would hear even with a perfectly healthy
antenna.** A transmitter can read 61 dB at a skimmer on a different
continent while sitting in a genuine propagation null locally -- the
RBN-vs-local comparison alone cannot distinguish "this receiver is broken"
from "this receiver's path to these three specific transmitters happens to
be weak tonight." That's exactly why this session didn't stop at the RBN
comparison: the **WWV check below is the actual local reference** this
conclusion rests on, not the RBN comparison by itself -- WWV's absence is
what turns "the local path to these three DX stations might just be
weak" into "there's very likely no antenna feeding this receiver at all."

A same-session sanity check: 8 s at 10.000 MHz (WWV, one of the strongest,
most reliable HF signals receivable almost anywhere in North America on
nearly any antenna, not a distant DX contact subject to a directional
propagation null) shows **no distinguishable carrier or AM-sideband
structure** -- the one elevated bin at exactly 10.000 MHz is a single,
isolated channel (immediate neighbors back to ambient within 1 channel),
the same narrow-single-bin signature as the comb artifact below, not the
broader structure real AM/CW modulation sidebands would leave. The most
likely explanation: **no functioning antenna is actually connected to this
RSP1B** (disconnected, wrong port, or a dummy load) -- not a manta code
defect. This directly corroborates the not-yet-merged
`docs/man166-167-20m-detection-gap-and-gain-scale` branch's own conclusion
("most plausibly the physical antenna/feedline path specifically feeding
the RSP1B... not resolvable from software alone"), now with a concrete,
repeatable, hardware-verified confirmation that goes further: **the
detector/DSP side is ruled out**, not just left open.

**Recommendation:** physically check the antenna connection on this test
rig before any further live-hardware software investigation. Issue #171's
title ("CONFIRMED manta-side") should be revisited -- this session's
evidence points the other way.

### Hypotheses ruled out first (for the record)

Before the live capture, this session tried to reproduce the dead zone
synthetically against `manta_engine::decode_samples` (`crates/manta-engine/
tests/man171_repro.rs`), matching JN1THL/VK2GR's exact offsets and SNRs.
All four still promote fine -- none reproduced the symptom, which is why
this session moved to a real hardware capture rather than trying a fifth
synthetic guess:

1. Plain two-tone + AWGN at the exact real offsets/SNRs.
2. A heavy simulated ADC/USB DC bias -- the channelizer isolates it
   cleanly to its own channel (no leakage), as its own unit tests predict.
3. A busy-band scene. Strengthened after Codex review (PR #174 round 3,
   P2, correct): the original version spaced background signals every
   3.7 kHz, wider than `FloorBank`'s 32-channel/3 kHz block, so no two
   carriers ever actually shared a floor block and the crowding-induced
   block-median-poisoning hypothesis was never really exercised. Fixed by
   packing extra signals *inside* each target's own 3 kHz block (76 total
   signals) -- still promotes fine.
4. Realistic HF fading (Watterson "Poor" 2-path/1 Hz Doppler on both
   targets). Re-scoped after Codex review (PR #174 round 1, P1, correct):
   `SignalSpec` applies the requested SNR as a *pre-fade, ensemble-nominal*
   amplitude (coppa's Watterson model is ensemble-normalized, `E|g|^2 =
   1`), so this specific 30 s realization's actual peak envelope SNR isn't
   controlled to match RBN's reported peak -- the original framing
   overclaimed a peak-matched reproduction it didn't actually do. Kept as
   weaker supporting evidence (both signals still promote at a *nominal*
   38/20 dB under fading) with the claim corrected to match what the test
   actually shows, not removed.

## Result 2: the ~8 kHz-spaced "startup transient" comb is real, reproduced live, is a genuine RF/hardware artifact locked to absolute frequency -- and now has a shipped fix

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

**Mechanism:** a persistent, unmodulated interferer keeps `Gate::update`'s
`rise[k]` true forever -- SPEC §2.2's neighborhood-floor clamp
(`effective_floor_db = min(own_floor, block_floor + 3 dB)`) exists
specifically so a channel can never "learn" to ignore its own parked
carrier, by design. Every comb tooth spawns a CANDIDATE, promotes, decodes
nothing (it's not Morse), runs the full `gc_hops` (30 s) window with zero
characters, closes `CloseReason::Silent` -- and, since `rise[k]` never
cleared, immediately re-spawns on the very next hop and repeats, for the
life of the process. `TrackManager::step_hop`'s `past_warmup` gate is a
single, global, fixed-hop-count switch -- every channel that's already
been rising throughout the 2 s warmup window becomes spawn-eligible in the
exact same hop, which is why the *first* burst looks synchronized (the
original doc's finding); it recurs continuously after that, not just once
at the `warmup_hops` boundary (not analyzed in the original doc, which
only looked at the first 15 events).

**Fix:** `DetectorConfig::silent_respawn_cooldown_hops` (new field,
default 11250 hops = `gc_hops`, same order as the window that earned it) --
a channel whose track just closed `CloseReason::Silent` may not spawn a
new CANDIDATE until that many hops later. This roughly halves a persistent
artifact's promotion rate (spawn -> 30 s ACTIVE -> Silent -> 30 s cooldown
-> repeat) rather than eliminating it entirely -- a rate limit, not a
permanent denylist, since a channel could in principle host a real signal
again later. Scoped to `CloseReason::Silent` only: `Unconfirmed` never
allocated a decoder at all, and `HangExpired`/`Merged`/`Evicted` all have
other, more direct explanations (a real observed RF gap, or track-manager
bookkeeping) that don't imply "this channel is a standing artifact."
Regression test: `track::tests::silent_close_starts_a_respawn_cooldown_on_
that_channel` (drives a stationary, never-decoding channel through a full
promote -> Silent-close -> immediate-respawn-attempt -> cooldown-elapses
-> respawn cycle directly via `TrackManager::step_hop`; verified to fail
without the fix -- 2 promotions where only 1 is expected before the
cooldown window elapses).

## Result 3 (retracted): the "90 s to first decode" churn was a testing artifact, not a real bug

An earlier version of this doc claimed a distinct, production-affecting
bug: that even a clean, always-on 20 dB synthetic CW signal (V1) could take
~90 s and 3 spurious promote/`Silent`-close cycles before decoding its
first character, based on the existing `process_hops_orders_track_
promoted_before_same_batch_decoder_updates` test rendering the full 120 s
V1 vector as **one single, unchunked `process_hops` call**.

**Codex review (PR #174 round 1, P2) correctly identified this as a
batching artifact of that test's own construction, not a production
behavior**, and traced it to the exact right mechanism: `TrackManager`'s
`Lifecycle::note_char_decoded()` (the GC/silent-timer reset) only ever
runs *after* `drain_pool()`, and a single-call batch defers `drain_pool()`
to the very end. That means **every** track promoted inside that one
unchunked call is structurally doomed to accumulate `silent_count` with
`char_emitted` always `false` and close `CloseReason::Silent` at exactly
`gc_hops` (30 s) -- regardless of whether the decoder would have "really"
decoded characters if fed in real time. V1's real signal cycled through 4
promote/Silent-close births (2.06 s, 32.16 s, 62.29 s, 92.34 s) purely
because the test happened to hand the whole 120 s to one call; this has
nothing to do with real decode convergence.

**Confirmed directly**: `manta-cli/tests/golden_v1.rs`'s
`v1_passes_end_to_end_from_wav` decodes the exact same V1 vector through
production's real code path (`manta decode` CLI -> `decode_wav` ->
`decode_samples`'s real 4096-sample chunking) and asserts -- and passes --
that **every event carries `track_id: 1`** across the full 120 s, with
`CER = 0.0155` (consistent only with losing the ~2.05 s warmup-inhibited
leading "CQ ", not 90 seconds of churn). Re-run locally this session:
passes. There is no production robustness gap here -- retracting the
"Result 3" framing entirely.

The offending test itself has been fixed to stop being a source of
artificial churn: it now renders a short (10 s, well under `gc_hops`) V1-
equivalent scene instead of the full 120 s vector, still spanning plenty
of hops past warmup (2 s) + confirm (~50 ms) for a real promotion and its
own real decoder output to land in the one unchunked batch its actual
point (`TrackPromoted` must sort before same-batch decoder-output events)
requires -- without ever needing a track to survive a full `gc_hops`
unassisted. Passes both before and after the Result 2 fix above; no
longer a barrier to it.

**Lesson**: verify a "production robustness gap" claim against the actual
production code path (chunked `decode_samples`/`decode_wav`) before
promoting a finding from an existing unit test's own incidental batching
choice -- especially when, as here, `decode_samples`'s own code comment
already explains exactly why it chunks (`crates/manta-engine/src/lib.rs`'s
`CHUNK_SAMPLES` comment references this exact class of bug and how
chunking avoids it). This doc originally missed that comment; a second
pair of eyes caught it.

## Next steps

1. **Physically check the antenna connection on this RSP1B test rig**
   before any further live-hardware software investigation -- this is now
   the most direct, concrete lead from this session.
2. Re-run this exact synchronized-capture method once a working antenna
   is confirmed, to see whether the dead zone persists (if manta's own
   detector genuinely still can't see a properly-fed strong signal, *that*
   would be the real manta-side reproduction to chase).
3. `crates/manta-input/examples/iq_probe.rs` and `crates/manta-engine/
   examples/man171_power_map.rs` (this session's scratch tools) are kept
   as reusable diagnostics for the next live-hardware session: capture via
   manta's own `SoapySdrIqSource` straight to `WavIqSource`'s expected
   format, and inspect raw channelizer power (mean/p99/max, not a naive dB
   average) independent of the detector.
4. If real fading-robustness work touches this area later (MAN-107..113),
   a genuinely peak-matched synthetic fading reproduction (pre-measure the
   faded envelope's realized peak, rescale to match a target peak exactly)
   would be a more rigorous tool than hypothesis 4 above -- not built here.
