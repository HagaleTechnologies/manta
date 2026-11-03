# MAN-186: the decimator's CPU cost, measured on a host (no Pi4 claim)

## Decision

A criterion bench, `crates/manta-dsp/benches/decimate.rs`, now measures
`manta_dsp::decimate::Decimator::process` at the rate/factor pairs the
`--capture-rate-hz` path (`manta-input::DecimatingSource`) builds.
`HalfbandStage::process` and `Decimator::process` are unchanged. The
ticket's zero-tap skip does not apply to this filter, so it is not
implemented.

The 2026-10-31 backlog sweep narrowed the ticket to "criterion bench only,
host numbers; no Pi4 claim (Pi4 work paused)". This record follows that
note.

## Why there is no zero-tap skip

The ticket assumed the classic halfband property: every tap at a nonzero
even offset from the centre is exactly zero. That holds only for a cutoff
at exactly `fs_in/4`, because `sinc(k/2) = 0` for every even nonzero `k`.
`design_halfband` places its cutoff at `CUTOFF_FRACTION = 0.235 * fs_in`
(`crates/manta-dsp/src/decimate.rs`), and `sinc(0.47 k)` has no exact
zeros.

A probe over the real 127-tap design (`design_halfband(HALFBAND_TAPS)`)
printed:

```text
taps=127 center h=0.470005 even-offset non-center: exactly_zero=0 nonzero=62 max|h|=2.971287e-2 sum|h|=4.841401e-1; odd sum|h|=1.187539e0
```

So 0 of the 62 even-offset taps are zero. The largest is 2.97e-2, about
6 % of the 0.470 centre tap. Zeroing them anyway would wreck the stopband
(the research phase's DTFT probe, same evaluation as the module's
`dtft::response_db`):

| Filter | worst stopband [60k, 96k] at 192k | fold-in region [26 275, 48 000] at 96k |
|---|---|---|
| real design (127 taps) | -96.88 dB | -86.49 dB |
| even taps zeroed | -30.45 dB | -30.45 dB |

That fails `halfband_stopband_at_least_78_db` and
`halfband_stopband_covers_frequencies_folding_inside_the_trusted_band`, and
moves the pinned -1 dB crossover. Going back to a true halfband (cutoff at
exactly `fs_in/4`) is the design MAN-196 rejected for its weak attenuation
just above the new Nyquist: see
[2026-10-14-man196-decimator-trusted-band.md](2026-10-14-man196-decimator-trusted-band.md).
A filter redesign is its own decision, not part of a bench ticket.

The archived design spec
(`docs/archive/plans/specs/2026-09-09-variable-width-capture-design.md`)
says the halfband choice "halves the MAC cost". That held only for a
cutoff at exactly `fs_in/4`. This record corrects it. The archived spec is
left as it was.

## Why the accumulation order stays

SPEC-decode-core §6 rule 4 requires a fixed operation order for per-sample
`f32` state. A reordered sum, such as a symmetric fold of mirrored taps,
rounds differently, and no test can prove it bit-identical to the current
order for every input once the result narrows to `f32`. The research phase
found the only bit-identical speed-up (a contiguous history buffer instead
of a `VecDeque`) worth about 5 %, inside bench noise on this host. Neither
change is made.

## Cost

Command:

```sh
cargo bench -p manta-dsp --bench decimate --locked -- --warm-up-time 1 --measurement-time 3
```

Host: AMD Ryzen 9 5950X 16-Core Processor (32 threads), rustc 1.98.0
(88d9e12ae 2026-08-18), run 2026-11-03 in the shared implementing container
at load average 43.25 (1 min) at start and 23.70 at end. Treat the absolute
numbers as indicative, as the MAN-273 record does.

Each id times one second of input. `_whole_1s` passes the whole second to
one `process` call. `_live_chunks` passes it as `4096 * factor` samples per
call, the shape `DecimatingSource::read` uses. Criterion's `[low mid high]`:

| Id | Time per 1 s of input |
|---|---|
| `decimate/192k_to_96k_x2_whole_1s` | [9.5726 ms 9.7190 ms 9.8704 ms] |
| `decimate/192k_to_96k_x2_live_chunks` | [9.3830 ms 9.5580 ms 9.7439 ms] |
| `decimate/192k_to_48k_x4_whole_1s` | [14.557 ms 14.866 ms 15.197 ms] |
| `decimate/192k_to_48k_x4_live_chunks` | [14.572 ms 14.899 ms 15.244 ms] |
| `decimate/192k_to_24k_x8_whole_1s` | [16.040 ms 16.321 ms 16.632 ms] |
| `decimate/192k_to_24k_x8_live_chunks` | [15.971 ms 16.255 ms 16.562 ms] |
| `decimate/96k_to_48k_x2_whole_1s` | [4.2796 ms 4.3101 ms 4.3493 ms] |
| `decimate/96k_to_48k_x2_live_chunks` | [4.2389 ms 4.2892 ms 4.3491 ms] |
| `decimate/96k_to_24k_x4_whole_1s` | [7.3094 ms 7.4555 ms 7.6072 ms] |
| `decimate/96k_to_24k_x4_live_chunks` | [7.8675 ms 8.0147 ms 8.1613 ms] |
| `decimate/48k_to_6k_x8_whole_1s` | [4.4605 ms 4.5505 ms 4.6427 ms] |
| `decimate/48k_to_6k_x8_live_chunks` | [4.5196 ms 4.6034 ms 4.6876 ms] |

Share of one host core (mid time of the whole-second case divided by 1 s):

| Case | Share of one core |
|---|---|
| 192k to 96k, x2 | 0.97 % |
| 192k to 48k, x4 | 1.49 % |
| 192k to 24k, x8 | 1.63 % |
| 96k to 48k, x2 | 0.43 % |
| 96k to 24k, x4 | 0.75 % |
| 48k to 6k, x8 | 0.46 % |

The chunked live shape costs the same as one whole-second call within
noise, so the per-read overhead of `DecimatingSource` is negligible. Each
extra stage runs at half the rate of the one before it, so at 192 kS/s the
second stage adds about 5.1 ms and the third a further 1.5 ms to the
first stage's 9.7 ms. The planning run on the same host at load 37 to 47
measured 10 to 20 % higher. The research run at load 19 measured close to
these figures.

## What this does not claim

No Raspberry Pi 4 or M-series Mac figure. Pi4 work is paused, per the
ticket's scope note. The M2 CPU budget in ROADMAP.md measures the native
192 kS/s pipeline (`crates/manta-engine/benches/cpu_budget.rs`), which has
no decimator, so this bench does not feed that gate. No CI job runs
`cargo bench`. CI compiles this bench through `cargo clippy --workspace
--all-targets`.

## Determinism

No library code changed, so the decimator's output is unchanged. The pins
the ticket names pass unchanged: `manta_dsp::decimate::tests::is_deterministic`,
`manta_dsp::decimate::tests::process_across_multiple_calls_matches_one_call`,
and `manta_input::decimate::tests::read_output_is_bit_identical_to_processing_the_whole_input_at_once`
compare with `to_bits()`. The golden vector
(`crates/manta-cli/tests/golden_decimated_capture.rs`) is tolerance-based
(CER < 0.02, frequency error <= 10 Hz), not byte-pinned, and passes.
