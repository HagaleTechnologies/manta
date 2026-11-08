# MAN-526: the detector judges channel power in linear units

Date: 2026-11-08. Ticket: MAN-526, from the 2026-11-07 performance review.

## Context

Each hop, `TrackManager::step_hop` turns all 2048 channels' linear power
(`HopOutput.power`) into dB with one `log10` per channel (`power_db`).
`manta-dsp::floor` then turned that dB value back into linear three times
per channel per hop:

- the floor's linear EMA (`smoothed_lin`, SPEC v2 §2.2) called
  `db_to_linear(p)`, a `powf`, in its seed and step loops;
- `Gate::update` called `10.0_f64.powf(power / 10.0)` to feed the two-look
  power windows;
- `Gate::judge` called `10.0_f64.powf((rise_floor + on_snr_db) / 10.0)` for
  the window threshold, once per channel per hop from `update` and from
  `hold`, although every input of `F_rise` changes only on the decimated
  push (every `DECIMATION_HOPS = 15` hops).

At 192 kS/s that is 4 × 2048 × 375 ≈ 3.07 M transcendental calls per
second before any decoding happens.

## Decision

Per channel per hop there is now exactly one power conversion (the
`log10` in `power_db`), plus at most one threshold `powf` on a hop whose
floor update was a decimated push. We read the ticket's first scenario
this way: "converted at most once per hop" counts conversions of channel
power. The threshold recompute is not a power conversion, and the
scenario's second clause allows it every 15 hops.

- **D1. Keep one `log10` per channel per hop.** Its dB vector still feeds
  the crash detector (SPEC §2.7), the gate's dB EMA (SPEC §2.3 defines `S`
  as an EMA of `PdB`), the FSK detector (§2.6), the histogram floor and
  per-track evidence. Removing it would redefine those in linear units,
  which is a SPEC change, not a performance fix.
- **D2. Pass `hop.power` beside the dB slice.** `FloorBank::update`,
  `FloorBank::update_holding_reference` and `Gate::update` take
  `power: &[f32]` next to `power_db: &[f64]`, the same hop in both units.
  The linear operand is `channelizer::power_linear(p) = p as f64 + 1e-20`:
  no allocation, no transcendental, and it keeps `power_db`'s guard, so
  digital silence stays `1e-20` linear and `spectral_reference_db` stays
  finite. SPEC §2.3 already defines the windows over linear power and SPEC
  v2 §2.2 defines `P_s` as an EMA of linear power, so no SPEC text
  changes. `power_linear` agrees with the old `10^(power_db(p)/10)` round
  trip to 1e-13 relative over 0 to `f32::MAX`
  (`power_linear_matches_the_db_round_trip_it_replaces`).
- **D3. `FloorBank` caches `F_rise`.** A new `rise_floor_db: Vec<f64>`
  holds SPEC §2.3's rise floor per channel. It is recomputed for every
  channel at the end of the decimated branch by `compute_rise_floor_db`,
  which holds the expression moved verbatim out of `Gate::judge`. Its
  initial value `HIST_MIN_DB` equals the old formula on a fresh bank.
- **D4. `Gate` caches the linear threshold, keyed by the `F_rise` value.**
  `thr_src_db` (NaN until first use) and `thr_lin` hold the last `F_rise`
  and its threshold. `refresh_threshold` recomputes with the unchanged
  expression only when `floor.rise_floor_db[k] != thr_src_db[k]`. Same
  expression and inputs, so the threshold is bit-identical to the per-hop
  one. A value key, not an update counter, stays correct when a gate is
  judged against a different `FloorBank` (`gate_hold_does_not_advance_the_ema`
  does that with `loud`).
- **D5. `Gate::hold` takes `&mut self`** and refreshes the cache the same
  way. A crash can hold for up to `CRASH_MAX_HOPS = 375` hops, about 25
  decimated pushes; an `&self` hold would fall back to a `powf` per channel
  per hop for the rest of the crash. The only production caller
  (`TrackManager::step_hop`) already held `&mut self.gate`.
- **D6. Change the signatures; add no dB-only wrappers.** A wrapper that
  derived linear power with `powf` would keep the cost for `manta doctor`
  and `manta check`, which now pass `&hop.power` too. The `floor.rs` unit
  tests speak dB through a `#[cfg(test)]` helper (`update_db`) that
  inverts `power_db` to the f32 the bank would see.
