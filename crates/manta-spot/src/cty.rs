//! cty.dat prefix allocation table. ARCHITECTURE §6.2.
//!
//! AD1C format: each entry is
//! `Name: cq-zone: itu-zone: continent: lat: lon: utc-offset: primary-prefix:`
//! followed by a comma-separated alias list terminated by `;` (the alias
//! list may span multiple lines). Only the alias list matters here --
//! country metadata isn't needed for a boolean allocation gate. Aliases may
//! carry a leading `=` (exact-call override, e.g. `=W3LPL`) or a trailing
//! `(zone)[itu]`-style annotation overriding that *specific alias*'s CQ/ITU
//! zone away from the entity header's default (e.g. China's `B0(23)` spots
//! as zone 23, not the header's zone 24) -- the zone override is parsed out
//! and applied per-alias; `[itu]`/`<coords>`/`{continent}` annotations are
//! still stripped, not modeled. An `=`-prefixed alias is an EXACT-call
//! override, not a prefix: it only matches a callsign identical to the
//! alias itself, never as a prefix of a longer call (e.g. `=K5AGC(3)`
//! must not make `K5AGCA` resolve through it instead of the generic `K`
//! prefix). A primary-prefix field itself starting with `*` (e.g.
//! Shetland Islands' `*GM/s`) marks a starred SUBENTITY -- a more specific
//! geographic subdivision of a parent entity that shares some of the same
//! exact-call aliases (e.g. `=GB1DAA` is listed under both Scotland and
//! Shetland Islands in the real vendored file); the starred subentity
//! wins that alias regardless of which one appears first in the file. One
//! entry embeds a non-callsign `=VERSION` marker (the file's own version
//! stamp) -- it's filtered out explicitly. `lon` is negated from the
//! file's raw value: AD1C stores longitude **west-positive** (e.g. the
//! United States entry reads `91.87`), while this module's `Entry::lon`
//! uses the ordinary east-positive convention (GeoJSON, most mapping
//! libraries) that `manta-server`'s JSON stream (`dxLon`/`deLon`) is
//! expected to emit.

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

/// One accumulated `(prefix, entry, is_exact)` row, tracked separately
/// from the public `Entry` -- `is_exact` (whether this alias had a
/// leading `=`) governs matching behavior (see `Table::prefix_entry`),
/// not entity metadata callers care about.
struct Row {
    prefix: String,
    entry: Entry,
    is_exact: bool,
}

pub struct Table {
    /// Sorted, deduplicated by prefix, ascending. A prefix maps to the
    /// entity it was declared under; when the same alias text appears
    /// under more than one entity (a starred subentity sharing an exact
    /// call with its parent -- the only case a well-formed `cty.dat`
    /// produces this for), the starred subentity's row wins regardless of
    /// file order (see module doc comment).
    entries: Vec<Row>,
}

impl Table {
    /// Parses a `cty.dat` file's full contents.
    pub fn parse(cty_dat: &str) -> Self {
        let mut rows: Vec<(Row, bool /* is_starred */)> = Vec::new();
        for raw_entry in cty_dat.split(';') {
            let raw_entry = raw_entry.trim();
            if raw_entry.is_empty() {
                continue;
            }
            let Some(alias_start) = raw_entry.rfind(':') else {
                continue; // malformed entry, skip
            };
            let Some((entry, is_starred)) = parse_header(&raw_entry[..alias_start]) else {
                continue; // malformed header, skip
            };
            for alias in raw_entry[alias_start + 1..].split(',') {
                if let Some((prefix, is_exact, zone_override)) = clean_alias(alias) {
                    let mut entry = entry.clone();
                    if let Some(zone) = zone_override {
                        entry.cq_zone = zone;
                    }
                    rows.push((
                        Row {
                            prefix,
                            entry,
                            is_exact,
                        },
                        is_starred,
                    ));
                }
            }
        }
        rows.sort_by(|a, b| a.0.prefix.cmp(&b.0.prefix));

        let mut entries: Vec<Row> = Vec::with_capacity(rows.len());
        for (row, is_starred) in rows {
            match entries.last_mut() {
                Some(last) if last.prefix == row.prefix => {
                    if is_starred {
                        *last = row; // starred subentity wins this alias
                    }
                    // else: first-seen (already in `entries`) stands.
                }
                _ => entries.push(row),
            }
        }
        Self { entries }
    }

    /// True if any prefix-length slice of `callsign` (from 1 character up
    /// to the whole string) is an allocated prefix or exact-call override.
    /// This is a boolean allocation gate, not a country lookup, so it
    /// doesn't matter *which* length matches, only that one does.
    pub fn is_allocated(&self, callsign: &str) -> bool {
        let call = callsign.to_uppercase();
        (1..=call.len()).any(|len| self.prefix_entry(&call, len).is_some())
    }

    /// Country-entity metadata for `callsign`'s longest matching allocated
    /// prefix (most specific entity wins, e.g. `KL7...` resolves to Alaska,
    /// not the generic `K` United States entry). `None` if unallocated.
    pub fn lookup(&self, callsign: &str) -> Option<&Entry> {
        let call = callsign.to_uppercase();
        (1..=call.len())
            .rev()
            .find_map(|len| self.prefix_entry(&call, len))
    }

