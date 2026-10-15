# 2026-10-15 — MAN-234: one bad input sample is replaced and counted, not fatal

**Status:** Implemented.

**Ticket:** MAN-234.

## Problem

One input sample whose real or imaginary part was NaN or infinite ended
`decode`, `run`/`listen` and `doctor` with a panic (exit 101). One finite
but very large sample (`f32::MAX`) did not panic; it made channel power
`+Inf`, latched the floor and gate averages at NaN, and left the run deaf
for the rest of the input, with no error. Every source can deliver such a
sample: a corrupt file, a driver bug, or a float WAV written with the
wrong scale.

## Reproduction

Measured at `a613c8c` on the 120 s V1 scene (96 kS/s float32 IQ, W1AW),
with one sample changed:

| input | `decode` | `listen --json` | `doctor` |
|---|---|---|---|
| clean | exit 0, 195 B of text | 439 lines | `DECODING` |
| NaN at 30 s | exit 101 at `track.rs:1030:62`, 0 B on stdout | exit 101 after 109 lines | exit 101 |
| +Inf at 30 s | as NaN | as NaN | as NaN |
| NaN or +Inf at 1 ms | exit 101 at `floor.rs:130:39` | exit 101 | exit 101 |
| `f32::MAX` at 1 ms (finite) | exit 1, `no signal found (input shorter than one filter length or empty)` | exit 0 after 20 s, 0 lines | exit 0, `NO_SIGNAL … Check antenna, gain` |

`manta calibrate` on NaN at 1 ms exited 1 with `no reference carrier was
measured`; on clean input it measured +0.00 ppm over 20 of 20 segments.

The ticket's annotated scenarios 2 and 3 ("text stops", "NO_SIGNAL")
used Inf and NaN. On this tree those panic instead, because ±Inf becomes
NaN power in the fold and FFT. The silent outcomes still occur with a
finite overflow value, as the last row shows.

## Decisions

