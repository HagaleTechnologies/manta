# MAN-194: burst-capable `decoder_input` — delay-compensated refiner drain/prime

## Problem

`manta-dsp::refine::Refiner` is an 11-tap symmetric FIR (`TAPS = 2*GROUP_DELAY_HOPS+1`,
`GROUP_DELAY_HOPS = D = 5`). A symmetric FIR applied causally has a fixed group delay of
`D` hops: `Refiner::push`'s output computed *at* hop `t` is not evidence about hop `t` —
it is the best available evidence about hop `t - D`, because the 11-tap window is
centered `D` taps behind the newest sample. This is stated in `refine.rs`'s own doc
comment and is a property of the filter, not a design choice.

`Track::decoder_input` (MAN-168, PR #178) currently ignores this and reports each hop's
own current `sample_ts`, with no delay compensation, as a shipped stopgap. Three review
rounds on PR #178 each found a new way this bites, all traced to the same root cause: an
interface that must emit exactly one `(amp, ts)` pair per input hop cannot represent
"nothing to report yet" or "here are several observations at once," which is exactly
what correct delay compensation requires at its boundaries:

1. **Reset/EOF discard** — at a channel reassignment or end-of-stream, up to `D` real,
   already-observed hops are sitting in the pipeline (not yet at the point where their
   delayed value is known) and are silently dropped. Measured: a 10-hop mark reduced to
   5 hops on reset; a final mark measured 7 hops instead of 12 at EOF.
2. **Warm-up priming** — the same one-report-per-hop limitation blocks a clean transition
   from "just started, no delayed data yet" to steady delayed reporting without either
   skipping hops (forbidden — collapses character boundaries) or duplicating an
   observation (fabricates evidence).
3. **Transition discontinuity** — PR #178's shipped stopgap (report raw current-hop
   magnitude during warm-up, switch to refined FIR output once "converged") mislabels
   content: the raw report at hop `t` describes hop `t`, but the very next refined report
   describes hop `t - D` — a backward jump in what the label claims to describe, which
   can fabricate a spurious edge or short space.

## Root cause, restated

All three are the same fact colliding with a signature that can only say one thing per
hop: **the correct value for hop `τ` is only knowable once hop `τ + D` has arrived**, and
**it stops being knowable at all once no more input will ever arrive from that source**
(reset or EOF). A burst-capable `decoder_input` — returning `Vec<(f32, f32, u64)>`
instead of `(f32, f32, u64)` — can emit zero, one, or several entries per input hop,
which is what a correct protocol needs at both ends.

## Design

### Steady-state: ring buffer, hold-back, then one-per-hop

Every hop (refinement enabled), `Track` pushes `(raw_amp_at_c, raw_power_at_k,
sample_ts)` onto a `VecDeque` of bounded capacity (`D + 1`) and runs `Refiner::push` as
today. Let `h` = hops pushed since the last reset (0-indexed, first push `h = 0`):

- **`h < D`**: nothing is popped; `decoder_input` returns an empty `Vec`. There is no
  valid delayed observation yet for any `τ = h - D < 0`. No filler is substituted — this
  removes the transition-discontinuity bug by construction, since nothing is ever
  reported "early" for later contradiction.
- **`h >= D`**: pop the front of the queue — by construction it holds exactly the entry
  for `τ = h - D`. Amplitude:
  - `h >= 2D` (the FIR's window for `τ` is fully real, no zero-padding): amp =
    `z_h`, the `Refiner::push` output computed this same hop (it already *is* the
    correct value for `τ` — no separate historical storage needed).
  - `D <= h < 2D` (window still partly zero-padded from the reset): amp = the popped
    entry's own raw magnitude at `τ` — real, correctly time-labeled evidence, just not
    narrowband-refined.
  - Emit one `(amp, raw_power_τ, ts_τ)`.

This reproduces today's per-hop cardinality once `h >= D`; the externally visible change
is that the timestamp carries the correct constant `D`-hop delay and amp/raw_power always
describe the same real instant (never a mix of "now" and "`D` hops ago" as today's
stopgap does).

### Drain: identical at reset and at EOF

`Track::drain_refiner_into_pending()`: pop everything remaining in the ring buffer and
emit each as `(raw_amp_τ, raw_power_τ, ts_τ)`. No attempt is made to improve FIR quality
for these — no more real input is ever coming from this source, so the window can never
become more real than it already is. This is what recovers the measured "10→5" and
"7 vs 12" hop losses: every real hop gets *some* report, even if not always
refined-quality for the last few before a boundary.

Drain is wired into **every** point where a `Track`'s refiner stops receiving new input,
not only the ticket's two literal scenarios — these are all instances of the same "no
more input from this source" event:

- The reset branch inside `decoder_input`, before constructing a fresh `Refiner` for the
  new centroid channel.
- Both `finish_decoder` / `finish_decoder_speed_only` call sites in `TrackManager::step_hop`
  (Merged / Evicted / Silent / HangExpired closures).
- `TrackManager::finish`, for every track still open at true end-of-stream.

The genuinely unrecoverable edge is the first `D` hops of a track's entire life (not a
mid-life reset — those can still drain something): there is no signal before track birth
to pad the FIR window with, so those hops have no evidence to contribute at any quality.
This is an honest, bounded (~13ms), physically-required gap, not a bug.

