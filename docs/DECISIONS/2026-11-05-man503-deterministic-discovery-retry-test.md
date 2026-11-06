# MAN-503: deterministic directed-discovery retry test

Date: 2026-11-05. Ticket: MAN-503.

## Evidence

The ticket reports an intermittent Windows CI failure in
`auto_retries_discovery_four_times_within_the_window`. That test counted
datagrams collected by a second thread over loopback UDP during a 300 ms
wall-clock window. Descheduling the probe can exhaust the window before
four request pairs are sent. Stopping the receiver thread can also leave
queued datagrams uncounted.

Planning reproduced scheduling sensitivity on Linux without source changes:
the focused test passed normally, then failed with two rather than four
requests after a driver paused its process with SIGSTOP for 400 ms,
80 ms into the run. This does not establish the cause of the reported
Windows incident or demonstrate the separate receiver-stop race.

## Decision

Extract the directed probe loop into a private generic helper with clock,
send and receive operations. The public `probe` still resolves and binds
the socket, installs the same 10 ms read timeout and uses real UDP and
`Instant`. The broadcast discovery loop is unchanged.

The retry test drives this same helper with a manual clock and synchronous
I/O. Receives advance synthetic time by the polling interval and return
either Unix's `WouldBlock` or Windows' `TimedOut`. The test compares
every send's timestamp, destination and exact bytes against four P2/P1
pairs at 0, 50, 100 and 150 ms, then checks `NoAnswer` at 300 ms.
There are no sockets, sleeps or receiver threads in this test. The real
`Instant` used to anchor manual time does not control its progress.

Two additional tests check jumps to and beyond the deadline and a P1
reply at 290 ms. The first P1 triggers an immediate extra P2 send and
fallback at 320 ms, beyond the overall window. A second P1 at 310 ms
does not extend that grace.

Production keeps deadline-before-send ordering, at most one request pair
per iteration, the original retryable errors and error contexts, P2
preference, and the first P1's fixed grace deadline. The existing socket
reply tests continue to exercise the real adapter. No retry-on-failure,
relaxed packet counts, dependency or public API was added.

## Verification limits

The new deterministic tests and repository checks run on Linux in the
implementation container. Windows and macOS hosts are unavailable here.
The Windows CI suite remains the platform integration check after
publication. Manual time and synchronous send recording remove runner
load and loopback delivery from the retry assertion; repeated passing
wall-clock runs alone would not prove that property.
