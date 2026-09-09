//! Callsign structural grammar. ARCHITECTURE §6.2 -- a cheap pre-filter for
//! obviously-garbled decoder output before the cty.dat lookup (which is the
//! real allocation gate, see `cty.rs`). Deliberately permissive: 3-7
//! alphanumeric characters with at least one digit, at least one letter,
//! ending in a letter, plus an optional portable designator (`/P`, `/QRP`,
//! `/MM`, `/AM`, `/M`, or `/<digit>`).

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

/// A blanket per-character repeat-count limit was tried here (2026-09-09)
/// to reject noise-decoded garble like `4AEEEEE`/`ER1EEAE`, but Codex
/// review on PR #154 found it also rejects real, allocated `master.scp`
/// callsigns that legitimately repeat a character 3+ times, non-
/// consecutively -- `9A5SSS`, `AA6AA`, `BG8GGG`, `DD5DD`, `DL0LOL` -- with
/// no way to allowlist around it since this runs before the SCP boost.
/// Structural repeat-counting can't distinguish those from garble; the
/// actual fix lives in `validator.rs`'s WPM-plausibility gate, scoped to
/// the `SpotType::Beacon` path those overnight false positives actually
/// came through.
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
    has_digit && has_letter && chars.last().unwrap().is_ascii_alphabetic()
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

    /// Real, allocated `master.scp` callsigns that repeat a character
    /// 2-4 times, several non-consecutively -- Codex review on PR #154
    /// found the earlier repeat-count check rejected all of these.
    #[test]
    fn accepts_real_callsigns_with_repeated_characters() {
        for call in [
            "K2ZZ", "W3SS", "N5FPP", "9A5SSS", "AA6AA", "BG8GGG", "DD5DD", "DL0LOL",
        ] {
            assert!(is_plausible(call), "{call} should be plausible");
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
