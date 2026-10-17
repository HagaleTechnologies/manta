# MAN-252: a stop request abandons a source reopen in flight

Date: 2026-10-16. Ticket: MAN-252 (a deferred P2 from PR #207's MAN-73
review on `crates/manta-cli/src/reconnect.rs`; it absorbed MAN-260, the
HPSDR hostname-lookup case).

## Problem

When a live source loses its connection, `ReconnectingSource` (MAN-73)
reopens it. Until this change it called the opener synchronously on the
decode thread. A stop request (SIGTERM/SIGINT, Windows Ctrl-C, a closed
stdout) was checked during the backoff sleep between attempts, but not while
an attempt was in flight. `manta run` starts draining its telnet/JSON/WS
clients only after `listen_with_observers` returns, so the time a reopen
takes was added in front of the 50 s `SHUTDOWN_DRAIN_DEADLINE`. A KiwiSDR
reopen can take up to 30 s of DNS and connect plus a 10 s handshake. An
HPSDR or ka9q hostname lookup, or a SoapySDR driver open, has no bound.
The total could overrun Docker's or Kubernetes' 60 s grace period, and the
process was then SIGKILLed before it recorded its dropped-spot counts.

Reproduced on `main` at `9f35f1a`: a fake KiwiSDR hung up, the daemon's
reopen stalled in TCP connect (a backlog-0 listener with one parked
connection makes Linux drop the SYNs), and a raw JSON-stream client was
connected. SIGTERM half a second into the reopen gave client EOF at 9.52 s
and exit at 9.55 s: the drain began only once the connect timed out.

A naive reproduction can pass by luck. When the reopen stalls in the
WebSocket handshake instead, SIGTERM can land on the decode thread, and the
handshake's blocking read then fails with EINTR. That depends on which
thread receives the signal, and it does not help a stalled connect (a futex
wait that `std` retries on EINTR) or glibc's resolver (which retries EINTR
itself). The regression test therefore stalls the connect phase.

A second defect showed up in the same reproduction. `ReconnectingSource`
reports a stop as EOF (`Ok(0)`), and `listen`'s startup-calibration fill
treated any EOF as `audio source ended during startup calibration`. A
source that dropped before its first two seconds were buffered, then
stopped during the backoff or the reopen, exited 1 with that error instead
of 0 (MAN-85's SIGTERM contract).

## Decision

Each reopen runs on its own short-lived thread, named `reopen-<kind>`
(`reopen-kiwi`, `reopen-hpsdr`, ...). `ReconnectingSource::read` waits for
the result in 100 ms slices (`REAL_ENV_SLEEP_SLICE`, the backoff sleep's
slice) and checks `stop` between them. When `stop` is set it returns
`Ok(0)` at once and abandons the attempt. The attempt finishes on its own
thread, where a late source is dropped, or dies with the process. A
successful reconnect behaves exactly as before: same backoff ladder, health
transitions, discontinuity accounting, input-health totals, and fatal
errors on a changed rate, centre or passband.

- The opener lives in `Arc<Mutex<Opener>>`, and the worker locks it for the
  call. If spawning the thread fails, the opener survives and the failure
  counts as one failed attempt, with the cause
  `could not start the reopen thread: <io error>` in the existing
  `reconnect attempt failed` warning.
- At most one attempt is pending. `read()` starts a new one only when none
  is in flight, so an abandoned attempt exists only after a stop.
- An opener panic is re-raised on the reading thread
  (`JoinHandle::join` then `resume_unwind`), as it was when the opener ran
  there. Turning it into a retried error would hide invariant violations
  such as `LiveSourceSpec`'s `expect`s.
- `listen`'s calibration fill treats an EOF that follows a stop request as
  the stop: it breaks out and ends through `finish()`. An EOF with no stop
  request is still `audio source ended during startup calibration`, so a
  replay shorter than the calibration window still exits 1. No
  deterministic replay sets `stop`.

### `Send`

The source a reopen builds crosses from the worker to the decode thread,
so everything `LiveSourceSpec::open` builds must be `Send`. The opener's
type is `Box<dyn FnMut() -> Result<Box<dyn IqSource + Send>> + Send>`.
`maybe_decimate`, `open_audio_source`, `audio_with_dial`,
`FixedCenterFreqSource` and `manta_input::DecimatingSource` carry
`Box<dyn IqSource + Send>`. Every production source type already was
`Send`; for SoapySDR, `soapysdr` 0.5.1 declares
`unsafe impl Send for RxStream`. `IqSource: Send` was not made a supertrait,
because `ReconnectingSource` itself would then have to be `Send`, forcing
its decode-thread-only `HealthSink` and every test source to change.

## Alternatives rejected

- **Per-source cancellation tokens** passed into `KiwiIqSource::connect`,
  `HpsdrDevice::open`, `Ka9qIqSource::open` and `SoapySdrIqSource::open`.
  That changes four public `manta-input` APIs and still needs a thread for
  the native SoapySDR open, which cannot be cancelled. The wrapper covers
  every live source in one place.
- **Starting the client drain before `listen` returns.** Today health, the
  final snapshot (MAN-64/MAN-128) and `TrackManager::finish()`'s last spots
  are all published before `shutdown_tx.send(true)`. Draining clients while
  the decode thread is still blocked risks clients leaving before those
  last spots arrive.
- **Anchoring `SHUTDOWN_DRAIN_DEADLINE` at the stop request.** Counting the
  50 s from the signal would take the decode loop's return time out of
  MAN-45's per-handler budget and break
  `the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline`,
  bringing back uncounted truncation. With the reopen abandoned, the drain
  already starts within about 100 ms of the stop.

## Bounds that remain

- Worst case from SIGTERM to exit during a reconnect: about 0.1 s until the
  stop is observed, then the existing backstop of 50 s
  (`SHUTDOWN_DRAIN_DEADLINE`) plus 2 s of runtime shutdown, about 52.1 s.
  That fits the documented 60 s grace (README, Dockerfile). The client
  drain itself is at most 40 s by design (MAN-45).
- A read already in flight on a connected source, including a reopened
  source's first read, is not interrupted. Each source bounds its own
  stall: Kiwi and HPSDR about 10 s, ka9q 10 s, SoapySDR about 2 s, live
  audio 5 s. That is the same for any stalled live source.
- A reopen that hangs (an unbounded DNS lookup or driver open) still holds
  up reconnection until it returns, as before. It no longer holds up
  shutdown. A per-attempt reopen timeout would be a separate change.
- The startup (first) open runs before the signal handlers are installed,
  so a signal then takes its default disposition and ends the process at
  once. That is unchanged.

## Verification

- `reconnect::tests::stop_during_a_blocked_reopen_returns_eof_without_waiting_for_it`
  and `reconnect::tests::a_panicking_opener_panics_the_reading_thread`.
- `listen::tests::listen_treats_eof_after_a_stop_request_during_calibration_as_a_clean_end`
  and its control,
  `listen::tests::listen_still_fails_when_a_source_ends_during_calibration_unasked`.
- `signal_shutdown.rs` (Linux):
  `sigterm_during_a_stalled_kiwisdr_reconnect_drains_and_exits_promptly`
  and
  `sigterm_during_a_kiwisdr_reconnect_before_calibration_completes_exits_zero`
  run a real daemon against a fake KiwiSDR whose reconnect stalls in TCP
  connect, with a JSON client connected. They require client EOF and exit
  0 within 3 s of SIGTERM, and no `reconnect attempt failed` line after
  it.
