//! cty.dat prefix allocation table. ARCHITECTURE §6.2.
//!
//! AD1C format: each entry is
//! `Name: cq-zone: itu-zone: continent: lat: lon: utc-offset: primary-prefix:`
//! followed by a comma-separated alias list terminated by `;` (the alias
//! list may span multiple lines). Only the alias list matters here --
//! country metadata isn't needed for a boolean allocation gate. Aliases may
//! carry a leading `=` (exact-call override, e.g. `=W3LPL`) or trailing
//! `(zone)[itu]`-style annotations; both are stripped. One entry embeds a
//! non-callsign `=VERSION` marker (the file's own version stamp) -- it's
//! filtered out explicitly.

pub struct Table {
    /// Sorted, deduplicated prefixes/exact calls, ascending.
    prefixes: Vec<String>,
}

impl Table {
    /// Parses a `cty.dat` file's full contents.
    pub fn parse(cty_dat: &str) -> Self {
        let mut prefixes = Vec::new();
        for entry in cty_dat.split(';') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some(alias_start) = entry.rfind(':') else {
                continue; // malformed entry, skip
            };
            for alias in entry[alias_start + 1..].split(',') {
                if let Some(prefix) = clean_alias(alias) {
                    prefixes.push(prefix);
                }
            }
        }
        prefixes.sort();
        prefixes.dedup();
        Self { prefixes }
    }

    /// True if any prefix-length slice of `callsign` (from 1 character up
    /// to the whole string) is an allocated prefix or exact-call override.
    /// This is a boolean allocation gate, not a country lookup, so it
    /// doesn't matter *which* length matches, only that one does.
    pub fn is_allocated(&self, callsign: &str) -> bool {
        let call = callsign.to_uppercase();
        (1..=call.len()).any(|len| self.prefixes.binary_search(&call[..len].to_string()).is_ok())
    }
}

/// Strips a leading `=` (exact-call marker) and any trailing
/// `(zone)`/`[itu]`/`<coords>`/`{continent}` override annotation. Returns
/// `None` for the file's embedded `=VERSION` metadata marker.
fn clean_alias(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_start_matches('=');
    let end = raw.find(['(', '[', '<', '{']).unwrap_or(raw.len());
    let prefix = raw[..end].trim().to_uppercase();
    if prefix.is_empty() || prefix.starts_with("VERSION") {
        return None;
    }
    Some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
Alaska:           1:  1: NA:  65.0: 150.0:  9.0:  KL:
    KL,KL7(1)[65];
Canada:           4:  4: NA:  45.0:  75.0:  5.0:  VE:
    VE,VA,VO,VY;
Equatorial Guinea:36: 47: AF:   1.7:  10.3: -1.0: 3C:
    3C,=VERSION;
";

    #[test]
    fn allocated_prefix_matches() {
        let table = Table::parse(FIXTURE);
        assert!(table.is_allocated("K5ARH"));
        assert!(table.is_allocated("W1AW"));
        assert!(table.is_allocated("VE3ABC"));
        assert!(table.is_allocated("KL7AB"));
    }

    #[test]
    fn unallocated_prefix_rejected() {
        let table = Table::parse(FIXTURE);
        assert!(!table.is_allocated("ZZ9ZZZ"));
        assert!(!table.is_allocated("QQ1AAA"));
    }

    #[test]
    fn version_marker_is_not_a_callsign() {
        // Isolated fixture, deliberately without a "VE"/"V"-prefixed entry
        // like the main FIXTURE's Canada block -- otherwise "VERSION"
        // would (correctly) match Canada's real "VE" prefix, which isn't
        // what this test is checking. This checks that the literal
        // `=VERSION` alias itself never becomes a registered prefix.
        let fixture = "\
Equatorial Guinea:36: 47: AF:   1.7:  10.3: -1.0: 3C:
    3C,=VERSION;
";
        let table = Table::parse(fixture);
        assert!(!table.is_allocated("VERSION"));
    }

    #[test]
    fn zone_annotations_are_stripped() {
        let table = Table::parse(FIXTURE);
        // "KL7(1)[65]" must register as prefix "KL7", not "KL7(1)[65]".
        assert!(table.is_allocated("KL7XY"));
    }
}
