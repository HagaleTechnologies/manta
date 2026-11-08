# MAN-544: a bounded bank of speed and timing hypotheses

Date: 2026-11-08. Ticket: MAN-544 (proposal §7 of
`docs/DECISIONS/2026-09-09-decoder-recall-research.md`). Baseline: main
`acaa9f7`. Status: spike landed, default off (`width: 1`).

## Context

The legacy decoder holds one `SpeedTracker` per track, a two-cluster
(dit/dah) online estimate of mark durations (`crates/manta-decode/src/timing.rs`).
When the sender's speed changes by more than about √3 in one step, every new
mark lands in one cluster and the tracker mis-locks: it recovers slowly by
EMA creep, or never. The same single estimate cannot hold both readings of
an ambiguous opening (all-dah text that looks like slower dits, a
mid-element fragment taken as the dit). MAN-398 and MAN-402 patched two of
those symptoms with one-shot rules; §7 asks for the general mechanism.

The ticket's own scenario (20 → 32 WPM, 40 ms gap unit) already passes on
main: the tracker follows a 1.6x step by EMA without a re-seed, losing one
character at the tight join. The committed `speed_step` guards
(`ticket_20_to_32_with_40ms_gap_unit_follows_the_step`,
`plain_20_to_32_follows_the_step`) pin that. Steps above √3 fail on main
(planning measurements, 25 dB, 60 s post-step window): 20 → 36 +5.1 pp,
15 → 30 +7.9 pp, 20 → 40 +17.7 to +53.2 pp, 20 → 50 +100 pp, 40 → 20
+6.7 pp, 32 → 16 +7.1 pp of CER over a steady control.

## Decision

Add `manta_decode::timing::TimingBank` (`timing/bank.rs`) behind
`DecodeConfig::speed_bank: SpeedBankConfig { width, switching }`, not a TOML
option, and keep it **off** (`width: 1`) because its spike gate did not pass
(below). The plan's D14 fallback applies: the bank code, its unit tests and
the measurement harness land; switching stays a follow-up.

What the bank does when enabled:

- **Shape.** The decoder's `SpeedTracker` stays the primary; the bank holds
  up to `width - 1` challenger trackers and the scores. `width: 1` returns
  from every method at its first line, so the default decode is main's.
- **Spawning, from evidence only.** At readiness, an ambiguous bootstrap
  spawns its other reading: a unimodal dit-assumed lock its dah-assumed twin,
  a dah-assumed (MAN-402-flipped) lock its dit-assumed twin, a bimodal split
  whose dit cluster is one mark (the MAN-398 fragment shape) a tracker built
  from the other marks. Later, `SPAWN_STREAK` live marks on one side of the
  primary's boundary spawn a tracker re-initialized from them (its unimodal
  3:1 alternative when it duplicates the primary). A candidate within
  `DUP_LOG_RATIO` of a live hypothesis on both centroids is not spawned.