### Interface

`Track::decoder_input` returns `Vec<(f32, f32, u64)>` (amp, raw_power, ts) — matches
MAN-194's specified signature plus the `raw_power` split PR #178 already introduced.
`Track::pending`'s element type is unchanged (`(f32, f32, Option<f32>, u64)`); call
sites in `step_hop` loop over the returned `Vec` and push each entry, computing
`spectral_ref_power` once per hop (current behavior, unchanged) and reusing it across
every entry in a burst — the six-neighbor floor estimate is slow-moving enough that a
`~13ms`-stale value at burst granularity is a non-issue.

When refinement is disabled (`refine_bw_hz <= 0.0`), `decoder_input` returns a
single-element `Vec` every hop — no buffering, no delay, unchanged from today.

## Files touched

- `crates/manta-dsp/src/refine.rs`: no change to `Refiner`'s FIR math or `push`/`reset`/
  `set_delta` signatures. Module doc comment updated to drop the "the engine does NOT use
  this for delayed pairing" caveat, since that's no longer true.
- `crates/manta-engine/src/track.rs`:
  - `Track`: replace `refiner_hops_since_reset: u64` bookkeeping with the ring buffer +
    hop counter described above; add `drain_refiner_into_pending`.
  - `Track::decoder_input`: new return type and hold-back/emit logic.
  - `TrackManager::step_hop`: both call sites (`Promoted` and `LifecycleEvent::None`
    arms) loop over the returned `Vec`; closure-handling block calls
    `drain_refiner_into_pending` before `finish_decoder`/`finish_decoder_speed_only`.
  - `TrackManager::finish`: calls `drain_refiner_into_pending` for every remaining track
    before its existing finalization.

## Testing plan

- `manta-dsp::refine` unit tests: extend with cases asserting the delay/hold-back
  contract directly (empty for `h < D`, correct `τ` pairing for `h >= D`, raw-vs-refined
  crossover at `h = 2D`) — this is the core, testable-in-isolation logic; push it down to
  `Refiner`-adjacent test code rather than only exercising it through the full engine.
- `manta-engine::track` tests: the ticket's own two Gherkin scenarios (mid-mark channel
  reassignment; final mark at EOF) as direct regression tests, asserting hop counts
  against the ticket's measured numbers (10 hops recovered on reset, 12 on EOF) — measure
  against the real pipeline per this repo's "measure, then pin" convention, not
  hand-derived expectations.
- Full workspace `cargo test` + the golden V-vector suite: V4/V5/V8w (the
  refinement-relevant vectors) need re-verification since the timestamp convention shifts
  by a constant `D`-hop offset — confirm this doesn't move any pass/fail boundary before
  and after.
- `cargo clippy --workspace --all-targets` and `cargo fmt --check`.
- Local `codex exec review --uncommitted` before pushing, per this repo's convention for
  refiner-adjacent changes (MAN-168's PR #178 found three real bugs this way).

## Risks / open questions for implementation

- Exact off-by-one indexing for `h`, `D`, `2D` thresholds needs care and dedicated unit
  tests — the design above is correct at the level of "which regime," not verified
  arithmetic.
- The `VecDeque` capacity and eviction discipline (must never grow past `D+1`, must never
  underflow on pop) needs an invariant check, ideally a debug assertion.
- Re-verify `finish_decoder_speed_only`'s existing contract (never force a decoder to
  resolve a partial character on Merged/Evicted/Silent) is unaffected — draining the
  refiner only delivers already-observed real evidence into `pending`; it does not call
  `TrackDecoder::finish()`, so the distinction PR #154 established is preserved by
  construction, but this should be an explicit test, not just an assertion in prose.
