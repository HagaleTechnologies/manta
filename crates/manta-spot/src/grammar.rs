//! Callsign structural grammar. ARCHITECTURE §6.2 -- a cheap pre-filter for
//! obviously-garbled decoder output before the cty.dat lookup (which is the
//! real allocation gate, see `cty.rs`). Deliberately permissive: 3-7
//! alphanumeric characters with at least one digit, at least one letter,
//! ending in a letter, at most `MAX_SAME_CHAR_REPEATS` of any one
//! character, plus an optional portable designator (`/P`, `/QRP`, `/MM`,
//! `/AM`, `/M`, or `/<digit>`).

/// True if `call` has the rough shape of an amateur-radio callsign.
pub fn is_plausible(call: &str) -> bool {
    let (base, portable) = match call.split_once('/') {
        Some((b, p)) => (b, Some(p)),
        None => (call, None),
    };
    if let Some(p) = portable {
        if !is_valid_portable(p) {
            return false;
        }
    }
    is_valid_base(base)
}

pub(crate) fn is_valid_portable(p: &str) -> bool {
    matches!(p, "P" | "QRP" | "MM" | "AM" | "M")
        || (p.len() == 1 && p.chars().next().unwrap().is_ascii_digit())
}

/// A real assigned callsign occasionally repeats one character twice
/// (vanity/special-event calls like `K2ZZ`, `W3SS` are real and common),
/// but three or more of the same character in a 3-7 character string is a
/// much stronger tell for noise-decoded garble than for a real
/// allocation -- confirmed against a real overnight RSP1B/40m capture
/// (2026-09-09, docs/DECISIONS): every false-positive "spot" that
/// repeated any character at all repeated it 2+ times (`4AEEEEE`,
/// `ER1EEAE`, `3EMEEEE`, ...). This alone doesn't catch every garbled
/// decode (many read as structurally plausible with zero repeats), but
/// it's a real, low-risk-of-regression signal worth checking.
const MAX_SAME_CHAR_REPEATS: usize = 2;

fn is_valid_base(base: &str) -> bool {
    let chars: Vec<char> = base.chars().collect();
    if chars.len() < 3 || chars.len() > 7 {
        return false;
    }
    if !chars.iter().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let has_digit = chars.iter().any(|c| c.is_ascii_digit());
    let has_letter = chars.iter().any(|c| c.is_ascii_alphabetic());
    if !(has_digit && has_letter && chars.last().unwrap().is_ascii_alphabetic()) {
        return false;
    }
    chars
        .iter()
        .all(|&c| chars.iter().filter(|&&other| other == c).count() <= MAX_SAME_CHAR_REPEATS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_shaped_callsigns() {
        for call in ["K5ARH", "W1AW", "4X1AA", "VE3ABC", "JA1ABC", "ZL2XYZ"] {
            assert!(is_plausible(call), "{call} should be plausible");
        }
    }

    /// A single doubled character is a common, real vanity/special-event
    /// callsign pattern (e.g. K2ZZ, W3SS) -- must not be rejected by the
    /// repeat-count check.
    #[test]
    fn accepts_a_single_doubled_character() {
        for call in ["K2ZZ", "W3SS", "N5FPP"] {
            assert!(is_plausible(call), "{call} should be plausible");
        }
    }

    /// Real overnight RSP1B/40m noise-floor false positives (2026-09-09,
    /// docs/DECISIONS) that repeated one character 3+ times -- every one
    /// of these was confirmed as a "spot" before this check existed.
    #[test]
    fn rejects_garble_with_excessive_character_repeats() {
        for call in [
            "4AEEEEE", "ER1EEAE", "3EMEEEE", "EII4EE", "E5YEEU", "TE7TT", "4ETENE",
        ] {
            assert!(!is_plausible(call), "{call} should be rejected");
        }
    }

    #[test]
    fn accepts_portable_designators() {
        for call in [
            "K5ARH/P",
            "K5ARH/QRP",
            "K5ARH/MM",
            "K5ARH/AM",
            "K5ARH/M",
            "K5ARH/3",
        ] {
            assert!(is_plausible(call), "{call} should be plausible");
        }
    }

    #[test]
    fn rejects_garble() {
        for call in [
            "",
            "ZZ",
            "12345",
            "ABCDEFG",
            "K5ARH/BOGUS",
            "TOOLONGCALLSIGN123",
        ] {
            assert!(!is_plausible(call), "{call} should be rejected");
        }
    }
}
