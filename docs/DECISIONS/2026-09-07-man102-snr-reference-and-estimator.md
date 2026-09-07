# 2026-09-07 — MAN-102: SNR estimator and wire reference bandwidth

**Status:** Implemented. Resolves decision D3 of
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` for the estimator half
(peak-hold aggregation) that D3 itself left to the implementing ticket.

## Context

manta reported spot SNR from `Demod::snr_2500_db()`
(`crates/manta-decode/src/envelope.rs`) — a ratio of the demod's own dual-EMA
keying rails (SPEC §3.2), pinned at M0 as an explicit stand-in ("Documented
stand-in, replaced at M2" — `docs/DECISIONS/2026-07-11-m0-implementation-pins.md`
pin 8) that was never replaced once the real SPEC §2.3 floor-based estimate
existed. It was also emitted on telnet/RBN-uplink with no conversion to the
500 Hz reference bandwidth RBN and CW Skimmer quote. Full research is in the
MAN-102 research document; this record carries the measurements and the
decisions the implementation made.

## Measurements

### The miscalibration reproduced exactly as the ticket described

Golden vector V1 (20 WPM, true SNR +20 dB/2500 Hz, clean AWGN, no fading —
`crates/manta-testkit/src/vectors.rs`) decoded through the release binary at
`49f05a4` reported `snr_db: 11.96` — an 8.0 dB understatement before any
bandwidth correction, consistent with the 2026-09-05 broad review's
independently-measured "slope ≈ 0.7, offset ≈ −5 dB" characterization of the
same rail-based estimator.

### Estimator candidates, measured before implementation

Three aggregations of the detector's floor-based `S − F` estimate
(`Track::current_snr_db`, `crates/manta-engine/src/track.rs`) were rendered
against a single-signal AWGN scene at eight true SNRs and compared:

| Aggregation | Slope | Worst-case bias | Verdict |
|---|---|---|---|
| Instantaneous sample at `TrackMeta` time | n/a (non-monotonic) | ~35 dB swing, negative-SNR spots on strong signals | **Rejected** |
| Key-down mean (hops where `current_snr_db ≥ on_snr_db`) | 0.695 | same compression as the rail estimator it replaces | **Rejected** |
| Peak-hold since the track's last `TrackMeta` | 0.974 | +2.2 dB (shrinking with SNR) | **Adopted** |

The instantaneous sample is a coin flip over a ~35 dB range because `S` is a
40 ms EMA that decays toward the floor on every key-up, and `TrackMeta` fires
once per 375 hops (1 Hz, SPEC §5) at an arbitrary phase of the keying — its
`meta_min` sits near the key-up floor at *every* true SNR. The key-down mean
recovers the same 0.70-ish slope defect as the rail estimator it replaces,
for an analogous reason: the key-down window includes the EMA's rise/fall
ramps, and at 20 WPM a 60 ms dit never fully settles a 40 ms time-constant
EMA, so averaging over the window drags the estimate down proportionally to
the signal's own keying excursion. The peak over the interval is the settled
key-down level — the statistic that actually tracks truth.

### End-to-end, after implementation (this container, this commit)

`crates/manta-engine/tests/snr_calibration.rs`'s ignored characterization
sweep (single-signal AWGN scenes, 60 s each, same noise seed, mean of all
validated spots' `snr_db` per point):

```
true_snr2500  reported
         0.0      2.10
         3.0      4.91
         6.0      7.77
        10.0     11.64
        16.0     17.60
        20.0     21.55
        25.0     26.51
        30.0  (no spots)
