# M3 sub-project 1 — `skimmer-spot` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `skimmer-spot`, a standalone, deterministic crate that turns a
`skimmer-decode` `DecoderEvent` stream into validated `Spot`s — callsign
grammar + cty.dat gate + SCP confidence boost + repetition gate + dedupe.

**Architecture:** One `Validator` struct owns per-track rolling word buffers,
a repetition-gate `BTreeMap`, and a dedupe `BTreeMap`. `Validator::ingest`
consumes one `&DecoderEvent` at a time and returns `Vec<Spot>` (usually
empty). Seven small modules (`grammar`, `context`, `cty`, `scp`,
`confidence`, `gate`, `dedupe`) each own one pure/stateful piece; `validator`
wires them together per ARCHITECTURE §6's pipeline order.

**Tech Stack:** Rust (edition 2021), `regex` (new workspace dep), `serde`
(already a workspace dep), vendored `cty.dat` (AD1C format) + `master.scp`
data files.

## Global Constraints

- Design doc: `docs/superpowers/specs/2026-07-25-m3-skimmer-spot-design.md`
  — read it first; this plan implements it verbatim.
- Determinism (SPEC-decode-core.md §6, binding on this crate too): no RNG,
  no wall clock, `BTreeMap`/`BTreeSet` (never `HashMap`/`HashSet`) on any
  path that affects `Spot` output ordering — `scp::Set`'s `HashSet` is the
  one documented exception (pure boolean membership, cannot affect
  ordering).
- `sample_ts` is always at the input stream's sample rate `fs` (SPEC §5) —
  every duration-based window (`gate`'s 90 s, `dedupe`'s 10 min) takes `fs`
  as a constructor parameter and converts internally; nothing hardcodes
  `96_000`.
- CI runs `cargo fmt --all --check` and
  `cargo clippy --workspace --all-targets -- -D warnings` — run both before
  every commit in this plan.
- Workspace convention: crate `Cargo.toml` uses `version.workspace = true`
  etc. (see any existing crate, e.g. `crates/skimmer-decode/Cargo.toml`);
  new external deps are added once to `[workspace.dependencies]` in the
  root `Cargo.toml`, then referenced as `{ workspace = true }`.
- This sub-project stops at a fully tested standalone crate — no wiring
  into `skimmer-engine`, no `skimmer-server`, no TOML config loader. See
  the design doc §1 for the exact scope boundary.

---

### Task 1: Scaffold the crate and vendor cty.dat / master.scp

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/skimmer-spot/Cargo.toml`
- Create: `crates/skimmer-spot/src/lib.rs`
- Create: `crates/skimmer-spot/data/cty.dat`
- Create: `crates/skimmer-spot/data/master.scp`
- Create: `crates/skimmer-spot/data/SOURCES.md`

**Interfaces:**
- Produces: an empty-but-building `skimmer-spot` crate the rest of this
  plan fills in; `crates/skimmer-spot/data/cty.dat` and
  `crates/skimmer-spot/data/master.scp` as `include_str!`-able text assets
  Task 4/5 consume.

- [ ] **Step 1: Register the crate and its new dependency in the workspace**

Edit root `Cargo.toml`:
- Add `"crates/skimmer-spot",` to `members`.
- Add `skimmer-spot = { path = "crates/skimmer-spot" }` to
  `[workspace.dependencies]` (alongside the other `skimmer-*` entries).
- Add `regex = "1"` to `[workspace.dependencies]` (alongside `serde`,
  `anyhow`, etc.).

- [ ] **Step 2: Create the crate manifest**

`crates/skimmer-spot/Cargo.toml`:

```toml
[package]
name = "skimmer-spot"
description = "Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate, dedupe"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
skimmer-decode = { workspace = true }
serde = { workspace = true }
regex = { workspace = true }
```

- [ ] **Step 3: Fetch and vendor the data files**

```bash
curl -sf --max-time 30 "https://www.country-files.com/cty/cty.dat" \
  -o crates/skimmer-spot/data/cty.dat
curl -sf --max-time 30 "https://www.supercheckpartial.com/MASTER.SCP" \
  -o crates/skimmer-spot/data/master.scp
wc -l crates/skimmer-spot/data/cty.dat crates/skimmer-spot/data/master.scp
```

Expected: both files non-empty (`cty.dat` ~1600 lines, `master.scp`
~50000 lines). If the fetch fails (no network), stop and flag it — this
step cannot proceed without the real files (no synthetic placeholder; see
design doc §2).

- [ ] **Step 4: Write the data provenance note**

`crates/skimmer-spot/data/SOURCES.md`:

```markdown
# Vendored data sources

## cty.dat

- Source: https://www.country-files.com/cty/cty.dat (AD1C's "big CTY" file)
- Retrieved: 2026-07-25
- Format: AD1C `cty.dat` -- see https://www.country-files.com/cty-dat-format/
- License/redistribution: freely distributed for use in amateur radio
  contest/logging software -- the convention every major contest logger
  (N1MM+, Win-Test, CQRLOG, TR4W) follows. No separate license file is
  published upstream. Flagged here for visibility, not treated as a
  blocker; revisit if this ever needs a stricter provenance trail.
- Refresh: re-run the `curl` in this crate's implementation plan (Task 1)
  and replace this file by hand -- no refresh automation yet.

## master.scp

- Source: https://www.supercheckpartial.com/MASTER.SCP
- Retrieved: 2026-07-25
- Upstream release: per the file's own header comment (`# Release ...`)
- Format: one callsign per line; `#`/`!!`-prefixed lines are comments/headers.
- License/redistribution: same convention as cty.dat -- bundled by contest
  logging software as a matter of course; no separate license published
  upstream. Same flag-not-block note applies.
