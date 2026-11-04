# MAN-205: legacy and HSMM rarely recover a call the other misses; fusion is not worth building on the bench

Date: 2026-11-03. Ticket: MAN-205, narrowed by the 2026-10-31 backlog sweep
to "measurement only; the criterion reports the rate; fusion only by a new
ticket if the gap is meaningful".

On 2430 paired truth identities from the synthetic sensitivity bench,
16.4% are recovered by exactly one engine at the identity level (14.3% at
the copy level), but almost all of it runs one way: legacy recovers 396
stations HSMM misses, and HSMM recovers 2 that legacy misses. The headroom a
perfect per-identity selector would add over legacy is 0.08 pp (identity)
and 0.16 pp (copy), far below the 1.00 pp bar, so decoder fusion is not
worth a ticket on this evidence; the real-audio remeasure below is the one
open reading, and it is external.

## Context

manta has two structurally different classical decoders on the same
evidence: the legacy character-local beam (SPEC v1 §4, the production
engine, `decode.engine = legacy`) and the HSMM token-passing decoder (SPEC
v2 §4, shipped but not stage-2-cleared: on B2 real audio it reached `as_word`
56.1% against the 60% bar, docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md).
Item 16 of docs/DECISIONS/2026-09-09-decoder-recall-research.md proposes
fusing genuinely different classical observations, but only after measuring
the oracle union: how often the correct identity is present in one branch
and absent from the other. If that gap is negligible, fusion has little to
gain. The fusion proposal assumed one classical engine; with two, the
measurement is possible for the first time. This record makes it.

## Method

**Tool.** `scripts/engine-overlap.py` (stdlib Python, tests in
`scripts/tests/test_engine_overlap.py`, run by `ci-full.yml`'s `test` job).
Its module docstring defines every metric below. It records; it never fails
a build on a rate.

**Corpus.** `manta bench sensitivity`'s default grid (conditions `awgn`,
`good`, `poor`; 15, 25 and 35 WPM; 0, 3, 6, 9, 12, 15, 20, 25 and 30 dB in
500 Hz), `--trials 3 --seed 1`, 10 stations per recording, 60 s recordings:
81 points, 243 recordings, 2430 truth identities per engine. The bench
renders each recording from `(seed, trial, stream)` and each station's
signal from `(condition, wpm, trial, seed)` alone
(`crates/manta-testkit/src/sensitivity.rs`, `derive_seed`), so both engines
decode byte-identical recordings, and trials 0 to 2 are the recordings the
research phase measured at `efa79c4`.

**Identity unit (D1).** A truth identity is one station in one recording:
the call it sent, at the frequency it sent on. It is defined without either
engine. The bench decodes the whole pipeline, and decoder output drives
track lifecycle (`crates/manta-engine/src/track.rs`), so the two engines can
end up with different track sets on the same recording; keying by track
would compare different things. `manta oracle` gives the strict
same-sample-span comparison on real audio (below).

