# 2026-10-15 — MAN-233: SoapySDR overflows are counted and warned, not silent

**Status:** Implemented (branch `MAN-233`). Scenario 2's hardware leg (a
real RSP1B under a starved host) is a post-merge runbook step: CI's
SoapySDR leg drives the `type=null` device, whose reads fail
`NotSupported` and cannot overflow. The ticket itself says "Verification
needs the hardware."

**Ticket:** MAN-233. Source: repository audit 2026-10-05 (findings
MAN-ARCH-002, MAN-REL-005 overflow-counter part). Exists because PR #208
(MAN-128) deferred "SoapySDR overflow events" with the recorded blocker
that the implement container has no `libsoapysdr` and no root — a blocker
reproduced in this planning container and designed around here.

## Problem

On the SoapySDR path — the one used in field tests — a driver stream
overflow means samples were lost, but the event was retried in silence:
`SoapySdrIqSource` had no `health_counters()` override (the trait default
returns `None`), so no `manta_input_*` series existed for
`source="soapy"`, and nothing was logged until 100 *consecutive*
overflows ended the connection. An operator could not tell "the band is
quiet" from "the host is dropping input".

## What changed

- `crates/manta-input/src/soapy.rs`: a shared `Arc<InputHealthCounters>`
  created at `open()`, exposed through `health_counters()`; the overflow
  arm of `read()` calls the free function `account_overflow` (the kiwi
  `account_snd_frame` pattern) which records exactly one gap event and,
  at most once per `OVERFLOW_WARN_INTERVAL` (fixed 60 s), fires a
  constructor-injected notice (`with_overflow_notice`, the audio
  `with_silence_notice` pattern — `manta-input` has no logging
  dependency by convention, so `manta-cli` injects
  `logging::warning(&soapy_overflow_warning(driver))`).
- `crates/manta-cli/src/main.rs`: the `soapy_overflow_warning` helper
  (wording pinned by a test) and the callback wiring in
  `LiveSourceSpec::open`'s Soapy arm.
- Docs: METRICS.md's three `manta_input_*` "Present when" cells,
  ARCHITECTURE §8, the `IqSource::health_counters` trait doc.

The publication pipeline needed no change: the CLI's existing poller
publishes eagerly once (the series exists at 0 from the very first
scrape) and then every 1 s, and `ReconnectingSource::InputHealthTotals`
(MAN-228) keeps the series monotonic across reconnects.

## Decisions

1. **One `record_gap()` per `ErrorCode::Overflow`; never
   `record_dropped`.** An overflow carries no lost-sample count — any `n`
   would be fabricated. The family HELP text ("one gap may account for
   several dropped packets") already states the event-vs-amount split, so
   `dropped_packets` stays 0 and `malformed_packets` stays 0 (no
   malformed-packet concept), both present at 0 from the first scrape —
   the whole per-source snapshot is published or none of it.
2. **Warn via a constructor-injected callback, rate-limited inside the
   source at a fixed 60 s.** The gate lives where the events live, so a
   burst of overflows inside one `read()` logs at most once. 60 s matches
   the MAN-122 status-line default. Rejected: CLI-side diffing in the 1 s
   poller (would change hpsdr/kiwi/ka9q behaviour or need source-specific
   gating in generic wiring); a config key (config-surface cost for
   marginal value — the counter carries the fine-grained data). Each
   (re)open rebuilds source + callback + rate-limit state, mirroring the
   audio silence notice.
3. **`Timeout` counts nothing.** A read timeout is how a quiet band
   presents; counting it would recreate the exact quiet-vs-loss
   conflation this ticket removes. Pinned by the unit test's
   "nothing else" assertion.
4. **Retry bounds and both `bail!` messages are byte-identical**
   (ticket non-goal). The `account_overflow` call is the first statement
   of the overflow arm, before the bound check, so the bound's own
   overflow is counted too.
5. **`take_discontinuity` untouched.** Millisecond ring-buffer drops are
   not connection-scale outages; advancing the sample clock on USB jitter
   would churn track segments. A sample-clock lag gauge and timestamp
   re-anchoring stay deferred until this counter or a soak shows real
   loss (ticket non-goal).
6. **Device-free unit tests via the free accounting function** (crafted
   `Instant` arithmetic, no sleeps) plus a null-device shared-`Arc` test
   under `SOAPY_TEST_LOCK`, because the `soapy` cargo feature cannot be
   compiled in the implement/validate containers — `soapysdr-sys`'s build
   script panics without the native library, and there is no root to
   install it. CI's `test-soapy` leg (clippy `-D warnings` + tests with
   `--features soapy`) is the compile/lint/test gate for every
   soapy-gated line.
7. **No new metric family, renderer change, or config key.** The generic
   publication machinery and the `"soapy"` label already existed; sources
   that count nothing (file, audio) keep publishing nothing ("absent
   means not measured").

## Verification

- Container-runnable: `cargo test -p manta-input --locked` (144 passed,
  2 ignored baseline), `cargo test -p manta-cli --locked`,
  `cargo test --workspace --locked`, `cargo fmt --all --check`, `cargo
  clippy --workspace --all-targets --locked -- -D warnings`, and
  `cargo test -p manta-cli --locked --test docs_consistency`.
- CI-only: the `test-soapy` leg runs the new `account_overflow` /
  null-device tests and lints the soapy-gated lines.
- Hardware (post-merge runbook step): on the RSP1B, starve the host
  during a sustained 192 kS/s capture; confirm
  `manta_input_gaps_detected_total{source="soapy"}` increases and at most
  one warning line per 60 s appears.

## Rollback

Additive metric series and one log line; no behaviour change. Revert is
one commit.
