# MAN-453: raw IQ recording reports its true outcome, and a stop ends a paced replay's wait

Date: 2026-11-01. Ticket: MAN-453.

**Status:** accepted. Amends
[MAN-309](2026-10-19-man309-iq-recording-and-replay.md) D3 and D6.
Operator guide: [docs/GUIDES/iq-recording.md](../GUIDES/iq-recording.md).

## Context

PR #380 (MAN-309, raw IQ recording) deferred three Codex P2 findings
under the [review convergence policy](2026-08-07-pr-review-convergence-policy.md)
(threads `r4352345395`, `r4352345400` and `r4385204541`). All three
reproduced at `4581725`:

- **Finish race.** `RecordingHandle::finish` timed out and set a
  `cancelled` flag, but the writer read that flag before it published.
  A writer that had passed the check published anyway, so a recording
  reported "completion is unconfirmed" could appear complete. A scratch
  test whose publication took 300 ms against a 20 ms `finish` returned
  `TimedOut` and found the final metadata published 600 ms later.
- **Thread spawn.** `start_with_sinks` spawned the writer with
  `.expect(...)`. Under a thread limit `run` panicked (exit 101) after it
  had created `PATH` and `PATH.meta.jsonl.partial`, and the next start
  then failed with `IQ recording destination already exists`.
- **Paced wait.** `PacedSource::read` slept through a recorded gap in one
  `std::thread::sleep`. SIGTERM during a 600 s gap left `run --realtime`
  running past 20 s; a 40 s gap exited 36 s after the signal, single input
  and two inputs alike. A service manager's kill in that window lost the
  recording (payload kept, metadata a 0-byte `.partial`).

## Decision

- **D1, publication is claimed.** `Shared::state` (an `AtomicU8`) replaces
  the `cancelled` flag and holds `PENDING`, `PUBLISHING`, `SETTLED` or
  `CANCELLED`. The writer publishes only after it moves `PENDING` to
  `PUBLISHING`. `finish` reports `TimedOut` only after it moves the state
  to `CANCELLED`, from `PENDING` at the deadline or from `PUBLISHING` once
  the grace is spent. Each move is a compare-exchange, so exactly one side
  decides. A writer that has claimed publication at the deadline gets up
  to a grace more, and `finish` reports what it did (`Finished` or
  `Failed`). `finish(timeout, grace)` takes the grace explicitly, so no
  caller gets a zero-grace default by accident. `run` passes the rest of
  one `RECORDING_PUBLICATION_GRACE` of 2 s that starts at the shared 5 s
  `RECORDING_FINISH_TIMEOUT`, for every recording of the run. 5 + 2 s
  stays inside the 50 s `SHUTDOWN_DRAIN_DEADLINE`, so `TimeoutStopSec=60`
  and the 57 s `stop_budget_floor` do not move; a grace per recording
  would have added up to 32 s for 16 inputs. The writer's settle step
  (`fetch_update` to `SETTLED` unless `CANCELLED`) also decides whether it
  warns: a cancelled writer stays silent, since `finish` has already said
  "unconfirmed". One case remains: a publication that blocks longer than
  the grace (a hung filesystem) is reported "unconfirmed" and may still
  appear. There is no rollback: it would be another call on the same hung
  filesystem, and the process exits right after `finish_recordings`.
  Publishing on the caller's thread would also close the race, but a hung
  link would then block shutdown silently until SIGKILL. A mutex held
  across filesystem calls would work too; the atomics match the file's
  style and nobody waits on the filesystem while holding a lock.
- **D2, a writer that cannot start is a startup error.** The writer thread
  starts through a `SpawnWriter` step (a function pointer, so a test can
  refuse it, as the `Publish` step lets a test fail publication).
  `start_with_sinks` returns its error as
  `cannot start the IQ recording writer for PATH: ...`, and
  `start_recording` removes the payload and `.partial` it created with
  `create_new`. The refused spawn has already dropped the task that owned
  them, so they are closed when removed, which matters on Windows. `run`
  prints one `error:` line and exits 1, as for every other failure to
  start a requested recording, and the same destination works on the next
  start. A multi-input daemon reports it through `open_inputs` as
  `input "NAME" failed to open: ...`. `run` does not decode on without
  the recording: the operator asked for it, and decoding without it would
  lose data silently. There is no automated `RLIMIT_NPROC` test: a
  headroom sweep over `ulimit -u` panicked at every headroom from 0 to 256
  threads and recorded normally from 512, because the kernel counts
  threads this container cannot see, so the threshold depends on the host.
- **D3, a stop ends a paced wait.** `PacedSource::with_stop_flag` makes
  each pacing wait sleep toward the same deadline in slices of at most
  100 ms (`PACED_WAIT_SLICE`, as `manta-cli`'s reconnect backoff) and
  return once the flag is set. The read still returns the samples and gap
  it already took, so decoded output is byte-identical and
  `RecordingSource` does not read the stop as an end of input. Both
  `wrap_file_replay` callers (single input and the multi-input opener)
  pass the run's stop flag, so `--realtime` and `--loop` replays stop
  within about 0.1 s even inside a recorded outage, and the recording is
  finalized as `stopped`. A kill that no stop precedes, or the second
  signal's immediate exit, still loses the recording, by design.

## Verification

- `crates/manta-input/src/recording.rs` unit tests:
  `a_writer_that_cannot_start_fails_the_recording_and_leaves_no_files`,
  `a_timeout_before_publication_begins_cancels_it_for_good`,
  `a_timeout_during_publication_waits_for_it_and_reports_the_result`
  (fails with `TimedOut` on the old `finish`),
  `a_publication_that_outlasts_the_grace_is_reported_unconfirmed_and_stays_quiet`,
  and `finish_honours_its_deadline_and_cancels_publication`, which now
  also checks that an unclaimed writer does not use up the grace.
- `crates/manta-input/src/replay.rs` unit tests:
  `a_stop_ends_a_paced_gap_wait_and_keeps_the_read` and
  `a_set_stop_flag_skips_the_pacing_wait`.
- `crates/manta-cli/src/main.rs` unit tests:
  `recording_finish_budget_shares_one_grace_after_the_deadline`,
  `recording_finalization_ends_inside_the_client_drain` and
  `wrap_file_replay_hands_the_stop_flag_to_the_pacing`.
- `crates/manta-cli/tests/iq_recording.rs`:
  `a_stop_signal_during_a_recorded_gap_ends_a_realtime_replay_promptly`
  replays a recording with a 600 s gap in real time, single input and as a
  two-input daemon, sends SIGTERM about 2 s into the gap, and requires
  exit 0 within 2 s with every recording finished as `stopped` and holding
  the gap. Without the stop flag in the pacing it fails at the 2 s check.