- **Scoring.** Every hypothesis scores every mark against its two
  centroids and every closed gap against 1, 3 and 7 of its own units (a
  classified gap, or a word gap the flush resolved, at its closed length:
  Codex review on PR #544), a
  max-component log-normal fit with equal class weights and a per-observation
  floor, decayed by `DECAY` per observation. A gap of `PAUSE_DITS` units of
  the slowest hypothesis is scored by none.
- **Switching.** A challenger qualifies when it leads the primary by
  `SWITCH_LEAD_NATS` on `SWITCH_CONFIRM_MARKS` consecutive marks, is at least
  that many marks old, and its gap score is strictly ahead of the primary's
  (the gap veto, so a challenger with no scored gap since its spawn cannot
  switch: `corroborated_lock_is_not_resplit_by_a_bimodal_streak`'s fading
  streak is the case marks alone cannot settle). With `switching: false` the
  bank only records the would-switch (`SwitchRecord { applied: false }`).
  With switching on, the trackers swap, the old primary stays as a
  challenger exempt from pruning for `RETAIN_MARKS`, and `TrackDecoder`
  resets its gap classifier, flushed one-character-word gaps and sender
  timing explicitly (the new primary's `regime()` may equal the old one's).
- **Pruning.** A challenger trailing by `PRUNE_NATS`, duplicating the
  primary, or older than `MAX_AGE_MARKS` without switching is dropped.

Constants (all inside the plan's D8 envelope):

| Constant | Value |
|---|---|
| `SPAWN_STREAK` | 4 marks |
| `SIGMA_MARK` / `SIGMA_GAP` (log) | 0.25 / 0.35 |
| `FLOOR` | 3.0 nats |
| `DECAY` | 0.90 per observation |
| `SWITCH_LEAD_NATS` | 4.0 |
| `SWITCH_CONFIRM_MARKS` | 2 (start value 3) |
| `PAUSE_DITS` | 12 |
| `DUP_LOG_RATIO` | 0.25 |
| `MIN_AGE_MARKS` / `RETAIN_MARKS` | 8 / 24 |
| `PRUNE_NATS` / `MAX_AGE_MARKS` | 12.0 / 64 |

MAN-398's and MAN-402's rules and the SPEC §4.1 regime rule stay in
`SpeedTracker` unchanged: the bank is additive, and removing them needs its
own measurement.

## Results

**Spike gate** (`crates/manta-engine/tests/speed_bank_spike.rs`, ignored,
release): 2,360 streams, observe-only bank on `TrackDecoder`, 375 Hz
envelopes. Steady: 28 texts × 7 speeds (12–40 WPM) × 4 attach offsets ×
clean/noise/fade. Steps: the 8 acceptance rungs with ratio ≥ 1.75 or ≤ 0.55.

| Setting | False would-switches (steady) | Steps proposed within 8 marks |
|---|---|---|
| streak 4, confirm 3 (plan start) | 3 (`IS`, `T0M` ×2; all 12 WPM, ±0.05 noise) | 2 of 8 (others at 9–10; 32 → 16 none) |
| streak 3, confirm 2 | 3 (`T0M` ×2, `EEE`; all 12 WPM noise) | 7 of 8 (32 → 16 at 9) |
| streak 4, confirm 2 (kept) | 2 (`T0M` ×2; 12 WPM, ±0.05 noise) | 3 of 8 (others at 9; 32 → 16 none) |

The primary is scored on a mark after it has learned it (the decoder feeds
it first), challengers before: a deliberate bias toward the primary. Scoring
the primary's pre-mark centroids instead (self-review finding) measured 7
false would-switches (`EE`, `EE EE` at 20 WPM, `EIS 5` at 16 WPM, `IS`,
`EEE`, `T0M` ×2 at 12 WPM, all under noise) and 6 of 8 steps in time, so the
bias stays.

Streak 3 also broke the bank's own `half_speed_down_step` unit test: its
three-mark streak included the last pre-step dah and proposed a 72.75 ms dit
for a 60 ms sender. The kept setting passes every bank unit test.

Good would-switches (wrong primary, right target) appear on the fragment-lock
texts the MAN-398 grid flagged (kept setting): `HH` 21, `SS` 34, `5NN` 22,
`599 001` 24, `EE` 35, `IS` 57, `E 0000` 33 across their 84 steady streams
each, so the spawning and scoring find the right hypothesis. Scoring the
word gaps the flush resolves (Codex review on PR #544) raised `HH`, `SS`,
`EE` and `IS` from 20, 31, 32 and 53 and left the false proposals and the
step timings above unchanged. Both remaining
false proposals are `T0M` at 12 WPM under noise, from 100.6 ms to a 144 ms
dit; the likely cause (not traced) is a noise-split 300 ms dah, whose halves
score better under the slower challenger.

**Grid baseline** (`man398_grid`, now parallel, 32 workers, 3 min): 288
rows, mean CER14 0.0919, mean CER30 0.0423, 140 rows at CER30 0, 24 rows at
CER30 ≥ 0.25, identical to the research's sequential numbers. A second run
with `MAN398_GRID_BASELINE` set to the first reported 288 identical rows at
both durations and 0 worse. With the bank
off the decode is main's by construction, and
`observe_only_bank_leaves_events_identical` pins that observe-only mode
changes no event on five streams (two speeds of `CQ TEST W1AW`, `HH` from a
mid-element hop, `MO`, a 30 → 12 hops-per-dit step).

**Cost** (`cargo bench -p manta-decode --bench legacy_300_tracks`, one run
on a shared 32-core container, criterion median of 10 samples, 300 tracks ×
20 s each):

| Scene | `default` | `width1` | `width3_observe` | observe / width1 |
|---|---|---|---|---|
| steady | 52.99 ms | 50.83 ms | 52.64 ms | +3.6 % |
| steps | 48.42 ms | 50.00 ms | 54.16 ms | +8.3 % |
| noisy | 52.25 ms | 63.96 ms | 61.81 ms | −3.4 % |

`default` and `width1` are the same configuration, so their spread (up to
22 % on `noisy`) is this machine's noise; the observe-only bank, which pays
the full three-hypothesis scoring cost, sits inside it and under the
ticket's 25 % bound on every scene. These runs predate scoring the
flushed word gaps, which adds one gap score per word to the observe-only
bank. A quiet-machine run against a `--save-baseline main` is still owed.

Not measured in this round: the D12 step sweep (`speed_step_sweep`),
`manta bench sensitivity` with `engine-overlap.py`, and every
switching-mode gate (the plan's Phases 3–4).

## Alternatives

- The one-rule `q75/q25 ≥ 2.6` re-seed (research spike A): changed no grid
  row and cut ≥ 1.9x up-steps from full-text CER 0.76–0.88 to 0.05–0.14. It
  is the baseline a switching bank must beat.
- A fixed 48-hypothesis lattice (research spike B): unstable, jumped far
  more than legacy at steady speed. Evidence-spawned challengers replace it.
- The beam per hypothesis: about 29 % of decode cost by the research's
  measurement. The bank scores marks and gaps only; the beam runs on the
  primary.
- The HSMM engine rides 2x steps but has a higher steady floor and 3.9x the
  CPU (MAN-205).

## Known limits

- `TT`/`I` and `HH`/`TTTT`: with no dah, a mark scale and its 3x reading fit
  marks and element gaps equally; only word gaps separate them. The bank's
  fragment-lock unit test uses `5NN` for that reason.
- One character lost at a tight 1.6x join (the ticket's 20 → 32): the new
  character gap is under two old units before any mark disagrees.
- The remaining false proposals are `T0M` at 12 WPM under noise (likely
  noise-split dahs).
- No real-recording or Pi 4 measurement.

## Follow-ups

- Switching (plan Phase 3): the open-character re-segmentation on a switch,
  default `{ width: 3, switching: true }`, and the end-to-end step tests
  20 → 40, 20 → 50, 40 → 20; then the plan's Phase 4 gates (D12 step gate,
  grid with `MAN398_GRID_BASELINE`, overlap, the 25 % CPU bound).
- Before that, close the spike: a split-dah guard (a mark followed by a gap
  under a quarter of the shorter centroid scores as one) for the 12 WPM
  false proposals, and a two-mark confirmation path for steep down-steps
  (32 → 16).
- Whether MAN-398/402 are redundant once the bank switches.
