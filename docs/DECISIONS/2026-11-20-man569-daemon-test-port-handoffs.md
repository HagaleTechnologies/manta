# MAN-569: daemon integration tests bind port zero or claim a band, never a released number

Date: 2026-11-20
Ticket: MAN-569 (bug, test infrastructure)
Status: decided and built; proved on Linux in this container. The Ubuntu and
macOS required checks run on the pull request, not here.
Extends: MAN-410 and
[MAN-523](2026-11-11-man523-loopback-test-stability.md) (the claimed band and
held sockets; D2 there named this exact race),
[MAN-549](2026-11-08-man549-shared-integration-test-helpers.md)
(`common/launch.rs` and `shared_test_helpers.rs`).

## Context

Three integration-test failures on main, none with a change to the failing
test's code:

- `man234_bad_samples.rs` `daemon_publishes_bad_samples_from_startup`, run
  43708277517 (macOS shard 7): `metrics_port = 49277: Address already in use`;
- `callsign_lookup.rs` `scenario_1_a_call_qrz_does_not_list_is_not_spotted`,
  run 43274361779 (Ubuntu shard 3): `telnet_port = 44795`;
- `hpsdr_discovery.rs`
  `two_radios_require_a_choice_and_mac_pinning_streams_from_flags_and_config`,
  run 42912967540 (Ubuntu shard 7): `metrics_port = 45903`.

Each took its ports from `launch::free_ports()`, which bound ephemeral
listeners, read their numbers and dropped them before returning; the child
daemon bound later, and any sibling socket could take the number in between.
`hpsdr_discovery.rs` and `docker_image.rs` had the same shape inline. The
`multi_input_hpsdr.rs` outage failures are not this: MAN-523 D1 records a
separate START-ordering race there, and this ticket does not touch them.

## Decisions

### D1. Daemon listeners bind port zero; the test reads what the daemon reports

Every fixture that starts a `manta` daemon writes `telnet_port = 0`,
`json_port = 0` and `metrics_port = 0` and launches through
`spawn_daemon_reporting_ports` (`crates/manta-cli/tests/common/launch.rs`),
which reads the bound addresses from the daemon's `listening:` banner. The
kernel chooses and the daemon holds each port in the same bind, so there is no
interval where a number exists unowned. The launcher pins the child's
`RUST_LOG` to `info` (the banner is an INFO record), drains stderr to EOF on a
thread and keeps every line for failure diagnostics, and kills and reaps the
child on drop. Fixtures keep their separate readiness waits:
`listening:` is not pipeline readiness.

### D2. A concrete port a config shares between two commands comes from the claimed band

`docker_image.rs` writes one TOML both the daemon and the Dockerfile health
command read, so it needs one concrete number: it takes it from
`src/test_ports.rs`'s claimed allocator under its Unix gate, as
`health_command.rs` already did. This is the ticket's allowed exception; the
band coordinates cooperating test processes and MAN-523 D2's foreign-TCP
limitation still applies to it.

### D3. The source guard rejects the removed helper, however it arrives

`shared_test_helpers.rs`'s `no_integration_test_uses_released_free_ports`
matches the exact identifier token `free_ports` — lines split on non-ASCII-
alphanumeric/underscore characters — so qualified calls, unqualified calls,
grouped and aliased imports and re-exports all match, while `free_port`
(singular, the claimed band) and `find_free_ports` do not. Standalone `//`
comment lines are exempt; `doctor_checks.rs` keeps its private allocator.
`no_test_releases_an_ephemeral_port_for_reuse` keeps matching the inline
release shapes (HPSDR discovery's bind/read/drop, the three-listener array,
the Docker temporary listener), with Docker off the pending list.

### D4. Uplink targets that must stay unreachable pin their ports

`node_health_acceptance.rs`'s two disconnected-uplink destinations come from
`unreachable_target_ports`: a connected socket pair holds each port bound with
no listener, so connects are refused and the kernel cannot re-deal the number
to the daemon's own port-0 bind.

## Verification

All in this Linux container, on the candidate tree:

- Scratch mutation (a scratch `.rs` under `tests/` with an old call, an
  aliased import and the old HPSDR/Docker inline forms, never compiled):
  `no_integration_test_uses_released_free_ports` failed naming
  `zzz_scratch_guard_mutation.rs:4` and `:17`;
  `no_test_releases_an_ephemeral_port_for_reuse` failed naming the file.
  Unmutated: `--test shared_test_helpers` passes 10/10.
- Scratch in `daemon_port_handoff.rs`: with a competing listener holding a
  concrete port, a port-zero daemon started and reported its trio, and the
  concrete-port daemon exited 1 with `error: binding the metrics server
  (metrics_bind_addr = "127.0.0.1", metrics_port = 34597): Address already in
  use (os error 98)` — the reported CI failure, captured by the launcher's
  panic. Scratch removed afterwards.
- `--test daemon_port_handoff` (two daemons under a churning sibling socket,
  six distinct nonzero ports, both metrics endpoints scraped) passed, then
  five more consecutive runs.
- The migrated binaries named in the ticket's verification suite passed on
  Linux, including `RUST_LOG=warn` around the MAN-234 daemon (the launcher
  re-pins the child's filter).
- The source audit `rg 'free_ports|TcpListener::bind|...'` over
  `crates/manta-cli/tests` leaves only held listeners, the claimed band, the
  pending MAN-523 list and `doctor_checks.rs`.

macOS and the Ubuntu shards are unverified here; the required checks run on
the pull request. The `multi_input_hpsdr.rs` outage remains MAN-523 D1's open
question.
