//! The one place manta's human-readable output style lives.
//!
//! Every operator-facing number the CLI prints goes through a function
//! here, so the same value never reads two different ways in two commands:
//! frequency in kHz to one decimal, SNR/WPM as whole numbers, confidence to
//! two decimals, and never a raw Rust Debug rendering (MAN-130).

use manta_spot::Spot;

pub fn khz(freq_hz: f64) -> String {
    format!("{:.1}", freq_hz / 1000.0)
}

pub fn db(snr_db: f32) -> i32 {
    snr_db.round() as i32
}

pub fn wpm(wpm: f32) -> i32 {
    wpm.round() as i32
}

/// Speed that may be absent. Never renders as `Some(..)`/`None`.
pub fn wpm_opt(wpm_val: Option<f32>) -> String {
    match wpm_val {
        Some(w) => wpm(w).to_string(),
        None => "unknown".to_string(),
    }
}

pub fn confidence(confidence: f32) -> String {
    format!("{confidence:.2}")
}

/// One spot, as text mode's stdout product. Deliberately the RBN cluster
/// line minus the `DX de <spotter>-#:` prefix and the Zulu timestamp, so
/// RBN-trained eyes parse it without relearning: `Spot` carries no
/// wall-clock time (see its own doc comment), and taking one from
/// `SystemTime::now()` here would break the byte-identical file-replay
/// contract this project depends on.
pub fn spot_line(spot: &Spot) -> String {
    format!(
        "{freq:>9} kHz  {call:<8} CW  {snr:>3} dB  {wpm:>3} WPM  {ctx:<7} conf {conf}",
        freq = khz(spot.freq_hz),
        call = spot.callsign,
        snr = db(spot.snr_db),
        wpm = wpm(spot.wpm),
        ctx = spot.spot_type,
        conf = confidence(spot.confidence),
    )
}

/// Flattens an error chain to one line: `error: <what failed>: <cause>`.
/// Lowercase `error:` matches what clap already prints for usage errors, so
/// the binary has one error style rather than two.
pub fn render_error(err: &anyhow::Error) -> String {
    let mut out = String::from("error");
    for cause in err.chain() {
        let text = one_line(&cause.to_string());
        if text.is_empty() {
            continue; // a Hint carries no text of its own
        }
        out.push_str(": ");
        out.push_str(&text);
    }
    out
}

/// Flattens one cause's own `Display` onto a single physical line.
/// Flattening the anyhow *chain* is not enough: an individual error can be
/// multiline by itself — most notably `toml::de::Error` from a malformed
/// `--server-config`, which renders a source snippet plus a caret over
/// three or more lines — which would break the one-line error contract
/// from the inside (MAN-130 remediation).
///
/// Only line breaks are rewritten, together with the indentation a
/// multiline diagnostic puts either side of them. An interior run of
/// spaces or tabs is part of the *value* being reported — `/tmp/foo  bar.wav`
/// is a different file from `/tmp/foo bar.wav` — so collapsing every
/// whitespace run (which `split_whitespace()` did) pointed the operator at
/// a filename that does not exist. Non-whitespace control characters are
/// escaped rather than passed through: an error text carrying a terminal
/// escape sequence would otherwise clear, recolour or otherwise falsify
/// the operator's screen, which the Debug rendering this ticket removed
/// used to prevent for free.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split('\n') {
        let segment = segment.trim_matches(|c: char| c == '\r' || c == ' ' || c == '\t');
        if segment.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        for c in segment.chars() {
            // A tab inside the line is meaningful whitespace, like a space.
            if c == '\t' || !c.is_control() {
                out.push(c);
            } else {
                out.extend(c.escape_debug());
            }
        }
    }
    out
}

