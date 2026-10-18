# MAN-398: gap-corroborated recovery from a fragment-seeded speed lock

Date: 2026-10-18. Ticket: MAN-398, split from MAN-6. Baselines: main
`d810279`, and MAN-6's port to decode-core v2, `origin/fix/dit-bad-lock-port`
at `dc872f6` (never merged). The out-of-scope high-speed dah-heavy rows
belong to MAN-402.

## Context

A track attached mid-element hands `Demod` an init window that opens partway
through a mark. That first run has no observed leading edge, so it is a
fragment, shorter than any real element. It becomes one of
`SpeedTracker`'s 5 bootstrap marks. `ClusterPair::initialize`'s
largest-ratio split isolates it as the whole dit cluster (`lo_n == 1`), and
the real dits become the dah cluster. For "HH" the ratio is about 3.3, inside
`[RATIO_MIN, RATIO_MAX] = [2.2, 6.0]`, so the constraint clamp never
re-anchors it. Every real mark of dit-heavy text then reads as a dah, and the
decoder prints `T` streams for the whole transmission.

SPEC §4.1's drift/regime rule cannot see this lock. It needs a 12-mark streak
whose mean is more than 40 % off its pre-streak centroid, and this streak sits
on its own centroid by construction. Main has no other mark-scale reinit.

The ticket's repros on main (14 s / 40 s CER):

| signal | seed | main | decode on main |
|---|---|---|---|
| `HH` @ 22.67 WPM, +29.31 kHz, 23.33 dB | 8986880970975364820 | 1.2500 / 1.0745 | `ETT TTTT TTM Y J Q Q Q J …` |
| `HH` @ 19.0 WPM, +20.25 kHz, 19.63 dB | 2041354727252151524 | 3.08 / 3.00 | behaves like the first |
| `5NN` @ 16.97 WPM, −35.43 kHz, 18.19 dB | 9855354123770958939 | 2.83 / 2.77 | `TT TT TTTTT` repeating |

MAN-6 added two recovery triggers: a CV-gated "homogeneous" one and a
"bimodal" one. Ported to main, they recover these repros but cost two things.
On the 288-row A/B grid
(`crates/manta-engine/tests/man398_grid.rs`) they make 13 dah-heavy rows
worse than main. Those rows turn into dit spam: row 15 `E 0000` decodes
`"1005EEEEEEEE"`. They also spot one bogus callsign (`RC3N`) on golden V8w.

The ticket asks for a fix that tells two cases apart:

- a dit estimate resting on a fabricated leading fragment;
- a genuine opening dit followed by many real dahs.

## Decision

`SpeedTracker::check_drift` (`crates/manta-decode/src/timing.rs`) has one
bad-lock trigger besides SPEC §4.1's regime rule. It runs only when that rule
did not fire, and it re-seeds the mark clusters from the 12-mark streak when
all three of these hold:

1. **All 12 streak marks were assigned to the dah cluster.**
2. **The dit cluster still rests on the lone bootstrap mark.**
   `lo_uncorroborated` is true when the 5-mark split put exactly one mark in
   `lo`. The first later mark assigned to the dit cluster clears it, and so
   does any reinit.
3. **The gap timing does not say the streak's marks are dahs.**
   `streak_marks_are_dahs()` is false when `μ_egap` is `None`, or when
   `μ_egap ≥ BADLOCK_DAH_GAP_FRAC · q`, where `q` is the streak's
   lower-quartile mark (`d[3]` of the 12 sorted).

`BADLOCK_DAH_GAP_FRAC = 1/√3 ≈ 0.577`. A dah lasts three element gaps and a
dit lasts one, so `μ_egap / q` sits near 1/3 for a dah streak and near 1 for
dits misread as dahs. `1/√3` is the log-space midpoint between them, the same
rule `ClusterPair::boundary` uses between mark clusters. It was not tuned:
any constant in roughly [0.45, 0.75] gives the same grid.

Why each choice:

- **The streak's marks, not `μ_dit`.** In a fragment lock on all-dah text
  (`MO`, `OO`, `T0M` opened mid-element), `μ_dit` is the fragment and the dah
  cluster is right. The element gaps then classify `InterElement`, so
  `μ_egap` holds the true dit and disagrees with `μ_dit`. A gate against
  `μ_dit` therefore fires and turns correct dahs into dits; the research
  gate measured exactly that on rows 18, 139, 235 and 257. Compared with the
  streak, the same gaps say "these marks are three gaps long" and the lock
  stays.
- **The lower quartile.** A mixed streak such as 5NN's (7 dits and 2 dahs per
  repeat) has its short marks at the dit, and those are what the gaps must be
  compared with. A pure dah streak has its lower quartile at the dah. The
  quartile is less noise-sensitive than the minimum.
- **`None` means "not dahs".** Against a fragment's dit unit, every real
  element gap of dit text measures at or above the character-gap boundary.
  `on_element_gap` is never fed, so the centroid starves exactly when the
  lock is wrong. Treating `None` as dah evidence would disable the trigger on
  the cases it exists for.
