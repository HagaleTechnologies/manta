//! Presentation-only measurements for human reports, never wire serialization.

/// RF or tone frequency, displayed in kHz.
pub fn khz(hz: f64) -> String {
    if hz.is_finite() {
        format!("{:.1} kHz", hz / 1000.0)
    } else {
        "unknown".into()
    }
}

fn integer(value: f32, unit: &str) -> String {
    if !value.is_finite() {
        return "unknown".into();
    }
    let rounded = value.round();
    let normalized = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{normalized:.0} {unit}")
}

/// SNR at the caller's reference bandwidth, rounded half away from zero.
pub fn db(value: f32) -> String {
    integer(value, "dB")
}

/// Speed in words per minute, rounded half away from zero.
pub fn wpm(value: f32) -> String {
    integer(value, "WPM")
}

/// Confidence, without clamping or altering the underlying measurement.
pub fn confidence(value: f32) -> String {
    if value.is_finite() {
        format!("{value:.2}")
    } else {
        "unknown".into()
    }
}

/// Explicit string quoting for terminal diagnostics and value inspection.
/// JSON handles quotes/backslashes; the terminal policy also escapes DEL/C1.
pub fn quoted(value: &str) -> String {
    crate::status_doc::escape_for_terminal(
        &serde_json::to_string(value).expect("string serialization"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoted_strings_preserve_spaces_and_unicode_without_terminal_controls() {
        assert_eq!(quoted("  café  "), "\"  café  \"");
        let text = quoted("x\r\n\t\0\u{1b}\u{7f}\u{85}\"\\  ");
        assert!(!text.chars().any(char::is_control));
        assert!(text.starts_with(r#""x\r\n\t\u0000\u001b\u{7f}\u{85}\"\\"#));
        assert!(text.ends_with("  \""));
    }

    use super::*;

    #[test]
    fn measurements_have_shared_units_and_precision() {
        assert_eq!(khz(14_012_349.9), "14012.3 kHz");
        assert_eq!(db(20.4), "20 dB");
        assert_eq!(db(20.5), "21 dB");
        assert_eq!(db(-20.5), "-21 dB");
        assert_eq!(db(-0.1), "0 dB");
        assert_eq!(db(0.0), "0 dB");
        assert_eq!(wpm(19.929_108), "20 WPM");
        assert_eq!(wpm(20.5), "21 WPM");
        assert_eq!(confidence(0.805_258_6), "0.81");
        assert_eq!(confidence(0.0), "0.00");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(db(bad), "unknown");
            assert_eq!(wpm(bad), "unknown");
            assert_eq!(confidence(bad), "unknown");
            assert_eq!(khz(f64::from(bad)), "unknown");
        }
    }
}