- Refresh: re-run the `curl` in this crate's implementation plan (Task 1)
  and replace this file by hand -- no refresh automation yet.
```

- [ ] **Step 5: Stub `lib.rs` and confirm the workspace still builds**

`crates/skimmer-spot/src/lib.rs`:

```rust
//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6.
```

Run: `cargo build --workspace`
Expected: builds clean (new empty crate compiles trivially).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/skimmer-spot
git commit -m "feat(spot): scaffold skimmer-spot crate, vendor cty.dat/master.scp"
```

---

### Task 2: `grammar` — callsign structural validation

**Files:**
- Create: `crates/skimmer-spot/src/grammar.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing (pure function, no prior task's types).
- Produces: `pub fn is_plausible(call: &str) -> bool`, used by Task 9
  (`validator`).

- [ ] **Step 1: Write the failing tests**

`crates/skimmer-spot/src/grammar.rs`:

```rust
//! Callsign structural grammar. ARCHITECTURE §6.2 -- a cheap pre-filter for
//! obviously-garbled decoder output before the cty.dat lookup (which is the
//! real allocation gate, Task 4). Deliberately permissive: 3-7 alphanumeric
//! characters with at least one digit, at least one letter, ending in a
//! letter, plus an optional portable designator (`/P`, `/QRP`, `/MM`, `/AM`,
//! `/M`, or `/<digit>`).

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
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod grammar;
```

Run: `cargo test -p skimmer-spot`
Expected: all three `grammar::tests::*` tests PASS (the implementation
above is written together with its tests in this plan, so there is no
separate red/green step here — just confirm green).

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/grammar.rs
git commit -m "feat(spot): callsign structural grammar"
```

---

### Task 3: `context` — CQ/DE/beacon parse + `SpotType`

**Files:**
- Create: `crates/skimmer-spot/src/context.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum SpotType { Cq, De, Beacon, Unknown }` (Debug, Clone,
  Copy, PartialEq, Eq, serde::Serialize) and
  `pub fn parse(text: &str) -> Option<(String, SpotType)>`. Both used by
  Task 8 (`dedupe`) and Task 9 (`validator`).

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/context.rs`:

```rust
//! CQ/DE/beacon context parse. ARCHITECTURE §6.1.
//!
//! Deliberately lightweight: matches the exact pattern families
//! ARCHITECTURE §6.1 lists (`CQ <call>`, `CQ TEST <call>`, `DE <call>`,
//! `<call> UP`, `V V V <call>`). Filler words between the keyword and the
//! call (e.g. "CQ DX CQ DX DE ...", "CQ CONTEST ...") are a known gap, not
//! handled by this first pass -- same "tracked, not blocking" treatment
//! this project gives other classical-parsing limitations (see the known
//! decode bugs tracked as GitHub issues).

use std::sync::LazyLock;
use regex::Regex;

/// The context a decoded callsign was found in. Carried on `Spot` as the
/// RBN spot-type flag (ARCHITECTURE §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SpotType {
    Cq,
    De,
    Beacon,
    Unknown,
}

static BEACON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bV\s+V\s+V\s+([A-Z0-9/]{3,15})\b").unwrap());
static DE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bDE\s+([A-Z0-9/]{3,15})\b").unwrap());
static CQ_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bCQ\b").unwrap());
static CQ_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bCQ(?:\s+TEST)?\s+([A-Z0-9/]{3,15})\b").unwrap());
static UP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([A-Z0-9/]{3,15})\s+UP\b").unwrap());

