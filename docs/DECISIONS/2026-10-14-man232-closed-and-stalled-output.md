# MAN-232: a closed stdout stops manta through the drain; a second stop signal exits at once

## Problem

Every verb wrote its results with `println!`/`eprintln!`/`print!`, which
panic when the write fails. Rust's runtime ignores SIGPIPE, so a reader
that exits surfaces as `EPIPE`, and the macros turn that `Err` into a
panic: exit 101, with no drain. Reproduced on `f33bf1c` (2026-10-14):

- `manta listen --source v1.wav --source-iq --json | head -1` — exit 101,
  `failed printing to stdout: Broken pipe (os error 32)`, from inside the
  decode loop, past the shutdown drain.
- `manta decode v1.wav | true` (also `decode --json`, `doctor` ± `--json`,
  `oracle`) — exit 101 with the same panic.
- `manta decode v1.wav > /dev/full` — exit 101 (`No space left on
  device`); a closed stderr likewise.
- A `run --json` whose stdout is a pipe nobody reads blocks once the
  kernel's 64 KiB pipe buffer fills. Both signal paths (`signal-hook`'s
  registry and ctrlc) install their OS handlers with `SA_RESTART`
  (`signal-hook-registry-1.4.8/src/lib.rs:187`,
  `ctrlc-3.5.2/src/platform/unix/mod.rs:84`), so the kernel restarts the
  blocked `write(2)` when the handler returns, the loop never gets back
  to its `stop` check (`listen.rs`'s chunk-loop head), and **only SIGKILL
  ends the process**: two SIGTERMs (or SIGINTs) 1 s apart left it alive.
- A hidden second panic: `tracing_subscriber::fmt()` defaults to
  `.log_internal_errors(true)`, whose fallback for a failed event write
  is an `eprintln!` (`tracing-subscriber 0.3.23 `fmt_layer.rs:1050-1054`)
  — a `[server]` daemon whose stderr reader left died with exit 101 on
  its own at its `ready: decoding` record.

## Decision

1. **D1 — exit 141 for a closed stdout**, through a typed
   `stdio::StdoutClosed` error that `main()` maps silently. Typed, not a
   `BrokenPipe` match at the top, so an EPIPE from a KiwiSDR/HPSDR socket
   or the uplink is never mistaken for a closed stdout and silenced. 141
   is 128 + SIGPIPE, what a shell reports for a SIGPIPE'd producer
   (`yes | head -1; echo ${PIPESTATUS[0]}` prints 141).
2. **D2 — other stdout write errors are ordinary errors** (exit 1,
   `error: writing to stdout: …`). For `run` they also stop the run
   through the drain first.
3. **D3 — stderr failures are ignored** (`errln!`, `logging::note`/
   `warning`, `tracing` with `log_internal_errors(false)`). Diagnostics
   must not end a daemon or change an exit status.
4. **D4 — the second stop signal exits from inside the handler** via
   one `signal_hook::low_level::register` action per stop signal that
   stores `stop` and then swaps a dedicated `signalled` flag, calling
   `_exit(128 + sig)` if the swap finds it already set. Only the
   stop-signal handlers set `signalled`. Any combination counts (SIGINT then
   SIGTERM). This holds at any time after installation, including during
   a healthy client drain: a second signal comes from an operator who
   wants out.
5. **D5 — SIGINT moves from ctrlc to signal-hook on Unix**, so one
   mechanism handles every stop signal; ctrlc becomes Windows-only (a
   counting closure, `exit(130)` on the second console event).
   Supersedes MAN-78 D3's "ctrlc keeps SIGINT on Unix".
6. **D6 — `run`'s callbacks write through `stdio::StopOnClose`.** The
   first failure sets the existing `stop` flag — the same drain a signal
   starts, so connected clients still drain — and is remembered; later
   lines are skipped; `finish()` after the drain returns it as 141 (or
   exit 1). The note `manta: stdout closed; stopping` is logged only for
   EPIPE, because other errors get `main`'s final `error:` line.
7. **D7 — `doctor` stops early**: `Sink` remembers its first failed
   write, and the doctor arm returns it right after the setup checks and
   again after the receiver/signal stage, so `doctor | true` does not run
   a signal check for nobody.
8. **D8 — every print macro in `main.rs` is converted** (including
   `soak`/`status` stdout → 141 and the stderr-only arms), so the guard
   can cover all of `main.rs`.
9. **D9 — tests live in `crates/manta-cli/tests/signal_shutdown.rs`** as
   the ticket asks; unit tests for `stdio` live in `stdio.rs`; the
   panicking-print ban is a guard in `output_style_guard.rs`.

One action per stop signal (D4): `stop.store(true)`, then
`if signalled.swap(true) { _exit(128 + signal) }` — two lock-free atomics
and an async-signal-safe `_exit`, nothing else. It is one registration,
not separate store / conditional-exit / arm registrations (the first
revision of this PR), for two reasons: a signal delivered between
separate registrations runs only some of them, and handlers for
*different* signals nest (signal-hook installs with an empty `sa_mask`),
so a load-then-store pair lets two concurrent stop signals both see "not
yet signalled" and neither exit. The swap makes "first or second?" a
single atomic decision, so the first stop signal only requests the drain
and any second one ends the process. Do not split it back into
`signal_hook::flag::register` / `register_conditional_shutdown` calls.
Only these handlers set `signalled`: a closed stdout and the post-loop
`stop_ready.store(true)` set `stop` without arming the forced exit, so
the first signal after a closed-pipe stop still drains normally.

Default SIGPIPE is NOT restored (the ticket forbids it: it would skip the
drain), and blocking writes stay (also a ticket non-goal: file replay and
batch decode keep them; one signal to a blocked run waits for the
reader).

## Exit statuses

| Status | When |
|---|---|
| 0 | Normal end, or one stop signal drained |
| 1 | Error (including a stdout write that failed for another reason, such as a full disk); `doctor`: a check failed |
| 2 | Usage error; `status`: could not ask the daemon |
| **141** | stdout's reader went away (128 + SIGPIPE). `run`/`listen` drains first. Applies to `run`/`listen`, `decode`, `doctor`, `oracle`, `soak` and `status` |
| **130 / 143 / 129** | A second SIGINT / SIGTERM / SIGHUP (SIGHUP only without `[server]`) while the first is being handled: immediate exit, the rest of the drain skipped. Windows: a second Ctrl-C/Ctrl-Break exits 130 |

~~101~~ (panic on a failed write) is no longer produced by these paths.

## Not changed

- Blocking writes: output to a reader that is still open but not reading
  waits for it (stall detection is MAN-128).
- No signal handlers for `soak`/`doctor`/`calibrate` (MAN-85's "not
  changed" stands).
- `bench`, `calibrate`, `config check/init`, `grade`, `health`, `check`
  still use panicking macros; `output_style_guard.rs`'s `NOT_YET` list
  names them (follow-up: the remaining commands should survive a closed
  stdout). `devices`/`config` already return an `Err` instead.
- The `manta completions bash | head` panic is inside upstream
  `clap_complete` (recorded in the MAN-133 validation).

## Evidence

`crates/manta-cli/tests/signal_shutdown.rs` pins all three Gherkin
scenarios (`a_reader_that_exits_early_stops_run_through_the_drain_with_141`,
`decode_exits_141_without_a_panic_when_stdout_is_closed` and friends,
`a_second_sigterm_ends_a_daemon_blocked_on_stdout` and
`a_second_sigint_ends_a_run_blocked_on_stdout`), the drain-preserving
behaviours (`a_daemon_whose_stdout_reader_exits_drains_and_exits_141`,
`one_signal_to_a_run_blocked_on_stdout_still_drains_once_the_reader_resumes`,
`a_daemon_whose_stderr_reader_left_still_exits_zero_through_the_drain`),
and the five MAN-85/MAN-78 single-signal tests still pass unchanged.
`output_style_guard.rs`'s `cli_commands_never_use_panicking_print_macros`
fails if a panicking print macro returns to a covered file. Decode text,
decode JSON, `run --json`, text-mode stdout/stderr and
`scripts/summarize-listen-jsonl.sh` output were verified byte-identical
to `f33bf1c` (SHA-256, 2026-10-14).
