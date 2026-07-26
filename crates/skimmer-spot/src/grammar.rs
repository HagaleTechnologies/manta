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

fn is_valid_portable(p: &str) -> bool {
    matches!(p, "P" | "QRP" | "MM" | "AM" | "M")
        || (p.len() == 1 && p.chars().next().unwrap().is_ascii_digit())
}

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

    #[test]
    fn accepts_portable_designators() {
        for call in ["K5ARH/P", "K5ARH/QRP", "K5ARH/MM", "K5ARH/AM", "K5ARH/M", "K5ARH/3"] {
            assert!(is_plausible(call), "{call} should be plausible");
        }
    }

    #[test]
    fn rejects_garble() {
        for call in ["", "ZZ", "12345", "ABCDEFG", "K5ARH/BOGUS", "TOOLONGCALLSIGN123"] {
            assert!(!is_plausible(call), "{call} should be rejected");
        }
    }
}
