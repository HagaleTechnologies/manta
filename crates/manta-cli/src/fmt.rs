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
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        assert_eq!(
            rendered,
            "error: read server config ./manta.toml: TOML parse error at line 2, column 9 | 2 | \
             port = \"nope\" | ^^^^^^ invalid type: string, expected u16"
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