/// An operator-facing suggestion attached to an error chain with
/// `.context(Hint(..))`. Its `Display` is empty so it never appears in the
/// `error:` line; `render_hint` pulls it back out by downcast.
#[derive(Debug)]
pub struct Hint(pub &'static str);

impl std::fmt::Display for Hint {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for Hint {}

/// The `hint:` line for an error, if one was attached.
pub fn render_hint(err: &anyhow::Error) -> Option<String> {
    // `anyhow::Error::downcast_ref` -- not `chain().find_map(..)` -- is what
    // finds a *context value*: each `chain()` link for a `.context(C)`
    // layer is anyhow's internal `ContextError<C, E>` wrapper, not `C`
    // itself, so downcasting the `&dyn Error` links never matches.
    err.downcast_ref::<Hint>().map(|h| format!("hint: {}", h.0))
}

// ---------------------------------------------------------------------
// The live character monitor's stderr line
// ---------------------------------------------------------------------

/// A live monitor that grows one fragment at a time on a stream shared
/// with other writers, and so owns an *unterminated* line: `listen`'s
/// per-character decode monitor on stderr is the only one today.
///
/// Anything else that writes to that stream -- `main`'s `error:` line, and
/// every `tracing` record the spot server's telnet/JSON tasks emit while
/// `listen` is still running -- must close the open line first, or its
/// output is glued onto the monitor's
/// (`CQ DE W1AW2026-.. json_stream: raw TCP client connected`). Terminating
/// the line once, after `listen` returns, cannot prevent those mid-run
/// collisions; `terminate` at every writer's own entry point can (MAN-130
/// review).
#[derive(Debug)]
pub struct MonitorLine {
    open: std::sync::atomic::AtomicBool,
}

impl MonitorLine {
    pub const fn new() -> Self {
        Self {
            open: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Append monitor text, remembering that the line is now open.
    pub fn append(&self, w: &mut impl std::io::Write, text: &str) {
        let _ = w.write_all(text.as_bytes());
        let _ = w.flush();
        self.open.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Close the open line, if there is one. Idempotent, so every writer
    /// can call it unconditionally and only the first one after monitor
    /// text emits the newline.
    pub fn terminate(&self, w: &mut impl std::io::Write) {
        if self.open.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }
}

impl Default for MonitorLine {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide monitor line on stderr.
static STDERR_MONITOR: MonitorLine = MonitorLine::new();

/// Write one fragment of `listen`'s live character monitor to stderr.
/// Takes the stderr lock for the write, which is what keeps a concurrent
/// `tracing` record (written through [`monitor_aware_stderr`], from a
/// server task's own thread) from landing inside it.
pub fn monitor_write(text: &str) {
    let mut err = std::io::stderr().lock();
    STDERR_MONITOR.append(&mut err, text);
}

/// Close the character monitor's line on stderr if it left one open.
/// Cheap and idempotent: call it before writing anything else to stderr.
pub fn end_monitor_line() {
    let mut err = std::io::stderr().lock();
    STDERR_MONITOR.terminate(&mut err);
}

/// A held stderr lock that starts on a fresh line -- see
/// [`monitor_aware_stderr`].
#[derive(Debug)]
pub struct MonitorAwareStderr(std::io::StderrLock<'static>);

impl std::io::Write for MonitorAwareStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The `tracing` subscriber's stderr writer: closes the character
/// monitor's line before the record's first byte, and holds the stderr
/// lock for the whole record so the monitor cannot resume inside it. Used
/// as a `MakeWriter` (`fn() -> impl Write`) exactly like `std::io::stderr`
/// was, so a log line always starts in column zero and the monitor always
/// resumes on a line of its own (MAN-130 review).
pub fn monitor_aware_stderr() -> MonitorAwareStderr {
    let mut err = std::io::stderr().lock();
    STDERR_MONITOR.terminate(&mut err);
    MonitorAwareStderr(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

    fn spot() -> Spot {
        Spot {
            callsign: "W1AW".to_string(),
            freq_hz: 14_000_744.059_194_47,
            snr_db: 17.780_464,
            wpm: 17.307_692,
            spot_type: SpotType::Cq,
            confidence: 0.805_258_6,
            track_id: 24,
            sample_ts: 548_480,
        }
    }

    #[test]
    fn spot_line_has_no_debug_rendering() {
        assert_eq!(
            spot_line(&spot()),
            "  14000.7 kHz  W1AW     CW   18 dB   17 WPM  CQ      conf 0.81"
        );
    }

    #[test]
    fn unknown_spot_type_reads_as_a_word() {
        let mut s = spot();
        s.spot_type = SpotType::Unknown;
        assert!(spot_line(&s).contains("unknown"));
    }

    #[test]
    fn absent_speed_is_not_an_option() {
        assert_eq!(wpm_opt(None), "unknown");
        assert_eq!(wpm_opt(Some(17.647_058)), "18");
    }

    #[test]
    fn error_chain_renders_on_one_line() {
        let err = anyhow::anyhow!("No such file or directory (os error 2)")
            .context("open WAV ./nope.wav");
        assert_eq!(
            render_error(&err),
            "error: open WAV ./nope.wav: No such file or directory (os error 2)"
        );
        assert_eq!(render_hint(&err), None);
    }

    /// A cause whose own `Display` spans several lines (a malformed
    /// `--server-config`'s `toml::de::Error` snippet-and-caret is the real
    /// case) must still render as ONE stderr line (MAN-130 remediation).
    #[test]
    fn a_multiline_cause_still_renders_on_one_line() {
        let toml_like = "TOML parse error at line 2, column 9\n  |\n2 | port = \"nope\"\n  \
                         |         ^^^^^^\ninvalid type: string, expected u16\n";
        let err = anyhow::anyhow!("{toml_like}").context("read server config ./manta.toml");
        let rendered = render_error(&err);
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(!rendered.contains('\r'), "{rendered}");
        // The caret line's own interior padding survives: only the line
        // breaks and the indentation around them are rewritten.
        assert_eq!(
            rendered,
            "error: read server config ./manta.toml: TOML parse error at line 2, column 9 | 2 | \
             port = \"nope\" |         ^^^^^^ invalid type: string, expected u16"
        );
    }

    /// MAN-130 review: flattening the message must not rewrite the *value*
    /// it reports. `split_whitespace()` turned `/tmp/foo  bar.wav` into
    /// `/tmp/foo bar.wav` -- a different, non-existent file, and the one the
    /// operator would then go looking for.
    #[test]
    fn interior_whitespace_in_an_error_value_is_preserved() {
        let err = anyhow::anyhow!("No such file or directory (os error 2)")
            .context("open WAV /tmp/foo  bar.wav");
        assert_eq!(
            render_error(&err),
            "error: open WAV /tmp/foo  bar.wav: No such file or directory (os error 2)"
        );
        let tabbed = anyhow::anyhow!("open WAV /tmp/a\tb.wav");
        assert_eq!(render_error(&tabbed), "error: open WAV /tmp/a\tb.wav");
    }

    /// MAN-130 review: an error carrying a terminal control sequence (a
    /// mistyped argument echoed back, say) must not be able to clear or
    /// recolour the operator's screen. The Debug rendering this ticket
    /// removed escaped those bytes for free; `render_error` has to do it
    /// deliberately.
    #[test]
    fn control_characters_in_an_error_are_escaped_not_executed() {
        let err = anyhow::anyhow!("unknown vector '\u{1b}[2Jbad'");
        let rendered = render_error(&err);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert_eq!(rendered, "error: unknown vector '\\u{1b}[2Jbad'");
    }

    /// The coordination contract every stderr writer relies on: monitor
    /// text leaves the line open, the next writer closes it exactly once,
    /// and a writer that finds no monitor text emits nothing extra. The
    /// stderr plumbing around this (`monitor_write`,
    /// `monitor_aware_stderr`) is the same two calls against a locked
    /// stderr instead of this buffer.
    #[test]
    fn a_log_line_never_lands_inside_the_monitor_line() {
        let monitor = MonitorLine::new();
        let mut out: Vec<u8> = Vec::new();

        // nothing written yet: no gratuitous blank line
        monitor.terminate(&mut out);
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "");

        // monitor text, then a log record through the monitor-aware writer
        monitor.append(&mut out, "CQ DE ");
        monitor.append(&mut out, "W1AW");
        monitor.terminate(&mut out);
        out.extend_from_slice(b"2026-01-01 json_stream: raw TCP client connected\n");
        // ... and the monitor resumes on a line of its own
        monitor.append(&mut out, "K");
        monitor.terminate(&mut out);
        // idempotent: a second writer adds nothing
        monitor.terminate(&mut out);

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "CQ DE W1AW\n2026-01-01 json_stream: raw TCP client connected\nK\n"
        );
    }

    #[test]
    fn a_hint_is_carried_beside_the_error_not_inside_it() {
        let err = anyhow::anyhow!("AudioIqSource requires 48000 Hz, got 96000").context(Hint(
            "--source needs a 48 kHz mono WAV; use `manta decode` for IQ WAVs",
        ));
        // the hint must NOT appear in the error line ...
        assert_eq!(
            render_error(&err),
            "error: AudioIqSource requires 48000 Hz, got 96000"
        );
        // ... and must be retrievable as its own line
        assert_eq!(
            render_hint(&err).as_deref(),
            Some("hint: --source needs a 48 kHz mono WAV; use `manta decode` for IQ WAVs")
        );
    }
}
