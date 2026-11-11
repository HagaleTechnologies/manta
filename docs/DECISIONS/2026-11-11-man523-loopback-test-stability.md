# MAN-523: loopback tests hold their ports, and changed socket tests run ten more times

Date: 2026-11-11
Ticket: MAN-523 (review 2026-11-07, CI/bugs)
Status: decided and built; proved on Linux only. The twenty-run main
observation below is post-merge work and has not happened yet.
Extends: MAN-410 (the claimed port band, `crates/manta-cli/src/test_ports.rs`),
MAN-508 (held non-listening sockets in `tests/list_refresh.rs`),
[MAN-518](2026-11-08-man518-build-test-executables-once-per-os.md) (shard
bundles) and [MAN-549](2026-11-08-man549-shared-integration-test-helpers.md)
(`shared_test_helpers.rs`). None is rewritten.

## Context

Four subjects failed on main or in the merge queue between 2026-10-21 and
2026-11-07 with no ticket and no change to their code. They name five test
functions:

- `manta-input/tests/recording.rs`:
  `a_recorded_source_reads_exactly_like_the_bare_one_and_replays_its_trace`;
- `manta-cli/tests/health_command.rs`: `health_escapes_peer_text` and
  `health_against_a_closed_port_exits_one_not_two`;
- `manta-cli/tests/multi_input_hpsdr.rs`:
  `declared_capacity_refuses_offline_and_unknown_capacity_requires_fallback`;
- `manta-server/tests/live_state_acceptance.rs`:
  `the_live_listener_admits_at_most_32_clients` (a Windows flake, MAN-353 §7a).

Two unit tests still bound `127.0.0.1:0`, dropped the listener and used the
number later, the race MAN-410 named: `list_http.rs`'s
`a_closed_port_is_connection_refused` and `list_refresh.rs`'s `closed_url`.

## Decisions

### D1. Each named test: what changed and what is known

| Test | Finding | Change | Status |
|---|---|---|---|
| live-client cap | Fixed sleeps of 1.5 s and 1 s raced the server's 500 ms protocol peek. Freezing the process for 2 s after 0.3 s reproduced `left: 0, right: 32` (research and plan phases). | Polls both the receiver count and the `live_clients` gauge for up to 10 s; proves a queued client is admitted by reading a record published after a slot was freed. | Root cause reproduced and fixed. |
| HPSDR capacity | MAN-542's validation saw `Address already in use` on port 38257: `launch::free_ports` releases its listeners before the child daemon binds. | `config()` takes its three ports from the claimed band (`src/test_ports.rs`, included by `#[path]`). | Mechanism observed; plausibly fixed. |
| health closed port | The test dialled a released ephemeral number a sibling could bind. | Holds a bound, non-listening `socket2` socket until `manta health` exits; also asserts empty stdout. | Race removed; the original failure was not reproduced. |
| health escaping | Uses the fake server, which binds and holds its listener. No mechanism found. | None to the test. The binary's other two released ports are gone (overlay config: held socket; real daemon: claimed band). | Unexplained. |
| recording | A filesystem and thread test with no socket. No mechanism found. | None. | Unexplained. |

A green run is not a root-cause proof. The two unexplained reports keep their
assertions unchanged and stay open until the observation in D5.

### D2. A port a test needs is held or claimed, never released

- A port that must refuse connections is a bound, non-listening TCP socket
  held for the probe's lifetime (`health_command.rs`'s `unreachable_port`,
  as MAN-508's `refused_source`). Linux and Windows refuse; macOS drops the
  SYN and the dial times out, so assertions allow both. A competing bind
  fails while the guard lives.
- A port a child daemon must bind comes from the claimed band 20256-20511
  (`src/test_ports.rs`). The UDP claim keeps cooperating test processes off
  the number. It does not stop an arbitrary foreign TCP listener; nothing at
  user level can.
- The two unit tests that need `ConnectionRefused` use the same band: a
  held listener would accept, and a held non-listening socket times out on
  macOS.
- `shared_test_helpers.rs` fails on the three released-port shapes MAN-523
  removed, in `tests/` and in `src/list_http.rs` and `src/list_refresh.rs`.
  Five older test files that release UDP or TCP ports with no observed
  collision are listed as `PENDING_RELEASED_PORTS`; the list only shrinks.

### D3. Changed socket tests run ten more times before merge

`scripts/ci-shards.py --repeat-base SHA` loads `scripts/socket-test-repeat.py`
after the ordinary tests. Unix shards use `--from-bundle`; Windows uses
`--inventory-only` after `cargo test --workspace --locked`, capturing the
executables with `--no-run --message-format=json`. It diffs SHA to HEAD and
selects per executable in the shard's slice, or the full Windows inventory:

