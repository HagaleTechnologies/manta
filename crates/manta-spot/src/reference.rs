//! MAN-316: reference lists that can change while manta runs, and the checks
//! a downloaded copy must pass before it may replace the one in use.
//!
//! [`Live`] is a multi-reader "current value + generation" cell: the decode
//! thread compares generations once per event and swaps its own `Arc`s only
//! when one moved, while the daemon's per-spot readers `load()` a snapshot.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::{cty, scp};

/// One reference list that a scheduled refresh may replace while readers
/// hold snapshots of the old one.
pub struct Live<T> {
    generation: AtomicU64,
    value: RwLock<Arc<T>>,
}

impl<T> std::fmt::Debug for Live<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live")
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl<T> Live<T> {
    pub fn new(value: Arc<T>) -> Self {
        Self {
            generation: AtomicU64::new(0),
            value: RwLock::new(value),
        }
    }

    /// A snapshot; never blocks a writer for longer than an Arc clone.
    pub fn load(&self) -> Arc<T> {
        self.value.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the value; the generation moves on after the write, so a
    /// reader that sees the new generation also sees the new value.
    pub fn store(&self, value: Arc<T>) {
        *self.value.write().unwrap_or_else(|e| e.into_inner()) = value;
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// How many times [`Live::store`] has run.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// The two reference lists a refresh can replace: AD1C's `cty.dat` and
/// `MASTER.SCP`. Shared as an `Arc` between the refresher, the decode
/// thread's validator and the daemon's spot servers.
#[derive(Debug)]
pub struct ReferenceLists {
    pub cty: Live<cty::Table>,
    pub scp: Live<scp::Set>,
}

impl ReferenceLists {
    pub fn new(cty: Arc<cty::Table>, scp: Arc<scp::Set>) -> Self {
        Self {
            cty: Live::new(cty),
            scp: Live::new(scp),
        }
    }
}

/// One call per major entity, spread across the file, that every complete
/// AD1C `cty.dat` allocates: `(call, entity)`. A download cut short at an
/// entry boundary still ends with `;` and can keep 90% of the prefixes
/// (South Africa is the bundled file's second-to-last entry), so this check is
/// what notices the missing tail.
pub const CTY_CANARIES: &[(&str, &str)] = &[
    ("K1ABC", "United States"),
    ("VE3ABC", "Canada"),
    ("G4ABC", "England"),
    ("UA3ABC", "European Russia"),
    ("JA1ABC", "Japan"),
    ("VK2ABC", "Australia"),
    ("PY1ABC", "Brazil"),
    ("ZS6ABC", "South Africa"),
];

/// Strips one leading UTF-8 BOM (`\u{feff}`), common in Windows-authored
/// text files: `str::trim` does not remove it, so left in place it corrupts
/// the first line's parse.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// MAN-316: parses a downloaded `cty.dat` and returns it only if it looks
/// like a complete AD1C file; `Err` is the reason, worded to follow
/// "download rejected: " in the operator's log. `bundled` is the built-in
/// copy, the size a download is measured against.
pub fn accept_cty(text: &str, bundled: &cty::Table) -> Result<cty::Table, String> {
    let text = strip_bom(text);
    let table = cty::Table::parse(text);
    if table.is_empty() {
        return Err("it lists no prefixes".into());
    }
    if !text.trim_end().ends_with(';') {
        return Err("it does not end with ';', so it is truncated".into());
    }
    if let Some((call, entity)) = CTY_CANARIES
        .iter()
        .find(|(call, _)| !table.is_allocated(call))
    {
        return Err(format!(
            "it does not allocate {call} ({entity}), so it is truncated or not AD1C's cty.dat"
        ));
    }
    if table.len() * 10 < bundled.len() * 9 {
        return Err(format!(
            "it lists {} prefixes, under 90% of the built-in copy's {}",
            table.len(),
            bundled.len()
        ));
    }
    Ok(table)
}

/// Longest line text a rejection quotes, in characters; a longer line (a
/// minified web page, say) is cut here and ends in `…`.
const QUOTED_LINE_CHARS: usize = 60;

/// MAN-316: parses a downloaded `MASTER.SCP` and returns it only if every
/// member line is a callsign and it lists at least half as many calls as
/// the built-in copy. Member lines are the ones `scp::Set::parse` keeps:
/// trimmed, not blank, not `#`/`!` comments or headers.
pub fn accept_scp(text: &str, bundled: &scp::Set) -> Result<scp::Set, String> {
    let text = strip_bom(text);
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let is_call = line
            .to_uppercase()
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/');
        if !is_call {
            // Control characters (an escape sequence's ESC, say) become `?`:
            // the reason reaches the log, and the download is untrusted.
            let mut quoted: String = line
                .chars()
                .take(QUOTED_LINE_CHARS)
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            if line.chars().count() > QUOTED_LINE_CHARS {
                quoted.push('…');
            }
            return Err(format!(
                "line {} is \"{quoted}\", not a callsign",
                index + 1
            ));
        }
    }
    let set = scp::Set::parse(text);
    if set.len() * 2 < bundled.len() {
        return Err(format!(
            "it lists {} calls, under half the built-in copy's {}",
            set.len(),
            bundled.len()
        ));
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture `main.rs` and `validator.rs` already use: an entity no
    /// real `cty.dat` has, so a table that allocates `QQ9ZZZ` is the new one.
    const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

    /// An Apache 404 page, as a misconfigured mirror or captive portal
    /// serves it with a 200.
    const WEB_PAGE: &str = "\
<!DOCTYPE HTML PUBLIC \"-//IETF//DTD HTML 2.0//EN\">
<html><head>
<title>404 Not Found</title>
</head><body>
<h1>Not Found</h1>
<p>The requested URL was not found on this server.</p>
<hr>
<address>Apache/2.4.62 (Debian) Server at www.country-files.com Port 80</address>
</body></html>
";

    #[test]
    fn live_store_bumps_generation_and_load_returns_the_new_value() {
        let a = Arc::new("a".to_string());
        let b = Arc::new("b".to_string());
        let live = Live::new(a.clone());
        assert_eq!(live.generation(), 0);
        let before = live.load();
        assert!(Arc::ptr_eq(&before, &a));
        live.store(b.clone());
        assert_eq!(live.generation(), 1);
        assert!(Arc::ptr_eq(&live.load(), &b));
        assert!(
            Arc::ptr_eq(&before, &a),
            "a snapshot taken before the store keeps the old value"
        );
        assert_eq!(*before, "a");
    }

    #[test]
    fn reference_lists_swap_each_list_independently() {
        let lists = ReferenceLists::new(
            Arc::new(cty::Table::parse(QQ9_ENTITY)),
            Arc::new(scp::Set::parse("K1ABC\n")),
        );
        lists.cty.store(Arc::new(cty::Table::parse(crate::CTY_DAT)));
        assert_eq!((lists.cty.generation(), lists.scp.generation()), (1, 0));
        assert!(lists.cty.load().is_allocated("K1ABC"));
        assert!(lists.scp.load().contains("K1ABC"));
        lists.scp.store(Arc::new(scp::Set::parse("W1AW\n")));
        assert_eq!((lists.cty.generation(), lists.scp.generation()), (1, 1));
        assert!(lists.scp.load().contains("W1AW"));
    }

    #[test]
    fn accept_cty_takes_the_bundled_file() {
        let bundled = cty::Table::bundled();
        let table = accept_cty(crate::CTY_DAT, &bundled).unwrap();
        assert_eq!(table.len(), bundled.len());
    }

    #[test]
    fn accept_cty_takes_a_superset() {
        let bundled = cty::Table::bundled();
        assert!(!bundled.is_allocated("QQ9ZZZ"));
        let table = accept_cty(&format!("{}{QQ9_ENTITY}", crate::CTY_DAT), &bundled).unwrap();
        assert!(table.is_allocated("QQ9ZZZ"));
        assert_eq!(table.len(), bundled.len() + 1);
    }

    #[test]
    fn accept_cty_rejects_a_web_page() {
        let err = accept_cty(WEB_PAGE, &cty::Table::bundled()).unwrap_err();
        assert_eq!(err, "it lists no prefixes");
    }

    #[test]
    fn accept_cty_rejects_a_file_cut_mid_entry() {
        // Just after the first alias comma past the halfway mark: inside an
        // entry's alias list.
        let mid = crate::CTY_DAT.len() / 2;
        let comma = crate::CTY_DAT.as_bytes()[mid..]
            .iter()
            .position(|&b| b == b',')
            .unwrap();
        let cut = &crate::CTY_DAT[..mid + comma + 1];
        let err = accept_cty(cut, &cty::Table::bundled()).unwrap_err();
        assert_eq!(err, "it does not end with ';', so it is truncated");
    }

    #[test]
    fn accept_cty_rejects_a_file_cut_at_an_entry_boundary_before_zs() {
        let bundled = cty::Table::bundled();
        let zs = crate::CTY_DAT.find("\nSouth Africa:").unwrap() + 1;
        let cut = &crate::CTY_DAT[..zs];
        assert!(cut.trim_end().ends_with(';'));
        assert!(
            cty::Table::parse(cut).len() * 10 >= bundled.len() * 9,
            "the cut keeps over 90% of the prefixes, so only a canary notices"
        );
        let err = accept_cty(cut, &bundled).unwrap_err();
        assert_eq!(
            err,
            "it does not allocate ZS6ABC (South Africa), so it is truncated or not AD1C's cty.dat"
        );
    }

    #[test]
    fn accept_cty_rejects_far_fewer_prefixes() {
        let bundled = cty::Table::bundled();
        let canary_entities: String = crate::CTY_DAT
            .split_inclusive(';')
            .filter(|entry| {
                let name = entry.trim_start().split(':').next().unwrap_or("");
                CTY_CANARIES.iter().any(|(_, entity)| *entity == name)
            })
            .collect();
        let err = accept_cty(&canary_entities, &bundled).unwrap_err();
        let n = cty::Table::parse(&canary_entities).len();
        assert_eq!(
            err,
            format!(
                "it lists {n} prefixes, under 90% of the built-in copy's {}",
                bundled.len()
            )
        );
    }

    #[test]
    fn accept_cty_strips_a_bom() {
        assert_eq!(strip_bom("\u{feff}K1ABC"), "K1ABC");
        assert_eq!(strip_bom("\u{feff}\u{feff}K1ABC"), "\u{feff}K1ABC");
        assert_eq!(strip_bom("K1ABC"), "K1ABC");
        let bundled = cty::Table::bundled();
        let table = accept_cty(&format!("\u{feff}{}", crate::CTY_DAT), &bundled).unwrap();
        assert_eq!(table.len(), bundled.len());
    }

    #[test]
    fn accept_scp_takes_the_bundled_file() {
        let bundled = scp::Set::bundled();
        let set = accept_scp(crate::MASTER_SCP, &bundled).unwrap();
        assert_eq!(set.len(), bundled.len());
    }

    #[test]
    fn accept_scp_strips_a_bom() {
        // Unstripped, the BOM hides the `!!Order` header's `!`, making it a
        // member line that is not a callsign.
        let bundled = scp::Set::bundled();
        let set = accept_scp(&format!("\u{feff}{}", crate::MASTER_SCP), &bundled).unwrap();
        assert_eq!(set.len(), bundled.len());
    }

    #[test]
    fn accept_scp_rejects_a_web_page() {
        let bundled = scp::Set::bundled();
        let err = accept_scp(WEB_PAGE, &bundled).unwrap_err();
        assert_eq!(
            err,
            "line 1 is \"<!DOCTYPE HTML PUBLIC \"-//IETF//DTD HTML 2.0//EN\">\", not a callsign"
        );
        // Line numbers count every line from 1, headers and blanks included.
        let err = accept_scp("!!Order,1,1\n# c\nk1abc\n\nnot a call\n", &bundled).unwrap_err();
        assert_eq!(err, "line 5 is \"not a call\", not a callsign");
        // '-' is never in a callsign, so this 200-character line is quoted
        // cut at 60 characters.
        let long = format!("K1ABC\n{}\n", "x-".repeat(100));
        let err = accept_scp(&long, &bundled).unwrap_err();
        assert_eq!(
            err,
            format!("line 2 is \"{}…\", not a callsign", "x-".repeat(30))
        );
        // An escape sequence in the download never reaches the log intact.
        let err = accept_scp("K1ABC\n\u{1b}[2J\n", &bundled).unwrap_err();
        assert_eq!(err, "line 2 is \"?[2J\", not a callsign");
    }

    #[test]
    fn accept_scp_rejects_under_half_the_bundled_size() {
        let bundled = scp::Set::bundled();
        let mut members = 0;
        let first_20k: String = crate::MASTER_SCP
            .lines()
            .take_while(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                    return true;
                }
                members += 1;
                members <= 20_000
            })
            .map(|line| format!("{line}\n"))
            .collect();
        let n = scp::Set::parse(&first_20k).len();
        assert_eq!(n, 20_000);
        let err = accept_scp(&first_20k, &bundled).unwrap_err();
        assert_eq!(
            err,
            format!(
                "it lists 20000 calls, under half the built-in copy's {}",
                bundled.len()
            )
        );
    }
}
