//! MAN-232: stdout and stderr writes that never panic. `println!` and
//! `eprintln!` panic when a write fails, and Rust's runtime ignores SIGPIPE,
//! so a reader that exits (`| head -1`, `| true`) made manta's next line exit
//! 101 -- in `run`, from inside the decode loop, past the shutdown drain.
//! stdout writes here return an error; stderr writes drop theirs, because a
//! lost diagnostic must neither stop a daemon nor change an exit status.
//! See docs/DECISIONS/2026-10-14-man232-closed-and-stalled-output.md.

use std::cell::{OnceCell, RefCell};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Exit status when stdout's reader has gone away: 128 + SIGPIPE, what a
/// shell reports for a producer that SIGPIPE killed.
pub const EXIT_STDOUT_CLOSED: u8 = 141;

/// A stdout write found its reader gone (EPIPE). `main` exits
/// [`EXIT_STDOUT_CLOSED`] for it and prints nothing.
#[derive(Debug)]
pub struct StdoutClosed;

impl std::fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// A failed stdout write as `main` reports it: a typed [`StdoutClosed`] for a
/// closed reader, and a contextual error for anything else (a full disk, a
/// closed file). Typed, not a `BrokenPipe` match at the top, so an EPIPE from
/// a KiwiSDR/HPSDR socket or the uplink is never mistaken for a closed stdout
/// and silenced.
pub(crate) fn stdout_error(e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::BrokenPipe {
        anyhow::Error::new(StdoutClosed)
    } else {
        anyhow::Error::new(e).context("writing to stdout")
    }
}

/// Write `args` to stdout and flush, mapping a failure through
/// [`stdout_error`]. One lock, as `println!` takes.
pub fn write_stdout(args: std::fmt::Arguments<'_>) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    out.write_fmt(args)
        .and_then(|()| out.flush())
        .map_err(stdout_error)
}

/// Write `args` to stderr, dropping any failure: a lost diagnostic must not
/// stop a command or change its exit status.
pub fn write_stderr(args: std::fmt::Arguments<'_>) {
    let _ = io::stderr().lock().write_fmt(args);
}

/// `println!` that returns `anyhow::Result<()>` instead of panicking.
macro_rules! outln {
    () => { $crate::stdio::write_stdout(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::stdio::write_stdout(format_args!("{}\n", format_args!($($arg)*))) };
}

/// `print!` that returns `anyhow::Result<()>` instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => { $crate::stdio::write_stdout(format_args!($($arg)*)) };
}

/// `eprintln!` that ignores a failed write instead of panicking.
macro_rules! errln {
    () => { $crate::stdio::write_stderr(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::stdio::write_stderr(format_args!("{}\n", format_args!($($arg)*))) };
}

pub(crate) use {errln, out, outln};

/// `run`'s stdout from inside the decode loop's callbacks, which cannot
/// return an error: the first failed write sets `stop` -- starting the same
/// drain a signal starts -- and is kept for `finish`, which runs after the
/// drain; later lines are skipped.
pub struct StopOnClose<W: Write = io::Stdout> {
    stop: Arc<AtomicBool>,
    out: RefCell<W>,
    failed: OnceCell<io::Error>,
}

impl StopOnClose {
    /// The decode loop's stdout, sharing `stop` with the signal handlers.
    pub fn stdout(stop: Arc<AtomicBool>) -> Self {
        Self::new(stop, io::stdout())
    }
}

impl<W: Write> StopOnClose<W> {
    pub fn new(stop: Arc<AtomicBool>, out: W) -> Self {
        Self {
            stop,
            out: RefCell::new(out),
            failed: OnceCell::new(),
        }
    }

    /// One line, exactly what `println!("{}", …)` wrote: the whole formatted
    /// line under one writer borrow, then a flush.
    pub fn line(&self, args: std::fmt::Arguments<'_>) {
        if self.failed.get().is_some() {
            return;
        }
        let written = {
            let mut out = self.out.borrow_mut();
            out.write_fmt(format_args!("{args}\n"))
                .and_then(|()| out.flush())
        };
        if let Err(e) = written {
            // Only EPIPE gets a note: any other error is reported by
            // `main`'s closing `error:` line.
            if e.kind() == io::ErrorKind::BrokenPipe {
                crate::logging::note("manta: stdout closed; stopping");
            }
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.failed.set(e);
        }
    }

    /// After the drain: [`StdoutClosed`] for a closed reader, else the write
    /// error; `Ok(())` when every write succeeded.
    pub fn finish(self) -> anyhow::Result<()> {
        self.failed
            .into_inner()
            .map_or(Ok(()), |e| Err(stdout_error(e)))
    }

    /// The wrapped writer, for tests that assert what was written.
    #[cfg(test)]
    fn into_writer(self) -> W {
        self.out.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn a_broken_pipe_is_stdout_closed_and_other_errors_name_stdout() {
        let closed = stdout_error(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(closed.downcast_ref::<StdoutClosed>().is_some());
        let full = stdout_error(io::Error::from_raw_os_error(28 /* ENOSPC */));
        assert!(full.downcast_ref::<StdoutClosed>().is_none());
        assert!(
            crate::fmt::render_error(&full).starts_with("error: writing to stdout: "),
            "{full:#}"
        );
    }

    /// A writer whose every write fails with `kind`, counting attempts.
    struct Failing {
        kind: io::ErrorKind,
        calls: Rc<Cell<usize>>,
    }

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.calls.set(self.calls.get() + 1);
            Err(io::Error::from(self.kind))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stop_on_close_sets_stop_once_and_skips_later_lines() {
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Rc::new(Cell::new(0));
        let out = StopOnClose::new(
            stop.clone(),
            Failing {
                kind: io::ErrorKind::BrokenPipe,
                calls: calls.clone(),
            },
        );
        out.line(format_args!("a"));
        out.line(format_args!("b"));
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(calls.get(), 1, "a second line must not be attempted");
        assert!(out
            .finish()
            .unwrap_err()
            .downcast_ref::<StdoutClosed>()
            .is_some());
    }

    #[test]
    fn stop_on_close_writes_exactly_what_println_did() {
        let stop = Arc::new(AtomicBool::new(false));
        let out = StopOnClose::new(stop.clone(), Vec::<u8>::new());
        out.line(format_args!("{}", "x y"));
        assert_eq!(out.into_writer(), b"x y\n");
        assert!(!stop.load(Ordering::Relaxed));
    }
}