**Two levels (D2).** Identity: the engine spotted the station's call within
150 Hz of it (`per_station[].spotted`). Copy: the station's decoded copy has
a character error rate strictly below 0.10 (`per_station[].cer`, the
bench's `GOOD_COPY_CER`). Copy isolates decoder text from the spot
validator, which MAN-201 (`d1ddf99`) rewrote just before this measurement.

**Metrics.** Both, A only, B only, Neither: the 2×2 of "recovered" across
the two engines. Recall: (Both + only) / n. Exactly one, the ticket's rate:
(A only + B only) / n. Union recall: (Both + A only + B only) / n, what a
perfect per-identity selector would reach. Headroom: union recall minus the
better engine's recall, which equals min(A only, B only) / n, the most
fusion could add over running the better engine alone. False identities:
calls an engine spotted that no station sent, as the bench pools them per
point; shared is the per-point multiset intersection.

**Intervals (D6, amended).** Percentile bootstrap, 2000 resamples,
`random.Random(205)`, so the output is byte-identical on rerun. The plan
resampled the 243 recordings. The self-review found that the bench reuses
one signal set (calls, keying, fading) and one noise trial at all nine SNRs
of a sweep (`crates/manta-cli/src/bench.rs`, `run_sweep`), so the nine
recordings of one signal set are not independent. The tool resamples whole
signal sets `(seed, condition, wpm, trial)` instead: 27 clusters here. The
nine signal sets of one trial still share its noise, so the intervals
remain somewhat optimistic.

**Truth only (D8).** No metric reads an engine's own spots count, first
spot time, spot SNR, decoded text or speed ratio; a unit test mutates those
and expects identical output. Correctness comes only from the bench's truth
matching.

**Refusals (D7).** The tool refuses reports whose duration, trials,
stations, sample rate, center frequency, payload, match tolerance or SNR
reference differ, a side that mixes engines, a point twice on one side, a
paired point whose stations differ, or no pairing at all. Points on one
side only are counted and printed.

**Code measured.** `manta 0.1.0 (git 212fa51abc76; features: none)`, the
release build of main at `212fa51` (MAN-54), which includes MAN-201. This
branch changes no Rust code.

## Commands

Run on 2026-11-03 in a 6-CPU, 9 GiB container (`S` a scratch directory):

```sh
cargo build --release -p manta-cli --locked
for c in awgn good poor; do
  target/release/manta bench sensitivity --engine legacy --conditions $c \
    --trials 3 --seed 1 --jobs 4 --json > "$S/legacy-$c.json" 2> "$S/legacy-$c.err"
done
target/release/manta bench sensitivity --engine hsmm --conditions awgn \
  --trials 3 --seed 1 --jobs 4 --json > "$S/hsmm-awgn.json" 2> "$S/hsmm-awgn.err"
# these two ran at the same time
target/release/manta bench sensitivity --engine hsmm --conditions good \
  --trials 3 --seed 1 --jobs 4 --json > "$S/hsmm-good.json" 2> "$S/hsmm-good.err"
target/release/manta bench sensitivity --engine hsmm --conditions poor \
  --trials 3 --seed 1 --jobs 3 --json > "$S/hsmm-poor.json" 2> "$S/hsmm-poor.err"
python3 scripts/engine-overlap.py bench \
  --a "$S"/legacy-awgn.json "$S"/legacy-good.json "$S"/legacy-poor.json \
  --b "$S"/hsmm-awgn.json "$S"/hsmm-good.json "$S"/hsmm-poor.json \
  --json "$S/overlap.json"
```

Each command's last stderr line ("done in N s"): legacy awgn 112 s, good
184 s, poor 180 s; HSMM awgn 439 s. HSMM good (627 s, `--jobs 4`) and poor
(637 s, `--jobs 3`) ran concurrently with each other to fit this phase's
deadline, so only the awgn pair is a like-for-like wall-time comparison:
HSMM took 3.9× legacy's time. The legacy runs and HSMM awgn shared the
container with a two-job `cargo build`.

Results never depend on `--jobs` (MAN-116 D8).

## Result

The tool's output, verbatim except that its headings sit one level lower
here (run twice; the two outputs are byte-identical):

### Engine overlap: legacy (A) vs hsmm (B)

2430 paired truth identities: 81 points, 3 trials of 10 stations, seed 1, 60 s recordings.
Unpaired points left out: legacy 0, hsmm 0.
Identity: the station's call spotted within 150 Hz of it. Copy: its decoded copy has CER below 0.10.
Exactly one is the ticket's oracle-gap rate: identities one engine recovers and the other misses.

#### Overall

| Level | n | Both | legacy only | hsmm only | Neither | legacy recall | hsmm recall | Exactly one | Union recall | Headroom |
|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 2430 | 969 | 396 | 2 | 1063 | 56.2% | 40.0% | 16.4% | 56.3% | 0.08 pp |
| Copy | 2430 | 770 | 343 | 4 | 1313 | 45.8% | 31.9% | 14.3% | 46.0% | 0.16 pp |

95% intervals, 2000 signal-set (seed, condition, speed, trial) resamples: exactly one (identity) 12.84% to 20.33%; headroom (identity) 0.00 to 0.21 pp; headroom (copy) 0.04 to 0.33 pp; legacy recall (identity) 46.30% to 66.09%; hsmm recall (identity) 28.52% to 51.36%.

#### False identities

|  | legacy | hsmm | Shared | Either |
|---|---|---|---|---|
| Calls spotted that no station sent | 5 | 5 | 1 | 9 |

Each trial's distinct bogus calls, pooled per point as the bench reports them; shared is the same call at the same point from both engines.

#### By condition (identity)

| Condition | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| awgn | 810 | 77.8% | 66.8% | 89 | 0 | 11.0% | 0.00 pp |
| good | 810 | 63.8% | 52.1% | 97 | 2 | 12.2% | 0.25 pp |
| poor | 810 | 26.9% | 1.0% | 210 | 0 | 25.9% | 0.00 pp |

#### By condition (copy)

| Condition | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| awgn | 810 | 80.0% | 64.9% | 123 | 1 | 15.3% | 0.12 pp |
| good | 810 | 46.8% | 30.6% | 134 | 3 | 16.9% | 0.37 pp |
| poor | 810 | 10.6% | 0.0% | 86 | 0 | 10.6% | 0.00 pp |

#### By speed (identity)

| Speed | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 15 WPM | 810 | 42.2% | 32.2% | 81 | 0 | 10.0% | 0.00 pp |
| 25 WPM | 810 | 61.0% | 42.8% | 148 | 1 | 18.4% | 0.12 pp |
| 35 WPM | 810 | 65.3% | 44.8% | 167 | 1 | 20.7% | 0.12 pp |

#### By speed (copy)

| Speed | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 15 WPM | 810 | 45.1% | 31.2% | 113 | 1 | 14.1% | 0.12 pp |
| 25 WPM | 810 | 47.5% | 32.1% | 126 | 1 | 15.7% | 0.12 pp |
| 35 WPM | 810 | 44.8% | 32.2% | 104 | 2 | 13.1% | 0.25 pp |

#### By SNR, 500 Hz (identity)

| SNR | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 dB | 270 | 1.5% | 0.0% | 4 | 0 | 1.5% | 0.00 pp |
| 3 dB | 270 | 21.9% | 0.7% | 57 | 0 | 21.1% | 0.00 pp |
| 6 dB | 270 | 43.3% | 20.0% | 63 | 0 | 23.3% | 0.00 pp |
| 9 dB | 270 | 54.4% | 43.7% | 29 | 0 | 10.7% | 0.00 pp |
| 12 dB | 270 | 61.9% | 51.9% | 27 | 0 | 10.0% | 0.00 pp |
| 15 dB | 270 | 67.8% | 56.7% | 31 | 1 | 11.9% | 0.37 pp |
| 20 dB | 270 | 78.9% | 60.7% | 49 | 0 | 18.1% | 0.00 pp |
| 25 dB | 270 | 86.3% | 63.0% | 64 | 1 | 24.1% | 0.37 pp |
| 30 dB | 270 | 89.6% | 63.0% | 72 | 0 | 26.7% | 0.00 pp |

#### By SNR, 500 Hz (copy)

| SNR | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 dB | 270 | 0.0% | 0.0% | 0 | 0 | 0.0% | 0.00 pp |
| 3 dB | 270 | 10.4% | 0.0% | 28 | 0 | 10.4% | 0.00 pp |
| 6 dB | 270 | 30.0% | 4.1% | 70 | 0 | 25.9% | 0.00 pp |
| 9 dB | 270 | 38.1% | 25.2% | 35 | 0 | 13.0% | 0.00 pp |
| 12 dB | 270 | 47.8% | 35.9% | 34 | 2 | 13.3% | 0.74 pp |
| 15 dB | 270 | 56.7% | 41.5% | 42 | 1 | 15.9% | 0.37 pp |
| 20 dB | 270 | 65.6% | 55.2% | 28 | 0 | 10.4% | 0.00 pp |
| 25 dB | 270 | 75.9% | 61.1% | 41 | 1 | 15.6% | 0.37 pp |
| 30 dB | 270 | 87.8% | 63.7% | 65 | 0 | 24.1% | 0.00 pp |

#### By trial (identity)

| Trial | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 | 810 | 45.1% | 33.8% | 93 | 2 | 11.7% | 0.25 pp |
| 1 | 810 | 62.7% | 43.8% | 153 | 0 | 18.9% | 0.00 pp |
| 2 | 810 | 60.7% | 42.2% | 150 | 0 | 18.5% | 0.00 pp |

#### By trial (copy)

| Trial | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 | 810 | 44.9% | 32.0% | 107 | 2 | 13.5% | 0.25 pp |
| 1 | 810 | 45.4% | 32.8% | 103 | 1 | 12.8% | 0.12 pp |
| 2 | 810 | 47.0% | 30.7% | 133 | 1 | 16.5% | 0.12 pp |

#### Fusion gate

Headroom over the better engine: 0.08 pp (identity), 0.16 pp (copy). Bar: 1.00 pp.
Reading: below the bar at both levels.

## Against the research run at `efa79c4`

The research phase paired the same 2430 recordings (seed 1, trials 0 to 2)
at `efa79c4`, before MAN-201 (`d1ddf99`) rewrote `manta-spot`'s validator
("only a settled `self_id` spots") and MAN-54 (docs only) landed.

| Level | Build | Both | legacy only | hsmm only | Neither | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| Identity | `efa79c4` (research) | 999 | 400 | 6 | 1025 | 16.7% | 0.25 pp |
| Identity | `212fa51` (this record) | 969 | 396 | 2 | 1063 | 16.4% | 0.08 pp |
| Copy | `efa79c4` (research) | 770 | 343 | 4 | 1313 | 14.3% | 0.16 pp |
| Copy | `212fa51` (this record) | 770 | 343 | 4 | 1313 | 14.3% | 0.16 pp |

The copy level reproduces exactly: decoder text did not change between the
two builds. The identity level moved: 38 more stations are spotted by
neither engine, 30 fewer by both, and HSMM-only falls from six to two. That is the validator, not the decoders: the plan phase found the
same on two single recordings (identical CER for every station on both
builds, changed spot outcomes; one research HSMM-only station, K1KBC at
awgn/15 WPM/6 dB, is now spotted by legacy too). The research quoted a
recording-level interval for exactly one (13.79% to 20.16%); this record's
signal-set interval (12.84% to 20.33%) is wider, as it should be.

## Decisions

**D3. The ticket's rate is "exactly one"; the gate is headroom.** The
Gherkin asks for the fraction of truth identities recovered by exactly one
engine, and the tool reports it at every level and in every breakdown. That
rate counts every identity the stronger engine wins over the weaker as if it
were complementary. A fusion stage can only add what the better engine
misses and the other gets, so the gate reads headroom over the better
engine. If HSMM were the better engine, promoting it would beat fusing it.

**D4. Bar: 1.00 pp of headroom at either level.** Item 16 proposes
promoting fusion only if it recovers at least 25% of the oracle gap. At 1 pp
of headroom that is 0.25 pp of recall, about one identity in 400, bought
with a second decoder at the measured 3.9× wall time, against a Pi 4
CPU budget M2 has not yet met. Below 1 pp even a perfect selector adds less
than one identity in 100. Reading either level keeps a gap hidden by one
level's shared validator from being dismissed.

**Verdict.** Below the bar at both levels: headroom 0.08 pp (identity) and
0.16 pp (copy) against 1.00 pp, and the upper ends of their 95% intervals
(0.21 pp and 0.33 pp) are below it too. Even a perfect selector between the
two engines would recover two to four identities in 2430 over legacy alone,
for a second decoder at 3.9× the wall time. No fusion ticket is proposed
from the bench. HSMM's misses are not complementary to legacy's here; the
large "exactly one" rate is legacy winning, mostly under Watterson poor,
where HSMM's recall is 1.0%.

**Duplicate-branch invariant.** The ticket's second scenario (two
byte-identical copies of one decoder's output must leave the selected call
and its evidence strength unchanged) needs fusion code, which this ticket
does not build. Any future fusion ticket must carry it as a required
acceptance criterion, together with item 16's other invariant (alignment
must not erase an unknown span to construct a valid-looking call). The tool
tests the measurement-side analogue: a report compared with itself gives
zero exactly-one, zero headroom, and a union recall equal to the single
engine's.

