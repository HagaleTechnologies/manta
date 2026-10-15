# 2026-10-14 — MAN-248: a per-minute ceiling on spots the RBN uplink forwards

**Status:** Implemented (branch `MAN-248`). Audit finding MAN-SEC-008
(P2, defense-in-depth): once `dry_run` is off, every bus spot is written
to each `[[rbn_uplink]]` target with no bound on volume, under the
operator's callsign.

## Context

The threat model accepts that input authenticity is unknowable
(`docs/DECISIONS/2026-09-02-man23-threat-model.md`): a faulty or
falsified RF input can produce validated-looking spots. Upstream
validation bounds what counts as a spot, not how many. So a fault could
turn a manta node into a high-volume source of wrong reports on the
shared network, at the cost of the operator's standing and other users'
data.

At `9636fe0`, `forward_loop` (`crates/manta-server/src/uplink.rs`)
checked `spot_types` then `dry_run`, then wrote every remaining spot
immediately — no outbound rate budget existed anywhere on the path (the
only `RateLimiter` there bounded lines received FROM the target, and its
overrun disconnects).

## Decisions

- **D1 — per-target key `max_spots_per_minute` (u32, default `600`,
  `0` disables).** Delivery, suppression, and every existing uplink
  counter are per target; two targets each forwarding the same bus spot
  are separately liable for their own volume. `0` = disabled mirrors the
  documented `*_per_ip` budget keys ("`0` removes the limit",
  `docs/CONFIG.md`) and `IpRateLimiter::new_with_override`'s
  `Some(0) -> u32::MAX`; the implementation maps `0 -> u32::MAX` so the
  hot path stays branch-free.
- **D2 — default `600`.** 600/min = 10 spots/s, roughly twice a dense
  contest single-passband estimate (~50 concurrently skimmable signals
  re-spotting at most every ~10 s ≈ 300/min). Generous first, tightened
  on counter evidence, per the ticket's rollout note. **The owner's
  human gate from the ticket — decide the production default from
  measured peak rates — stays open**. The peak rate below the ceiling
  comes from `increase(manta_uplink_target_sent_total[1m])`;
  `manta_uplink_ceiling_suppressed_total` shows only when the ceiling
  is exceeded.
- **D3 — reuse `RateLimiter`, owned by `serve_with_live_dry_run`,
  above the reconnect loop.** A per-connection budget would reset the
  flood bound on every reconnect — precisely when a flappy target can
  multiply volume. Fixed 60 s window semantics (resets at the first
  event after the window elapses), the repo's established budget
  semantics, documented in the CONFIG.md row.
- **D4 — check AFTER the dry-run check, immediately before the write.**
  A dry-run spot is never "about to be written", and `dry_run` flips
  live via SIGHUP (MAN-78) — dry-run periods must not consume
  forwarding budget. Over budget: count and skip; no disconnect, no
  queue, no error — the connection stays up, the opposite of the
  response limiter's overrun handling.
- **D5 — a separate counter, not the combined `suppressed`.**
  `manta_uplink_suppressed_total`'s HELP text promises "dry-run or its
  spot_types setting" and `docs/METRICS.md` pins that meaning; the
  ticket requires a distinguishable suppressed-by-ceiling counter. New
  families: `manta_uplink_ceiling_suppressed_total` (aggregate) and
  `manta_uplink_target_ceiling_suppressed_total{target}`.
- **D6 — restart-only, like every uplink key except `dry_run`.**
  SIGHUP reloads only `dry_run`; `reload.rs`'s existing
  `rbn_uplink[N].<key>` classification needs no change, and a changed
  ceiling correctly blocks the dry-run flip in that reload (existing
  `dry_run_skipped` logic).
- **D7 — visible at startup and in `config check`.** Both startup log
  lines and the `config check` summary print
  `max_spots_per_minute={value}` (MAN-159's visible-safe-default
  precedent: a limit nobody can see is only half a fix).

## What is NOT in scope

Authenticating inputs (non-goal; threat model); which spot types are
forwarded (MAN-91); validation thresholds (MAN-95); uplink
liveness/logging (MAN-44's note, MAN-90); queueing or deferred retry of
over-budget spots (the ticket says count and skip); any `/status`
surface change (StatusDoc is MAN-44's document; `SUPPR` keeps its
documented dry-run/spot_types meaning).

## Verification

- `crates/manta-server/tests/uplink_acceptance.rs`:
  `forwarding_stops_at_the_ceiling_and_the_connection_stays_up`
  (ceiling 3, burst of 5: exactly 3 written in order, 2 counted, 0
  reconnects, health `connected`, and a follow-up spot suppressed with
  the connection still up),
  `dry_run_spots_consume_no_forwarding_budget`,
  `zero_max_spots_per_minute_disables_the_ceiling`, and the
  ceiling-stays-0 assertion added to
  `spot_types_all_forwards_every_type` (ticket scenario 3).
- `crates/manta-server/src/config.rs`:
  `uplink_max_spots_per_minute_roundtrips_and_rejects_bad_values`
  (defaults, `0`, `-1`/`1.5`/`plenty`/typo rejected).
- `crates/manta-server/src/metrics.rs`:
  `ceiling_suppression_renders_and_stays_out_of_the_combined_suppressed_counter`.
- The 60 s window reset itself is pinned by `rate_limit.rs`'s existing
  paused-clock tests (reused helper; no new semantics introduced).

## Rollout / rollback

Behaviour changes only above 600 forwarded spots/min/target. Rollback:
set `max_spots_per_minute = 0` (restart), or revert. A sustained rise in
`manta_uplink_ceiling_suppressed_total` during genuine traffic means the
ceiling is too low: raise it or set `0` (restart). A rise from a runaway
input means fixing the input or setting `dry_run = true` (SIGHUP, on
Unix, when it is the edit's only `[[rbn_uplink]]` change). Lowering the ceiling
is only for a node whose forwarded rate stays well below it;
`docs/RUNBOOKS/uplink-health.md` says how.
