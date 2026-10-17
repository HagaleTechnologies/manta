//! Callsign structural grammar. ARCHITECTURE §6.2 -- a cheap pre-filter for
//! obviously-garbled decoder output before the cty.dat lookup (which is the
//! real allocation gate, see `cty.rs`). Deliberately permissive: 3-7
//! alphanumeric characters with at least one digit, at least one letter,
//! ending in a letter, plus an optional portable designator (`/P`, `/QRP`,
//! `/MM`, `/AM`, `/M`, or `/<digit>`). A fixed list of non-callsign CW
//! conventions (`NON_CALLSIGN_TOKENS`) is rejected by exact match first,
//! whatever cty.dat would say about it (MAN-105).
//!
//! Scope: decoder output only. NOT operator-identity validation -- see
//! `manta_server::config::check_operator_callsign` (MAN-45/MAN-89) for why a
//! configured station callsign needs a wider, separately-designed rule
//! (real DXCC prefix overrides, and RBN's `CALL-N-#` per-band SSIDs).

/// Common CW conventions that are never a callsign, rejected by exact,
/// ASCII-case-insensitive match against the base call (the part before any
/// portable designator) (MAN-105). Some match an allocated cty.dat prefix --
/// `5NN` is Nigeria's `5N`, `3NN` falls inside China's `3H`-`3U` block -- so
/// without this list they passed both gates and spotted, and, once in
/// MAN-100's support ledger, out-supported a real call ending in the same
/// text (`HA5NN`). The rest already fail the shape rules below (no digit,
/// no letter, or under 3 characters) and are listed so they stay rejected
/// if those rules are ever loosened. `4NN` completes the cut-number RST
/// family for an operator `--cty` file that might allocate it.
///
/// Matching is exact on purpose: real `master.scp` calls contain these as
/// substrings (`DJ5NN`, `5NNHR`, `OM2AGN`, `VK4QRZ`, `KN4ABC`), and no line
/// of the vendored `master.scp` is one of them. Merged-word artifacts
/// (`TU5NN`) are callsign-shaped and left to word segmentation.
const NON_CALLSIGN_TOKENS: &[&str] = &[
    "5NN", "4NN", "3NN", "599", "TEST", "TU", "QRZ", "AGN", "K", "KN",
];

/// True if `call` has the rough shape of an amateur-radio callsign.
pub fn is_plausible(call: &str) -> bool {
    let (base, portable) = match call.split_once('/') {
        Some((b, p)) => (b, Some(p)),
        None => (call, None),
    };
    if NON_CALLSIGN_TOKENS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(base))
    {
        return false;
    }
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

    #[test]
    fn rejects_non_callsign_conventions() {
        for token in NON_CALLSIGN_TOKENS {
            let lower = token.to_ascii_lowercase();
            let portable = format!("{token}/P");
            for call in [*token, lower.as_str(), portable.as_str()] {
                assert!(!is_plausible(call), "{call} should be rejected");
            }
        }
    }

    /// Real calls contain the listed conventions as substrings; only an
    /// exact match is rejected.
    #[test]
    fn non_callsign_check_is_exact_match_only() {
        for call in [
            "DJ5NN", "W5NN", "5NNHR", "UR5NN", "OM2AGN", "VK4QRZ", "KN4ABC",
        ] {
            assert!(is_plausible(call), "{call} should be plausible");
        }
    }

    #[test]
    fn no_listed_token_is_a_real_master_scp_call() {
        for line in crate::MASTER_SCP.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let base = line.split_once('/').map_or(line, |(b, _)| b);
            assert!(
                !NON_CALLSIGN_TOKENS
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(base)),
                "master.scp lists {line}, whose base call is a listed convention"
            );
        }
    }
}
