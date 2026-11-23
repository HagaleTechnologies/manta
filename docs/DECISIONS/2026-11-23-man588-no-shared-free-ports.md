# MAN-588: no shared released-port free_ports(), and the golden roots include their helpers by path

Date: 2026-11-23
Ticket: MAN-588 (refactor, test infrastructure)
Status: decided and built; proved on Linux in a 32-core container. The
Ubuntu and macOS required checks run on the pull request, not here.
Extends: [MAN-523](2026-11-11-man523-loopback-test-stability.md) D3 (which
test targets are socket-repeat candidates).
Supersedes: the "port reservation" in `common/launch.rs` that
[MAN-549](2026-11-08-man549-shared-integration-test-helpers.md) named, and
the sentence in
[MAN-569](2026-11-20-man569-daemon-test-port-handoffs.md) D3 that kept the
shared definition "for fixtures that hold no daemon". Both records stay as
written.

## Context

MAN-569 moved every daemon fixture off `free_ports()`, which bound
ephemeral listeners, read their numbers and dropped them before returning.
Its guard, `shared_test_helpers.rs`'s
`no_integration_test_uses_released_free_ports`, failed any use of the
token outside three files: `doctor_checks.rs` (its private band
allocator), `common/launch.rs` (the definition) and `common/mod.rs` (the
re-export). The definition stayed "for fixtures that hold no daemon
(MAN-570's main)".

That reason no longer holds. MAN-570's planned caller, the two unreachable
uplink targets in `node_health_acceptance.rs`, uses
`unreachable_target_ports()` instead. No integration test calls the
helper, and the guard already fails any call outside the three files, so a
fixture that holds no daemon could not call it either. What was left was a
helper a new test author would find and copy, and a guard that let a new
definition or re-export in `common/` pass.

PR #606 (MAN-568) deleted the helper and ran into CI cost.
`scripts/socket-test-repeat.py` treats every `.rs` file under
`tests/common/` as part of each `mod common;` target. A target is a
candidate when that closure binds a socket, and `common/mod.rs`'s
`fake_server` and `common/ka9q.rs` do. So an edit under `tests/common/`
repeats every test of every `mod common;` target ten times. Six roots
(`golden_v8`, `golden_v8w`, `golden_vc3`, `golden_close_pairs`,
`golden_v7_v9_v10`, `close_pair_sweep`) bind no socket and declared
`mod common;` only to reach `common/decode.rs` and `common/pileup.rs`.
Their repeats are what cancelled #606's merge-group shards 2 and 4.

## Decisions

### D1. Delete the helper

`common/launch.rs` no longer defines `free_ports()`, and `common/mod.rs`
no longer re-exports it. The leaf keeps the one `manta` launcher and its
`MANTA_*` scrub. A daemon a test starts binds port zero and reports its
ports through `spawn_daemon_reporting_ports` (`tests/man570/daemon.rs`).
An unreachable connect target comes from `unreachable_target_ports` in the
same file, and a concrete shared port from the claimed band
(`src/test_ports.rs`). MAN-549's "one shared port helper" now means those
daemon helpers, still single in `man570/daemon.rs`, plus the claimed band.

### D2. The guard rejects a new definition or re-export

`free_ports_hits` already counted a `fn free_ports` line, because its
token matcher sees `free_ports` in it. Only the exemption list let the
definition and re-export pass. It now exempts `doctor_checks.rs` alone, so
a definition, re-export or call anywhere else is named `file:line`. The
guard keeps its name. `the_scan_names_a_restored_definition_or_re_export`
pins the definition-shaped red path in the tree, next to MAN-569's
call-shaped one.

### D3. The six non-socket roots include their helpers by `#[path]`

Each of the six roots replaces `mod common;` with
`#[path = "common/decode.rs"] mod decode;`, `#[path = "common/pileup.rs"]
mod pileup;`, or both. `common/mod.rs` declares neither leaf, and
`common_mod_leaves_the_golden_decode_helpers_to_path_includes` fails if it
does again. All six roots were detached, not just the three pileup roots:
both choices leave the same worst shard, but six also cut shards 7 and 1
and save about 24.5 runner-minutes on every later `tests/common/` edit
(`golden_close_pairs` takes 81.4 s a pass, `golden_v7_v9_v10` 65.4 s).

### D4. This is not a cost exemption under MAN-523 D3

MAN-523 D3 makes a target a candidate when its sources bind a socket, and
exempts no test for cost. None of the six roots, `decode.rs` or
`pileup.rs` binds a socket. They were candidates only through
`fake_server` and `ka9q`, which they never call. The repeat script is
unchanged and still selects any of them the day its own closure binds.

### D5. The leaves stay where they are

`decode.rs` and `pileup.rs` stay under `tests/common/` (MAN-521's record
names those paths). Each gains an inner `#![allow(dead_code)]`, as
`launch.rs` has, because `common/mod.rs`'s allow no longer covers them.
No outer attribute sits between `#[path]` and `mod`: the closure walk's
path-attribute pattern needs them adjacent, or it would miss the leaf.

### D6. No CI or script change, one pull request

`.github/workflows/ci-full.yml` already sets `timeout-minutes: 90` on all
three shard jobs, and the measured worst shard fits under it (Verification).
Nothing in `.github/` or `scripts/` changes, and MAN-584's open timeout
question stays with its owner. A separate detach pull request would edit
`tests/common/` too and pay the full repeat cost twice.

### D7. If CI cancels a shard anyway

Do not raise `timeout-minutes`, retry to green, quarantine or exempt a
test (MAN-523 D4). Record the shard, the OS and the
`repeatPlan`/`repeatResults` evidence on the pull request and hand the
timeout question to the owner of `/.github/` (MAN-584).

## Verification

- Guard red runs, from the plan: with the old three-entry exemption list,
  `the_scan_names_a_restored_definition_or_re_export` fails with
  `left: ["zzz_new_port_helper.rs:1"]`. With main's `launch.rs` and
  `mod.rs` restored, `no_integration_test_uses_released_free_ports` fails
  naming `common/launch.rs:41` and `common/mod.rs:30`, and
  `common_mod_leaves_the_golden_decode_helpers_to_path_includes` names
  `common/mod.rs:489` and `common/mod.rs:493`. All three pass on this
  change.
- `git grep` for a `fn free_ports` definition or a `free_ports` import
  under `crates/manta-cli/tests` (outside `shared_test_helpers.rs`) finds
  only `doctor_checks.rs`'s private allocator.
- The six roots list the same tests as before (4, 6, 3, 10, 8 and 5) and
  pass.
- Selection and budget: the repository's own `plan()` from
  `scripts/socket-test-repeat.py` and `assign()` from `scripts/ci-shards.py`,
  run over this change against its merge base, select 250 tests in 30
  executables and none from the six roots. Each selected test's reason is
  `common/decode.rs` or `common/launch.rs`. The budget below is one serial
  pass per selected test, measured by MAN-588's plan in a 32-core Linux
  container, times ten, plus `scripts/ci-test-weights.json` for the
  ordinary run. Setup and download time is not counted.

| Shard | Ordinary (min) | Repeats, deletion alone | Repeats, this change | Total, this change |
|---|---|---|---|---|
| 0 | 14.6 | 2.1 | 2.1 | 16.7 |
| 1 | 14.5 | 26.2 | 12.6 | 27.1 |
| 2 | 14.7 | 139.8 | 28.1 | 42.8 |
| 3 | 14.5 | 52.3 | 52.3 | 66.8 |
| 4 | 14.9 | 107.4 | 22.2 | 37.1 |
| 5 | 14.7 | 5.9 | 5.9 | 20.6 |
| 6 | 15.0 | 20.9 | 20.9 | 35.9 |
| 7 | 14.9 | 74.5 | 36.5 | 51.4 |

Shard 3, the worst at about 67 minutes, carries `multi_input_hpsdr`,
`iq_recording`, `signal_shutdown` and `calibrate`. All four bind sockets,
and any `launch.rs` edit selects them.