- **D1 — one guard, at the channelizer's input, plus one in
  `calibrate`.** `Channelizer::process` runs
  `manta_dsp::sample_guard::replace_unusable` in place on the newly
  appended tail of its window, before any hop is computed, and adds the
  count to `Channelizer::bad_samples()`. Every decoded sample crosses this
  point: `decode`, `run`/`listen`, `soak`, `doctor` and `check`.
  `manta calibrate` does not use the channelizer, so
  `calibrate_with_clock` applies the same function to its read buffer.
  Both sites share one predicate, one replacement value and one count
  definition. The streaming read loop in `listen.rs` (MAN-73, PR #207) is
  unchanged. Rejected:
  - A `SanitizingSource` wrapper. A wrapper has to be added at every
    composition site and can be forgotten there: under MAN-196, doctor's
    `SampleBoundedSource` forgot to forward `trusted_band_hz`.
  - A check in each source. The security review asked for one guarantee
    that does not depend on the source: "non-finite samples are replaced
    and counted at the single read choke point for every source, so the
    guarantee does not depend on which source is in use, and no input
    value can end the process".
  - Swapping the `partial_cmp(...).unwrap()` comparators for a total
    order, alone. The ticket lists it as a non-goal: it turns the panic
    into a NaN latch that leaves the run permanently deaf.
- **D2 — a sample is usable iff `|re| ≤ 1e17` and `|im| ≤ 1e17`**
  (`MAX_SAMPLE_MAGNITUDE`). NaN compares false and ±Inf exceeds the
  bound, so both are unusable. `is_finite` alone is not enough: one finite
  `f32::MAX` sample at 1 ms reproduces scenario 3 (doctor `NO_SIGNAL`,
  `listen` exit 0 with no output), because channel power becomes `+Inf`
  and the floor and gate averages latch NaN. Why `1e17`: the prototype
  filter's absolute tap sum `Σ|h|` is 1.33746–1.33821 for N = 64…4096, so
  `|X[k]| ≤ √2·Σ|h|·B ≈ 1.893·B` and power `≤ 3.58·B² = 3.6e34` at
  `B = 1e17`, four decades under `f32::MAX` (3.4e38). A lower bound such
  as `1e6` would reject recordings that are only unusually scaled, such
  as int24 samples stored as floats in a WAV. See "Bound evidence".
- **D3 — the replacement is `0+0j`.** It is deterministic and adds no
  energy; the measured effect is in "Replacement is benign". Holding the
  last good sample was rejected: it is state, and it is not deterministic
  across chunk boundaries without more bookkeeping.
- **D4 — what is counted.** A sample replaced at the channelizer input
  (or in `calibrate`'s buffer) counts once, even when both components are
  bad. The count only grows within a process and is summed across
  reconnect segments. Samples discarded unprocessed during a
  calibration-fill discontinuity are not counted. A filter that runs
  before the channelizer can turn one bad source sample into several
  counted ones: the decimator's halfband stages (about 64 per stage), the
  audio Hilbert filter (129 taps) and the KiwiSDR resampler. This is
  documented in `docs/METRICS.md`, not hidden.
- **D5 — the `partial_cmp(...).unwrap()` sites stay** (`floor.rs:130`,
  `track.rs:1030`/`1061`/`2166`, `doctor.rs:179`). The guard makes them
  unreachable. No `#[should_panic]` tripwire test is added.
- **D6 — `manta check` keeps its error.** It still fails with
  `source check failed: nonfinite input sample` on non-finite raw input,
  as MAN-125 decided
  ([2026-10-10-man125-source-diagnostics.md](2026-10-10-man125-source-diagnostics.md)):
  a diagnostic should say the input is bad. Its channelizer call is
  guarded like the others, so a finite sample above `1e17` can no longer
  trip its `nonfinite channel power` check.
- **D7 — reporting.**
  - `manta decode` (after its output) and `manta calibrate` print a
    stderr line when the count is above 0: `warning: <path> contained 1 bad sample (NaN, infinite,
    or larger than 1e17 in magnitude); each was replaced with silence and
    decoding continued -- check the source's sample format or driver`.
  - `decode --json`, `doctor --json` and `calibrate --json` gain a
    `bad_samples` field, omitted when 0, so clean output is unchanged.
  - `run`/`listen` print one stderr line at the first bad sample:
    `warning: <input> delivered 1 bad sample (…); replaced with silence,
    decoding continues. Later ones are counted [in
    manta_input_bad_samples_total] and totalled when the run ends`. The
    bracketed part appears with a `[server]` table. When the run ends, a
    total line in `decode`'s wording follows only if the total grew after
    the first line. `<input>` is the file path for `--source`, else `the
    <kind> input` (for example `the soapy input`).
  - The `doctor` and `soak` reports gain `input: N bad sample(s) (NaN,
    infinite, or larger than 1e17 in magnitude) replaced with silence`
    when the count is above 0.
  - `decode`'s "no signal found" errors gain `; N bad input samples (…)
    were replaced with silence` when the count is above 0, so input that
    was all bad samples says so.
  - The daemon publishes `manta_input_bad_samples_total{source}`, a
    counter at 0 from startup for every source kind with a `[server]`
    table, summed across reconnects and never decreasing.
  - Every CLI string is built in `crates/manta-cli/src/fmt.rs` from
    `MAX_SAMPLE_MAGNITUDE`, formatted `{:e}` (`1e17`), and pinned by
    tests, so the text and the constant cannot drift.
- **D8 — no configuration key.** The bound is not a tuning parameter.
  SPEC §9 and `docs/CONFIG.md` are unchanged.
- **D9 — no wiki edit in this change.** The wiki manifest's hash refresh
  belongs to the next `/wiki-update`.
- **D10 — CHANGELOG `### Decoder output`.** Output changes only for input
  that contains a bad sample, and that input used to panic or go silent.
  Clean input decodes byte-identically.

## Bound evidence

- Scaling the whole V1 file by ×1e6 through ×1e19 decodes byte-identically
  on this tree. The ×1e17 file peaks at 2.7e16, under the bound, so it is
  not touched by the guard.
- Sustained 1 s bursts at `1e17`, `1e18` and `4e18` panic neither
  `decode` nor `doctor`.
- `full_scale_bound_keeps_every_power_finite` pins the bound against the
  prototype filter: a window of samples at the bound gives finite power in
  every channel.

## Replacement is benign

- Zeroing the sample at t = 30 s left `decode` text and `decode --json`
  text byte-identical. `listen --json` kept all 439 lines with the same
  events, glyphs and timestamps; ten lines differed only in
  `TrackMeta.freq_hz` (about 1e-4 Hz) and `CharDecoded.confidence` (about
  1e-5), all between 30.07 s and 33.74 s.
- Zeroing the sample at 1 ms left every output byte-identical.

## MAN-203 hand-off

A finite sample below the bound can still hurt a track. One sample of
`1e8` or `1e9` during an active V1 track silences that track for the rest
of the file: `1e9` at t = 5 s leaves `FQ CQ DE` and nothing for the next
115 s. `1e7` is harmless. The threshold is relative to the signal level
(whole files at ×1e17 decode fine), so no absolute guard can fix it. This
is impulse handling, MAN-203's territory (impulse, DC and IQ correction).

## Verification

- `crates/manta-dsp/src/sample_guard.rs`: unit tests for the predicate
  (NaN, ±Inf, `±f32::MAX` and the next float above `1e17` in either
  component are unusable; zero, subnormals, full scale and `±1e17` itself
  are usable) and for `replace_unusable` (each bad sample zeroed and
  counted once; a usable slice never written).
- `crates/manta-dsp/src/channelizer.rs`:
  `a_bad_sample_is_replaced_before_it_reaches_any_hop`,
  `a_bad_sample_produces_the_same_hops_as_silence`,
  `bad_samples_accumulate_across_process_calls`,
  `full_scale_bound_keeps_every_power_finite`.
- `crates/manta-engine`: `decode_samples_survives_a_bad_sample_mid_track`,
  `decode_samples_survives_a_finite_overflow_sample_at_start`,
  `no_signal_error_names_replaced_samples` (`lib.rs`);
  `bad_samples_observer_counts_across_a_reconnect_segment` (`listen.rs`);
  `doctor_on_a_finite_overflow_sample_is_not_no_signal` (`doctor.rs`);
  `a_bad_sample_does_not_change_the_measured_ppm` (`calibrate.rs`).
- `crates/manta-cli/tests/man234_bad_samples.rs`: the ticket's three
  scenarios through `decode`, `run --json` and `doctor`; `check`
  unchanged; the daemon metric present from startup.
- `crates/manta-cli/src/fmt.rs`: the warning, live-warning and report
  lines, pinned verbatim.
- `crates/manta-server/src/metrics.rs`:
  `input_bad_samples_total_is_labelled_and_never_goes_backwards`.

## What is NOT in scope

Impulse, DC and IQ-imbalance handling below the bound (MAN-203);
changing `manta check`'s error (D6); a configurable bound (D8); removing
the `unwrap` sites (D5).

## Rollout / rollback

Clean input is unaffected. A rising `manta_input_bad_samples_total` means
the source or its driver is delivering corrupt or mis-scaled floats;
`docs/RUNBOOKS/node-health.md` says what to check. Rollback is a revert,
which restores the panic and the silent latch.
