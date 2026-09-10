# Variable-width capture (decimated channelizer input): Design

Design for issue #169, motivated by tonight's live-hardware investigation
(`docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`,
`docs/DECISIONS/2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`): a
narrower 96 kHz capture produced the first plausible real CW catch of the
night, at half manta's usual 192 kHz passband. Today the channelizer's
input rate is whatever the SDR natively reports — there is no decimation
stage between source and channelizer, so the only usable capture
bandwidths are whichever of the hardware's native rates happen to satisfy
`fs/93.75` being a power of two (`crates/manta-dsp/src/channelizer.rs`).
On the RSP1B, that's 96k/192k/384k/768k only — no way to reach the ~48 kHz
bandwidth conventional CW-skimmer software uses on the same hardware.

## 1. Scope

In scope:

- New `manta-dsp::decimate` module: a cascaded halfband FIR decimator
  (decimate-by-2, run `log2(factor)` times) operating on `Complex32` IQ,
  analogous in structure to `manta-dsp::proto`'s prototype-filter design.
- A new `DecimatingSource` wrapper in `manta-input` implementing
  `IqSource`, composing an inner boxed source with the decimator cascade.
- A new `--capture-rate-hz` CLI flag (optional, default = source's native
  rate = today's behavior) wired through `manta-cli`'s `open_source`/
  `open_hpsdr_source` for soapy, hpsdr, and kiwi sources.
- One new golden vector (SPEC-decode-core.md §7): a clean-signal scene
  synthesized at 192 kHz, decimated through the real cascade to 48 kHz,
  run through the full `listen()` pipeline end-to-end.
- Decimator-level unit tests (passband flatness, stopband rejection past
  the new Nyquist, DC gain, determinism) mirroring `proto.rs`'s test shape.
- ARCHITECTURE.md §3 and SPEC-decode-core.md updates (new §1.x decimation
  section, `input.capture_rate_hz` config key, new vector entry).

Out of scope (explicitly deferred):

- Concurrent multi-pipeline capture (simultaneous wide + narrow
  channelizers off one or more sources). This ticket is single-width-per-
  session: one `IqSource` (now possibly wrapping a decimator) feeds exactly
  one channelizer feeds exactly one `TrackManager`, same shape as today.
  Real orchestration/output-routing work for concurrent pipelines is a
  follow-up issue if ever wanted.
- `AudioIqSource`: already fixed at 48 kHz (`TARGET_RATE_HZ`), no
  decimation applicable or needed. Its dormant `coppa-audio` resampler gap
  (no `rubato` dep, no `mod resampler;`) is unrelated — that's an
  *upsampling-from-arbitrary-device-rates* problem, not this ticket's
  *downsampling-a-power-of-two-multiple* problem, and stays out of scope.
- General arbitrary-ratio resampling. Every realistic target rate here
  (192k, 96k, 48k, 24k...) is an exact power-of-two divisor of a
  power-of-two-table-compatible source rate, so only exact-ratio halfband
  decimation is needed, never a fractional resampler.
- A pileup-style (V8-shaped) vector at the new rate. One clean-signal
  vector proves the decimator doesn't corrupt decode; broader multi-signal
  coverage at 48 kHz can follow later if real-world use surfaces a need.

## 2. Components

### `manta-dsp::decimate` (new)

A cascade of halfband FIR decimate-by-2 stages, `log2(factor)` of them
chained, where `factor = native_fs / target_fs` (validated to be an exact
power of two at construction — no fractional/non-power-of-two factor is
accepted).

- **Filter design:** Kaiser-windowed halfband lowpass, cutoff at
  `fs_in/4` (the new Nyquist after decimate-by-2), same
  `bessel_i0`/bessel-Kaiser machinery `manta_dsp::proto` already has —
  reused directly rather than reimplemented. A halfband filter has every
  other tap forced to zero by construction (except the center tap),
  which both halves the multiply-accumulate cost per output sample and
  keeps passband ripple/rolloff well-behaved near DC, where CW tones live.
  Same target stopband attenuation as the channelizer prototype (80 dB,
  SPEC §1.2's `KAISER_BETA`) so the decimator doesn't become the weakest
  link in the alias-rejection chain.
- **Structure:** `Decimator::new(fs_in: f64, factor: usize) ->
  Result<Self, String>` — errors if `factor` isn't a power of two, or if
  `fs_in / factor` doesn't itself satisfy the channelizer's `fs/93.75`
  power-of-two constraint (same validation shape as
  `Channelizer::new`/`SingleChannelExtractor::new`, so a bad
  `--capture-rate-hz` fails at construction, not silently downstream).
  Internally builds `log2(factor)` halfband stages, each a direct-form FIR
  + drop-every-other-sample, chained front to back.
- **`process(&mut self, iq: &[Complex32]) -> Vec<Complex32>`:** streaming,
  same sliding-buffer-with-`read`-index pattern as
  `single.rs::SingleChannelExtractor` and `channelizer.rs::Channelizer` (so
  it composes cleanly across arbitrary input chunk sizes — required since
  `DecimatingSource::read` is driven by the caller's buffer size, not a
  fixed hop).
- **Tests** (mirroring `proto.rs`): DC gain ≈ unity through the full
  cascade; passband flatness near DC; stopband rejection ≥ 78 dB past the
  new Nyquist; determinism (same input twice → identical output, bit-for-
  bit); chunked-vs-whole processing equivalence (same pattern as
  `channelizer.rs::process_across_multiple_calls_matches_one_call`).

### `manta-input::DecimatingSource` (new, in a new `decimate.rs` or folded
into `lib.rs` alongside the `IqSource` trait — decided at implementation
time based on file size)

Wraps `Box<dyn IqSource>` + a `manta_dsp::decimate::Decimator`:

```rust
pub struct DecimatingSource {
    inner: Box<dyn IqSource>,
    decimator: manta_dsp::decimate::Decimator,
    fs_out: f64,
    // internal: buffered decimated samples not yet returned to the caller,
    // since factor-of-N decimation of an arbitrary-length inner read()
    // won't line up 1:1 with the caller's requested buf.len()
}
```

- `sample_rate()` returns the *decimated* rate — callers (`listen()`,
  `Channelizer::new`) need no changes; they already just call
  `src.sample_rate()` and build the channelizer from whatever they get
  back.
- `center_freq_hz()` passes through unchanged (decimation doesn't shift
  the RF center).
- `read()` pulls from `inner`, runs it through `decimator.process`,
  buffers any leftover decimated samples, and fills the caller's `buf`
  from that internal buffer — same "drain what's available, keep the
  remainder" shape the decimator's own internal buffer already uses, one
  level up.
- Constructed only when `target_rate_hz` differs from the inner source's
  native rate; `manta-cli` skips the wrapper entirely (zero overhead, zero
  behavior change) when `--capture-rate-hz` is omitted or equals the
  native rate.

### CLI wiring (`manta-cli`)

- New optional flag `--capture-rate-hz: Option<f64>`, added to each
  subcommand's soapy/hpsdr/kiwi option group alongside the existing
  `--soapy-rate`/`--hpsdr-rate` flags (not `requires`d by them — it's
  optional and orthogonal to which source is selected).
- `open_source`/`open_hpsdr_source`: after opening the real device at its
  native rate (unchanged), if `capture_rate_hz` is `Some(r)` and `r !=
  native_fs`, wrap the boxed source in
  `manta_input::DecimatingSource::new(source, r)?` before returning it.
  Construction errors (non-power-of-two factor, non-table target rate)
  surface as the normal `anyhow` `Err` path already used for every other
  open-time validation failure here.
- Kiwi: same treatment — KiwiSDR's server-side rate is also effectively
  fixed per connection, so the same wrapper applies uniformly, per the
  "all live RF sources" scope decision.

## 3. Data flow

```
SDR hardware (native rate, e.g. 192 kHz)
  -> SoapySdrIqSource / HpsdrDevice / KiwiSource  (unchanged, IqSource)
  -> [DecimatingSource, only if --capture-rate-hz < native]  (new)
  -> Channelizer::new(effective_fs, ...)   (unchanged call site)
  -> TrackManager / decode pipeline         (completely unchanged)
```

`manta-engine::listen` needs **zero code changes** — it already only ever
calls `src.sample_rate()` once, up front, and builds everything downstream
from that value. The decimator is invisible to it by design.

## 4. Golden vectors / determinism

New vector (SPEC-decode-core.md §7, tentatively "V31" or a `§7.2`
decimation-specific table depending on how §7's numbering is organized at
implementation time — decided when the section is actually edited):

- **Scene:** same clean-20 signal shape as V1 (20 WPM, +20 dB, offset
  +12.34 kHz, W1AW), synthesized at `fs = 192 000` by `manta-testkit`
  (not 96 000 — needs to start above the 48 kHz target to exercise a real
  decimate-by-4 cascade).
- **Path:** testkit IQ -> `Decimator::new(192_000.0, 4)` -> 48 kHz IQ ->
  full `manta-engine::listen`-equivalent batch decode (same harness style
  `decode_wav`/existing V1-V10 tests use) at `fs = 48 000`.
- **Pass criteria:** same as V1 — char accuracy = 100%, 1 track, freq
  error ≤ 10 Hz (the fine-frequency interpolation should be unaffected by
  decimation since it operates entirely in the post-decimation channel
  grid).
- **Determinism:** same 3-runs-identical-SHA-256 requirement (§6) applies;
  the decimator's cascade must be as deterministic as every other stage
  (sequential f64 accumulation internally, matching `channelizer.rs`/
  `single.rs`'s existing convention).

This is new coverage `channelizer_multisignal.rs` doesn't provide — that
suite proves the *channelizer* generalizes across table sizes when fed
clean-at-that-rate IQ; it says nothing about a signal that was captured at
one rate and decimated down, which is the actually-new code path here.

## 5. Docs

- **ARCHITECTURE.md §3:** add the decimation stage to the input-layer
  description — SDR reports native rate, an optional decimator narrows it
  to the operator-selected `capture_rate_hz` before the channelizer ever
  sees it.
- **SPEC-decode-core.md:** new subsection (after §1, before §2 — exact
  numbering decided at edit time) documenting the halfband design
  (cutoff, Kaiser beta/stopband target, cascade rule), the
  `input.capture_rate_hz` config key added to §9's table, and the new
  vector added to §7.
- **CLAUDE.md Status line:** update once implemented to note variable-
  width capture as done, per this repo's "update docs as part of the
  change" convention.

## 6. Testing summary

- `manta-dsp::decimate` unit tests: DC gain, passband flatness, stopband
  rejection, determinism, chunked-vs-whole equivalence.
- `manta-input::DecimatingSource` tests: wraps a synthetic in-memory
  `IqSource` (same `FixedFreqSource`-style test double `listen.rs` already
  has), asserts `sample_rate()` reports the decimated value, asserts
  output sample count matches the expected decimation ratio over a known
  input length.
- New golden vector in `manta-testkit`/wherever V1-V10 live, per §4 above.
- `manta-cli` integration: a construction-time error test for an invalid
  `--capture-rate-hz` (non-power-of-two factor, or a target that doesn't
  satisfy the channelizer's own table constraint) surfacing as a clean
  `Err`, not a panic — matching every other CLI validation test's shape.
