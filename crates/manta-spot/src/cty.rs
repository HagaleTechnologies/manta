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

/// Per-entity metadata carried alongside a prefix, from the `cty.dat`
/// header line's `cq-zone`/`continent`/`lat`/`lon` fields (see the module
/// doc comment for the header format). Used by `manta-server` to populate
/// the JSON spot stream's `dxContinent`/`dxCqZone` fields (real vendored
/// data, not fabricated) -- see ARCHITECTURE §7 / dispensa ADR-0011.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub continent: String,
    pub cq_zone: u16,
    pub lat: f64,
    pub lon: f64,
}

pub struct Table {
    /// Sorted, deduplicated `(prefix, entry)` pairs, ascending by prefix.
    /// A prefix maps to the entity it was declared under; if the same
    /// prefix string appears in more than one entity's alias list (not
    /// expected in a well-formed `cty.dat`), the first parse order wins.
    entries: Vec<(String, Entry)>,
}

impl Table {
    /// Parses a `cty.dat` file's full contents.
    pub fn parse(cty_dat: &str) -> Self {
        let mut entries = Vec::new();
        for raw_entry in cty_dat.split(';') {
            let raw_entry = raw_entry.trim();
            if raw_entry.is_empty() {
                continue;
            }
            let Some(alias_start) = raw_entry.rfind(':') else {
                continue; // malformed entry, skip
            };
            let Some(entry) = parse_header(&raw_entry[..alias_start]) else {
                continue; // malformed header, skip
            };
            for alias in raw_entry[alias_start + 1..].split(',') {
                if let Some(prefix) = clean_alias(alias) {
                    entries.push((prefix, entry.clone()));
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        Self { entries }
    }

    /// True if any prefix-length slice of `callsign` (from 1 character up
    /// to the whole string) is an allocated prefix or exact-call override.
    /// This is a boolean allocation gate, not a country lookup, so it
    /// doesn't matter *which* length matches, only that one does.
    pub fn is_allocated(&self, callsign: &str) -> bool {
        let call = callsign.to_uppercase();
        (1..=call.len()).any(|len| self.prefix_entry(&call[..len]).is_some())
    }

    /// Country-entity metadata for `callsign`'s longest matching allocated
    /// prefix (most specific entity wins, e.g. `KL7...` resolves to Alaska,
    /// not the generic `K` United States entry). `None` if unallocated.
    pub fn lookup(&self, callsign: &str) -> Option<&Entry> {
        let call = callsign.to_uppercase();
        (1..=call.len())
            .rev()
            .find_map(|len| self.prefix_entry(&call[..len]))
    }

    fn prefix_entry(&self, prefix: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|(p, _)| p.as_str().cmp(prefix))
            .ok()
            .map(|idx| &self.entries[idx].1)
    }
}

/// Parses the header portion of one `cty.dat` entry (everything before the
/// alias list's leading colon), e.g.
/// `"United States:    5:  8: NA:  40.0:  75.0:  5.0:  K"`.
fn parse_header(header: &str) -> Option<Entry> {
    let fields: Vec<&str> = header.split(':').map(str::trim).collect();
    // Name, cq-zone, itu-zone, continent, lat, lon, utc-offset, primary-prefix.
    if fields.len() != 8 {
        return None;
    }
    Some(Entry {
        cq_zone: fields[1].parse().ok()?,
        continent: fields[3].to_uppercase(),
        lat: fields[4].parse().ok()?,
        lon: fields[5].parse().ok()?,
    })
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

    #[test]
    fn lookup_returns_the_matched_entitys_metadata() {
        let table = Table::parse(FIXTURE);
        let entry = table.lookup("K5ARH").expect("K5ARH should resolve");
        assert_eq!(entry.continent, "NA");
        assert_eq!(entry.cq_zone, 5);
        assert_eq!(entry.lat, 40.0);
        assert_eq!(entry.lon, 75.0);
    }

    #[test]
    fn lookup_prefers_the_longest_matching_prefix() {
        let table = Table::parse(FIXTURE);
        // "KL7AB" matches both the generic "K" (United States, zone 5) and
        // the more specific "KL7" (Alaska, zone 1) -- Alaska must win.
        let entry = table.lookup("KL7AB").expect("KL7AB should resolve");
        assert_eq!(entry.cq_zone, 1);
        assert_eq!(entry.continent, "NA");
    }

    #[test]
    fn lookup_returns_none_for_unallocated_callsign() {
        let table = Table::parse(FIXTURE);
        assert!(table.lookup("ZZ9ZZZ").is_none());
    }
}