```

Slope and residual bias are consistent with the pre-implementation estimate
above (worst-case residual ~+2.1 dB at the low end, shrinking as true SNR
rises). The 30 dB point produces no validated spot at all in this scene —
confirmed unrelated to this fix (present before it too, for the same scene
shape); not investigated further here, see Follow-ups.

V1 end to end: `manta decode v1.wav --json` now reports `snr_db: 21.55`
(release binary, this commit) against a true +20 dB/2500 Hz signal — versus
`11.96` before. The gap to CW Skimmer's ~27 dB reference (500 Hz, the wire
surface's separate +6.9897 dB conversion applied on top) closes accordingly.

### Full workspace stays green

`cargo test --workspace`, `cargo fmt --all --check`, and
`cargo clippy --workspace --all-targets -- -D warnings` all pass clean with
every change in this record applied — including every golden vector, both
chunking-determinism tests, and the full telnet/uplink/JSON acceptance
suites. No test in the repository asserted a specific pipeline-produced SNR
value before this ticket (every existing `snr_db`/`snr_2500_db` fixture is
either a `SignalSpec` generation input or a hand-constructed literal used to
test formatting logic in isolation), so this reproduces the research
document's finding that the regression risk against the existing suite was
in wiring correctness, not pinned numeric expectations.

## Decisions

**D-102.1 — Peak-hold aggregation, not instantaneous or key-down-mean.**
`Track::snr_peak_db` (`crates/manta-engine/src/track.rs`) tracks
`max(current_snr_db)` since the track's last `TrackMeta`, reset once a
`TrackMeta` is actually emitted for that track (driven by the decoder's real
emission, not a duplicated hop counter, so the two mechanisms cannot drift
apart). SPEC §2.3 specifies the *quantity* (`S − F`); this ticket adds the
aggregation SPEC did not previously pin. **Round-1 review correction:** the
first landing reset the window once per `drain_pool` call (batch end), not
at the hop the report actually fired on, which made the reported value
depend on the caller's chunk size (`decode_samples` batches raw samples at
4096, `listen`/`soak_metrics` at 2048) — see Remediation below.

**D-102.2 — Plumb via a `set_snr_2500_db` setter on `TrackDecoder`, mirroring
the existing `set_freq_hz` precedent.** `manta-decode` has no dependency on
`manta-engine` (confirmed from each crate's `Cargo.toml`); the floor/gate
state that computes `S − F` lives one crate above `TrackDecoder`. Rather than
rewriting `TrackMeta` after the fact in `drain_pool`, `TrackManager` calls
`decoder.set_snr_2500_db(peak - SNR_BW_CORR_DB)` before `push_envelope` on
every queued hop, exactly as it already does for `set_freq_hz`. The setter
stores `Option<f32>` and `push_envelope` falls back to
`demod.snr_2500_db()` when unset, so `manta-decode`'s own unit tests and
`manta-testkit`'s roundtrip vectors (which drive a `TrackDecoder` standalone)
keep working unchanged.

**D-102.3 — No empirical calibration constant for the residual bias.** The
peak-hold estimator's residual (+2.1 dB at 0 dB true SNR, shrinking to near
zero by 25 dB) is not corrected with a fitted offset. A constant tuned
against one clean-AWGN vector family would not generalize to fading (V4/V5/
V8w), pileups, or real band noise, and would bake a scene-specific number
into the wire format. MAN-116 (real-RBN-archive calibration) is where a
residual offset gets characterized against real data, if one is warranted at
all — not here, and not on synthetic vectors alone.

**D-102.4 — Exact conversion constant, not the literal `7.0` from D3's
prose.** `RBN_REF_BW_CORRECTION_DB = 6.9897` (`10·log10(2500/500)`,
`crates/manta-server/src/rbn.rs`) and `SNR_BW_CORR_DB = 14.3`
(`10·log10(2500/93.75)`, made `pub` in `crates/manta-decode/src/envelope.rs`
and re-exported at the crate root so `manta-engine` uses the identical
constant rather than a second, independently-defined copy). D3 says "+7 dB";
6.9897 rounds to the same integer for all but a 0.0103-wide fractional
window, and the derived value stays self-documenting against its formula.

**D-102.5 — `Demod::snr_2500_db()` (the M0 rail estimate) is untouched.**
SPEC §4.5's confidence multiplier `q = clamp(SNR_2500/20, 0.3, 1.0)`
(`crates/manta-decode/src/decoder.rs::emit_char`) still calls it directly.
D3 is explicit that "the confidence formula … stay[s] defined in 2500 Hz
exactly as SPEC documents today — nothing about the internal pipeline
changes." The two values were the same number by coincidence before this
ticket (both defaulted to the rail estimate); they are now intentionally
decoupled, not merged.

**D-102.6 — Land independently of MAN-88.** MAN-88 (AK1A column rendering of
the same `rbn::format_line`) is not on `main` at this commit (`git branch -a`
shows only `main`). Its own plan explicitly carves the SNR reference-
bandwidth conversion out of its scope, reserving it for this ticket. No
rebase coordination was needed.

**D-102.7 — Add `snrRefHz` to `SpotMessage` now; write the dispensa proposal
rather than block on it.** `wiki/pages/spot-output-contract.md` records that
dispensa's `spots.v1.schema.json` is "not yet frozen … treat the field set
as design-phase until the ADR lands" — there is no frozen contract this
addition breaks. The residual risk (a strict-mode validator rejecting an
unknown key) is real but bounded to consumers that validate strictly, and is
mitigated by the ready-to-lift schema fragment below.

## Known side effect (accepted, not fixed here)

Under fading (V4, Watterson CCIR-good, nominal +10 dB), the floor-based
peak-hold correctly reports genuine per-window signal-strength swings that
the old compressed rail estimate never surfaced. `manta-spot::dedupe`'s
`SNR_IMPROVEMENT_DB = 6.0` re-spot override threshold now fires somewhat
more often on a fading station as a result. The threshold itself is outside
D3's scope and is not retuned here — see Follow-ups FU-2.

## Remediation (2026-09-07, validate round 1)

`/catalyst-dev:validate-plan` at code-review step FAILed on two CONFIRMED
correctness findings in the first landing, both fixed in this round:

- **Finding 1 — a never-keyed track (steady carrier/birdie) emitted
  `TrackMeta`.** `TrackDecoder::push_envelope`
  (`crates/manta-decode/src/decoder.rs`) read `self.demod.snr_2500_db()` as
  both the SNR *value* and, implicitly, the emit *gate* (it returns `None`
  exactly when `!self.demod.running()`). Falling back to an
  engine-supplied `self.snr_2500_db` — always `Some` in production, since
  `manta-engine` calls `set_snr_2500_db` on every queued hop for every
  track it drives — bypassed that gate. Fixed by gating explicitly on
  `self.demod.running()` before reading either source. Regression test:
  `decoder::tests::track_meta_is_not_emitted_for_a_track_whose_demod_never_initialized`.
- **Finding 2/3 — reported SNR depended on the caller's `process_hops`
  chunk size.** The peak window reset once per `drain_pool` call, at batch
  end, seeded from `current_snr_db` as of the batch's *last* hop — not from
  the hop the report actually fired on. Moved the reset into `drain_pool`
  itself: `Track::pending` now queues the *raw* per-hop SNR, and
  `drain_pool` walks each track's queued items in order, maintaining
  `snr_peak_db` and resetting it the instant a given push's
  `TrackDecoder::push_envelope` call actually returns a `TrackMeta`. This
  makes the window boundary track the real report regardless of batch
  boundaries. Regression test:
  `track::tests::track_meta_snr_is_invariant_to_process_hops_chunk_size`
  (V1 driven through two `TrackManager`s at 4096- and 2048-sample chunks;
  every `TrackMeta.snr_2500_db` compares bit-identical).
- The fallback test's weak assertion (test-coverage finding) was
  tightened: `decoder::tests::track_meta_falls_back_to_the_rail_estimate_when_unset`
  now asserts the emitted value equals `demod.snr_2500_db()` exactly, not
  merely that some `TrackMeta` exists.
- Re-ran `crates/manta-engine/tests/snr_calibration.rs`'s ignored sweep
  after the fix: identical to the table in Measurements above (this V1/V-
  style single-continuous-signal scene's reports happen to land on batch
  boundaries either way, so the chunk-dependence bug never perturbed this
  particular characterization).
- **FU-6** (closing the dangling pointer left by the first landing): the
  30 dB "no spots" point in the sweep above pre-dates this ticket (present
  in the plan's own baseline sweep too) and is out of MAN-102's scope; if
  it needs its own investigation, file it as a new ticket rather than
  tracking it here.

## Cross-repo proposal — ready to lift into dispensa

```jsonc
// dispensa contracts/spots/spots.v1.schema.json -- proposed addition
"snrRefHz": {
  "type": "integer",
  "description": "Reference bandwidth in Hz that `snr` is quoted in. Skimmer sources that follow the RBN/CW Skimmer convention report 500; manta reports its native channel measurement referenced to 2500 on this stream, and applies the 500 Hz conversion only on its telnet/RBN-uplink surface.",
  "minimum": 1,
  "examples": [500, 2500]
}
```

Open question the dispensa-side PR must confirm before merging (not
answerable from this repo — dispensa is not checked out here): whether the
schema sets `"additionalProperties": false`, which would reject this key
from a strict-mode consumer until the schema itself is updated.

## Follow-ups (recorded here — no ticket-filing credential in this container)

- **FU-1** — dispensa: add `snrRefHz` to `spots.v1.schema.json` / ADR-0011
  per the fragment above, and confirm `additionalProperties`.
- **FU-2** — `manta-spot`: re-characterize `SNR_IMPROVEMENT_DB = 6.0`
  (`crates/manta-spot/src/dedupe.rs`) against the floor-based estimate now
  that it produces genuine, larger swings under fading.
- **FU-3** — re-run the `#[ignore]`d Pi 4 `cpu_budget` bench once hardware is
  reachable. The added per-hop cost here is one `f32::max` and one extra
  `f32` carried per queued hop per track; expected inside noise, but the M2
  Pi4 acceptance leg is independently still open (see `CLAUDE.md` Status).
