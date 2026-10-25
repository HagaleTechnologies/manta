# MAN-402: a unimodal speed bootstrap reads its marks as dahs when the gaps between them say so

Date: 2026-10-25. Ticket: MAN-402, found by the MAN-398 A/B grid
(`crates/manta-engine/tests/man398_grid.rs`). Baseline: main `40d275a`.

## Context

`SpeedTracker` initializes its mark clusters from the first five marks
(SPEC §4.1). When they form one cluster (`max/min < 2`),
`ClusterPair::initialize` has to guess what they are. Over the 150 ms dit
clamp ceiling they must be dahs (pinned decision 20). At or under it, it
assumes dits: `μ_dit = mean`, `μ_dah = 3·mean`, unconfirmed until a mark of
at least `2·μ_dit` arrives.

Dahs are 90–150 ms from 40 down to 24 WPM, so in that band five equal dahs
look like dits. Text built from multi-dah characters (`MO`, `OO`, `0000`,
`T0M`) then never supplies the long mark that would confirm the guess. Every
dah decodes as a dit. Against the dah-sized `μ_dit`, a character gap (three
dits, one dah) classifies as an element gap and a word gap (seven dits,
about 2.3 dahs) as a character gap: `MO` becomes five "dits", `5`, with no
word breaks. On main:

| grid row | signal | seed | 14 s / 30 s CER | 30 s decode |
|---|---|---|---|---|
| 66 | `MO` @ 33.04 WPM, −15.28 kHz, 24.13 dB | 9030173154109632877 | 1.0000 / 1.0000 | `H5555555555555555555555555555I` |
| 70 | `0000` @ 37.52 WPM, +30.07 kHz, 28.61 dB | 9381957236689149590 | 1.0000 / 1.0000 | (empty) |
| 67 | `T0M` @ 36.15 WPM, +23.79 kHz, 28.52 dB | 9506045227576176519 | 1.0000 / 1.0000 | `E` |
| 65 | `OO` @ 36.61 WPM, −1.94 kHz, 20.66 dB | 5205264819695832789 | 1.0000 / 1.0000 | `5S` |

MAN-398's bad-lock trigger does not apply. It handles a bimodal split whose
dit cluster is one fragment (`lo_n == 1`). Here the split is unimodal
(`lo_n == 0`), so that trigger never runs.

## Decision

The completed gaps between the five bootstrap marks resolve the ambiguity.
`TrackDecoder::on_run` (`crates/manta-decode/src/decoder.rs`) buffers runs
in `pending` until the tracker is ready. At that moment, before the buffer
replays, it collects the duration of every space bounded by a mark on both
sides (`completed_gaps_ms`; leading silence and an unfinished trailing space
are excluded) and calls `SpeedTracker::resolve_unimodal_bootstrap`
(`crates/manta-decode/src/timing.rs`). That flips the bootstrap to dahs when
all of the following hold:

1. **The bootstrap is the unresolved, dit-assumed unimodal case:** ready,
   unconfirmed, and `placeholder_is_lo` false. A confirmed split and the
   over-ceiling dah-assumed case are never touched. The observed centroid
   `m` must also be at least 60 ms (`3 × DIT_CLAMP_MS.0`): a shorter dah
   would put the dit under the 20 ms clamp, beyond 60 WPM. Without this
   floor a heavily weighted keyer's dits (52 ms marks, 28 ms gaps at
   30 WPM) would pass the gap test below and flip; the self-review of this
   change found that case, and `unimodal_bootstrap_without_dah_scale_gaps_is_kept`
   pins it.
2. **At least two gaps are dah-scale.** A gap `g` counts when it is finite
   and `m/6 ≤ g < m/√3`, where `m` is the observed mark centroid. A dah is
   three element gaps long, so its element gaps sit near `m/3`; under dits
   they sit near `m`. `1/√3` is the log-space midpoint, the value
   `BADLOCK_DAH_GAP_FRAC` uses, defined separately as
   `UNIMODAL_DAH_GAP_MAX_FRAC` because the two rules compare different
   things. `1/6 = 1/RATIO_MAX` is the shortest element gap any supported
   dash/dot ratio allows, so a shorter gap is a glitch.
3. **The counted gaps agree:** population coefficient of variation
   `< 0.35`, the value SPEC §4.1's drift rule uses for a homogeneous
   streak.

The flip sets `μ_dah = m` and `μ_dit = m/3`. This is the pinned-decision-20
direction: `μ_dit` stays a provisional placeholder (unconfirmed,
`placeholder_is_lo`), and the first genuine dit (`≤ 0.5·μ_dah`) re-anchors
and confirms it. The constraints apply and the WPM reporting EMA is
re-seeded from the corrected `dit_estimate_ms`. A second call cannot flip
back, because the direction changed.

Nothing has consumed the buffered runs yet, so the replay that follows
classifies every gap, learns sender timing and assembles characters in the
corrected scale, once. MAN-265's `reseed_wpm_after_warmup` and
`demod.set_dit_ms` run after the replay as before. No regime bump or
gap-classifier reset is needed.

**Why two gaps, not three.** Five marks can straddle two character or word
boundaries. Looped `MO` attached at M's second dah hands the bootstrap
T | char gap | O | word gap | M, which has only two element gaps. The
planning prototype required three and left the ticket's own row 66 at
CER 1.0 while fixing rows 65, 67, 70 and 91. With two it fixed all five.
`dah_heavy_attach_with_two_element_gaps_decodes` pins that attach position.

## Alternatives rejected

- **A lower absolute ceiling** (e.g. 90 ms). It would read genuine slow dits
  (13–24 WPM, 50–90 ms) as fast dahs. Mark durations alone cannot separate
  the two in the overlap; the gaps can.
