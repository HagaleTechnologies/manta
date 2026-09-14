# MAN-56: input packet-loss/malformed counters on the Prometheus `/metrics` endpoint

Follow-up from MAN-22 (PR #75): a malformed HPSDR UDP packet no longer kills
the process, and is counted via `GapStats.malformed_packets` alongside the
pre-existing `dropped_packets`/`gaps_detected` — but none of those three
counters reached `manta-server`'s Prometheus `/metrics` endpoint.
`HpsdrIqSource::gap_stats()` was a plain getter nobody called outside tests.

The ticket framed this as a design decision between two options and asked
for review before an "unsupervised improvisation." This run had no human
reviewer reachable in-session; the decision is recorded here, resolved from
the evidence in the companion research document, not left open.

## Decision: extend the `IqSource` trait (option 1), not CLI-layer metrics-handle passing (option 2)

**D1.** A structurally identical problem already shipped this way.
`IqSource::confirmed_live_handle()` (MAN-55, `crates/manta-input/src/lib.rs`)
is a live-updating, per-source-type, optional signal that must survive `src`
moving into `manta_engine::listen()` and reach `manta-server::Metrics` from
`manta-cli`'s wiring layer — exactly this problem's shape. It resolved as a
default-`None` trait method returning a shared handle, polled generically
after the concrete type is erased to `Box<dyn IqSource>`. There is no reason
for `GapStats`'s counters to resolve differently, and doing so would leave
two competing patterns for the same class of problem in the same codebase.

Option 2 (threading a metrics-handle abstraction through `manta-cli`'s
source-opening functions before erasure) was rejected because the
"before erasure" window barely exists: the concrete `HpsdrIqSource` is only
ever held, unerased, inside `open_hpsdr_source()` in `manta-cli/src/main.rs`,
for the single line between `HpsdrDevice::open()` returning and
`Box::new(...)` erasing it. Threading a `manta_server::Metrics`-shaped
abstraction into that function — and transitively into `HpsdrConfig`/
`HpsdrDevice::open`, hardware-facing `manta-input` APIs with no reason to
know Prometheus exists — would be more invasive than the trait extension,
and would still only cover HPSDR, leaving no reusable mechanism if
kiwi/soapy/audio ever grow their own counters. The trait-extension approach
costs those four source types nothing today (the default `None`/no-op).

No crate-dependency problem exists either way, but the trait extension keeps
the dependency graph *and* the abstraction boundary consistent with
`confirmed_live_handle`: `manta-input` stays unaware `manta-server`/`Metrics`
exist; only `manta-cli` (already depending on both) reads the trait method's
return value and pushes it into `Metrics`.

## Two constraints invisible from the ticket text, found by tracing the code

**D2. Feature gating.** `IqSource` is compiled unconditionally (no `#[cfg]`
on the trait itself), but `GapStats`/`GapDetector` live inside
`crates/manta-input/src/hpsdr.rs`, gated behind
`#[cfg(feature = "hpsdr")] pub mod hpsdr;`, and `hpsdr = []` is a non-default
Cargo feature. Naming `GapStats` (or any `hpsdr`-module type) in the trait's
signature would make the trait — and every non-HPSDR build, including plain
`cargo test --workspace`, which CI runs with no `--features` — require
`--features hpsdr` to compile at all. Resolution: a new crate-root type,
`InputHealthCounters`, defined in `crates/manta-input/src/lib.rs` with no
feature gate. `GapStats` survives unchanged as HPSDR's own snapshot type
(now backed by `InputHealthCounters` under the hood), so no existing test
moved.

**D3. Unenforced wrapper forwarding.** Any `IqSource` wrapper type must
remember to forward a new trait method, or the wrapped source's signal
silently vanishes via the trait's default. `manta-cli`'s
`FixedCenterFreqSource` (used whenever `--dial-freq-hz` is set, which can
wrap an `HpsdrIqSource`) already had this obligation for
`confirmed_live_handle()` and got it right; it now also forwards
`health_counters()`. Not enforceable by the type system — a future wrapper,
or a reviewer missing this one, could still silently drop the signal. Flagged
in both trait methods' doc comments so a reader adding a new wrapper is
warned in the one place they're most likely to be reading.

## Other resolved design points

**D4. `AtomicU64` counters behind an `Arc`, not a mutex-guarded snapshot.**
Mirrors `confirmed_live`'s stated rationale in `hpsdr.rs`: a metrics reader
must never take the packet-pump's lock. `Ordering::Relaxed` throughout —
these are independent monotonic counters with no ordering relationship to
anything else, same as `confirmed_live`. Reading the three counters is *not*
one atomic snapshot across all three; irrelevant for monotonic counters
sampled periodically, and noted in `InputHealthCounters`'s doc comment so a
future reader doesn't mistake it for a bug.

**D5. Push (periodic sample), not pull (scrape-time callback).** A pull
design would store an `Arc<dyn Fn() -> …>` inside `Metrics`, inverting the
boundary `metrics.rs`'s module doc establishes (`manta-server` computes only
what it genuinely knows; the wiring layer injects the rest) and running
foreign-crate code inside the HTTP handler. Push matches both existing
injection precedents (`set_active_tracks`, `set_source_health`), keeps
`Metrics` a plain data holder testable without closures, and costs three
relaxed atomic loads plus one `BTreeMap` insert per tick.

