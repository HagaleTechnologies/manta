# MAN-281: `manta status` saturates a spoofed endpoint's json/ws client count

Date: 2026-10-24. Ticket: MAN-281, a Codex P2 finding from review round 2
of PR #95 (MAN-44), thread `PRRT_kwDOTQvU8M6o6jWQ`, deferred under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md` (from round 2
on, a P2 finding becomes a ticket).

## Context

`render_human` (`crates/manta-server/src/status_doc.rs`) printed
`doc.json_clients + doc.ws_clients`. Both fields are `i64` and arrive
unchecked from whatever `manta status --addr` reaches: `parse_status_doc`
(`crates/manta-cli`) checks only `schema_version`, so a spoofed `/status`
body with `json_clients = 9223372036854775807` and `ws_clients = 1` is
schema-valid. That sum panicked with `attempt to add with overflow` in a
debug build and rendered `json/ws clients  -9223372036854775808` in a
release build. It is the numeric sibling of PR #95's terminal-escape
hardening of the same document
(`docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md`).

## Decision

The sum is `doc.json_clients.saturating_add(doc.ws_clients)`. An
overflowing pair renders `json/ws clients  9223372036854775807` in both
profiles; any pair that does not overflow renders exactly as before
(`2 + 1` is still `json/ws clients  3`). Saturation was chosen over
summing as `i128` because it meets the ticket's "saturated (or widened),
never negative" criterion in one call and keeps the printed figure inside
the fields' own type.

## Evidence

- Red: `human_render_saturates_overflowing_client_counts`
  (`crates/manta-server/src/status_doc.rs`) failed on unmodified `be8e088`
  with `attempt to add with overflow` at `status_doc.rs:248:9`; 0 passed,
  1 failed.
- Green: `cargo test -p manta-server --lib --locked status_doc::` is 13
  passed, 0 failed (the 12 existing status tests plus the new one).
  `cargo test -p manta-server --lib --locked --release
  status_doc::tests::human_render_saturates_overflowing_client_counts --
  --exact` passes, so release no longer wraps.

## Not done

- No schema, `STATUS_SCHEMA_VERSION`, `--json`, health or exit-code
  change: the fields stay `i64` and the JSON output does no arithmetic.
- A negative peer value is still rendered as sent (`telnet_clients` is
  printed alone, and two negative counts can still sum to a negative
  figure); range-checking peer counts is outside this ticket, which
  covers only the overflow of the sum.
- The uplink totals summed in `StatusDoc::from_metrics` are unchanged:
  they run over the daemon's own `Metrics` before serialization, not over
  peer input.