    /// Looks up `call[..len]`, but an exact-call alias (`is_exact`) only
    /// counts as a match when EITHER: `len == call.len()` -- a literal
    /// full match, covering both a plain exact alias (`=K5AGC` matching
    /// bare `K5AGC`) and an alias that already carries its own portable
    /// suffix (`=EA5IYX/P`, a distinct real DXCC entity from its base
    /// call -- matching literal `EA5IYX/P`, not stripped) -- OR `len`
    /// reaches the callsign's BASE length, i.e. `call` is that alias plus
    /// a valid portable designator the alias itself doesn't carry
    /// (`=4U1UN` must still resolve `4U1UN/P`). Neither condition is met
    /// by a callsign merely PREFIXED by the alias (`=K5AGC` must not make
    /// `K5AGCA` resolve through it instead of the generic `K` prefix).
    fn prefix_entry(&self, call: &str, len: usize) -> Option<&Entry> {
        let slice = &call[..len];
        let idx = self
            .entries
            .binary_search_by(|row| row.prefix.as_str().cmp(slice))
            .ok()?;
        let row = &self.entries[idx];
        if row.is_exact && len != call.len() && len != exact_match_base_len(call) {
            return None;
        }
        Some(&row.entry)
    }
}

/// The callsign length an exact-call alias must match against: the whole
/// call, or -- if `call` carries a valid portable designator (`/P`,
/// `/QRP`, `/MM`, `/AM`, `/M`, `/<digit>`; see `grammar::is_valid_portable`
/// for the exact set) -- just the base, so `4U1UN/P` still matches an
/// `=4U1UN`-only alias. An invalid/unrecognized suffix after `/` (garbage,
/// or a second callsign glued on) is NOT stripped -- the full string must
/// match, same as before.
fn exact_match_base_len(call: &str) -> usize {
    match call.split_once('/') {
        Some((base, portable)) if crate::grammar::is_valid_portable(portable) => base.len(),
        _ => call.len(),
    }
}

/// Parses the header portion of one `cty.dat` entry (everything before the
/// alias list's leading colon), e.g.
/// `"United States:    5:  8: NA:  40.0:  75.0:  5.0:  K"`. Returns the
/// entity metadata plus whether the primary-prefix field marks this a
/// starred subentity (a leading `*`, e.g. `*GM/s` for Shetland Islands).
fn parse_header(header: &str) -> Option<(Entry, bool)> {
    let fields: Vec<&str> = header.split(':').map(str::trim).collect();
    // Name, cq-zone, itu-zone, continent, lat, lon, utc-offset, primary-prefix.
    if fields.len() != 8 {
        return None;
    }
    let raw_lon: f64 = fields[5].parse().ok()?;
    let is_starred = fields[7].starts_with('*');
    Some((
        Entry {
            cq_zone: fields[1].parse().ok()?,
            continent: fields[3].to_uppercase(),
            lat: fields[4].parse().ok()?,
            lon: -raw_lon, // west-positive (AD1C) -> east-positive; see module doc.
        },
        is_starred,
    ))
}

