# MAN-515: Refinement replays only where the engine flip can change a word

Date: 2026-11-09. Ticket: MAN-515 (follow-up from MAN-208, PR #520).
Baseline: main `7020ede`. Status: landed; supersedes the hypothesis ladder of
[MAN-208](2026-11-05-man208-delayed-refinement.md) for legacy and edge-legacy
bases.

## Context

MAN-208's scheduler replays a withheld word's preserved hops on a fixed
two-rung ladder: an even attempt flips the engine (legacy or edge-legacy to
HSMM, HSMM to legacy), an odd attempt reruns the base engine with the speed
seeded. A later decoded word re-admitted an already refined episode for its
next rung. `Validator::apply_refinement` moved the occurrence's repetition
support to the refined reading before checking that the reading could ever be
spotted.

Measured on the 126 standing sensitivity recordings (10 stations keying
`CQ CQ DE <call> <call> K` for 60 s at 96 kS/s, seed 1; awgn, good and poor;
15, 25 and 35 WPM; trials 0 and 1; 500 Hz SNR 0 to 24 dB) on main, with the
default legacy engine:

- 142 admissions, 108 replays, 120,378 replayed hops, 0 rescues.
- The legacy-to-HSMM flip ran 73 times. It changed 40 words: 19 from a
  correct callsign to garbage, 21 from wrong to wrong, none from wrong to
  right. 33 flips read nothing.
- One real spot was lost against refinement off: `N3NXI` at
  `good/35wpm/t1/0dB`. The flip re-read it as `EEETT`, and the support move
  took the occurrence away from `N3NXI`.
- Seeded second attempts read the same word as the first in 54 of 57 cases.
  None had a new speed; 53 were re-admitted by a later word alone.

With an HSMM base (21 AWGN recordings) the HSMM-to-legacy flip changed 67
readings, 36 of them from wrong to right, for 7 rescues.

The 288-case MAN-398 grid is blind to refinement: at `c0127da` its rows were
byte-identical with refinement on and off, and none of its 576 decodes
admitted an episode.

## Decision

1. **The engine flip is gated on the base engine**
   (`manta_engine::refine::engine_flip_allowed`). Only an HSMM base flips, to
   legacy. A legacy or edge-legacy base replays the seeded timing hypothesis
   only. This is the measured signal the ticket asks for: the flip runs only in
   the direction where it has changed words for the better. No per-word signal
   could be calibrated, because the legacy direction has no positive class.
2. **A flip-less episode is re-admitted only on a new stable speed or a
   separation**, never on a later word alone, because an unchanged seed
   repeats itself deterministically. `max_refinements_per_episode` still caps
   the total. The HSMM-base ladder is unchanged, later-word re-admission
   included. The speed trigger stays a fresh-admission path: the only genuine
   legacy-base correction measured (`K3BGC` to `K3BGK`, `good/25wpm/t0/12dB`)
   was speed-admitted.
3. **A narrow guard in `Validator::apply_refinement_with_outcome`**: when the
   refined call differs, the old call is spottable (allowlisted, or
   grammar-plausible with an allocated prefix) and the refined call is not,
   the refinement is rejected. Support, the rival ledger and the word are left
   alone, and a `Refined` withheld decision records `Grammar` or `Cty`. A
   broad guard (refuse every unspottable reading) was measured and rejected:
   on the HSMM base it lost `AC3GYH` at `awgn/35wpm/t0/9dB`, where the flip
   re-read the garbled context word `NEE` as `DE` and so gave the next word its
   `DE <call>` context. `NEE` is not spottable, so the narrow guard still
   applies `NEE` to `DE`, and it still blocks `N3NXI` to `EEETT`.
4. **Outcome counters.** `RefineCounters` gains `replays_total`,
   `changed_total`, `rejected_total` and the internal `engine_flips_total`.
   `/metrics` exports the first three as `manta_refine_replays_total`,
   `manta_refine_changed_total` and `manta_refine_rejected_total`
   (`docs/METRICS.md`). The share of replays that change a word is
   `changed / replays`.

## Results

Pre-change (main, figures above, from the planning measurement): on the
legacy base 51 of 108 replays (47%) changed a word, and 19 of those changes
damaged a correct call. With the flip removed and one seeded attempt per
speed, the planning variant ran 73 replays, of which 12 (16%) changed a word,
none from correct to wrong; replayed hops fell from 120,378 to 83,697 (-30%)
and admissions from 142 to 100, with no spot lost in any of the 126
recordings.

Branch A/B against main `7020ede` on the same corpus, by `man515_refine_ab`
(`crates/manta-engine/tests/man515_refine_ab.rs`; main's rows are committed in
`crates/manta-engine/tests/data/man515_ab_main_legacy.tsv` and
`man515_ab_main_hsmm.tsv`). A row is worse when it loses a correct callsign or
gains a bogus one.

| Base engine | Recordings | Identical | Better | Worse | Correct calls, main → branch | Bogus calls, main → branch |
| --- | --- | --- | --- | --- | --- | --- |
| legacy (default) | 126 | 125 | 1 | 0 | 605 → 606 | 5 → 5 |
| HSMM | 21 (AWGN, trial 0) | 21 | 0 | 0 | 107 → 107 | 1 → 1 |

- The better legacy row is `good/35wpm/t1/0dB`: main spots nothing there,
  the branch spots `N3NXI` again.
- `MAN515_AB_BASELINE=crates/manta-engine/tests/data/man515_ab_main_legacy.tsv cargo test --release --locked -p manta-engine --test man515_refine_ab -- --ignored --nocapture`
  prints `AB-COMPARE identical 125 better 1 worse 0` and passes.
- The HSMM run covers the 21 AWGN trial-0 recordings, the same set the
  pre-change HSMM measurement used. Both full HSMM runs were OOM-killed in the
  container's 9 GiB cgroup after those rows (`memory.events` `oom_kill`), so
  the fading HSMM rows are unmeasured. The narrow guard did not lose a correct
  call on either base, so the D4 fallback (remove the guard) did not fire.
- The MAN-398 grid (288 cases, `MAN398_GRID_JOBS=4`) printed byte-identical
  `GRID` rows on main and on the branch: 288 identical at 14 s and 30 s, none
  better, none worse. The grid cannot see refinement, so this guards only the
  causal path.
- The A/B harness prints spots, not scheduler counters. The share of
  legacy-base replays that change a word is therefore the planning
  measurement of this design: 12 of 73 replays (16%), against 51 of 108 (47%)
  on main, with 19 of main's changes damaging a correct call and none of the
  branch design's.

The integration tests pin the mechanism on synthetic scenes
(`crates/manta-engine/tests/man515_refine_gate.rs`): a legacy or edge-legacy
base makes zero flips and is not re-admitted by a later word; a new stable
speed re-admits it once; an HSMM base still flips first and rescues; a garbage
reading of a spottable call is counted as rejected and the call's own later
repetition still spots it. In MAN-208's crowded-node test the looser scene now
records one rejected replay, with its 23 rescues, their order and every other
counter unchanged.

## Alternatives

- **A per-word confidence or SNR gate on the flip.** Not built: no threshold
  can be fitted without a positive class, and the legacy flip has none on the
  corpus.
- **A lower `max_refinements_per_episode` default.** Not needed: the
  re-admission rule removes the identical second replay without changing the
  key's meaning for an HSMM base.
- **Forcing the spectral reference on for the HSMM replay of legacy hops**
  (tried in research). It changed nothing.
- **The broad guard.** Rejected, see Decision 3.

## Known limits

- Real off-air audio is unmeasured; there is none in the container. The
  ticket's "real audio" premise is neither reproduced nor refuted.
- The corpus is one seed and two trials of synthetic AWGN and Watterson
  fading.
- Reopening the legacy flip needs a corpus with a demonstrated flip rescue.

## Follow-ups

- A measured legacy-base flip rescue, if one is ever found, reopens
  Decision 1.
