# MAN-18: Raspberry Pi 4 CPU-budget gate — status and estimate

MAN-18 asks to validate ROADMAP.md's M2 accept criterion ("< 1 full core on
a Raspberry Pi 4" at 192 kS/s / 300 active tracks) on real Pi4 hardware.
This doc records what this session could and couldn't do about it, and
should not be read as closing the gate — the gate stays open until someone
runs `docs/RUNBOOKS/m2-pi4-cpu-budget.md` on an actual Pi4.

## No Pi4-class hardware was reachable this session

Checked `~/.ssh/config` and `~/.ssh/known_hosts` across the fleet
(aldebaran, rigel, sophon, vega, pandora, plus the network-appliance hosts).
Result: no Raspberry Pi or other Linux/aarch64 SBC. The only aarch64 hosts
in reach are Apple Silicon Macs (sophon, vega — both M-series, same class
as aldebaran) — those are the *other*, already-passing leg of this gate,
not a substitute for it. Per MAN-18's own instructions, this is reported
explicitly rather than silently substituted.

No cloud ARM instance was provisioned either — that would cost money
(guardrail: ask before spending) and, more importantly, server-class ARM
(e.g. AWS Graviton, Neoverse cores) is not meaningfully closer to a Pi4's
Cortex-A72 than an Apple M-series chip is; it would not answer the actual
question.

## What was measured instead: a reconfirmed Mac baseline

Re-ran the existing gate test on this session's machine (aldebaran, Apple
M4 Pro, `cargo test --release`, per
`crates/manta-engine/tests/cpu_budget.rs`):

```
cpu_budget: 5.73s wall / 15.00s audio = 0.382x realtime (Mac budget: < 0.5x)
```

Three runs across two checkouts ranged 0.372x–0.399x — consistent with the
0.360x originally recorded in
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` (different
M-series chip, same ballpark). Mac leg still clearly passes.

Also re-checked that pins doc's item 7 concern (wall-clock ratio vs. true
CPU-time) empirically, via `/usr/bin/time -l`: whole-process user CPU
(9.13s) tracked wall clock (9.17s) closely with rayon's default thread
pool, and pinning `RAYON_NUM_THREADS=1` changed the decode-section ratio
only from 0.393x to 0.372x — noise-level, not a real speedup. Confirms this
workload is still essentially single-core-bound at 300 tracks on this
hardware, i.e. the existing wall-clock-only test methodology is not
currently hiding multi-core cost. Worth re-checking on Pi4 too (see
runbook step 4) since Pi4's 4 weaker cores could behave differently under
scheduling pressure than a Mac's many strong cores — no reason to assume
it does, but it hasn't been checked there.

## A cross-architecture estimate for the Pi4 leg (NOT a measurement)

To make this ticket's finding actionable rather than a bare "couldn't run
it," here is an order-of-magnitude estimate, clearly not a substitute for
runbook data:

- Published Geekbench 6 single-core scores: Apple M4 Pro ≈ 3330 (average of
  246 MacBook Pro 14" 2024 submissions); Raspberry Pi 4 (BCM2711,
  Cortex-A72) ≈ 340 (raspberrypi.com's own Pi5-launch benchmarking post,
  comparing stock Pi4 against Pi5). Ratio ≈ **9.8x** single-core throughput
  in the Mac's favor.
- Applying that ratio to the measured single-core-bound Mac ratio (0.372x–
  0.382x): **estimated Pi4 ratio ≈ 3.6x–3.8x realtime.**
- Budget is < 1.0x. The estimate misses by a wide margin, not a narrow one
  — sensitivity-checked down to a per-core ratio as low as ~2.6x (well
  below every published Apple-Silicon-vs-Cortex-A72 comparison, including
  the older, weaker M1) still projects a Pi4 fail. Geekbench 6 is a mixed
  synthetic suite, not manta's actual DSP workload (channelizer FFTs,
  envelope detection, decoder-pool logic), so the true ratio could
  plausibly run higher (worse for Pi4) or somewhat lower, but there's no
  plausible reading of public cross-architecture data that closes a 3.6x
  gap down to under 1.0x.

**Working conclusion: the Pi4 leg of the M2 CPU-budget gate is likely to
fail on real hardware, probably not marginally.** This is an estimate, held
with moderate-not-high confidence, and MAN-18 should stay open (not closed
as "gate passes") until `docs/RUNBOOKS/m2-pi4-cpu-budget.md` produces a
real number. But it's confident enough to act on for the one decision that
was actually time-sensitive here:

## MAN-30 recommendation: do not close it on this basis

MAN-30's own ticket text flags it as "a candidate to close outright rather
than build if [MAN-18's] Pi4 CPU-budget gate passes without it." This
session's estimate says the opposite is more likely — the gate likely
fails by a wide-enough margin that a single mitigation (sub-segment
restriction, MAN-30's scope) may not even be sufficient on its own.
**Recommend MAN-30 stays open and in the backlog** pending the real Pi4
number; closing it now on the unverified assumption that the gate passes
would be the ticket's own stated failure mode.

## If it does fail on real Pi4 hardware: what to try, roughly in order of leverage

1. **MAN-30 (band sub-segment restriction)** — cuts the number of
   simultaneously-tracked signals directly, which is the dominant cost
   driver (300 active tracks is the whole point of this bench scene).
   Likely necessary but per the estimate's margin, possibly not sufficient
   alone if the real gap is anywhere near 3x.
2. **Lower the channelizer sample rate for Pi4 deployments** (96 kS/s
   instead of 192 kS/s — ROADMAP already lists 96/192 kS/s as the PFB
   channelizer's supported rates) — halves the passband and therefore the
   per-hop channelizer/detector work, at the cost of narrower simultaneous
   band coverage.
3. **Lower the track cap** (`PipelineConfig`'s track-count ceiling,
   independent of sub-segment filtering) — a blunter version of #1 that
   doesn't require MAN-30's scheduling/config surface, just a smaller
   `track_cap` default or a Pi4-tier config profile.
4. **Decoder-pool algorithmic work** — the rayon parallelism measured
   essentially inert at 300 tracks / 15s on this session's hardware (see
   above); if that holds on Pi4 too, there's no "just use more cores"
   escape hatch, and actual per-track decode cost reduction (profiling
   `manta-decode`'s hot path) would be the fallback once #1–#3 are
   exhausted.
5. **Accept a documented Pi4-tier feature reduction** (e.g. ship Pi4 as a
   lower-track-count, single-band-segment tier rather than matching the
   Mac's full-band 300-track claim) if none of the above close the gap —
   last resort, and a product decision, not an engineering one.

## Scope note

This ticket is measurement/benchmarking only. No `manta-engine` or
`manta-dsp` source was touched — only this decision doc and
`docs/RUNBOOKS/m2-pi4-cpu-budget.md`, per MAN-18's own instruction not to
rework shared pipeline code from a benchmarking ticket.