/// Strips a leading `=` (exact-call marker) and any trailing
/// `(zone)`/`[itu]`/`<coords>`/`{continent}` annotation, returning the
/// cleaned prefix, whether it was an exact-call alias (leading `=`), and
/// that alias's CQ-zone override if the `(zone)` annotation is present and
/// parses as one. Returns `None` for the file's embedded `=VERSION`
/// metadata marker.
fn clean_alias(raw: &str) -> Option<(String, bool, Option<u16>)> {
    let raw = raw.trim();
    let is_exact = raw.starts_with('=');
    let raw = raw.trim_start_matches('=');
    let end = raw.find(['(', '[', '<', '{']).unwrap_or(raw.len());
    let prefix = raw[..end].trim().to_uppercase();
    if prefix.is_empty() || prefix.starts_with("VERSION") {
        return None;
    }
    let zone_override = raw[end..]
        .strip_prefix('(')
        .and_then(|rest| rest.split(')').next())
        .and_then(|zone_str| zone_str.parse().ok());
    Some((prefix, is_exact, zone_override))
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
        // cty.dat's raw 75.0 is west-positive (AD1C convention); the United
        // States sits west of Greenwich, so the east-positive `Entry::lon`
        // must come out negative.
        assert_eq!(entry.lon, -75.0);
    }

    #[test]
    fn lon_sign_is_flipped_for_an_eastern_hemisphere_entity_too() {
        // Japan is east of Greenwich; cty.dat's west-positive raw value is
        // negative (-138.38 in the real vendored file), so the corrected
        // east-positive `Entry::lon` must come out positive.
        let fixture = "\
Japan:            25: 45: AS:  36.0: -138.0:  9.0:  JA:
    JA;
";
        let table = Table::parse(fixture);
        let entry = table.lookup("JA1ABC").expect("JA1ABC should resolve");
        assert_eq!(entry.lon, 138.0);
    }

    #[test]
    fn starred_subentity_wins_a_shared_exact_call_over_its_parent_entity() {
        // Real vendored-data shape: Scotland and Shetland Islands both
        // list =GB1DAA; Shetland's primary-prefix is starred (*GM/s),
        // marking it the more specific subentity that should win,
        // regardless of which block appears first in the file.
        let fixture = "\
Scotland:                 14: 27: EU: 56.82:  4.18:  0.0:  GM:
    GM,=GB1DAA;
Shetland Islands:         14: 27: EU: 60.50:  1.50:  0.0:  *GM/s:
    =GB1DAA;
";
        let table = Table::parse(fixture);
        let entry = table.lookup("GB1DAA").expect("GB1DAA should resolve");
        assert_eq!(
            entry.lat, 60.50,
            "Shetland's starred subentity must win the shared exact call, not Scotland (parsed first)"
        );
    }

    #[test]
    fn exact_call_alias_does_not_match_as_a_prefix_of_a_longer_call() {
        // Real vendored-data shape: the US block lists =K5AGC(3) as an
        // exact-call zone override alongside the generic K prefix (zone
        // 5). K5AGC itself must resolve to zone 3, but a longer call that
        // merely starts with "K5AGC" must NOT match it as a prefix --
        // that's the generic K prefix's job.
        let fixture = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,=K5AGC(3);
";
        let table = Table::parse(fixture);
        assert_eq!(
            table.lookup("K5AGC").expect("K5AGC should resolve").cq_zone,
            3,
            "the exact call itself must use its override"
        );
        assert_eq!(
            table.lookup("K5AGCA").expect("K5AGCA should resolve").cq_zone,
            5,
            "a longer call must fall through to the generic K prefix, not match =K5AGC as a substring prefix"
        );
    }

    #[test]
    fn exact_alias_that_already_carries_its_own_portable_suffix_still_matches_literally() {
        // Regression (round-6 review): real vendored cty.dat lists
        // `=EA5IYX/P` as its own exact alias under Balearic Islands,
        // distinct from mainland Spain's `EA` generic prefix -- the alias
        // ITSELF already has a portable suffix baked in (this specific
        // operator's portable designation is its own DXCC entity, not a
        // generic "any portable variant of this base call" rule). The
        // round-5 fix's `exact_match_base_len` unconditionally stripped
        // ANY portable-looking suffix off the INPUT before comparing,
        // which wrongly rejected a literal full match against an alias
        // that itself contains a slash -- falling through to the generic
        // `EA` prefix and emitting mainland Spain's lat/lon/zone instead
        // of the Balearic Islands'.
        let fixture = "\
Spain:                    14: 37: EU:  40.32:   -3.68: -1.0: EA:
    EA;
Balearic Islands:         14: 37: EU:  39.60:    2.95: -1.0: EA6:
    EA6,=EA5IYX/P;
";
        let table = Table::parse(fixture);
        let balearic = table
            .lookup("EA5IYX/P")
            .expect("EA5IYX/P should resolve via its exact alias");
        assert_eq!(
            balearic.lat, 39.60,
            "must resolve Balearic Islands, not mainland Spain"
        );
    }

    #[test]
    fn exact_call_alias_still_resolves_a_portable_variant_of_the_same_call() {
        // Regression (round-5 review): cty.dat never lists a separate
        // alias for a portable variant -- =K5AGC(3) covers K5AGC AND
        // K5AGC/P, K5AGC/QRP, etc. The round-4 fix above (exact aliases
        // don't match as a substring prefix of a LONGER call) initially
        // over-corrected by comparing against the whole input including
        // any portable suffix, which wrongly rejected K5AGC/P entirely
        // (falling through to the generic K prefix's zone 5 instead of
        // K5AGC's own zone-3 override).
        let fixture = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,=K5AGC(3);
";
        let table = Table::parse(fixture);
        assert_eq!(
            table.lookup("K5AGC/P").expect("K5AGC/P should resolve").cq_zone,
            3,
            "a valid portable suffix on an exact-alias call must still resolve the alias's own zone"
        );
        assert_eq!(
            table
                .lookup("K5AGC/QRP")
                .expect("K5AGC/QRP should resolve")
                .cq_zone,
            3
        );
        // Still must not regress the original round-4 fix: a genuinely
        // longer call (not a portable suffix at all) must NOT match.
        assert_eq!(
            table
                .lookup("K5AGCA")
                .expect("K5AGCA should resolve")
                .cq_zone,
            5,
            "a longer call must still fall through to the generic K prefix"
        );
    }

    #[test]
    fn per_alias_zone_override_wins_over_the_entity_header_default() {
        let fixture = "\
China:            24: 44: AS:  35.0: -103.0: -8.0: BY:
    BY,B0(23);
";
        let table = Table::parse(fixture);
        assert_eq!(
            table.lookup("B0ABC").expect("B0ABC should resolve").cq_zone,
            23,
            "B0's (23) annotation must override the header's zone 24"
        );
        assert_eq!(
            table.lookup("BYABC").expect("BYABC should resolve").cq_zone,
            24,
            "BY has no override annotation, so the header's zone 24 stands"
        );
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
