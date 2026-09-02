# CHAR_GAP_DITS threshold fix (high-WPM inter-character gap misclassification)

## Background

`crates/manta-engine/tests/roundtrip_iq.rs`'s `iq_roundtrip_with_noise`
proptest carried a known, pre-existing flaky failure noted (not fixed) in
`docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md` pin #12, confirmed via
`git stash` to predate the M2 channelizer work entirely. This doc records the
root-cause investigation (`superpowers:systematic-debugging`) and fix.

## Root cause

Minimal failing case: `text="AB", wpm=33.14012, snr=24.410885, offset_khz=-7,
noise_seed=694100648224208083` decoded to `":"` instead of `"AB"`.

Direct instrumentation of the raw `Demod` envelope stream showed the mark
sequence was measured correctly (`dit, dah, dah, dit, dit, dit`, matching
"A"+"B"'s true keyed elements) and the true inter-character gap (90.67ms,
between A's dah and B's dah) was correctly the largest of the five
inter-element/inter-character gaps in the sequence. The corruption happens
one layer up, in `GapClassifier::classify` (`crates/manta-decode/src/timing.rs`):

```
gap dur_ms=90.667  mu_dit_ms=49.778  u=1.8214  class=InterElement
```

`u = gap_ms / mu_dit_ms = 1.82` fell under SPEC §4.2's nominal
`CHAR_GAP_DITS = 2.0` threshold, so the real character boundary was
classified as merely inter-element. Both letters' six marks merged into one
un-split run (`dit,dah,dah,dit,dit,dit` = `.--...`, not a valid Morse
pattern); the beam decoder in `beam.rs` force-fit it to the nearest valid
glyph, `:` (`---...`), by flipping the lowest-confidence element.

**Why `mu_dit_ms` runs high:** `Demod`'s hysteresis+debounce (SPEC §3.3,
`debounce_ms = 12.0` plus hysteresis turn-on/off lag) adds a roughly
constant ~15-20ms overshoot to every measured mark, but doesn't inflate gap
durations by the same amount — gap starts are delayed by debounce, gap ends
are marked early by the same. At high WPM (short true dit period, here
36.2ms), that constant-ms overshoot is a large fraction of the dit, so
`mu_dit_ms` (built from the mark cluster) runs meaningfully high relative to
true keyed timing, while the character gap itself isn't inflated to match —
compressing `gap_ms / mu_dit_ms` below the nominal 3-dit ideal and, in this
realization, below the 2.0-dit decision boundary entirely.

## Fix

**[DEVIATION]** Lowered `CHAR_GAP_DITS` from SPEC §4.2's pinned `2.0` to
`1.6` (`crates/manta-decode/src/timing.rs`). `2.0` and `1.6` were compared
before choosing 1.8/1.4 too:

| threshold | failures / 500 (seed A) | failures / 500 (seed B) |
|-----------|--------------------------|--------------------------|
| 2.0 (spec nominal) | 11 | 11 |
| 1.8 | 5 | — |
| 1.6 | 4 | 4 |
| 1.4 | 4 (no further gain over 1.6) | — |

1.6 was chosen: it captures the full gain available from this lever (1.4
fixes nothing further), stays closer to the spec's nominal 2.0 than lower
values, and reproduced the same net improvement across two independent
random seeds with **zero cases regressing pass → fail** — every case that
fails at 1.6 also failed at 2.0.

Verified: `timing::tests::gap_classification_nominal` updated for the new
boundary; the pre-existing pinned regression
(`crates/manta-engine/tests/roundtrip_iq.proptest-regressions`, `"PA"` at
32.79 WPM) now passes; a new deterministic regression test
(`crates/manta-engine/tests/regression_char_gap_high_wpm.rs`) pins the
exact "AB" case above.

## Known limitations found during the sweep, deliberately not fixed here

The 500-case sweeps (10-40 WPM, `jitter: None`) surfaced two further,
distinct failure patterns, out of scope for this fix (not lowered by any
`CHAR_GAP_DITS` value tried, present identically at 2.0 and 1.6):

1. **Missing inter-word space at high WPM** (e.g. `"RN XJ0Z"` → `"RNXJ0Z"`,
   `"N6 LR3"` → `"N6LR3"`): characters decode correctly but the word
   boundary is lost — almost certainly the same underlying mechanism
   (mu_dit_ms inflation compressing gap ratios) now hitting `WORD_GAP_DITS`
   instead of `CHAR_GAP_DITS`. Left alone since the user's decision scoped
   this investigation to `CHAR_GAP_DITS` specifically; a `WORD_GAP_DITS`
   sweep is a natural follow-up.
2. **Total decode failure on some short (2-char) high-WPM texts** (e.g.
   `"VE"`, `"DA"`, `"Z5"`, `"D5"` → `""`, zero `CharDecoded` events):
   reproduces identically at both 2.0 and 1.6, so unrelated to this fix.
   Not investigated further — a separate root-cause pass is needed.

Both are new findings, not previously tracked; flagging here so they aren't
lost.