- **D7. Keep the histogram floor in dB.** Its `u16` ring and `u8`
  histogram are dB-binned and it runs every 15th hop only.
- **D9. Add `crates/manta-dsp/benches/floor_gate.rs`.** One iteration is
  375 hops, one second of 192 kS/s input, through `power_db`,
  `FloorBank::update` and `Gate::update` with `TrackManager::new`'s
  defaults (on 11.5 dB, off 3 dB, windows [38, 38], EMA rise at 12 dB).
  Its time per iteration is the fraction of one core. It compiles under
  `clippy --all-targets` and is never run in CI, like `benches/fsk.rs`.
- **D10. No `CHANGELOG.md` entry.** MAN-83's rule lists decoder-output
  changes only, and no output changed (below).

## Measurements

`cargo bench -p manta-dsp --bench floor_gate --locked`, release profile,
AMD Ryzen 9 5950X (32 logical CPUs), shared host with load average 17 to 20
during the runs. Base is `915db0e` with the same bench ported to the old
signatures, built in its own worktree and `CARGO_TARGET_DIR` (two trees
sharing one target dir silently reuse each other's rlibs, MAN-312).
Interleaved pairs, criterion's median per iteration (one second of input):

| Pair | Base `915db0e` | MAN-526 | Change |
|---|---|---|---|
| 1 | 54.119 ms | 23.662 ms | -56 % |
| 2 | 46.370 ms | 19.104 ms | -59 % |
| 3 | 41.567 ms | 18.704 ms | -55 % |

On this x86 host the floor and gate path went from about 4.2 to 5.4 % of
one core to about 1.9 to 2.4 %. The remaining time is the kept `log10`, the
window sums and the judge. There is no Cortex-A72 measurement, so the
ticket's 0.12 to 0.25 core estimate for the Pi 4 is unverified; the bench
is there to measure it.

## Decode output

No golden moved, so nothing was re-pinned. SPEC §6.4 rule 4 applies
because `power_linear(p)` is not bit-identical to
`db_to_linear(power_db(p))`: the linear EMA and the window sums now see
values a few ulp apart from before. The threshold itself is bit-identical
(D4).

Evidence, base `915db0e` against this branch, both `dev` builds in
separate target dirs. The 20 `manta gen` vectors were generated with the
base binary; v7, v8, v8w, v10, v34 (`write_fixture_set`) and v9
(`write_v9_fixture_set`) with an uncommitted testkit example on base. Each
was decoded with `manta decode <wav> --json --engine legacy` and
`--engine hsmm` (the default `spectral_beta = 0.5`, so `hsmm` reads the
changed linear EMA through `spectral_reference_db`). `cmp` found all 52
outputs byte-identical:

| Vector | Engine | Spots | Events | sha256 (first 12) | Base vs branch |
|---|---|---|---|---|---|
| v1 | legacy | 1 | 438 | `5fe7c66d25ee` | identical |
| v1 | hsmm | 1 | 3667 | `e889616eca80` | identical |
| v2 | legacy | 1 | 434 | `7548d5d4d86c` | identical |
| v2 | hsmm | 1 | 4733 | `13d06c98c096` | identical |
| v3 | legacy | 1 | 353 | `d067da52ed06` | identical |
| v3 | hsmm | 1 | 1847 | `de0a73fcce6c` | identical |
| v4 | legacy | 1 | 482 | `e5965b0d8863` | identical |
| v4 | hsmm | 1 | 4518 | `8a1189f85352` | identical |
| v5 | legacy | 0 | 504 | `0f3e20232be3` | identical |
| v5 | hsmm | 0 | 4242 | `503cc6abedff` | identical |
| v6 | legacy | 1 | 432 | `f2c5db358c9b` | identical |
| v6 | hsmm | 2 | 4071 | `fb11d2dbb013` | identical |
| v7 | legacy | 2 | 1019 | `dba5584b9318` | identical |
| v7 | hsmm | 2 | 9050 | `ebab28ff8826` | identical |
| v8 | legacy | 48 | 23093 | `b65f6b5a25e0` | identical |
| v8 | hsmm | 55 | 195314 | `849fcfde31a1` | identical |
| v8w | legacy | 48 | 25581 | `8268c2574920` | identical |
| v8w | hsmm | 1 | 196900 | `34d9847067c1` | identical |
| v9 | legacy | 0 | 362 | `c0c82c67c0bb` | identical |
| v9 | hsmm | 0 | 3947 | `d906842c6471` | identical |
| v10 | legacy | 1 | 396 | `eaf7403a4161` | identical |
| v10 | hsmm | 0 | 3238 | `01057c98ab2a` | identical |
| v34 | legacy | 3 | 435 | `47d54611c924` | identical |
| v34 | hsmm | 1 | 3706 | `e3adc62dae59` | identical |
| vr1 | legacy | 1 | 756 | `d0662e28040c` | identical |
| vr1 | hsmm | 1 | 6470 | `abfea51b19ba` | identical |
| vr2 | legacy | 1 | 834 | `917324a47c18` | identical |
| vr2 | hsmm | 1 | 6491 | `01678d247707` | identical |
| vr3 | legacy | 1 | 598 | `ab07eae32c5e` | identical |
| vr3 | hsmm | 1 | 5461 | `91ee50d8fa6a` | identical |
| vr4 | legacy | 1 | 603 | `fa23ce11ada6` | identical |
| vr4 | hsmm | 1 | 5725 | `ac59aa783b75` | identical |
| vr5 | legacy | 0 | 354 | `01031e1a1e46` | identical |
| vr5 | hsmm | 1 | 5535 | `dccb24d56381` | identical |
| vr6a | legacy | 1 | 553 | `c014993dd941` | identical |
| vr6a | hsmm | 1 | 5119 | `e999bb0460df` | identical |
| vr6b | legacy | 1 | 525 | `5d4845ee9259` | identical |
| vr6b | hsmm | 1 | 4554 | `6d3b332f48f5` | identical |
| vr7 | legacy | 1 | 618 | `3a506cd8acc6` | identical |
| vr7 | hsmm | 1 | 6602 | `a97b0b6a8377` | identical |
| vr8 | legacy | 0 | 587 | `edece5cde95b` | identical |
| vr8 | hsmm | 0 | 5279 | `92b09f2e48ff` | identical |
| vw1 | legacy | 1 | 243 | `7aca501186dd` | identical |
| vw1 | hsmm | 1 | 2113 | `3def5ac9312b` | identical |
| vw2 | legacy | 1 | 247 | `e0a77bb8495e` | identical |
| vw2 | hsmm | 0 | 2090 | `1b0c86e8489c` | identical |
| vw3 | legacy | 1 | 210 | `976ed97771fa` | identical |
| vw3 | hsmm | 1 | 1833 | `153a83fdbd60` | identical |
| vw4 | legacy | 1 | 204 | `5167a837ddae` | identical |
| vw4 | hsmm | 1 | 1591 | `add135dd8fdd` | identical |
| vw5 | legacy | 2 | 776 | `4d178feff7de` | identical |
| vw5 | hsmm | 2 | 6294 | `e81bc1c4258a` | identical |

The workspace suite's golden and determinism tests (`golden_*`,
`determinism*.rs`) pass unchanged on this branch. They compare runs of one
binary, so they cannot see a base-to-branch change; the 52-output `cmp`
above is the evidence for the ticket's second scenario.

A window mean within about 1e-16 relative of the threshold could still
flip one rise decision on some future input. None did here. If one does,
re-pin the moved golden with its before/after diff, per the ticket.

## Follow-up

- **D8, not done here.** `TrackManager::spectral_ref_power` still takes
  `FloorBank::spectral_reference_db`'s `log10` and turns it back into
  linear with `powf`, per decoding track per hop. It is off on the default
  `legacy` engine (MAN-195) and the ticket does not name it. A
  `FloorBank::spectral_reference_lin` returning the minimum of
  `smoothed_lin` directly would remove both calls on `edge-legacy` and
  `hsmm`. To be filed as a ticket.
- Any new write to `FloorBank` state outside the decimated branch must
  refresh `rise_floor_db` too, or the gate judges against a stale `F_rise`.
  `rise_threshold_is_recomputed_only_when_the_floor_changes` checks the
  cache against the formula on every hop.