- **Broadening MAN-398's trigger**, or adding a CV term to it. Its scope is a
  fragment-seeded bimodal split. The MAN-398 record limits it on purpose,
  and changing it would re-open the dah-heavy regressions that record
  measured.
- **Using the later `μ_egap` EMA** as evidence, after the replay. Under the
  wrong scale the character gaps of dahs classify as element gaps and enter
  that EMA, so it is polluted exactly when the scale is wrong.
- **A live recovery after replay, or a longer warmup buffer.** Both need
  reset and partial-character policies, and the measured cases do not need
  them.
- **Text-specific rules or a language model.** The evidence is timing-only
  and applies to any unresolved unimodal opening.

## Results

Fixtures: `crates/manta-engine/tests/regression_man402_dah_heavy.rs`
regenerates the grid's rows from its PRNG (no rounded offsets or SNRs). For
every row keying `MO`, `0000`, `OO` or `T0M` at 30 WPM or more (rows 19, 43,
46, 65, 66, 67, 70, 91, 114, 162, 186, 190, 214, 234, 238, 258, 286), it
asserts at 30 s that CER is under 0.25, at least five whitespace-delimited
tokens equal the word, and the decode ends in five of them. On main 14 of
the 17 fail (19, 43 and 162 already passed). With the change all pass. The
two ticket fixtures also decode identically, events included, on a repeat
run.

| grid row | 14 s / 30 s CER, main | with MAN-402 | 30 s decode, with MAN-402 |
|---|---|---|---|
| 65 `OO` | 1.0000 / 1.0000 | 0.0250 / 0.0118 | `MO OO OO … OO O` |
| 66 `MO` | 1.0000 / 1.0000 | 0.0244 / 0.0114 | `TO MO MO … MO M` |
| 67 `T0M` | 1.0000 / 1.0000 | 0.0256 / 0.0118 | `0M T0M T0M … T0M T` |
| 70 `0000` | 1.0000 / 1.0000 | 0.0435 / 0.0204 | `J000 0000 0000 … 0000 0000` |
| 91 `T0M` | 1.0000 / 1.0000 | 0.0256 / 0.0118 | `0M T0M T0M … T0M T` |

Hermetic (`crates/manta-decode/src/decoder.rs` tests): clean rectangular
loops of 30 `MO`, `0000`, `OO` and `T0M` words decode exactly at 22.5, 25,
30 and 37.5 WPM, with the reported speed within 2 WPM of the keyed speed.
`HH` and `EE` controls stay exact. In the planning probe on main, only
22.5 WPM decoded (its 160 ms dahs are over the ceiling). A five-mark `MO`
transmission at 30 WPM decodes `MO` and reports 30 WPM. With the decoder's
call to `resolve_unimodal_bootstrap` removed, all four new decoder tests
fail.

Full grid A/B, main versus this change:
`cargo test --release -p manta-engine --test man398_grid --locked -- --ignored --nocapture`
on each build, rows compared on the input tuple, seed, CER14 and CER30. All
288 inputs match. No row's CER rises at either duration. Seventeen rows
improve: the 14 failing high-speed rows above, plus `E 0000` rows 111
(28.22 WPM, CER30 0.8600 → 0.0400), 183 (22.78 WPM, 0.8718 → 0.0513) and
207 (24.78 WPM, 0.8605 → 0.0465). Rows at CER30 ≥ 0.25 drop from 34 to 17.
A baseline-against-itself comparison reports zero changes. The grid was
rerun after the 60 ms floor was added; every row matched the run before it.

Golden vectors, thresholds unchanged:
`cargo test --release -p manta-cli --test golden_v1 --test golden_v7_v9_v10 --locked`
passes V1, V7, V9, V10 and V51 (the HSMM stage-2 gates stay ignored).
`cargo test --release -p manta-cli --test golden_v8_v8w --locked no_bogus`
passes V8, V8w and VC3 with no bogus calls and their genuine-spot floors.

## Known limits

- **Single-dah characters.** `TT` has no element gaps, so nothing
  corroborates dahs. In the planning probe, clean `TT` still failed at
  25–37.5 WPM with this rule. This change does not resolve that ambiguity.
- **Lower-speed dah-heavy rows still fail** (CER30 after this change): 42
  `MO` 26.96 WPM (0.9155), 94 `0000` 28.13 WPM (0.9189), 113 `OO` 26.79 WPM
  (0.9194), 142 `0000` 25.46 WPM (0.6667), 283 `T0M` 26.18 WPM (0.6721).
  Their decodes open with an `E` and then `T` runs (`ET TTT TT …`), not
  the unimodal `H5555…` shape; their cause was not traced here. They are
  unchanged from main, not regressions, and not claimed here.
- **Row 129** (`EE`, 37.18 WPM, 12.33 dB) decodes nothing on main and with
  this change. Its cause was not traced; it is outside the dah bootstrap.
- **Heavy keyer weighting at 60 ms and up.** Dits keyed with marks more
  than about 27 % over their nominal length have gaps under `m/√3` too, so
  five of them opening a transmission (`HH`, `5`) would flip to dahs. Five
  marks and their gaps cannot tell that apart from dahs. No recovery path
  for it was traced, and no grid row or golden vector keys weighted dits,
  so this case is unmeasured.
- **The finish-time bootstrap** (MAN-271, two to four marks at end of signal)
  is unchanged and does not run this check.
- M2's CPU-budget legs, live-SDR soak and VR/fading gates are separate and
  untouched. The added work is one pass over at most four gaps per track
  bootstrap; no Pi 4 measurement was taken.