/// Scans `text` for the first CQ/DE/beacon context pattern, returning the
/// callsign candidate (uppercased) and its spot type. `None` if no pattern
/// matches at all -- the caller decides whether to fall back to
/// grammar-only, type-`Unknown` validation.
///
/// A `DE <call>` match is classified `Cq` (not `De`) when a bare `CQ` token
/// also appears anywhere in `text` -- the common "CQ CQ DE <call>"
/// transmission shape, where the callsign always follows `DE` but the
/// operator is calling CQ, not answering one.
pub fn parse(text: &str) -> Option<(String, SpotType)> {
    if let Some(caps) = BEACON_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::Beacon));
    }
    if let Some(caps) = DE_RE.captures(text) {
        let spot_type = if CQ_TOKEN_RE.is_match(text) {
            SpotType::Cq
        } else {
            SpotType::De
        };
        return Some((caps[1].to_uppercase(), spot_type));
    }
    if let Some(caps) = CQ_CALL_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::Cq));
    }
    if let Some(caps) = UP_RE.captures(text) {
        return Some((caps[1].to_uppercase(), SpotType::De));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cq_de_call_call_is_cq_type() {
        assert_eq!(
            parse("CQ CQ DE K5ARH K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn plain_de_call_without_cq_is_de_type() {
        assert_eq!(
            parse("DE K5ARH K"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn cq_test_call_is_cq_type() {
        assert_eq!(
            parse("CQ TEST K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Cq))
        );
    }

    #[test]
    fn call_up_is_de_type() {
        assert_eq!(
            parse("K5ARH UP UP"),
            Some(("K5ARH".to_string(), SpotType::De))
        );
    }

    #[test]
    fn v_v_v_call_is_beacon_type() {
        assert_eq!(
            parse("V V V K5ARH K5ARH"),
            Some(("K5ARH".to_string(), SpotType::Beacon))
        );
    }

    #[test]
    fn no_pattern_returns_none() {
        assert_eq!(parse("K5ARH TU 5NN"), None);
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod context;
pub use context::SpotType;
```

Run: `cargo test -p skimmer-spot`
Expected: all `context::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/context.rs
git commit -m "feat(spot): CQ/DE/beacon context parse"
```

---

### Task 4: `cty` — prefix allocation table

**Files:**
- Create: `crates/skimmer-spot/src/cty.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing (parses a `&str` in AD1C `cty.dat` format).
- Produces: `pub struct Table` with `pub fn parse(cty_dat: &str) -> Table`
  and `pub fn is_allocated(&self, callsign: &str) -> bool`, used by Task 9.

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/cty.rs`:

```rust
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
        let table = Table::parse(FIXTURE);
        assert!(!table.is_allocated("VERSION"));
    }

    #[test]
    fn zone_annotations_are_stripped() {
        let table = Table::parse(FIXTURE);
        // "KL7(1)[65]" must register as prefix "KL7", not "KL7(1)[65]".
        assert!(table.is_allocated("KL7XY"));
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod cty;
```

Run: `cargo test -p skimmer-spot`
Expected: all `cty::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/cty.rs
git commit -m "feat(spot): cty.dat prefix allocation table"
```

---

### Task 5: `scp` — Super Check Partial membership

**Files:**
- Create: `crates/skimmer-spot/src/scp.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing (parses a `&str` in `MASTER.SCP` format).
- Produces: `pub struct Set` with `pub fn parse(master_scp: &str) -> Set`
  and `pub fn contains(&self, callsign: &str) -> bool`, used by Task 9.

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/scp.rs`:

```rust
//! master.scp (Super Check Partial) membership. ARCHITECTURE §6.3.
//!
//! Format: one callsign per line; `#`/`!!`-prefixed lines are
//! comments/headers. `HashSet` here is the one documented exception to
//! this crate's "no `HashMap`/`HashSet` on an output-ordering path" rule
//! (SPEC-decode-core.md §6 rule 3) -- membership is a pure boolean lookup
//! that cannot affect `Spot` output ordering.

use std::collections::HashSet;

pub struct Set {
    calls: HashSet<String>,
}

impl Set {
    /// Parses a `MASTER.SCP` file's full contents.
    pub fn parse(master_scp: &str) -> Self {
        let calls = master_scp
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
            .map(str::to_uppercase)
            .collect();
        Self { calls }
    }

    pub fn contains(&self, callsign: &str) -> bool {
        self.calls.contains(&callsign.to_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
!!Order,1,1
#
# Super Check Partial
# Release 2026.07.24
#
K5ARH
W1AW
VE3ABC
";

    #[test]
    fn member_calls_are_found() {
        let scp = Set::parse(FIXTURE);
        assert!(scp.contains("K5ARH"));
        assert!(scp.contains("w1aw")); // case-insensitive
    }

    #[test]
    fn non_member_calls_are_absent() {
        let scp = Set::parse(FIXTURE);
        assert!(!scp.contains("ZZ9ZZZ"));
    }

    #[test]
    fn header_and_comment_lines_are_not_members() {
        let scp = Set::parse(FIXTURE);
        assert!(!scp.contains("!!Order,1,1"));
        assert!(!scp.contains("#"));
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod scp;
```

Run: `cargo test -p skimmer-spot`
Expected: all `scp::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/scp.rs
git commit -m "feat(spot): master.scp membership set"
```

---

### Task 6: `confidence` — `c_call` formula + SCP boost

**Files:**
- Create: `crates/skimmer-spot/src/confidence.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing (pure functions over `&[f32]` / `f32`).
- Produces: `pub fn c_call(char_confidences: &[f32], reps: u32) -> f32` and
  `pub fn apply_scp_boost(c: f32, in_scp: bool) -> f32`, used by Task 9.

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/confidence.rs`:

```rust
//! SPEC-decode-core.md §4.6 per-callsign confidence, plus the cty/SCP
//! adjustment the spec explicitly defers to this crate (ARCHITECTURE §6.3).

/// SPEC §4.6: geometric mean of per-character confidences times a
/// repetition factor (`r=1 -> 0.5`, `r=2 -> 0.75`, `r=3 -> 0.875`, ...).
///
/// `c_call = (prod cᵢ)^(1/n) * (1 - 0.5^r)`
pub fn c_call(char_confidences: &[f32], reps: u32) -> f32 {
    assert!(
        !char_confidences.is_empty(),
        "a callsign has at least one character"
    );
    let n = char_confidences.len() as f32;
    let log_sum: f32 = char_confidences
        .iter()
        .map(|c| c.max(f32::EPSILON).ln())
        .sum();
    let geo_mean = (log_sum / n).exp();
    let rep_factor = 1.0 - 0.5f32.powi(reps as i32);
    geo_mean * rep_factor
}

/// SCP membership: multiplicative boost capped at 1.0. Absence is neutral
/// -- ARCHITECTURE §6.3: "absence only lowers it [relatively, by not
/// getting the boost], never gates."
pub fn apply_scp_boost(c: f32, in_scp: bool) -> f32 {
    if in_scp {
        (c * 1.15).min(1.0)
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn rep_factor_matches_spec_examples() {
        assert_relative_eq!(c_call(&[1.0], 1), 0.5, epsilon = 1e-6);
        assert_relative_eq!(c_call(&[1.0], 2), 0.75, epsilon = 1e-6);
        assert_relative_eq!(c_call(&[1.0], 3), 0.875, epsilon = 1e-6);
    }

    #[test]
    fn one_low_confidence_character_tanks_the_geometric_mean() {
        let high = c_call(&[1.0, 1.0, 1.0], 3);
        let one_low = c_call(&[1.0, 1.0, 0.1], 3);
        assert!(one_low < high);
    }

    #[test]
    fn scp_boost_raises_confidence_but_never_gates() {
        assert!(apply_scp_boost(0.5, true) > 0.5);
        assert_eq!(apply_scp_boost(0.5, false), 0.5);
    }

    #[test]
    fn scp_boost_is_capped_at_one() {
        assert_eq!(apply_scp_boost(0.95, true), 1.0);
    }
}
```

- [ ] **Step 2: Add the `approx` dev-dependency, wire the module, run tests**

Add to `crates/skimmer-spot/Cargo.toml`:

```toml
[dev-dependencies]
approx = { workspace = true }
```

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod confidence;
```

Run: `cargo test -p skimmer-spot`
Expected: all `confidence::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/Cargo.toml crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/confidence.rs
git commit -m "feat(spot): SPEC §4.6 c_call confidence + SCP boost"
```

---

### Task 7: `gate` — repetition requirement

**Files:**
- Create: `crates/skimmer-spot/src/gate.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct RepetitionGate` with `pub fn new(fs: f64) -> Self`
  and `pub fn record(&mut self, track_id: u32, callsign: &str, sample_ts: u64) -> usize`
  (returns the in-window distinct-decode count including this one), used
  by Task 9.

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/gate.rs`:

```rust
//! Repetition gate: a callsign must decode >= 2 distinct times within a
//! 90 s window on its track before first spot. SPEC §4.6 / ARCHITECTURE
//! §6.4. `sample_ts`-based, never wall clock (SPEC-decode-core.md §6 rule
//! 2). `BTreeMap`, never `HashMap` (rule 3) -- this state feeds directly
//! into whether/when a `Spot` is emitted.

use std::collections::BTreeMap;

const WINDOW_SECONDS: f64 = 90.0;

pub struct RepetitionGate {
    window_samples: u64,
    seen: BTreeMap<(u32, String), Vec<u64>>,
}

impl RepetitionGate {
    pub fn new(fs: f64) -> Self {
        Self {
            window_samples: (WINDOW_SECONDS * fs) as u64,
            seen: BTreeMap::new(),
        }
    }

    /// Records one decode of `callsign` on `track_id` at `sample_ts`.
    /// Returns the number of distinct decodes within the trailing window
    /// (including this one).
    pub fn record(&mut self, track_id: u32, callsign: &str, sample_ts: u64) -> usize {
        let entry = self
            .seen
            .entry((track_id, callsign.to_string()))
            .or_default();
        entry.push(sample_ts);
        let cutoff = sample_ts.saturating_sub(self.window_samples);
        entry.retain(|&ts| ts >= cutoff);
        entry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_decode_counts_as_one() {
        let mut gate = RepetitionGate::new(FS);
        assert_eq!(gate.record(1, "K5ARH", 0), 1);
    }

    #[test]
    fn second_decode_within_window_counts_as_two() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        assert_eq!(gate.record(1, "K5ARH", 100_000), 2);
    }

    #[test]
    fn decode_outside_window_resets_the_count() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        let window_samples = (WINDOW_SECONDS * FS) as u64;
        assert_eq!(gate.record(1, "K5ARH", window_samples + 1), 1);
    }

    #[test]
    fn different_tracks_and_callsigns_are_independent() {
        let mut gate = RepetitionGate::new(FS);
        gate.record(1, "K5ARH", 0);
        assert_eq!(gate.record(2, "K5ARH", 0), 1);
        assert_eq!(gate.record(1, "W1AW", 0), 1);
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod gate;
```

Run: `cargo test -p skimmer-spot`
Expected: all `gate::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/gate.rs
git commit -m "feat(spot): repetition gate"
```

---

### Task 8: `dedupe` — re-spot suppression

**Files:**
- Create: `crates/skimmer-spot/src/dedupe.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: `crate::context::SpotType` (Task 3).
- Produces: `pub struct Dedupe` with `pub fn new(fs: f64) -> Self` and
  `pub fn should_emit(&mut self, callsign: &str, freq_hz: f64, snr_db: f32, spot_type: SpotType, sample_ts: u64) -> bool`,
  used by Task 9.

- [ ] **Step 1: Write the module with its tests**

`crates/skimmer-spot/src/dedupe.rs`:

```rust
//! Re-spot suppression. ARCHITECTURE §6.5. `sample_ts`-based (SPEC
//! -decode-core.md §6 rule 2). `BTreeMap`, never `HashMap` (rule 3).

use crate::context::SpotType;
use std::collections::BTreeMap;

const FREQ_BUCKET_HZ: f64 = 300.0;
const SUPPRESSION_SECONDS: f64 = 600.0;
const SNR_IMPROVEMENT_DB: f32 = 6.0;

struct LastSpot {
    sample_ts: u64,
    snr_db: f32,
    spot_type: SpotType,
}

pub struct Dedupe {
    suppression_window_samples: u64,
    last: BTreeMap<(String, i64), LastSpot>,
}

impl Dedupe {
    pub fn new(fs: f64) -> Self {
        Self {
            suppression_window_samples: (SUPPRESSION_SECONDS * fs) as u64,
            last: BTreeMap::new(),
        }
    }

    fn bucket(freq_hz: f64) -> i64 {
        (freq_hz / FREQ_BUCKET_HZ).round() as i64
    }

    /// True if a spot for this `(callsign, freq_hz)` should be emitted now
    /// -- no prior spot, the suppression window has elapsed, SNR improved
    /// by at least `SNR_IMPROVEMENT_DB`, or the spot type changed. Records
    /// the new spot as the latest one when it returns true.
    pub fn should_emit(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        snr_db: f32,
        spot_type: SpotType,
        sample_ts: u64,
    ) -> bool {
        let key = (callsign.to_string(), Self::bucket(freq_hz));
        let emit = match self.last.get(&key) {
            None => true,
            Some(prev) => {
                sample_ts.saturating_sub(prev.sample_ts) >= self.suppression_window_samples
                    || snr_db - prev.snr_db >= SNR_IMPROVEMENT_DB
                    || spot_type != prev.spot_type
            }
        };
        if emit {
            self.last.insert(
                key,
                LastSpot {
                    sample_ts,
                    snr_db,
                    spot_type,
                },
            );
        }
        emit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_spot_always_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0));
    }

    #[test]
    fn immediate_repeat_same_snr_and_type_is_suppressed() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(!d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 1000));
    }

    #[test]
    fn snr_jump_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 26.0, SpotType::Cq, 1000));
    }

    #[test]
    fn type_change_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::De, 1000));
    }

    #[test]
    fn window_elapsing_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        let window_samples = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit(
            "K5ARH",
            14_027_000.0,
            20.0,
            SpotType::Cq,
            window_samples + 1
        ));
    }

    #[test]
    fn different_freq_bucket_is_independent() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_030_000.0, 20.0, SpotType::Cq, 1000));
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs` and run the tests**

Add to `crates/skimmer-spot/src/lib.rs`:

```rust
pub mod dedupe;
```

Run: `cargo test -p skimmer-spot`
Expected: all `dedupe::tests::*` PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/dedupe.rs
git commit -m "feat(spot): dedupe/re-spot suppression"
```

---

### Task 9: `validator` — `Spot` + `Validator::ingest` integration

**Files:**
- Create: `crates/skimmer-spot/src/validator.rs`
- Modify: `crates/skimmer-spot/src/lib.rs`

**Interfaces:**
- Consumes: `skimmer_decode::events::DecoderEvent`,
  `skimmer_decode::tree::{Glyph, Prosign}` (existing crate);
  `crate::{grammar, context::{self, SpotType}, cty, scp, confidence, gate::RepetitionGate, dedupe::Dedupe}`
  (Tasks 2-8).
- Produces: `pub struct Spot { callsign: String, freq_hz: f64, snr_db: f32,
  wpm: f32, spot_type: SpotType, confidence: f32, track_id: u32,
  sample_ts: u64 }` (all fields `pub`) and
  `pub struct Validator` with
  `pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self`
  and `pub fn ingest(&mut self, event: &DecoderEvent) -> Vec<Spot>`. This
  is the crate's public entry point — Task 10's golden tests and any
  future `skimmer-engine` wiring use it.

- [ ] **Step 1: Write the module**

`crates/skimmer-spot/src/validator.rs`:

```rust
//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::dedupe::Dedupe;
use crate::gate::RepetitionGate;
use crate::grammar;
use crate::scp;
use skimmer_decode::events::DecoderEvent;
use skimmer_decode::tree::{Glyph, Prosign};
use std::collections::{BTreeMap, VecDeque};

/// How many recently-completed words a track remembers for context
/// parsing. Calls/context keywords always appear within a handful of
/// words of each other in practice; this bound keeps `TrackState` small
/// without needing a time-based window here (the repetition gate and
/// dedupe windows, which *do* need to be time-based, live in `gate.rs`/
/// `dedupe.rs`).
const WORD_WINDOW: usize = 16;

/// A validated spot, ready for `skimmer-server` to serialize and emit.
/// No wall-clock timestamp -- that conversion happens at the
/// `skimmer-server` boundary (SPEC-decode-core.md §5), not here.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Spot {
    pub callsign: String,
    pub freq_hz: f64,
    pub snr_db: f32,
    pub wpm: f32,
    pub spot_type: SpotType,
    pub confidence: f32,
    pub track_id: u32,
    pub sample_ts: u64,
}

#[derive(Default)]
struct Word {
    text: String,
    confidences: Vec<f32>,
    /// Set once this word has been offered to the validation pipeline as a
    /// context-match candidate, so a later, unrelated word boundary that
    /// re-scans a growing window doesn't re-process it.
    attempted: bool,
}

#[derive(Default)]
struct TrackState {
    words: VecDeque<Word>,
    current: Word,
    freq_hz: f64,
    snr_db: f32,
    wpm: f32,
}

pub struct Validator {
    cty: cty::Table,
    scp: Option<scp::Set>,
    tracks: BTreeMap<u32, TrackState>,
    gate: RepetitionGate,
    dedupe: Dedupe,
}

impl Validator {
    pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self {
        Self {
            cty: cty::Table::parse(cty_dat),
            scp: master_scp.map(scp::Set::parse),
            tracks: BTreeMap::new(),
            gate: RepetitionGate::new(fs),
            dedupe: Dedupe::new(fs),
        }
    }

    /// Feeds one decoder event in. Returns zero or more validated spots
    /// (almost always zero -- a spot only comes out on the event that
    /// completes a passing candidate's word).
    pub fn ingest(&mut self, event: &DecoderEvent) -> Vec<Spot> {
        match event {
            DecoderEvent::CharDecoded {
                track_id,
                glyph,
                confidence,
                ..
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                match glyph {
                    Glyph::Char(c) => {
                        track.current.text.push(c.to_ascii_uppercase());
                        track.current.confidences.push(*confidence);
                    }
                    Glyph::Prosign(Prosign::Err) => {
                        // SPEC §4.4: operator-error prosign discards the
                        // current word buffer back to the previous
                        // boundary.
                        track.current = Word::default();
                    }
                    Glyph::Prosign(_) => {}
                }
                Vec::new()
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                if !track.current.text.is_empty() {
                    let word = std::mem::take(&mut track.current);
                    track.words.push_back(word);
                    if track.words.len() > WORD_WINDOW {
                        track.words.pop_front();
                    }
                }
                self.try_spot(*track_id, *sample_ts)
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                self.tracks.entry(*track_id).or_default().wpm = *wpm;
                Vec::new()
            }
            DecoderEvent::TrackMeta {
                track_id,
                snr_2500_db,
                freq_hz,
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                track.snr_db = *snr_2500_db;
                track.freq_hz = *freq_hz;
                Vec::new()
            }
        }
    }

    fn try_spot(&mut self, track_id: u32, sample_ts: u64) -> Vec<Spot> {
        let Some((candidate, spot_type)) = self.tracks.get(&track_id).and_then(|track| {
            let joined: String = track
                .words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            context::parse(&joined)
        }) else {
            return Vec::new();
        };

        let (freq_hz, snr_db, wpm) = {
            let track = self.tracks.get(&track_id).unwrap();
            (track.freq_hz, track.snr_db, track.wpm)
        };
        let char_confidences = {
            let track = self.tracks.get_mut(&track_id).unwrap();
            let Some(word) = track.words.iter_mut().rev().find(|w| w.text == candidate) else {
                return Vec::new();
            };
            if word.attempted {
                return Vec::new();
            }
            word.attempted = true;
            word.confidences.clone()
        };

        if !grammar::is_plausible(&candidate) {
            return Vec::new();
        }
        if !self.cty.is_allocated(&candidate) {
            return Vec::new();
        }

        let reps = self.gate.record(track_id, &candidate, sample_ts) as u32;
        let mut confidence = confidence::c_call(&char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        if reps < 2 {
            return Vec::new();
        }
        if !self
            .dedupe
            .should_emit(&candidate, freq_hz, snr_db, spot_type, sample_ts)
        {
            return Vec::new();
        }

        vec![Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            track_id,
            sample_ts,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

    fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for c in text.chars() {
            events.push(DecoderEvent::CharDecoded {
                track_id,
                sample_ts: ts,
                glyph: Glyph::Char(c),
                confidence: 0.95,
            });
            ts += 100;
        }
        events.push(DecoderEvent::WordBoundary {
            track_id,
            sample_ts: ts,
        });
        ts += 100;
        (events, ts)
    }

    fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for word in words {
            let (mut w_events, next_ts) = word_events(track_id, word, ts);
            events.append(&mut w_events);
            ts = next_ts;
        }
        events
    }

    fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
        events.iter().flat_map(|e| v.ingest(e)).collect()
    }

    #[test]
    fn full_pipeline_spots_a_repeated_valid_callsign() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
        assert_eq!(spots[0].track_id, 1);
    }

    #[test]
    fn ungrammatical_text_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let words = ["DE", "12345", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert!(spots.is_empty());
    }

    #[test]
    fn error_prosign_discards_current_word() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let mut events = vec![
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 0,
                glyph: Glyph::Char('D'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 100,
                glyph: Glyph::Char('E'),
                confidence: 0.9,
            },
            DecoderEvent::WordBoundary {
                track_id: 1,
                sample_ts: 200,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 300,
                glyph: Glyph::Char('K'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 400,
                glyph: Glyph::Prosign(Prosign::Err),
                confidence: 0.0,
            },
        ];
        // after the <ERR> prosign, the partial "K" must be gone.
        for e in events.drain(..) {
            v.ingest(&e);
        }
        let track = v.tracks.get(&1).unwrap();
        assert!(track.current.text.is_empty());
        assert_eq!(track.words.len(), 1);
        assert_eq!(track.words[0].text, "DE");
    }
}
```

- [ ] **Step 2: Wire the module into `lib.rs`**

Replace the contents of `crates/skimmer-spot/src/lib.rs` with:

```rust
//! Callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition gate,
//! dedupe. ARCHITECTURE §6.

pub mod confidence;
pub mod context;
pub mod cty;
pub mod dedupe;
pub mod gate;
pub mod grammar;
pub mod scp;
pub mod validator;

pub use context::SpotType;
pub use validator::{Spot, Validator};
```

- [ ] **Step 3: Run the full crate test suite**

Run: `cargo test -p skimmer-spot`
Expected: all tests across every module PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/skimmer-spot/src/lib.rs crates/skimmer-spot/src/validator.rs
git commit -m "feat(spot): Validator::ingest, wiring all modules together"
```

---

### Task 10: SPEC-decode-core.md V11–V15 + crate-level golden tests

**Files:**
- Modify: `docs/SPEC-decode-core.md`
- Create: `crates/skimmer-spot/tests/golden_v11_v15.rs`

**Interfaces:**
- Consumes: `skimmer_spot::{Validator, Spot, SpotType}` (Task 9),
  `skimmer_decode::events::DecoderEvent`, `skimmer_decode::tree::Glyph`
  (existing crate).
- Produces: nothing further consumed by other tasks — this is the
  sub-project's acceptance evidence.

- [ ] **Step 1: Add the V11–V15 vector table to SPEC-decode-core.md**

Insert a new `### 7.1` subsection immediately after the existing `## 7.
Golden test vectors` table (after the `M0 = V1 passing end-to-end...`
paragraph, before the `---` that starts `## 8. Module map`):

```markdown
### 7.1 `skimmer-spot` validator vectors (M3 sub-project 1)

Unlike V1–V10 (testkit-synthesized IQ), these operate at the
`DecoderEvent`-stream level -- hand-built event sequences feeding
`Validator::ingest` directly, no IQ synthesis involved. Implemented as
crate-level tests in `crates/skimmer-spot/tests/golden_v11_v15.rs`.

| # | Name | Scenario | Pass criteria |
|---|---|---|---|
| V11 | context-parse | Each of `CQ <call>`, `CQ TEST <call>`, `DE <call>`, `<call> UP`, `V V V <call>` | Correct `SpotType` assigned per pattern family |
| V12 | bogus-prefix | Structurally-valid callsign with a prefix absent from cty.dat | 0 spots, even though grammar passes |
| V13 | scp-boost | Same callsign/confidences with vs. without SCP membership | `c_call` strictly higher when a member; absence never rejects |
| V14 | repetition-gate | 1 decode vs. 2 decodes of the same callsign within 90 s | 1 rep never spots; 2 reps does |
| V15 | dedupe | Repeat spot inside the 10 min window, then an SNR jump >= 6 dB | Suppressed inside the window; allowed after the SNR jump |
```

- [ ] **Step 2: Write the golden test file**

`crates/skimmer-spot/tests/golden_v11_v15.rs`:

```rust
//! SPEC-decode-core.md §7.1 V11-V15: skimmer-spot validator vectors.

use skimmer_decode::events::DecoderEvent;
use skimmer_decode::tree::Glyph;
use skimmer_spot::{Spot, SpotType, Validator};

const FS: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for c in text.chars() {
        events.push(DecoderEvent::CharDecoded {
            track_id,
            sample_ts: ts,
            glyph: Glyph::Char(c),
            confidence: 0.95,
        });
        ts += 100;
    }
    events.push(DecoderEvent::WordBoundary {
        track_id,
        sample_ts: ts,
    });
    ts += 100;
    (events, ts)
}

fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for word in words {
        let (mut w_events, next_ts) = word_events(track_id, word, ts);
        events.append(&mut w_events);
        ts = next_ts;
    }
    events
}

fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
    events.iter().flat_map(|e| v.ingest(e)).collect()
}

#[test]
fn v11_context_parse_sets_spot_type() {
    let cases: &[(&[&str], SpotType)] = &[
        (&["CQ", "CQ", "DE", "K5ARH", "K5ARH", "K"], SpotType::Cq),
        (&["DE", "K5ARH", "K"], SpotType::De),
        (&["CQ", "TEST", "K5ARH", "K5ARH"], SpotType::Cq),
        (&["K5ARH", "UP", "UP"], SpotType::De),
        (&["V", "V", "V", "K5ARH", "K5ARH"], SpotType::Beacon),
    ];
    for (words, expected_type) in cases {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let mut spots = run(&transmission_events(1, words, 0), &mut v);
        spots.extend(run(&transmission_events(1, words, 100_000), &mut v));
        let hit = spots
            .iter()
            .find(|s| s.callsign == "K5ARH")
            .unwrap_or_else(|| panic!("no K5ARH spot for words {words:?}, got {spots:?}"));
        assert_eq!(hit.spot_type, *expected_type, "words {words:?}");
    }
}

#[test]
fn v12_bogus_prefix_rejected() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "ZZ9ZZZ", "K"];
    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert!(
        spots.is_empty(),
        "bogus-prefix callsign must never be spotted, got {spots:?}"
    );
}

#[test]
fn v13_scp_membership_boosts_confidence_without_gating_absence() {
    let scp_fixture = "K5ARH\n";
    let words = ["DE", "K5ARH", "K"];
    let words_not_in_scp = ["DE", "K9ABC", "K"];

    let run_twice = |v: &mut Validator, words: &[&str]| -> Vec<Spot> {
        let mut spots = run(&transmission_events(1, words, 0), v);
        spots.extend(run(&transmission_events(1, words, 100_000), v));
        spots
    };

    let mut v_scp = Validator::new(FS, CTY_FIXTURE, Some(scp_fixture));
    let mut v_noscp = Validator::new(FS, CTY_FIXTURE, None);
    let with_scp = run_twice(&mut v_scp, &words);
    let without_scp = run_twice(&mut v_noscp, &words);
    assert!(!with_scp.is_empty() && !without_scp.is_empty());
    assert!(
        with_scp[0].confidence > without_scp[0].confidence,
        "SCP membership must raise confidence: {} vs {}",
        with_scp[0].confidence,
        without_scp[0].confidence
    );

    let mut v_absent = Validator::new(FS, CTY_FIXTURE, Some(scp_fixture));
    let no_member = run_twice(&mut v_absent, &words_not_in_scp);
    assert!(
        !no_member.is_empty(),
        "SCP absence must not gate a structurally-valid, cty-allocated call"
    );
}

#[test]
fn v14_repetition_gate_requires_two_reps() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "K5ARH", "K"];
    let once = run(&transmission_events(1, &words, 0), &mut v);
    assert!(once.is_empty(), "1 rep must never spot, got {once:?}");

    let twice = run(&transmission_events(1, &words, 100_000), &mut v);
    assert!(!twice.is_empty(), "2 reps must spot");
}

#[test]
fn v15_dedupe_suppresses_then_allows_on_snr_jump() {
    let mut v = Validator::new(FS, CTY_FIXTURE, None);
    let words = ["DE", "K5ARH", "K"];

    let mut spots = run(&transmission_events(1, &words, 0), &mut v);
    spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
    assert_eq!(spots.len(), 1, "must spot exactly once so far, got {spots:?}");

    let suppressed = run(&transmission_events(1, &words, 200_000), &mut v);
    assert!(
        suppressed.is_empty(),
        "re-spot inside the suppression window with no SNR/type change must be suppressed"
    );

    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 6.0,
        freq_hz: 0.0,
    });
    let allowed = run(&transmission_events(1, &words, 300_000), &mut v);
    assert!(
        !allowed.is_empty(),
        "an SNR jump >= 6 dB must override dedupe suppression"
    );
}
```

- [ ] **Step 3: Run the golden tests**

Run: `cargo test -p skimmer-spot --test golden_v11_v15`
Expected: `v11_context_parse_sets_spot_type`,
`v12_bogus_prefix_rejected`,
`v13_scp_membership_boosts_confidence_without_gating_absence`,
`v14_repetition_gate_requires_two_reps`,
`v15_dedupe_suppresses_then_allows_on_snr_jump` all PASS.

- [ ] **Step 4: Commit**

```bash
git add docs/SPEC-decode-core.md crates/skimmer-spot/tests/golden_v11_v15.rs
git commit -m "test(spot): SPEC V11-V15 validator golden vectors"
```

---

### Task 11: Docs wrap-up + full workspace verification

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `ROADMAP.md`

**Interfaces:**
- Consumes: nothing new.
- Produces: nothing further consumed — final documentation sync.

- [ ] **Step 1: Add the missing dependency edge to ARCHITECTURE.md's graph**

In `ARCHITECTURE.md` §2's dependency graph, `skimmer-spot` needs
`DecoderEvent`/`Glyph` from `skimmer-decode` — an edge the original graph
omitted (drawn before this crate existed). Change:

```
skimmer-cli ──▶ skimmer-engine ──▶ skimmer-input ──▶ skimmer-dsp
                     │        ├──▶ skimmer-dsp ──────▶ coppa-dsp
                     │        ├──▶ skimmer-decode
                     │        └──▶ skimmer-spot
                     └──▶ skimmer-server
skimmer-testkit ──▶ skimmer-dsp, skimmer-decode, coppa-channel
```

to:

```
skimmer-cli ──▶ skimmer-engine ──▶ skimmer-input ──▶ skimmer-dsp
                     │        ├──▶ skimmer-dsp ──────▶ coppa-dsp
                     │        ├──▶ skimmer-decode
                     │        └──▶ skimmer-spot ──────▶ skimmer-decode
                     └──▶ skimmer-server
skimmer-testkit ──▶ skimmer-dsp, skimmer-decode, coppa-channel
```

- [ ] **Step 2: Update ROADMAP.md's M3 section**

In `ROADMAP.md`'s M3 section, after the `**Accept when:**` block, add a
status paragraph (matching the style of M2's closing paragraph):

```markdown
`skimmer-spot` (callsign/CQ-DE validation, cty.dat/SCP cross-check,
repetition gate, dedupe) is complete as a standalone crate -- see
`docs/superpowers/specs/2026-07-25-m3-skimmer-spot-design.md` and SPEC
-decode-core.md §7.1 (V11-V15). Remaining M3 sub-projects: wiring
`skimmer-spot` into `skimmer-engine`'s live pipeline, `skimmer-server`
(telnet + JSON/WebSocket output, TOML config, metrics), and the RBN parity
benchmark (needs ≥ 2 h of recorded contest-weekend IQ with RBN reference
spots -- a data dependency not yet resolved).
```

- [ ] **Step 3: Full workspace verification**

Run, in order:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all three commands exit 0. If `cargo fmt` reports diffs, run
`cargo fmt --all` and re-check. If clippy reports warnings in the new
crate, fix them (do not `#[allow]` without a documented reason).

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md ROADMAP.md
git commit -m "docs(m3): skimmer-spot dependency edge + ROADMAP status"
```

- [ ] **Step 5: Push and open the PR**

```bash
git push
gh pr create --title "feat(spot): M3 sub-project 1 -- skimmer-spot validation crate" --body "$(cat <<'EOF'
## Summary
- New `skimmer-spot` crate: callsign grammar, CQ/DE/beacon context parse,
  cty.dat allocation gate, SCP confidence boost, repetition gate, dedupe --
  ARCHITECTURE §6 implemented end to end as a standalone, deterministic
  crate (`Validator::ingest(&DecoderEvent) -> Vec<Spot>`).
- Vendors real `cty.dat` (AD1C) and `master.scp` (Super Check Partial)
  snapshots; provenance in `crates/skimmer-spot/data/SOURCES.md`.
- Adds SPEC-decode-core.md §7.1 V11-V15 validator golden vectors.
- Out of scope (follow-up sub-projects): wiring into `skimmer-engine`,
  `skimmer-server`, TOML config, RBN parity benchmark.

Design: `docs/superpowers/specs/2026-07-25-m3-skimmer-spot-design.md`
Plan: `docs/superpowers/plans/2026-07-25-m3-skimmer-spot-plan.md`

## Test plan
- [x] `cargo test -p skimmer-spot` (unit tests, every module)
- [x] `cargo test -p skimmer-spot --test golden_v11_v15` (V11-V15)
- [x] `cargo fmt --all --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
gh pr merge --auto --squash
```

(Per this repo's multi-agent hygiene policy, auto-merge is armed
immediately after opening — CI gates the actual merge.)
