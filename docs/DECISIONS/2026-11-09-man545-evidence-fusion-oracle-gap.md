# MAN-545: fusing legacy and HSMM evidence loses identities on the bench (net -34 and -48 of 2430); below the bar, item 16 closed

Date: 2026-11-09. Ticket: MAN-545.

Evidence-level fusion of the two classical engines does not recover the
oracle gap MAN-205 measured. On 2430 paired truth identities from the
synthetic sensitivity bench, the oracle gap is 398 identities: legacy spots
395 that HSMM misses, and HSMM spots 3 that legacy misses (the headroom).
The realizable calibrated substitution fuser (`fused:cal-sub`) recovers none
of the 3 HSMM-only identities and loses 35 that legacy alone spots: net -34
identities (-1.40 pp, 95% interval -2.18 to -0.70 pp). The calibrated span
fuser (`fused:cal-span`) recovers 2 of the 3 and loses 53: net -48 (-1.98
pp, -3.21 to -0.91 pp). Both lose spots at 14 and 16 bench points above
+10 dB. In the ticket's literal reading, legacy alone holds 99.2% of the
gap, `cal-sub` 90.5% and `cal-span` 86.4%. In the gated reading (net over
the 3 headroom identities), they recover -1133% and -1600% against a bar of
+25%. Neither meets the adoption bar, so no confirmation run was made and
no live-path ticket is proposed. Even the truth-aware ceilings, which no
fuser can reach, add at most 8 identities (0.33 pp), and at most 3 of those are
HSMM-only identities; the rest are identities neither engine spots alone.

## Context

[MAN-205](2026-11-03-man205-legacy-hsmm-engine-overlap.md) measured the
oracle union of manta's two classical decoders on the synthetic sensitivity
bench: 16.4% of 2430 truth identities are recovered by exactly one engine,
but almost all of that is legacy winning (396 legacy-only against 2
HSMM-only), so a perfect per-identity selector adds 0.08 pp over legacy.
Output-level fusion is not worth building on that evidence.

Item 16 of [the decoder recall research](2026-09-09-decoder-recall-research.md)
proposes something different: align the two engines' per-character evidence
(winners, alternatives and confidences) and decide character by character or
span by span before the spot validator runs. That could, in principle,
produce identities neither engine spots alone. Its promotion bar is
recovering at least 25% of the oracle gap. MAN-545 measures it.

