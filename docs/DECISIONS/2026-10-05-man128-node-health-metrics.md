# 2026-10-05 — MAN-128: node health from metrics alone

**Status:** Implemented (branch `MAN-128`). Records the design decisions made
while implementing MAN-128's four Gherkin scenarios: per-band/per-type spot
counts, a real `/healthz` endpoint, a decode-latency histogram plus process
uptime/build-info, and per-target RBN uplink labels — plus the ticket's
technical-notes item of generalizing MAN-56's gap-stat wiring beyond HPSDR.

## Context

The 2026-09-05/06 broad review (`docs/DECISIONS/2026-09-06-broad-review-decisions.md`)
flagged these as observability gaps distinct from three already-tracked
tickets this one does not duplicate: MAN-45 finding 3 (`manta_active_tracks`,
merged), MAN-64 finding 1 (`source_health` one-sidedness, still open as PR
#100) and MAN-56 (HPSDR-specific gap-stat wiring, merged). See the MAN-128
research document (`thoughts/shared/research/`) for the full before/after
scrape evidence.

## Decisions

- **D1 — new family, not a label on the aggregate.** `manta_spots_total`
  stays unlabeled; `manta_spots_by_band_total{band,type}` is a new, separate
  family incremented in the same `record_spot(&Spot)` call, so the two can
  never drift apart and existing dashboards/tests reading the aggregate are
  unaffected.
- **D2/D3 — decode latency is per-chunk wall-clock time**, histogrammed with
  bucket bounds `0.0005` … `2.5` s, spanning every table rate's chunk budget
  (2048 samples / fs) with headroom on both sides. Of the three candidate
  intervals the research document traced (per-chunk, signal-to-spot,
  scrape-freshness), per-chunk is the only one that is a true distribution
  and needs no new cross-boundary plumbing; signal-to-spot latency is
  deferred (see below).
- **D4 — uptime is captured at `Metrics::new()`,** not from `resolve_epoch`
  (which is the *recording's* mtime on file replay, not a process-start
  time). `manta_start_time_seconds` (Unix, for humans) and
  `manta_uptime_seconds` (monotonic `Instant`, immune to NTP steps).
- **D5 — build-info via a new `crates/manta-cli/build.rs`.** `MANTA_GIT_SHA`
  env override, else `git rev-parse --short=12 HEAD`, else `"unknown"` —
  never fails the build. No `Cargo.toml` edit needed (verified: Cargo
  auto-detects `build.rs`). `.dockerignore` excludes `.git`, so a Docker
  build reports `"unknown"` honestly rather than failing.
- **D6 — per-target uplink registry.** A `UplinkTarget` handle (atomics),
  registered on `Metrics` in config order before `uplink::serve` is spawned
  (so a disabled or never-connected target still renders). The seven
  existing aggregate series are **derived by summing** over the registry
  instead of being separately-mutated shared atomics, structurally removing
  the MAN-42 class of cross-target desync bugs.
- **D7 — label is `host:port`**, with `#N` appended to the 2nd+ exact
  duplicate, label values escaped. **Matches PR #95 (MAN-44)'s own choice**
  so the two converge on one registry instead of two.
- **D8 — disabled targets still render**, with `manta_uplink_target_enabled 0`,
  so "configured but off" is visible and distinct from "not configured".
- **D9/D10 — `/healthz` health model.** Healthy only when: every registered
  source is healthy (`source_health` non-empty, all true), every registered
  listener is up (`listener_up` non-empty, all true), and the decode loop has
  made measurable progress within `DECODE_STALL_THRESHOLD` (10 s) since it
  was armed (or was never armed — library use only). `200 OK`/`ok\n...` or
  `503 Service Unavailable`/`unhealthy\n...`, one line per check. The
  **uplink is deliberately excluded**: an RBN outage must not make an
  orchestrator restart a node that is decoding fine.
- **D11 — `manta_healthy`/`manta_listener_up{listener}`** on `/metrics` are
  computed by the exact same `health_at(now)` evaluation `/healthz` uses, so
  the two surfaces can never disagree (the ticket title: "from its metrics
  alone").
- **D12 — listener-up tracking.** `start_spot_server` marks each listener up
  after its bind succeeds, and a watcher task marks it down if its `serve`
  task ever completes (those loops never return normally, so completion
  means a panic/abort).
- **D13 — generalizing gap-stats means `KiwiIqSource::health_counters()`.**
  An SND frame shorter than its 17-byte header is malformed. A forward `seq`
  jump of `1 < Δ ≤ 65536` is one gap event with `Δ−1` dropped frames; `Δ==0`
  or a larger/backward jump re-baselines silently (most likely a
  server-side counter reset, not real loss — kiwirecorder.py's own `seq`
  handling has no narrower documented wrap tolerance). Soapy is deferred: it
  cannot be compiled or tested in this environment (no `libsoapysdr-dev`).
- **D14 — new `crates/manta-server/src/health.rs`**, not `status.rs`: PRs #95
  and #136 both already claim that filename for unrelated work.
- **D15 — phase order** (spot counts → uptime/build-info → latency
  histogram → uplink registry → `/healthz` → Kiwi gap-stats → e2e test/docs)
  minimizes rework; each phase leaves the workspace green independently.

## Convergence with other open PRs

- **PR #95 (MAN-44, per-target uplink + `/status`)**: chose the identical
  `manta_uplink_target_{connected,sent_total,suppressed_total,reconnects_total}{target="host:port"}`
  names and escaping (D7). When it rebases, it should adopt this registry
  and keep only its own `/status`/flapping-classification logic on top.
- **PR #100 (MAN-64, per-IP metrics budget + terminal `source_health`
  write)**: composes with the `source` check without change — its terminal
  write simply makes that check fail too, once it lands.
- **PR #136 (MAN-122, startup banner/status line)**: its
  `record_pipeline_batch` progress counter overlaps conceptually with the
  latency histogram's `_count`. Whichever lands second should reuse the
  other rather than keep two progress signals.

## Deferred (follow-up tickets, no Linear access from this environment)

1. SoapySDR overflow events → `manta_input_*{source="soapy"}`.
2. Pass `MANTA_GIT_SHA` as a Docker build-arg (`Dockerfile` and
   `release-publish.yml` — both CODEOWNERS-gated, out of this ticket's
   scope).
3. A signal-to-spot (track onset → first spot) latency metric.
4. Live-verify Kiwi seq-gap counting against a public receiver (the existing
   live test is `#[ignore]`; no network receiver is reachable in CI or this
   environment).

## References

- Ticket: MAN-128
- Research: `thoughts/shared/research/2026-09-07-MAN-128-*.md`
- Plan: `thoughts/shared/plans/2026-10-05-MAN-128-*.md`
- `ARCHITECTURE.md` §8, `docs/RUNBOOKS/node-health.md`,
  `docs/RUNBOOKS/network-exposure.md`