- **No CV term.** Dit-heavy text with some dahs (5NN, HV) streaks with CV
  near 0.6. MAN-6's CV gate left those locked, which was MAN-6 validation's
  M1 residual.
- **No bimodal trigger.** It alone spots `RC3N` on V8w: the gap gate with
  bimodal enabled still fails identically, and with it disabled V8w passes.
  Its one unique grid recovery (row 125, `TEST` @ 27.96 WPM) is also
  recovered by the CV-free trigger.

When the trigger fires it re-seeds from the 12-mark ring (not the last 5) and
has the regime-change side effects: the §4.1a gap centroid is reset, the
regime counter bumps (so `TrackDecoder` resets the §4.1b sender timing
model), and `take_badlock_recovered()` returns true once. `TrackDecoder`
then replaces its `GapClassifier`. That classifier's clusters were
bootstrapped from gap ratios against the wrong `μ_dit` and cannot correct
themselves, so without the reset every word gap after recovery would stay
classified as a character gap. `TrackDecoder` also drops
`single_flush_gaps`, the flushed one-character-word gap lengths the new
classifier's Farnsworth rebuild (`reinit_from_flushed`) would otherwise read:
they are in the same wrong dit unit.

SPEC §4.1 records the rule as a **[DEVIATION]** (MAN-398).

## Alternatives

All measured on current main, by this ticket's planning session or the
research it builds on. Grid counts are identical / better / worse against
main, under a strict per-row comparison of CER at 14 s and 30 s.

| trigger | grid | V8w | why rejected |
|---|---|---|---|
| port as shipped: homogeneous CV-gated + bimodal | 255 / 20 / 13 | fails, `RC3N` | dah-heavy regressions; V8w |
| homogeneous CV-gated, no bimodal | 259 / 16 / 13 (ticket) | passes | dah-heavy regressions |
| homogeneous, no CV gate, no bimodal (MAN-6 `5572cd0c`) | 257 / 18 / 13 (ticket) | passes | dah-heavy regressions |
| `μ_egap` vs `μ_dit`, CV-gated, no bimodal (research) | 267 / 16 / 5 | passes | rows 18, 139, 235, 257 newly broken; the CV gate leaves 5NN-shaped mixed streaks locked |
| **`μ_egap` vs streak lower quartile, no CV gate, no bimodal** | **270 / 18 / 0** | **passes** | chosen |
| conditional leading-fragment discard in `Demod` | not built | not run | indistinguishable in mark space |

The port's 13 worse rows were 15, 63, 87, 159 and 231 (`E 0000` at
34.6–39.8 WPM), 18 and 162 (`MO`), 19, 43, 139 and 235 (`T0M`), 142
(`0000`) and 257 (`OO`). Mechanism: on an all-dah streak under the 150 ms dit
ceiling, the homogeneous trigger re-seeds from the streak, and the unimodal
bootstrap then assumes dits (pinned decision 20).

**The `Demod` discard was rejected, not deferred.** The ticket's second
candidate direction was to drop the first run when it is implausibly short
against the marks that follow. In mark space, a fragment before real dits
(`[20, 66.7, 66.7, …]`) and a genuine opening dit before real dahs
(`[37.3, 112, 112, …]`) have the same shape, and the ratio to the marks after
each is also the same. So any such rule fires on both. That is how MAN-6's
unconditional discard regressed V1/V10 before it was reverted. The only
measured separator is the gap pitch of the stream that follows, and
`SpeedTracker` already has it. `crates/manta-decode/src/envelope.rs` is
unchanged.

## Measurements

**Grid A/B, main `d810279` vs this change**
(`cargo test --release -p manta-engine --test man398_grid -- --ignored
--nocapture | grep ^GRID`, compared row by row on the printed 4-decimal CERs
at 14 s and 30 s): **identical 270 / better 18 / worse 0** over 288 rows.
The 18 better rows:

| rows | text |
|---|---|
| 1, 49, 193, 241 | `HH` |
| 9, 33, 177, 201 | `EE` |
| 154, 178, 226 | `SS` |
| 179, 203, 227 | `IS` |
| 125 | `TEST` |
| 170 | `5NN` |
| 191 | `EIS 5` |
| 283 | `T0M` |

Rows 18, 139, 235 and 257, the research gate's regressions, are identical to
main. So is row 142 (`0000` @ 25.46 WPM, 0.6875 / 0.6667), which is bad on
both builds and belongs to MAN-402.

**The ticket's repros** (`dit_dominated_bad_lock_converges` in
`crates/manta-engine/tests/regression_man6_persistent_garble.rs`), 14 s / 40 s
CER:

| signal | main | this change | 40 s decode |
|---|---|---|---|
| `HH` @ 22.67 | 1.2500 / 1.0745 | 0.4062 / 0.1383 | `ETT TTTT TTM Y HH HH … HH H` |
| `HH` @ 19.0 | 3.08 / 3.00 | 0.7308 / 0.2405 | `ETT TTTT TTTT TTTT TSH HH HH … HH H` |
| `5NN` @ 16.97 | 2.83 / 2.77 | 0.9130 / 0.3000 | `ETTT TT TT TTTTT TT TE 5NN 5NN … 5NN 5N` |