The ticket's literal bar is degenerate. Its oracle gap ("identities exactly
one engine got") is almost all legacy-only identities, which legacy already
spots, so legacy alone "holds" nearly all of it. The well-posed gap is the
headroom H: identities HSMM spots and legacy misses (MAN-205's D3). This
record reports both readings and gates on the second.

## Method

**Baseline (D1).** Legacy with refinement (MAN-208) off. `decode.engine`
stays `legacy` in production, so fusion is judged by what it adds over
legacy. Refinement is off for both engines and every fuser, so no fuser gets
MAN-208's cross-engine rescue for free. A production legacy report
(refinement on) is written for reference.

**Gap definitions (D2).** With A = legacy, B = HSMM and F = the fused
report, over paired truth identities: oracle gap G = A only + B only (the
ticket's reading); gap held = identities of G that F spots, over |G|;
headroom H = B only; gains = identities F spots that A misses, split into
"from B only" and "from neither"; losses = identities A spots that F misses;
net = gains minus losses; recovered of headroom = net / |H|.

**Adoption gate (D3), identity level only.** F meets the bar only if all
four hold: net ≥ 0.25 × |H|; net ≥ 0.25 pp of n with the lower end of its
95% bootstrap interval above 0; at every bench point above +10 dB (500 Hz) F
spots at least as many identities as A, pooled over trials; and F's false
identities are not more than A's. The tool's exit code never depends on the
gate.

**Pre-registered fusers (D4), fixed before any full-grid number existed.**
All are legacy-anchored: legacy's tracks, track ids and non-text events
stay, and only legacy `CharDecoded` and `WordBoundary` events change.

| Report `engine` | Kind | Rule |
|---|---|---|
| `fused:noop` | control | Runs the fusion path and changes nothing. Must equal `legacy` exactly. |
| `fused:dup-sub`, `fused:dup-span` | control | `cal-sub` and `cal-span` with legacy given as both branches. Must equal `legacy` exactly. |
| `fused:oracle-legacy-alts` | ceiling | A legacy character that is a substitution against the truth takes a legacy runner-up equal to the truth glyph. No HSMM. |
| `fused:oracle-sub` | ceiling | At legacy-vs-HSMM substitution columns, the truth glyph if any proposal (both winners, both engines' alternatives) is it. |
| `fused:oracle-span` | ceiling | Per disagreement region, HSMM's side when the station transcript with only that region swapped is strictly closer to the truth by edit distance; ties keep legacy. |
| `fused:cal-sub` | realizable | At substitution columns, the glyph with the highest summed calibrated probability; ties keep legacy; `?` never replaces a glyph. |
| `fused:cal-span` | realizable | Per region, HSMM's side when its mean calibrated log-odds is strictly higher than legacy's; an empty side scores minus the other. |

The ceilings read the truth, so they can never be adopted. They bound what a
fuser of the same unit can reach. Only `cal-sub` and `cal-span` are read
against the gate.

**Alignment and regions (D5).** Each station's transcript is built as the
bench builds it: tracks within 150 Hz, in ascending track id, words split by
`WordBoundary`, tracks joined by a separator that carries no event.
Alignment is Levenshtein with a fixed tie order (diagonal, then legacy-only,
then HSMM-only). A region is a maximal run of non-matching columns. A region
never crosses a track separator. Inserted characters keep HSMM's glyph and
raw confidence; inserted timestamps are clamped into legacy's neighbours.

**Calibration (D6), cross-fitted by trial.** Per engine, three tables over
20 equal-width bins of the raw value (winner characters, winner spaces,
alternatives), each P(right | bin) with Laplace smoothing. Trial t's tables
come from the other trials' stations only, so no realizable fuser sees the
truth of the recording it fuses. `calibration.json` holds tables fitted on
all three trials, for a confirmation run on fresh trials.

**Invariants (D7), enforced in every fuser.** (a) Fusing a branch with a
byte-identical copy of itself changes neither the text, nor any confidence,
nor any spot. (b) A legacy `?` is never deleted, and `?` never replaces a
real glyph. The tests are in `crates/manta-engine/tests/man545_fusion.rs`
(`duplicate_branch_changes_nothing_sub`, `duplicate_branch_changes_nothing_span`,
`unknown_span_is_never_erased`, `question_mark_never_replaces_a_glyph`,
`span_policy_replaces_a_question_mark_only_one_for_one`), run in CI. At grid
scale, the instrument panics if `fused:noop`, `fused:dup-sub` or
`fused:dup-span` differs from legacy in any station field or bogus call.

**Legacy runner-ups.** The legacy beam keeps up to three runner-up glyphs
per character and discarded them. `DecodeConfig::legacy_alternatives`
(default off, Rust-only, not a TOML key or CLI flag) makes it emit them as
`alternatives` without changing any winner, confidence, timestamp or spot.
It stays after this record (D9), so the measurement is reproducible from
main.

**The instrument.** `crates/manta-engine/tests/man545_fusion_grid.rs`
(`#[ignore]`, so CI does not run it). It decodes the bench grid three times
per recording (legacy with runner-ups and refinement off, HSMM with
refinement off, production legacy), applies every fuser, replays each fused
event stream through the production spot validator, scores it as `manta
bench sensitivity` does, and writes one bench-format report per engine and
fuser. It panics if replaying the unmodified legacy events does not
reproduce the decode's spots exactly.

**How the ticket's "Given" is met (D11).** The CLI bench report keeps no
decoded text, confidences or alternatives, so no fuser can read it. The
instrument writes legacy and HSMM reports in the bench's format from the
same scenes, and the slice check below proves them equal, station by
station, to `manta bench sensitivity --json` with a refinement-off config.

**Code measured.** `manta 0.1.0 (git 4111941c46cc; features: none)`, the
release build of this branch at `4111941`, plus the instrument and its
scorer (test files only; nothing in the `manta` binary differs).

## Commands

Run on 2026-11-09 in a 6-CPU (`cpu.max 600000 100000`), 9 GiB container
(`S` a scratch directory):

```sh
cargo build --release -p manta-cli --locked
# Slice: jobs independence and CLI parity
for j in 1 2; do
  MAN545_OUT=$S/slice-$j MAN545_TRIALS=1 MAN545_CONDITIONS=good MAN545_WPM=25 \
    MAN545_SNR=12,20 MAN545_JOBS=$j cargo test --release -p manta-engine --locked \
    --test man545_fusion_grid -- --ignored --nocapture
done
diff -r $S/slice-1 $S/slice-2      # prints nothing (run as $S/a and $S/b)
printf '[decode.refine]\nenabled = false\n' > $S/refine-off.toml
for e in legacy hsmm; do
  target/release/manta bench sensitivity --engine $e --config $S/refine-off.toml \
    --conditions good --wpm 25 --snr-db 12,20 --trials 1 --jobs 1 --json > $S/cli-$e.json
done
target/release/manta bench sensitivity --engine legacy --conditions good --wpm 25 \
  --snr-db 12,20 --trials 1 --jobs 1 --json > $S/cli-legacy-refine.json
# Full grid: the bench's default grid, trials 0 to 2
MAN545_OUT=$S/grid MAN545_JOBS=9 cargo test --release -p manta-engine --locked \
  --test man545_fusion_grid -- --ignored --nocapture
for p in oracle-legacy-alts oracle-sub oracle-span cal-sub cal-span noop; do
  python3 -I scripts/engine-overlap.py fusion --a $S/grid/legacy.json --b $S/grid/hsmm.json \
    --f $S/grid/fused-$p.json --json $S/grid/fusion-$p.json > $S/grid/fusion-$p.md
done
python3 -I scripts/engine-overlap.py bench --a $S/grid/legacy.json --b $S/grid/hsmm.json
python3 -I scripts/engine-overlap.py bench --a $S/grid/legacy.json --b $S/grid/legacy-refine.json
```

**Slice parity (D11).** On good, 25 WPM, 12 and 20 dB, trial 0, a check of
every point's `per_station`, `bogus_calls`, `spotted`, `cer_mean`,
`good_copy`, `spot_snr_db` and every header field except `engine`,
`config_file` and `command` found the instrument's `legacy.json`,
`hsmm.json` and `legacy-refine.json` equal to the CLI's `cli-legacy.json`,
`cli-hsmm.json` and `cli-legacy-refine.json`. This proves the instrument
scores as the bench does, and that the runner-up switch changes no legacy
spot or CER. The one-job and two-job slice outputs are byte-identical. The
`fused:noop` slice report gives 0 legacy-only and 0 fused-only identities
in `engine-overlap.py bench`, and `fusion` reads "below the adoption bar"
with net 0.

**Grid.** 243 recordings, each decoded three times; `real 19m35.737s`. The
nine jobs share six CPUs, so the wall times below are contended:

```
MAN545-CHARS	truth_items=302940	legacy_chars=221350	with_runner_up=220575
MAN545-CHARS	sub=36348	hsmm_right=4313	in_hsmm_alts=150	in_legacy_alts=1305
MAN545-CHARS	del=27915	hsmm_right=2217
MAN545-CHARS	ins=29276
MAN545-TIMING	pairs=203439	under_25ms=178642	under_100ms=1551	under_250ms=1133	at_least_250ms=22113
MAN545-CHECK replay=faithful noop=identical dup-sub=identical dup-span=identical
MAN545-WALL	legacy=1210.1	hsmm=6562.3	legacy+refine=1377.6
```

`MAN545-CHARS` counts legacy's alignment against the keyed truth over every
station: substitutions, deletions (a truth item legacy lacks) and
insertions, with how many of them HSMM's winner got right, how many had the
truth among HSMM's alternatives (HSMM winner wrong), and how many had it
among legacy's own runner-ups. Items include word spaces. `MAN545-TIMING`
pairs characters both engines decoded right: 87.8% of the pairs sit within
25 ms of each other, and 10.9% differ by 250 ms or more. The `fusion` cal-sub
output is byte-identical on a second run.

## Result

The tool's output, verbatim except that its headings sit one level lower.

#### Evidence fusion: fused:cal-sub (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:cal-sub 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 55.6% | 398 | 90.5% | 3 | 0 | 1 | 0 | 35 | -34 | -1133.3% |
| Copy | 2430 | 48.2% | 47.2% | 380 | 92.6% | 1 | 0 | 1 | 0 | 27 | -26 | -2600.0% |

95% intervals, 2000 signal-set resamples: net (identity) -2.18 to -0.70 pp; net (copy) -1.77 to -0.49 pp.

##### Points above +10 dB where F spots fewer than A

| Point | legacy spotted | fused:cal-sub spotted |
|---|---|---|
| seed 1 good/15 WPM/15 dB | 23 | 22 |
| seed 1 good/25 WPM/12 dB | 28 | 27 |
| seed 1 poor/15 WPM/12 dB | 1 | 0 |
| seed 1 poor/15 WPM/15 dB | 2 | 1 |
| seed 1 poor/15 WPM/20 dB | 9 | 6 |
| seed 1 poor/15 WPM/25 dB | 15 | 11 |
| seed 1 poor/15 WPM/30 dB | 20 | 18 |
| seed 1 poor/25 WPM/12 dB | 4 | 2 |
| seed 1 poor/25 WPM/15 dB | 7 | 4 |
| seed 1 poor/25 WPM/20 dB | 17 | 13 |
| seed 1 poor/25 WPM/25 dB | 25 | 22 |
| seed 1 poor/25 WPM/30 dB | 25 | 24 |
| seed 1 poor/35 WPM/12 dB | 7 | 6 |
| seed 1 poor/35 WPM/15 dB | 14 | 13 |

##### False identities

|  | legacy | fused:cal-sub |
|---|---|---|
| Calls spotted that no station sent | 4 | 5 |

##### Adoption gate (identity)

Recovered of headroom: -1133.3% (bar 25%). Net: -1.40 pp, 95% interval -2.18 to -0.70 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 14. False identities: 5 against 4.
Reading: below the adoption bar: recovered of headroom below 25%, net gain below 0.25 pp or its interval not above 0, fewer spots above +10 dB at 14 points, more false identities.

#### Evidence fusion: fused:cal-span (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:cal-span 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 55.1% | 398 | 86.4% | 3 | 2 | 3 | 0 | 53 | -48 | -1600.0% |
| Copy | 2430 | 48.2% | 42.9% | 380 | 65.3% | 1 | 1 | 1 | 0 | 132 | -130 | -13000.0% |

95% intervals, 2000 signal-set resamples: net (identity) -3.21 to -0.91 pp; net (copy) -6.83 to -3.95 pp.

##### Points above +10 dB where F spots fewer than A

| Point | legacy spotted | fused:cal-span spotted |
|---|---|---|
| seed 1 good/25 WPM/12 dB | 28 | 27 |
| seed 1 good/35 WPM/12 dB | 30 | 29 |
| seed 1 poor/15 WPM/15 dB | 2 | 1 |
| seed 1 poor/15 WPM/20 dB | 9 | 6 |
| seed 1 poor/15 WPM/25 dB | 15 | 9 |
| seed 1 poor/15 WPM/30 dB | 20 | 15 |
| seed 1 poor/25 WPM/12 dB | 4 | 3 |
| seed 1 poor/25 WPM/15 dB | 7 | 4 |
| seed 1 poor/25 WPM/20 dB | 17 | 13 |
| seed 1 poor/25 WPM/25 dB | 25 | 23 |
| seed 1 poor/25 WPM/30 dB | 25 | 23 |
| seed 1 poor/35 WPM/12 dB | 7 | 5 |
| seed 1 poor/35 WPM/15 dB | 14 | 13 |
| seed 1 poor/35 WPM/20 dB | 21 | 20 |
| seed 1 poor/35 WPM/25 dB | 27 | 23 |
| seed 1 poor/35 WPM/30 dB | 27 | 24 |

##### False identities

|  | legacy | fused:cal-span |
|---|---|---|
| Calls spotted that no station sent | 4 | 3 |

##### Adoption gate (identity)

Recovered of headroom: -1600.0% (bar 25%). Net: -1.98 pp, 95% interval -3.21 to -0.91 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 16. False identities: 3 against 4.
Reading: below the adoption bar: recovered of headroom below 25%, net gain below 0.25 pp or its interval not above 0, fewer spots above +10 dB at 16 points.

#### Evidence fusion: fused:oracle-legacy-alts (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:oracle-legacy-alts 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 57.2% | 398 | 99.2% | 3 | 0 | 5 | 0 | 0 | +5 | 166.7% |
| Copy | 2430 | 48.2% | 48.8% | 380 | 99.7% | 1 | 0 | 14 | 0 | 0 | +14 | 1400.0% |

95% intervals, 2000 signal-set resamples: net (identity) 0.04 to 0.37 pp; net (copy) 0.25 to 1.03 pp.

##### Points above +10 dB where F spots fewer than A

None.

##### False identities

|  | legacy | fused:oracle-legacy-alts |
|---|---|---|
| Calls spotted that no station sent | 4 | 4 |

##### Adoption gate (identity)

Recovered of headroom: 166.7% (bar 25%). Net: 0.21 pp, 95% interval 0.04 to 0.37 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 0. False identities: 4 against 4.
Reading: below the adoption bar: net gain below 0.25 pp or its interval not above 0.

#### Evidence fusion: fused:oracle-sub (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:oracle-sub 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 57.4% | 398 | 99.5% | 3 | 1 | 7 | 0 | 0 | +8 | 266.7% |
| Copy | 2430 | 48.2% | 48.8% | 380 | 99.7% | 1 | 0 | 14 | 0 | 0 | +14 | 1400.0% |

95% intervals, 2000 signal-set resamples: net (identity) 0.12 to 0.58 pp; net (copy) 0.29 to 0.95 pp.

##### Points above +10 dB where F spots fewer than A

None.

##### False identities

|  | legacy | fused:oracle-sub |
|---|---|---|
| Calls spotted that no station sent | 4 | 4 |

##### Adoption gate (identity)

Recovered of headroom: 266.7% (bar 25%). Net: 0.33 pp, 95% interval 0.12 to 0.58 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 0. False identities: 4 against 4.
Reading: meets the adoption bar.

#### Evidence fusion: fused:oracle-span (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:oracle-span 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 57.4% | 398 | 100.0% | 3 | 3 | 5 | 0 | 0 | +8 | 266.7% |
| Copy | 2430 | 48.2% | 48.6% | 380 | 100.0% | 1 | 1 | 7 | 0 | 0 | +8 | 800.0% |

95% intervals, 2000 signal-set resamples: net (identity) 0.12 to 0.58 pp; net (copy) 0.16 to 0.53 pp.

##### Points above +10 dB where F spots fewer than A

None.

##### False identities

|  | legacy | fused:oracle-span |
|---|---|---|
| Calls spotted that no station sent | 4 | 3 |

##### Adoption gate (identity)

Recovered of headroom: 266.7% (bar 25%). Net: 0.33 pp, 95% interval 0.12 to 0.58 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 0. False identities: 3 against 4.
Reading: meets the adoption bar.

#### Evidence fusion: fused:noop (F) against legacy (A) and hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:noop 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 57.0% | 57.0% | 398 | 99.2% | 3 | 0 | 0 | 0 | 0 | +0 | 0.0% |
| Copy | 2430 | 48.2% | 48.2% | 380 | 99.7% | 1 | 0 | 0 | 0 | 0 | +0 | 0.0% |

95% intervals, 2000 signal-set resamples: net (identity) 0.00 to 0.00 pp; net (copy) 0.00 to 0.00 pp.

##### Points above +10 dB where F spots fewer than A

None.

##### False identities

|  | legacy | fused:noop |
|---|---|---|
| Calls spotted that no station sent | 4 | 4 |

##### Adoption gate (identity)

Recovered of headroom: 0.0% (bar 25%). Net: 0.00 pp, 95% interval 0.00 to 0.00 pp (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 0. False identities: 4 against 4.
Reading: below the adoption bar: recovered of headroom below 25%, net gain below 0.25 pp or its interval not above 0.

### Causal overlap (refinement off for both engines)

#### Overall

| Level | n | Both | legacy only | hsmm only | Neither | legacy recall | hsmm recall | Exactly one | Union recall | Headroom |
|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 991 | 395 | 3 | 1041 | 57.0% | 40.9% | 16.4% | 57.2% | 0.12 pp |
| Copy | 2430 | 793 | 379 | 1 | 1257 | 48.2% | 32.7% | 15.6% | 48.3% | 0.04 pp |

95% intervals, 2000 signal-set (seed, condition, speed, trial) resamples: exactly one (identity) 12.92% to 20.29%; headroom (identity) 0.00 to 0.33 pp; headroom (copy) 0.00 to 0.12 pp; legacy recall (identity) 47.45% to 66.63%; hsmm recall (identity) 29.47% to 52.59%.

#### Fusion gate

Headroom over the better engine: 0.12 pp (identity), 0.04 pp (copy). Bar: 1.00 pp.
Reading: below the bar at both levels.

### Refinement off against production legacy

#### Overall

| Level | n | Both | legacy only | legacy+refine only | Neither | legacy recall | legacy+refine recall | Exactly one | Union recall | Headroom |
|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 1382 | 4 | 0 | 1044 | 57.0% | 56.9% | 0.2% | 57.0% | 0.00 pp |
| Copy | 2430 | 1172 | 0 | 0 | 1258 | 48.2% | 48.2% | 0.0% | 48.2% | 0.00 pp |

95% intervals, 2000 signal-set (seed, condition, speed, trial) resamples: exactly one (identity) 0.00% to 0.37%; headroom (identity) 0.00 to 0.00 pp; headroom (copy) 0.00 to 0.00 pp; legacy recall (identity) 47.45% to 66.63%; legacy+refine recall (identity) 47.33% to 66.42%.

## Against MAN-205 and the research run

MAN-205 measured, at `212fa51` with refinement on for both engines,
legacy-only 396, HSMM-only 2 and a headroom of 0.08 pp. The research phase
of this ticket measured, at `acaa9f7` with refinement on, legacy-only 361,
HSMM-only 4 and 0.16 pp. Here, at `4111941` with refinement off for both
engines (the causal baseline, D1), legacy-only is 395, HSMM-only 3 and the
headroom 0.12 pp. All three agree: the gap is legacy winning, and HSMM's
headroom is 2 to 4 identities in 2430. Refinement changes legacy by 4
identities (refinement off spots 4 that production legacy misses, and
none the other way).

## Decisions

**Verdict (D3): below the adoption bar, item 16 closed.** Both realizable
fusers fail all of D3's first three conditions (net ≥ 25% of headroom; net
≥ 0.25 pp with the interval above 0; no fewer spots at any point above +10
dB), and `cal-sub` also fails the fourth (5 false identities against
legacy's 4). The ticket's bar of 25% of the oracle gap is not met in either
reading: the realizable fusers hold less of the literal gap than legacy
alone does (90.5% and 86.4% against 99.2%), and their net over the headroom
is negative. No confirmation run (D8) applies, and no live-path ticket is
proposed.

**Why calibrated fusion loses.** The fusers trade a legacy character for an
HSMM one when HSMM's calibrated evidence is higher. HSMM is the weaker
engine almost everywhere on this bench (40.9% against 57.0% identity
recall), and per-character calibration over 20 confidence bins cannot tell
which of HSMM's confident characters are right well enough to beat legacy's
own choice. Each wrong switch can break a callsign repetition legacy had
copied, and the spot validator needs whole calls, so a few character errors
cost whole identities. Of the bench points above +10 dB where `cal-sub`
spots fewer than legacy, 12 of 14 are Watterson poor, where HSMM is
weakest.

**What the ceilings say about HSMM's contribution.** `fused:oracle-legacy-alts`
uses only legacy's own runner-ups and the truth, and adds 5 identities.
`fused:oracle-sub` adds HSMM's winners and alternatives as proposals and
adds 8, of which 1 is an HSMM-only identity. So, beyond legacy's own
runner-ups, HSMM's per-character evidence adds at most 3 identities at the
substitution unit, even for a fuser that knows the truth. `MAN545-CHARS`
says the same at the character level: HSMM's winner is right at 4313 of
legacy's 36348 substitutions (11.9%), and HSMM's alternatives hold the
truth at 150 more; legacy's runner-ups hold it at 1305.

**What the span unit adds.** `fused:oracle-span` also reaches +8, with 3
HSMM-only identities among them and one false identity fewer. It can repair
insertions and deletions, which make up most of legacy's errors (27915
deletions and 29276 insertions against 36348 substitutions), but whole
regions of HSMM text are rarely closer to the truth than legacy's. The
realizable span fuser (`cal-span`) loses more than `cal-sub`, because a
wrong region swap replaces several characters at once.

**CPU cost.** A live fuser runs HSMM beside legacy. Here HSMM's summed
decode time was 5.4 times legacy's (6562 s against 1210 s, contended);
MAN-205 measured 3.9 times uncontended. Legacy plus HSMM would cost roughly
five times legacy alone, against a Raspberry Pi 4 single-core budget M2 has
not met. With a negative result this cost buys nothing.

**The legacy runner-up switch stays (D9).** `DecodeConfig::legacy_alternatives`
is default off, unreachable from TOML and the CLI, and costs one boolean
test per character when off. Keeping it keeps this record reproducible from
main.

## Invariants

Both item-16 invariants are required behaviour of every fuser, ceilings
included, and CI-run tests in `crates/manta-engine/tests/man545_fusion.rs`
prove them on synthetic event streams: (a) duplicating a branch changes
nothing (`duplicate_branch_changes_nothing_sub`,
`duplicate_branch_changes_nothing_span`, `noop_reproduces_the_input`); (b)
alignment never erases an unknown span and `?` never replaces a real glyph
(`unknown_span_is_never_erased`, `question_mark_never_replaces_a_glyph`,
`span_policy_replaces_a_question_mark_only_one_for_one`). At grid scale the
instrument checked every recording and printed
`MAN545-CHECK replay=faithful noop=identical dup-sub=identical dup-span=identical`.

## Limits

- Synthetic corpus only, one seed, three trials. Real audio is not measured
  here. The B2 real-audio remeasure stays external, with the `manta oracle`
  commands in the [MAN-205 record](2026-11-03-man205-legacy-hsmm-engine-overlap.md).
  `manta oracle` compares output, not evidence, so it cannot run a fuser;
  an evidence-level remeasure on B2 would need this instrument's fusers fed
  from a recorded decode.
- Edit-distance alignment only. No `sample_ts` alignment and no lockstep
  two-engine decode.
- Legacy-anchored fusion only: legacy's tracks and non-text events stay.
  A fuser that could adopt HSMM's tracks is not measured.
- Two realizable fusers, pre-registered. No parameter was tuned after the
  grid ran.
- Contended wall times (nine jobs on six CPUs).

## Follow-ups

None from this record. Item 16's stack-rank row and section in the
[decoder recall research](2026-09-09-decoder-recall-research.md) are marked
closed.