**No permanent extra branch.** Both engines run only offline, in the bench
and in `manta oracle`. `manta run` runs one decoder, as before.

## Real-audio remeasure (external, Tony)

The synthetic bench is enough to close MAN-205: the scope note asks for the
rate, and the bench gives it with intervals. Real audio may still differ.
On B2 at 2026-09-09 the oracle's `as_word` rate was legacy 26.7% and HSMM
56.1%, the opposite ordering to the bench, so the overlap there may differ
too. `manta oracle` is the tracker-bypassed comparison: for each reference
spot both engines decode identical channel samples. On the machine holding
the B2 recording and its RBN CSV:

```sh
for e in legacy hsmm; do
  manta oracle B2_20251129_000000_7080kHz.source.wav \
    ground-truth/B2_20251129_000000_7080kHz.rbn.csv --spotter K5TR \
    --capture-start 2025-11-29T00:00:00Z --engine $e --jsonl b2-$e.jsonl
done
python3 scripts/engine-overlap.py oracle --a b2-legacy.jsonl --b b2-hsmm.jsonl \
  --a-name legacy --b-name hsmm
```

Both runs must share the recording, the RBN CSV, `--spotter`,
`--capture-start`, `--window-s` and `--mode`. Give it an RBN CSV that covers
only the recording's span: a reference spot past the recording's end counts
as missed by both engines and dilutes every rate. The report's levels are
`as_word` (the reference call is a whole word of the decoded window) and
`framed` (it follows CQ, DE or CQ TEST). A headroom at or above 1.00 pp
there would justify a fusion ticket even though the bench does not. This
step is not an acceptance criterion of MAN-205.

## Limits

- Synthetic corpus only. Real audio is not measured here (above).
- Whole pipeline at the bench: the two engines' track sets can differ on the
  same recording, so the identity level mixes decoder and tracker effects.
  The copy level is the purer decoder comparison.
- The bench's SNRs, speeds and its AWGN and Watterson good/poor models only.
- One seed with three trials. The plan asked for six; this run's deadline
  allowed three. Trials are independent replicates, so `--trials 6 --seed 1`
  adds three fresh trials (new noise, fading and callsigns) and keeps these
  three unchanged; the commands above with `--trials 6` reproduce it.
- False identities pool per point across trials, so "shared" can match the
  same bogus call from different trials of one point.