The test asserts CER40 < 0.5, CER40 < CER14, and that the decode ends in five
word-bounded copies of the text once a trailing partial word is dropped.

**Goldens** (release): V8, V8w and VC3 spot 0 bogus callsigns
(`golden_v8_v8w -- no_bogus`, 3 passed). V1, V7, V9, V10 and V51 pass
(`golden_v1`, `golden_v7_v9_v10`). No golden threshold changed.

**Hermetic decoder probe** (zero-noise `rect_envelope` fed to a fresh
`TrackDecoder` from hop `skip`; the count is the number of word-bounded
copies of the text). The counts are the planning session's; the decoder
tests named below assert their thresholds, and on the port's code
`genuine_opening_dit_then_dahs_decodes_cleanly` and
`mid_element_start_on_sparse_dah_text_recovers` fail, as the table says:

| case | main | port | this change |
|---|---|---|---|
| `E 0000` ×10, dit_hops 14, skip 0 | 10 | 0 (`E 0005EEEEEEEEE`) | 10 |
| `HV` / `HU` / `SV` ×30, dit_hops 25, skip 68 | 0 / 0 / 0 | 0 / 0 / 0 | 27 / 27 / 27 |
| `5NN` ×30, dit_hops 25, skip 68 | 0 | 28 | 28 |
| `HH` (port's test), skip 68 | 0 | 27 | 27 |
| `AU` (port's tests), skip 118 | 0 | 26 | 26 |

The first three rows are the decoder tests
`genuine_opening_dit_then_dahs_decodes_cleanly`,
`mid_element_start_on_sparse_dah_text_recovers` and
`mid_element_start_on_mixed_dit_text_recovers`.

**Gate margin.** This comes from the planning session's prototype
instrumentation, which logged `μ_egap / q` on every evaluation of the third
condition (that is, whenever conditions 1 and 2 held) across all 288 rows at
14 s and 30 s: 3303 evaluations.

- Kept the lock (dah evidence): 3267, ratio 0.328–0.415, median 0.37. These
  are every `0000`, `E 0000`, `MO`, `OO` and `T0M` row.
- Fired with a populated centroid: 2, both row 125 `TEST`, ratio 0.856.
- Fired on `None`: 34, on the `HH`, `EE`, `SS`, `IS`, `5NN` and `EIS 5` rows.
- Between 0.45 and 0.75: none.

The threshold sits ×1.39 above the highest dah ratio and ×1.48 below the
lowest populated dit ratio. The dah side rests on 31 rows. The populated dit
side rests on one grid row (125), the only decoder measurement of it. The
unit test `fragment_lock_on_dits_recovers_when_gaps_match_the_marks` pins
the tracker's populated-centroid fire at ratio 1.0 with hand-fed element
gaps (a 20 ms fragment, 53 ms marks and gaps). It fails if the MAN-398
trigger is removed or every populated centroid is read as dahs. It does not
bound the threshold: any value up to 1.0 passes it. In the decoder, those
gaps would read as character gaps against the 20 ms dit.

## Known limits

- **Text of single-dah characters only** (e.g. `TT TT`) that opens with a
  fragment or a lone dit, with dahs under 150 ms. It has no element gaps, so
  `μ_egap` stays `None`, the trigger fires, and the unimodal re-seed assumes
  dits. No grid text has this shape. Measured hermetically (`TT` ×30, 48 ms
  dit, a first mark of 5–36 hops cut from the end of the first dah): main
  decodes 27–29 word-bounded `TT` when that first mark is 9–27 hops long,
  and this change decodes 5–7, ending in `IIII…`. That is how main already
  decodes the same stream from a clean start (`IIII…`, 0 `TT`), because a
  144 ms dah is under the 150 ms dit ceiling. So the change takes away a
  rescue that only a lucky first mark gave. Timing alone cannot tell this
  text from dits at a third of the speed: `TT` at 25 WPM has the mark and
  gap pattern of `I` at 8 WPM. Separating them needs a word-gap or language
  model.
- **Fragment locks with `hi/lo > 6`.** The constraint clamp re-anchors these
  and main's own SPEC regime rule then breaks them, with or without this
  change. The unit test `fragment_lock_on_all_dah_text_is_kept` therefore
  uses a 30 ms fragment against 144 ms dahs; a 20 ms one would exceed
  `RATIO_MAX`.
- **High-speed dah-heavy text** (`0000`, `MO`, `T0M`, `OO` at about 25–40 WPM)
  that is bad on main is equally bad here: MAN-402.
- The trigger covers the legacy engine only. The HSMM engine
  (`decode.engine = hsmm`, not promoted) has no 5-mark bootstrap.

## Supersedes

MAN-6's bad-lock design: the bimodal trigger (`is_credible_bimodal`,
`BADLOCK_MIN_CLUSTER`) and the CV-gated homogeneous trigger. Its decision
record, dated 2026-09-04, exists only on the unmerged port branch and is not
carried over. Neither are its SPEC §3.4
note and its wiki edits. MAN-6 validation's M2 findings flagged its coverage
claims as false, for example that recovery "holds by construction" for any
mixed streak. This record does not repeat them: what it claims is limited to
the measurements above.
