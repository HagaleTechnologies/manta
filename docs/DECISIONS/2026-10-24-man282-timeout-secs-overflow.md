# MAN-282: an absurd `--timeout-secs` fails with a plain error — 2 for `manta status`, 1 for `manta health` — instead of a panic

**Status:** accepted. The `checked_add` fix is in `query_daemon`
(`crates/manta-cli/src/main.rs`), pinned by two regression tests.

## Context

Deferred from PR #95 (MAN-44) review round 2, Codex P2 thread
`PRRT_kwDOTQvU8M6o6jWX`, per [the PR review convergence
policy](2026-08-07-pr-review-convergence-policy.md). MAN-81's validation had
already flagged the same bug independently and deferred it here.

`--timeout-secs` is a clap-valid `u64` on both `manta status` and `manta
health`, with no range validation. The helper both commands share,
`query_daemon`, computed its one end-to-end deadline as
`tokio::time::Instant::now() + timeout`. For values whose deadline is
unrepresentable (e.g. `u64::MAX`), that add panics: tokio's `Instant +
Duration` defers to std's, whose impl is
`checked_add(...).expect("overflow when adding duration to instant")` — an
unconditional `.expect`, so debug and release panic identically. `manta status
--timeout-secs 18446744073709551615` exited 101 with a backtrace before any
request, bypassing the documented exit-2 "couldn't ask" path; `manta health`
panicked the same way.

## Decision

Fail — don't clamp — at the one shared site. `query_daemon` now computes

```rust
let deadline = tokio::time::Instant::now()
    .checked_add(timeout)
    .ok_or_else(|| anyhow!("invalid --timeout-secs {timeout_secs}: too large"))?;
```

and the `Err` flows through exit-code plumbing that already existed:
`Command::Status`'s handler maps any `run_status` error to exit 2, and
`health_cmd::run` maps any probe error to `manta health: {e}` + exit 1. Zero
new error-handling code outside the helper.

- **`checked_add`-and-fail, not a clap `value_parser` range.** A range needs
  an invented ceiling duplicated on both subcommands' arg definitions, and it
  would reject values below the real overflow boundary, which varies with how
  long the host has been up (`Instant` wraps a monotonic clock).
  `checked_add` rejects exactly the inputs the platform cannot represent:
  verified `--timeout-secs 1000000000000` (≈31,700 years, representable on
  this host) still takes the ordinary refused/exit-2 path, so no script's
  legitimate large bound is foreclosed. The acceptance boundary is the
  platform's, not an invented constant.
- **Not clamping.** Tokio's own crate-private `far_future()` comment notes
  "1000 years overflows on macOS, 100 years overflows on FreeBSD" — any
  numeric cap is platform-fragile — and a clamp silently substitutes a
  different bound than the operator asked for.
- **One fix site covers both commands.** `run_status` and `health_cmd::probe`
  both call `query_daemon`.
- **`manta health` exits 1, not 2.** Health never exits 2 — Docker reserves
  it. The ticket's Gherkin names only `manta status`, whose fixed behavior is
  the required exit 2 with a plain `invalid --timeout-secs` message; health's
  fixed behavior for the same input is exit 1 per its existing never-2
  contract.

## What shipped

The one-line fix, two regression tests
(`status_with_an_absurd_timeout_secs_exits_two_with_a_plain_message` in
`crates/manta-cli/tests/cli.rs`,
`health_with_an_absurd_timeout_secs_exits_one_not_two` in
`crates/manta-cli/tests/health_command.rs`), and this record. The flag's
defaults (5 for status, 3 for health), its help text, the Docker
`HEALTHCHECK`'s `--timeout-secs 3` (`docs/RUNBOOKS/node-health.md`), and the
runbook's exit-code table are unchanged: `docs/RUNBOOKS/uplink-health.md`'s
exit-2 row ("could not reach or parse the daemon's status") already covers
this failure class, as it does bad `--addr` and unreadable configs today.

Verified in debug and release: absurd status → exit 2 with `error: invalid
--timeout-secs 18446744073709551615: too large`; absurd health → exit 1 with
the same line prefixed `manta health: `; `--timeout-secs 1` and
`--timeout-secs 1000000000000` against a closed port → the unchanged
refused/exit-2 path. `cargo test -p manta-cli --locked --test cli status` →
4 passed; `--test health_command` → 12 passed.

## Related

- [MAN-44 uplink status surface](2026-09-04-man44-uplink-status-surface.md):
  the CR-A exit-code contract (status 0/1/2) this fix stays inside.
- [MAN-81 Docker config mount and healthcheck](2026-10-11-man81-docker-config-mount-and-healthcheck.md):
  health's never-2 contract; the same PR's validation independently flagged
  this bug and deferred it here.
- [PR review convergence policy](2026-08-07-pr-review-convergence-policy.md):
  the rule that made this a ticket instead of an inline PR #95 fix.
