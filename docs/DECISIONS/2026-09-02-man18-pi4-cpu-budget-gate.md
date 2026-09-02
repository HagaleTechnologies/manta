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
`crates/manta-engine/tests/cpu_budget.rs`).

First pass used wall-clock timing only, same as the test originally did:

```
cpu_budget: 5.73s wall / 15.00s audio = 0.382x realtime (Mac budget: < 0.5x)
```

Three runs across two checkouts ranged 0.372x–0.399x — consistent with the
0.360x originally recorded in
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` (different
M-series chip, same ballpark).

That pins doc's item 7 already flagged the real risk here: ROADMAP's
criterion is a **CPU-time** budget ("< 1 full core"), and wall-clock only
equals it when the pipeline is single-core-bound. A first check of this
via `/usr/bin/time -l` around the whole test *process* (compile-free
startup, scene rendering, decode, all lumped together) found whole-process
user CPU (9.13s) tracking wall clock (9.17s) closely, suggesting near-1.0x
parallelism. That measurement was too coarse to trust, though — the
non-decode work in that window (scene rendering, harness startup) dilutes
whatever parallelism the decode section itself has. Addressed by
instrumenting the test directly (per a Codex review finding on this PR):
`cpu_budget_mac_under_half_core` now calls `getrusage(RUSAGE_SELF)`
immediately before and after the `decode_samples` call only, isolating
its own `(user + sys)` CPU time from everything else in the test process.
That gives a materially different, more trustworthy answer:

```
cpu_budget: 300 unique tracks decoded (scene has 300 signals)
cpu_budget: 5.72s wall / 15.00s audio = 0.381x realtime wall-clock (Mac budget: < 0.5x)
cpu_budget: 7.08s (user+sys) CPU / 15.00s audio = 0.472x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)
```

Three runs of the instrumented test: wall-clock ratio 0.375x–0.388x (matches
the coarser measurement above), but **CPU-time ratio 0.457x–0.472x** —
roughly **1.2x-1.25x parallelism** inside `decode_samples` itself, not the
~1.0x the whole-process measurement implied. Two things follow:

1. **The Mac leg still passes, but by a much thinner margin than the
   wall-clock number alone suggests** — 0.457x-0.472x against a < 0.5x
   budget is ~6-9% headroom, not the ~24% headroom 0.38x wall-clock would
   imply. Worth watching if the workload or track count grows.
2. **The wall-clock-only test methodology *was* hiding some multi-core
   cost**, exactly the risk pins doc item 7 flagged as unconfirmed-but-
   plausible — now confirmed, at a modest (~1.2x) degree on this hardware.
   Whether Pi4's 4 weaker, differently-scheduled cores show a similar or
   different parallelism factor is unknown; the runbook now has the
   instrumented test so that's answered directly on real hardware instead
   of assumed.

## A cross-architecture estimate for the Pi4 leg (NOT a measurement)

To make this ticket's finding actionable rather than a bare "couldn't run
it," here is an order-of-magnitude estimate, clearly not a substitute for
runbook data. It uses the **CPU-time ratio** (0.457x-0.472x, mean ≈ 0.463x)
as the baseline, not the wall-clock ratio — CPU-seconds-of-work-per-
audio-second is the quantity that should scale with single-core throughput
roughly independent of how many cores execute it, whereas a wall-clock
ratio's scaling also depends on how much parallelism is available to
exploit (4 weaker Pi4 cores vs. many stronger Mac cores), which is a second
unknown this estimate has no data to pin down.

- Published Geekbench 6 single-core scores: Apple M4 Pro ≈ 3330 (average of
  246 MacBook Pro 14" 2024 submissions); Raspberry Pi 4 (BCM2711,
  Cortex-A72) ≈ 340 (raspberrypi.com's own Pi5-launch benchmarking post,
  comparing stock Pi4 against Pi5). Ratio ≈ **9.8x** single-core throughput
  in the Mac's favor.
- Applying that ratio to the measured CPU-time ratio (0.463x mean):
  **estimated Pi4 CPU-time ratio ≈ 4.5x** (core-seconds consumed per
  audio-second) — i.e. roughly 4.5 Pi4 cores' worth of continuous work to
  keep up with 1 audio-second, on a board that only has 4.
- Budget is < 1.0x. The estimate misses by a wide margin, not a narrow one
  — sensitivity-checked down to a per-core ratio as low as ~2.2x (well
  below every published Apple-Silicon-vs-Cortex-A72 comparison, including
  the older, weaker M1) still projects a Pi4 fail. Geekbench 6 is a mixed
  synthetic suite, not manta's actual DSP workload (channelizer FFTs,
  envelope detection, decoder-pool logic), so the true ratio could
  plausibly run higher (worse for Pi4) or somewhat lower, but there's no
  plausible reading of public cross-architecture data that closes a 4.5x
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
   alone if the real gap is anywhere near 4x.
2. **Lower the channelizer sample rate for Pi4 deployments** (96 kS/s
   instead of 192 kS/s — ROADMAP already lists 96/192 kS/s as the PFB
   channelizer's supported rates) — halves the passband and therefore the
   per-hop channelizer/detector work, at the cost of narrower simultaneous
   band coverage.
3. **Lower the track cap** (`PipelineConfig`'s track-count ceiling,
   independent of sub-segment filtering) — a blunter version of #1 that
   doesn't require MAN-30's scheduling/config surface, just a smaller
   `track_cap` default or a Pi4-tier config profile.
4. **Decoder-pool algorithmic work** — rayon parallelism measured modest
   (~1.2x-1.25x, not the "essentially inert" this session first thought
   before instrumenting `decode_samples` directly — see above) at 300
   tracks / 15s on this session's hardware, and there's no guarantee Pi4's
   4 weaker cores see the same or better speedup from it. If the real Pi4
   parallelism factor turns out lower than Mac's, that's *more* pressure on
   #1-#3, not less. Actual per-track decode cost reduction (profiling
   `manta-decode`'s hot path) is the fallback once those are exhausted.
5. **Accept a documented Pi4-tier feature reduction** (e.g. ship Pi4 as a
   lower-track-count, single-band-segment tier rather than matching the
   Mac's full-band 300-track claim) if none of the above close the gap —
   last resort, and a product decision, not an engineering one.

## Scope note

This ticket is measurement/benchmarking only. No `manta-engine` or
`manta-dsp` *production* source was touched, per MAN-18's own instruction
not to rework shared pipeline code from a benchmarking ticket. The one
code change in this PR is to the bench harness itself —
`crates/manta-engine/tests/cpu_budget.rs` (the `#[ignore]`d test this
whole runbook exists to run) — adding the `getrusage`-based CPU-time
measurement and the active-track-count assertion described above, both in
response to review findings on this PR. That's the harness MAN-18 was
asked to run and write up, not the pipeline it measures.