- candidates: an integration target whose sources (the root file and every
  file it includes by `mod` or `#[path]`) contain `TcpListener::bind` or
  `UdpSocket::bind`; a lib or bin whose `src/` does; and `manta-input`'s
  `recording` target as a named seed;
- a changed line inside a `#[test]`-style function selects that test;
- any other changed line in a candidate integration target, or in a helper
  it includes, selects every test of the target;
- in `src/`, a changed line in `#[cfg(test)]` code outside a test function
  selects every test of that file's module; a changed whole-file test module
  (`#[cfg(test)] mod test_ports;`) selects its own tests and those of every
  module that names it; production code selects nothing;
- added blank and `//` lines select nothing; a deletion always counts;
- `#[ignore]` tests are never selected.

Each selected test runs ten times as `<executable> --exact <name>`, in the
shard's working directory and environment, with the compiler tripwires
still in place. Each run must exit 0 and report exactly one passed test.
Evidence lives in the shard JSON as `repeatPlan` and `repeatResults`, apart
from the ordinary `results`, and is uploaded with `if: always()`. A failure
fails the shard, and so `test (ubuntu-latest)` or `test (macos-latest)`.
No change relevant to a shard prints and records an explicit no-op.

The base is the event's own: `github.sha` for a dispatch (the main commit
its merge step merges in), the first parent of a pull request's merge commit
(`HEAD^1`, read from the commit object, since the PR's recorded base may be
older), the merge-group base, or a push's `before`. The shard fetches it into its shallow checkout. A missing,
all-zero or unfetchable base fails the shard as unverified; it never passes
as an empty diff. A PR's dispatched run uses main's workflow, so the merge
group is the first run of a new workflow's arguments.

Cost: repeats add execution time only, no compile. A helper change repeats
whole targets; one `--exact` pass over `multi_input_hpsdr` takes about
190 s, so ten more are about 32 minutes in its shard. The Unix shards'
`timeout-minutes` goes from 35 to 50 so that fits; a shard with nothing to
repeat finishes as before, and the merge queue's 60-minute check timeout
still bounds the run. A change whose repeats do not fit fails as unverified.
No test is exempt for cost.

### D4. No retry to green, no quarantine, no nextest

A failed run stays failed: the repeat runner never reruns a failure, and
nothing marks a test flaky. The queue's single requeue
(`queue-retry-handler.yml`) is unchanged. The ticket offered `cargo nextest`
retries as a cheap option; they turn a failure into a pass and nextest's
process per test would drop the process-local locks MAN-410, MAN-498 and
MAN-518 rely on. Libtest's `--exact` gives the repetition with nothing new
to install.

### D5. Post-merge observation

The ticket's "20 consecutive CI runs on main" scenario is observed after
merge, not in the implementing branch. For each OS, take the first twenty
push-to-main CI runs after this lands, in order, and check that all five
functions ran and passed in each. A skipped OS run does not count. Any
failure is recorded against this record's D1 row; until twenty clean runs
exist, the status is "pre-merge verification complete, post-merge stability
pending".

## Verification (Linux, 32 cores, this branch)

- Live cap under freeze (SIGSTOP 0.05 s or 0.3 s after start, held 2 s or
  4 s, three times each): 12 of 12 passed.
- `cargo test -p manta-cli --test health_command --test shared_test_helpers
  --test multi_input_hpsdr -p manta-server --test live_state_acceptance`:
  all passed (health 16, helpers 8, HPSDR 18, server live 9).
- Unit tests `list_http::`, `list_refresh::`, `test_ports::`: 45 passed.
- The `recording` binary 20 times: 20 passed. The `health_command` binary
  20 times, siblings included: 20 passed. The HPSDR capacity test 10 times
  with `--exact`: 10 passed. The recording test 20 times with `--exact`,
  pinned to one core shared with a busy loop: 20 passed. None reproduced a
  failure, so D1's "unexplained" rows stand.
- `python3 -m unittest scripts.tests.test_socket_test_repeat`: 23 tests,
  including a real Cargo bundle where the edited test runs ten more times,
  its siblings once, and a failure on iteration seven fails the shard.

No macOS or Windows run happened here. Windows repetition is wired into
the workflow, but has not been verified on Windows in this session.

### D6. Not built in this change

- Function-level selection uses a line scanner, not a Rust parser: macro-
  generated test names and aliased socket types (`use ... as`) are not
  followed. Helper and target-level fallbacks cover most of that.
