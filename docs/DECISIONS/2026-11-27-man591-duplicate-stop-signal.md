# MAN-591: a stop signal within half a second of the first is the same stop request

## Context

MAN-232 D4 made every second stop signal call `_exit` immediately. GNU
`timeout -s INT` or `-s TERM` sends a signal to the child and then to its
process group (`kill(child, SIG)` followed by `kill(0, SIG)`). When both
reach manta during a busy decode, the second skips IQ recording finalization:
the payload exists, but the metadata writer leaves an empty `.partial` file.
In the reproducer, 80 of 80 runs on main exited 130 or 143. A single signal
finished cleanly in 40 of 40 runs, typically in 11–23 ms for a file source.

## Decision

Amend MAN-232 D4: Unix stop handlers treat any stop signal within 500 ms of
the first as the same request, including mixed signal types. A stop signal
at least 500 ms later still calls `_exit(128 + signal)` from the handler,
even if the decode thread is blocked on stdout. The handler uses
`clock_gettime(CLOCK_MONOTONIC)` and an atomic compare-exchange to establish
the first stop time without locks or allocation. One registration performs
the stop store and timing decision, preserving MAN-232's nested-handler rule.

The 130 / 143 / 129 exit-status row in MAN-232 now applies to a second
SIGINT / SIGTERM / SIGHUP (where SIGHUP is a stop signal) at least 500 ms
after the first. A duplicate inside that window drains and exits 0.

Windows console handling remains unchanged. A forced exit or SIGKILL still
skips recording finalization and leaves a `.partial` that replay refuses.

## Evidence

`a_timeout_style_sigint_keeps_the_recording_replayable` and
`a_timeout_style_sigterm_keeps_the_recording_replayable` exercise the two
signals and verify metadata, payload size and replay. The two
`a_duplicate_sig*_within_half_a_second_still_drains_a_recording_*` tests
pin the busy stdout and daemon paths. The existing MAN-232 second-signal
tests and `stop_signal_forces_exit_only_half_a_second_after_the_first` pin
the later forced exit and exact boundary.