- **FU-4** — decide whether SPEC §4.5's confidence `q` should ever move off
  the rail estimate onto the calibrated floor-based value, once MAN-116's
  real-data calibration exists. Explicitly deferred by D3 and D-102.5 above.
- **FU-5** — MAN-116: characterize the residual peak-hold bias (+2.1 dB at
  the low end of the measured sweep, shrinking with SNR) against real RBN
  archive spots, and decide then — not now, and not from synthetic AWGN
  vectors alone — whether a correction offset is warranted.

## References

- Ticket: MAN-102
- Governing decision: `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D3
- Origin of the M0 stand-in: `docs/DECISIONS/2026-07-11-m0-implementation-pins.md` pin 8
- `crates/manta-decode/src/envelope.rs` — `SNR_BW_CORR_DB` (now `pub`), `Demod::snr_2500_db()` (unchanged)
- `crates/manta-decode/src/decoder.rs` — `TrackDecoder::set_snr_2500_db`, `push_envelope`'s fallback, `emit_char`'s unchanged `q`
- `crates/manta-engine/src/track.rs` — `Track::snr_peak_db`, `step_hop`, `spawn`, `drain_pool`
- `crates/manta-engine/tests/snr_calibration.rs` — the calibration bound test and characterization sweep
- `crates/manta-server/src/rbn.rs` — `RBN_REF_BW_CORRECTION_DB`, `format_line`
- `crates/manta-server/src/spot_message.rs` — `SNR_REF_HZ`, `SpotMessage::snr_ref_hz`
- `docs/SPEC-decode-core.md` §2.3, §4.5, §5 — updated normative text
- `ARCHITECTURE.md` §7 — updated worked example and surface description
- `wiki/pages/spot-output-contract.md` — dual reference-bandwidth note