**D6. Poll interval: 1 s** (`manta-cli`'s `INPUT_HEALTH_POLL_INTERVAL`), far
below any realistic Prometheus scrape interval (>= 10s), so a scrape never
sees more than ~1 s of staleness. Deliberately slower than the
`confirmed_live_handle` poll (200 ms), which is tuned for a one-shot startup
transition, not a forever-loop.

**D7. Publish once immediately, then loop.** Without an eager first push,
the three series would be absent from `/metrics` for the first second of
the daemon's life, making "series missing" ambiguous between "not an HPSDR
source" and "just started."

**D8. `manta-server` gets its own `InputHealth` struct; the two crates share
no type.** `manta-cli` — which already depends on both — does the
translation in a small `input_health_of` helper. A named-field struct
rather than positional `u64` args: three same-typed counters are a
transposition footgun no test using equal values would catch.

**D9. The metric family is `manta_input_*`, labeled `{source="…"}`.**
Matches the existing `manta_<area>_<name>_total` convention
(`manta_uplink_sent_total`, `manta_spots_dropped_lagged_total`) and reuses
the exact `source` label `manta_source_health` already uses, so an operator
can join the two families.

**D10. Sources with no packet-loss model publish no series, not a frozen
zero.** `ARCHITECTURE.md` §8 already flags `manta_active_tracks` (served but
never populated) as a misleading pattern for an operator to read as live
data. `set_input_health` is only ever called for sources that return
`Some(..)` from `health_counters()` — today, only HPSDR.

**D11. Multi-DDC labeling: one series per device, not per DDC.** A dropped
Metis packet carries every configured receiver's samples, so the counters
are device-wide by construction (`hpsdr.rs`'s "Loss/reorder handling" module
doc). Every `HpsdrIqSource` handle from one device shares one
`Arc<InputHealthCounters>`. `manta-cli` opens `ddc_count: 1` today, so the
`source="hpsdr"` label is unambiguous; if multi-DDC CLI support lands later,
the single shared series remains the *correct* representation, not a
limitation of this design.

**D12. `GapDetector::observe`/`record_malformed` keep their `&mut self`
receivers** even though `&self` would now suffice (the counters they touch
are atomics behind a shared `Arc`). Narrowing them is a public API change
with no caller benefit — `Inner` holds `&mut self` regardless — and would
widen the diff for reviewers with no corresponding value.

## What this does NOT do

- Does not implement input-*ring*-overrun tracking. `manta-engine::soak`'s
  documented deviation needs an upstream `coppa-audio` API addition
  (`CpalSource` doesn't expose `overflow_count()`), a different root cause,
  out of scope here.
- Does not add counters to `KiwiIqSource`, `SoapySdrIqSource`,
  `AudioIqSource`, or `WavIqSource`. They have none today; they get the
  trait's default `None` at zero cost.
- Does not fix `manta_active_tracks` (served-but-never-populated) or MAN-64
  (`manta_source_health` is one-sided). Separate, already-tracked gaps.
- Does not introduce a Cargo `metrics` feature. `ARCHITECTURE.md`'s old
  "(feature `metrics`)" phrasing was pre-existing drift — no such feature
  has ever existed; the endpoint is unconditionally compiled.
- Does not change `GapStats`, `GapDetector::stats()`, or
  `HpsdrIqSource::gap_stats()`'s public signatures or behavior. Every
  pre-existing test using them passes unmodified.

## Verification

No hardware is reachable from this environment (`CLAUDE.md` Status). The
counters' *semantics* under real RF conditions — that `dropped_packets`
tracks genuine UDP loss on a busy LAN rather than scheduler jitter — is
pre-existing MAN-11/M2 acceptance territory and unaffected by this change
(only where the results are published changed, not the detection
arithmetic). Verified in this environment by: `manta-server`'s
storage/render unit tests and HTTP acceptance test
(`crates/manta-server/src/metrics.rs`,
`crates/manta-server/tests/metrics_acceptance.rs`); `manta-input`'s
`InputHealthCounters` unit tests and HPSDR fake-UDP-device integration tests
(`crates/manta-input/src/lib.rs`, `crates/manta-input/src/hpsdr.rs`);
`manta-cli`'s wrapper-forwarding and translation-helper unit tests
(`crates/manta-cli/src/main.rs`); and an end-to-end acceptance test spawning
the real `manta` binary against a fake loopback HPSDR device and scraping
its real `/metrics` HTTP endpoint
(`crates/manta-cli/tests/hpsdr_metrics_acceptance.rs`).
