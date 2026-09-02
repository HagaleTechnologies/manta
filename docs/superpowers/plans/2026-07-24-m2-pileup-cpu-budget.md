# M2 Pileup Validation + CPU-Budget Bench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement SPEC-decode-core.md §7's V8/V8w pileup golden vectors and ROADMAP.md's M2 CPU-budget criterion bench (192 kS/s, 300 active tracks, < 50% of one core on an M-series Mac).

**Architecture:** Reuse the existing `manta-testkit::scene`/`vectors` generic multi-signal scene infra (already supports arbitrary-N `SignalSpec` scenes and per-signal `Manifest.expected_freqs_hz`/`keyed_texts`) to build a 50-signal pileup vector pair (V8 AWGN, V8w Watterson CCIR-poor). Golden tests match each decoded track back to its originating signal by nearest `TrackMeta.freq_hz`, giving precise per-signal CER instead of a loose substring guess. The CPU-budget bench drives `manta_engine::decode_samples` (the crate's actual public full-pipeline entry point — `TrackManager` itself is private) with a synthetic 300-tone 192 kS/s scene, via both a `criterion` profiling target and a separate `#[ignore]`d wall-clock `#[test]` that does the real budget assertion (perf assertions don't belong in CI; benches aren't run by `cargo test` by default).

**Tech Stack:** Rust, `criterion` (new dev-dependency), existing `manta-testkit`/`manta-engine`/`manta-cli` crates.

## Global Constraints

- SPEC-decode-core.md §7: `fs = 96 000`, 120 s scene, SNR quoted in 2500 Hz, for V8/V8w. Text payload default: `CQ CQ DE <CALL> <CALL> K`.
- SPEC-decode-core.md §7 V8: 50 signals, 10–35 WPM, −2..+25 dB, uniform over ±45 kHz, unique calls; AWGN, jitter 8%; pass = ≥45/50 callsigns validated, 0 bogus callsigns.
- SPEC-decode-core.md §7 V8w: same scene, Watterson CCIR-poor, jitter 8%; pass = ≥90% of signals with mean SNR ≥ +6 dB decoded CER < 10%, 0 bogus callsigns, 0 cross-channel ghost decodes.
- ROADMAP.md M2 accept: criterion bench, full pipeline at 192 kS/s with 300 active tracks, < 50% of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4 (Pi4 leg explicitly deferred per the approved design — Tony runs it later on real hardware).
- All `manta-testkit` randomness is ChaCha8-seeded, hand-rolled via `next_u64()` (pinned decision 2) — no new RNG crate, no `rand::Rng::gen_range`.
- If V8/V8w fail their first real run: diagnose as a real bug first (per this repo's escalation policy). Only fall back to `#[ignore]` + a documented reason + a filed GitHub issue if investigation shows it's a genuine, already-known classical-decoder limitation (the same family as V5/V6's fading-robustness gap) — never silently widen a threshold to force a pass.
- Full spec: `docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md`.

---

### Task 1: Deterministic pileup callsign fixture list

**Files:**
- Create: `crates/manta-testkit/src/callsigns.rs`
- Modify: `crates/manta-testkit/src/lib.rs` (add `mod callsigns;`)

**Interfaces:**
- Produces: `pub(crate) fn pileup_calls() -> Vec<String>` — 50 unique, deterministic ham-style callsigns, ChaCha8-seeded.

- [ ] **Step 1: Write the failing test**

Create `crates/manta-testkit/src/callsigns.rs`:

```rust
//! Deterministic fixture callsigns for SPEC §7 V8/V8w pileup scenes.
//! ChaCha8-seeded (pinned decision 2), not 50 hand-picked real-looking
//! calls.

use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use std::collections::BTreeSet;

const PREFIXES: [&str; 25] = [
    "W2", "W3", "W4", "W5", "W6", "W7", "W8", "W9", "W0", "K1", "K2", "K3", "K4", "K6", "K7",
    "N3", "N4", "N5", "N6", "N7", "AA1", "AB2", "AC3", "VE3", "VE7",
];
const SUFFIX_LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const PILEUP_CALLS_SEED: u64 = 0x534B_494D_5638; // "SKIMV8"

/// 50 unique, deterministic fixture callsigns for pileup scenes (SPEC §7
/// V8/V8w). Uses `crate::u01` (lib.rs, already `pub(crate)`) for the same
/// hand-rolled ChaCha8 conversion every other generator in this crate uses
/// (pinned decision 2) -- no local reimplementation.
pub(crate) fn pileup_calls() -> Vec<String> {
    let mut rng = ChaCha8Rng::seed_from_u64(PILEUP_CALLS_SEED);
    let mut calls = BTreeSet::new();
    while calls.len() < 50 {
        let prefix = PREFIXES[(crate::u01(&mut rng) * PREFIXES.len() as f64) as usize];
        let suffix: String = (0..3)
            .map(|_| {
                SUFFIX_LETTERS[(crate::u01(&mut rng) * SUFFIX_LETTERS.len() as f64) as usize] as char
            })
            .collect();
        calls.insert(format!("{prefix}{suffix}"));
    }
    calls.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_50_unique_deterministic_calls() {
        let a = pileup_calls();
        let b = pileup_calls();
        assert_eq!(a.len(), 50);
        assert_eq!(a, b, "must be deterministic across calls");
        let unique: BTreeSet<&String> = a.iter().collect();
        assert_eq!(unique.len(), 50, "all 50 calls must be unique");
    }
}
```

- [ ] **Step 2: Wire the module into the crate**

In `crates/manta-testkit/src/lib.rs`, add `mod callsigns;` alongside the existing `pub mod` list (this one stays private — only `vectors.rs` uses it):

```rust
pub mod cer;
pub mod keyer;
mod callsigns;
pub mod noise;
pub mod scene;
pub mod vectors;
pub mod wav;
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p manta-testkit callsigns:: -- --nocapture`
Expected: `produces_50_unique_deterministic_calls ... ok`

- [ ] **Step 4: Commit**

```bash
git add crates/manta-testkit/src/callsigns.rs crates/manta-testkit/src/lib.rs
git commit -m "feat(testkit): deterministic 50-call pileup fixture list"
```

---

### Task 2: V8/V8w VectorSpec generators

**Files:**
- Modify: `crates/manta-testkit/src/vectors.rs`

**Interfaces:**
- Consumes: `crate::callsigns::pileup_calls() -> Vec<String>` (Task 1), `crate::u01(&mut ChaCha8Rng) -> f64` (existing, `pub(crate)` in `lib.rs`), `SignalSpec`/`Jitter`/`WattersonFade`/`WattersonPreset` (existing).
- Produces: `pub fn v8() -> VectorSpec`, `pub fn v8w() -> VectorSpec`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/manta-testkit/src/vectors.rs`'s `#[cfg(test)] mod tests` block (near the other `*_spec_matches_spec_table` tests):

```rust
#[test]
fn v8_spec_matches_spec_table() {
    let spec = v8();
    assert_eq!(spec.fs, 96_000.0);
    assert_eq!(spec.duration_s, 120.0);
    assert_eq!(spec.signals.len(), 50);
    for s in &spec.signals {
        assert!((10.0..=35.0).contains(&s.wpm), "wpm {} out of range", s.wpm);
        assert!(
            (-2.0..=25.0).contains(&s.snr_2500_db),
            "snr {} out of range",
            s.snr_2500_db
        );
        assert!(
            (-45_000.0..=45_000.0).contains(&s.offset_hz),
            "offset {} out of range",
            s.offset_hz
        );
        assert!(s.jitter.is_some(), "V8 must have 8% jitter");
        assert!(s.watterson.is_none(), "V8 is AWGN-only, no fading");
    }
    let unique_offsets: std::collections::BTreeSet<i64> =
        spec.signals.iter().map(|s| s.offset_hz as i64).collect();
    assert_eq!(unique_offsets.len(), 50, "all 50 offsets must be distinct");
}

#[test]
fn v8w_spec_matches_v8_scene_plus_fading() {
    let v8 = v8();
    let v8w = v8w();
    assert_eq!(v8w.signals.len(), 50);
    for (a, b) in v8.signals.iter().zip(v8w.signals.iter()) {
        assert_eq!(a.offset_hz, b.offset_hz, "V8w must reuse V8's offsets");
        assert_eq!(a.wpm, b.wpm, "V8w must reuse V8's WPM");
        assert_eq!(a.snr_2500_db, b.snr_2500_db, "V8w must reuse V8's SNR");
        assert!(b.watterson.is_some(), "V8w must add Watterson fading");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p manta-testkit v8 -- --nocapture`
Expected: FAIL with "cannot find function `v8` in this scope" (and `v8w`)

- [ ] **Step 3: Implement `pileup_scene`/`v8`/`v8w`**

Add near the top of `crates/manta-testkit/src/vectors.rs`, alongside the existing imports:

```rust
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
```

Add after `v10()` (end of the vector-generator functions, before the `Manifest`/`write_fixture_set` section):

```rust
/// Shared SPEC §7 V8/V8w scene: 50 signals, WPM 10..35, SNR (2500 Hz)
/// -2..25 dB, offsets uniform over +/-45 kHz with reject-redraw separation
/// (clear of the 1-channel/93.75 Hz merge threshold), 8% jitter. `v8w`
/// passes `Some(WattersonPreset::Poor)` to add CCIR-poor fading to every
/// signal on top of the identical AWGN-only scene `v8` uses -- same base
/// seed, so offsets/WPM/SNR/jitter are bit-identical between the two.
fn pileup_scene(name: &'static str, watterson: Option<WattersonPreset>) -> VectorSpec {
    const BASE_SEED: u64 = 0x534B_494D_5638; // "SKIMV8" -- shared by v8/v8w
    const MIN_SEPARATION_HZ: f64 = 300.0;
    let calls = crate::callsigns::pileup_calls();
    let mut rng = ChaCha8Rng::seed_from_u64(BASE_SEED);

    let mut offsets: Vec<f64> = Vec::with_capacity(50);
    'draw: while offsets.len() < 50 {
        let candidate = -45_000.0 + crate::u01(&mut rng) * 90_000.0;
        for &existing in &offsets {
            if (candidate - existing).abs() < MIN_SEPARATION_HZ {
                continue 'draw;
            }
        }
        offsets.push(candidate);
    }

    let signals = (0..50)
        .map(|i| SignalSpec {
            text: format!("CQ CQ DE {0} {0} K", calls[i]),
            loop_text: true,
            wpm: 10.0 + crate::u01(&mut rng) as f32 * 25.0,
            offset_hz: offsets[i],
            snr_2500_db: -2.0 + crate::u01(&mut rng) as f32 * 27.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: BASE_SEED ^ (i as u64 + 1),
            }),
            qsb: None,
            watterson: watterson.map(|preset| WattersonFade {
                preset,
                seed: BASE_SEED ^ 0xA5A5_0000 ^ (i as u64 + 1),
            }),
            char_wpm: None,
        })
        .collect();

    VectorSpec {
        name,
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: BASE_SEED,
        signals,
    }
}

/// SPEC §7 V8 "pileup-50": 50 signals, AWGN, jitter 8%.
pub fn v8() -> VectorSpec {
    pileup_scene("v8", None)
}

/// SPEC §7 V8w "pileup-50-fading": same scene as V8, Watterson CCIR-poor.
pub fn v8w() -> VectorSpec {
    pileup_scene("v8w", Some(WattersonPreset::Poor))
}
```

Note: `pileup_scene`'s per-signal WPM/SNR draw order means iteration order matters for reproducing "identical scene/seeds" — since `v8()` and `v8w()` both call `pileup_scene` with the same `BASE_SEED` and draw in the same order (offsets first, then per-signal wpm+snr in the same `0..50` loop), the two calls produce bit-identical `wpm`/`snr_2500_db`/`offset_hz` sequences. Only the `watterson` field differs.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p manta-testkit v8 -- --nocapture`
Expected: both `v8_spec_matches_spec_table` and `v8w_spec_matches_v8_scene_plus_fading` pass.

- [ ] **Step 5: Run the full manta-testkit test suite**

Run: `cargo test -p manta-testkit`
Expected: all pass, no regressions.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-testkit/src/vectors.rs
git commit -m "feat(testkit): V8/V8w pileup-50 vector generators"
```

---

### Task 3: V8 golden test

**Files:**
- Create: `crates/manta-cli/tests/golden_v8_v8w.rs`

**Interfaces:**
- Consumes: `manta_testkit::vectors::{v8, v8w, write_fixture_set, VectorSpec, Manifest}` (Task 2), `manta_testkit::cer::cer`, the `manta` CLI binary's `decode --json` output shape (`report["events"]`, `report["text"]`, event fields `track_id`/`sample_ts`/`event`/`glyph`/`freq_hz`, same shape `golden_v7_v9_v10.rs` already parses).
- Produces (for Task 4, same file): `decode_report`, `per_track`, `call_from_keyed_text`, `match_tracks_by_freq`, `bogus_calls` helper functions.

- [ ] **Step 1: Write the test file with all shared helpers + the V8 test**

Create `crates/manta-cli/tests/golden_v8_v8w.rs`:

```rust
//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" approximate the future manta-spot validator
//! (M3) the same way V5/V6 approximate "callsign validated" today -- see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

/// Group `report["events"]` by `track_id`, returning each track's decoded
/// text and its last-reported TrackMeta freq_hz. Same shape as
/// golden_v7_v9_v10.rs's helper of the same name.
fn per_track(report: &serde_json::Value) -> BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u64, f64> = BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = ev["track_id"].as_u64().unwrap();
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"]["Char"].as_str() {
                    texts.entry(tid).or_default().push_str(c);
                }
            }
            "WordBoundary" => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            "TrackMeta" => {
                freqs.insert(tid, ev["freq_hz"].as_f64().unwrap());
            }
            _ => {}
        }
    }
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

/// Extract the callsign from this project's SPEC §7 payload template
/// "CQ CQ DE <CALL> <CALL> K" (word index 3).
fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(3)
        .expect("keyed text must follow the 'CQ CQ DE <CALL> <CALL> K' template")
}

/// For each expected signal (`manifest.expected_freqs_hz` order), the
/// decoded track whose last-reported freq_hz is closest to that signal's
/// expected absolute frequency.
fn match_tracks_by_freq<'a>(
    manifest: &manta_testkit::vectors::Manifest,
    tracks: &'a BTreeMap<u64, (String, Option<f64>)>,
) -> Vec<(&'a str, Option<f64>)> {
    manifest
        .expected_freqs_hz
        .iter()
        .map(|&expected_freq| {
            tracks
                .values()
                .min_by(|(_, fa), (_, fb)| {
                    let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                    let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                    da.partial_cmp(&db).unwrap()
                })
                .map(|(text, freq)| (text.as_str(), *freq))
                .unwrap()
        })
        .collect()
}

/// Callsign-shaped, >=2-rep tokens in `decoded_text` that are not in
/// `known_calls` -- SPEC §7 V8/V8w's "0 bogus callsigns spotted".
fn bogus_calls(decoded_text: &str, known_calls: &HashSet<&str>) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in decoded_text.split_whitespace() {
        if (3..=7).contains(&word.len())
            && word.chars().all(|c| c.is_ascii_alphanumeric())
            && word.chars().any(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_alphabetic())
        {
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(word, n)| n >= 2 && !known_calls.contains(word))
        .map(|(word, _)| word.to_string())
        .collect()
}

#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let spec = manta_testkit::vectors::v8();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8 fixture must have 50 unique callsigns"
    );

    let matched = match_tracks_by_freq(&manifest, &tracks);
    let mut validated = 0;
    for (i, keyed_text) in manifest.keyed_texts.iter().enumerate() {
        let call = call_from_keyed_text(keyed_text);
        let (decoded_text, _freq) = matched[i];
        if decoded_text.matches(call).count() >= 2 {
            validated += 1;
        }
    }
    assert!(
        validated >= 45,
        "V8 must validate >= 45/50 callsigns, got {validated}/50"
    );

    let mut bogus = Vec::new();
    for (_tid, (decoded_text, _freq)) in &tracks {
        bogus.extend(bogus_calls(decoded_text, &known_calls));
    }
    assert!(
        bogus.is_empty(),
        "V8 must spot 0 bogus callsigns, got {bogus:?}"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p manta-cli --test golden_v8_v8w v8_pileup -- --nocapture`

This is a real 120 s / 50-signal scene decode, expect it to take tens of seconds to a few minutes. Report the actual result (pass, or the actual `validated`/`bogus` numbers on failure) — do not assume it passes.

- [ ] **Step 3: Handle the result**

- If it passes: proceed to Step 4.
- If it fails: this is a real measurement, not a bug in the test. Investigate per this plan's Global Constraints escalation policy — check whether the shortfall is scattered (real decode-quality gap) or concentrated in specific SNR/offset ranges (possible channelizer edge effect, cf. the V2 near-channel-edge bug). Do NOT mark `#[ignore]` without a documented root-cause finding, matching how V2/V5/V6 were escalated (see `crates/manta-cli/tests/golden_v2_v3.rs`'s doc comments for the required style: a paragraph explaining what was measured, what was ruled out, and why the remaining gap is accepted as a known limitation, plus a filed GitHub issue if it's a new finding). Report back with the real numbers and your diagnosis before deciding whether to `#[ignore]` it.

- [ ] **Step 4: Commit**

```bash
git add crates/manta-cli/tests/golden_v8_v8w.rs
git commit -m "test(cli): V8 pileup-50 golden gate"
```

(If Step 3 required a documented `#[ignore]` instead, commit that version with its doc comment and mention the filed issue number in the commit message.)

---

### Task 4: V8w golden test

**Files:**
- Modify: `crates/manta-cli/tests/golden_v8_v8w.rs`

**Interfaces:**
- Consumes: all helpers from Task 3 (same file).

- [ ] **Step 1: Add the V8w test**

Append to `crates/manta-cli/tests/golden_v8_v8w.rs`:

```rust
#[test]
fn v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts() {
    let spec = manta_testkit::vectors::v8w();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let matched = match_tracks_by_freq(&manifest, &tracks);
    let strong: Vec<usize> = spec
        .signals
        .iter()
        .enumerate()
        .filter(|(_, s)| s.snr_2500_db >= 6.0)
        .map(|(i, _)| i)
        .collect();
    assert!(
        !strong.is_empty(),
        "V8w must have at least one >= +6 dB signal"
    );

    let mut good = 0;
    for &i in &strong {
        let (decoded_text, _freq) = matched[i];
        let cer = manta_testkit::cer::cer(&manifest.keyed_texts[i], decoded_text);
        if cer < 0.10 {
            good += 1;
        }
    }
    let pct = good as f64 / strong.len() as f64;
    assert!(
        pct >= 0.90,
        "V8w must decode >= 90% of >= +6 dB signals at CER < 10%, got {good}/{} ({:.1}%)",
        strong.len(),
        pct * 100.0
    );

    let mut bogus = Vec::new();
    for (_tid, (decoded_text, _freq)) in &tracks {
        bogus.extend(bogus_calls(decoded_text, &known_calls));
    }
    assert!(
        bogus.is_empty(),
        "V8w must spot 0 bogus callsigns, got {bogus:?}"
    );

    // 0 cross-channel ghost decodes: no fixture call's >=2-rep substring
    // appears in more than one distinct track.
    for call in &known_calls {
        let hits = tracks
            .values()
            .filter(|(text, _)| text.matches(call).count() >= 2)
            .count();
        assert!(
            hits <= 1,
            "callsign {call} decoded (>=2 reps) in {hits} distinct tracks, expected <= 1 (ghost decode)"
        );
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p manta-cli --test golden_v8_v8w v8w_pileup -- --nocapture`

Real Watterson-faded 120 s / 50-signal decode — expect a few minutes. Report the actual numbers.

- [ ] **Step 3: Handle the result**

Same escalation policy as Task 3 Step 3. V8w is the more likely of the two to hit this repo's known classical-decoder fading-robustness gap (same family as V5/V6, both already `#[ignore]`d for exactly this reason) — if so, that's an expected, not surprising, outcome; document it the same way, with a filed issue.

- [ ] **Step 4: Run the full manta-cli test suite (excluding other known-ignored tests)**

Run: `cargo test -p manta-cli`
Expected: no regressions in the existing V1/V3/V4/V7/V9/V10 golden tests.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-cli/tests/golden_v8_v8w.rs
git commit -m "test(cli): V8w pileup-50-fading golden gate"
```

(Same note as Task 3 Step 4 if `#[ignore]` was required.)

---

### Task 5: CPU-budget criterion bench

**Files:**
- Modify: `Cargo.toml` (workspace root — add `criterion` to `[workspace.dependencies]`)
- Modify: `crates/manta-engine/Cargo.toml` (add `criterion` dev-dependency + `[[bench]]`)
- Create: `crates/manta-engine/benches/cpu_budget.rs`

**Interfaces:**
- Consumes: `manta_engine::{decode_samples, PipelineConfig}` (existing public API — `TrackManager` itself is private, so the bench must go through this entry point), `manta_testkit::scene::{render_scene, SignalSpec}` (existing, already a dev-dependency of `manta-engine`).
- Produces: `cpu_budget_scene()` (duplicated into Task 6's test file, same convention as this repo's other per-file test-helper duplication).

- [ ] **Step 1: Add the `criterion` dependency**

In `Cargo.toml` (workspace root), add to `[workspace.dependencies]` (alphabetical-ish, next to the other dev/test-oriented deps):

```toml
criterion = "0.8"
```

In `crates/manta-engine/Cargo.toml`, add to `[dev-dependencies]` and a new `[[bench]]` section:

```toml
[dev-dependencies]
coppa-audio = { workspace = true }
criterion = { workspace = true }
proptest = { workspace = true }
manta-testkit = { workspace = true }

[[bench]]
name = "cpu_budget"
harness = false
```

- [ ] **Step 2: Write the bench**

Create `crates/manta-engine/benches/cpu_budget.rs`:

```rust
//! ROADMAP.md M2 accept criterion: full pipeline at 192 kS/s with 300
//! active tracks uses < 50% of one core on an M-series Mac AND < 1 core on
//! a Raspberry Pi 4. This is the `cargo bench` profiling target; the
//! actual Mac-budget assertion is the `#[ignore]`d test in
//! `tests/cpu_budget.rs` -- perf assertions don't belong in a criterion
//! group, and this bench isn't wired into CI (GitHub-hosted runners aren't
//! Mac-series or Pi4 hardware, and perf assertions on shared CI runners
//! are flaky). See
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use num_complex::Complex32;
use manta_engine::PipelineConfig;
use manta_testkit::scene::{render_scene, SignalSpec};
use std::time::Duration;

/// 300 simultaneous keyed tones spread across a 192 kS/s passband, evenly
/// spaced ~600 Hz apart (well clear of the 93.75 Hz channel-merge
/// threshold). No accuracy requirement -- this only needs to drive the
/// detector into promoting ~300 concurrent ACTIVE tracks so the bench
/// exercises real channelizer + detector + decoder-pool cost, not decode
/// correctness.
fn cpu_budget_scene() -> (Vec<Complex32>, f64, f64, PipelineConfig) {
    const FS: f64 = 192_000.0;
    const CENTER_FREQ_HZ: f64 = 14_000_000.0;
    const DURATION_S: f64 = 15.0;
    const N_SIGNALS: usize = 300;

    let signals: Vec<SignalSpec> = (0..N_SIGNALS)
        .map(|i| {
            let offset_hz = -90_000.0 + i as f64 * (180_000.0 / (N_SIGNALS - 1) as f64);
            SignalSpec {
                text: "CQ CQ DE K1BNC K1BNC K".into(),
                loop_text: true,
                wpm: 20.0,
                offset_hz,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            }
        })
        .collect();
    let (samples, _texts) =
        render_scene(&signals, FS, DURATION_S, Some(0x4350_555F_4250_5431)).unwrap();
    (samples, FS, CENTER_FREQ_HZ, PipelineConfig::default())
}

fn bench_cpu_budget(c: &mut Criterion) {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let mut group = c.benchmark_group("cpu_budget");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(150));
    group.warm_up_time(Duration::from_secs(1));
    group.bench_function("192khz_300tracks", |b| {
        b.iter(|| {
            manta_engine::decode_samples(
                black_box(&iq),
                black_box(fs),
                black_box(center_freq_hz),
                black_box(&cfg),
            )
            .unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, bench_cpu_budget);
criterion_main!(benches);
```

- [ ] **Step 3: Run the bench**

Run: `cargo bench -p manta-engine --bench cpu_budget`

This runs 10 iterations of a 192 kHz / 15 s / 300-track full decode each — likely several minutes total. Run it with `run_in_background` if your tool supports it, or expect to wait. Report the printed mean time per iteration.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/manta-engine/Cargo.toml crates/manta-engine/benches/cpu_budget.rs
git commit -m "perf(engine): 192 kS/s / 300-track CPU-budget criterion bench"
```

---

### Task 6: CPU-budget wall-clock assertion test

**Files:**
- Create: `crates/manta-engine/tests/cpu_budget.rs`

**Interfaces:**
- Consumes: `manta_engine::{decode_samples, PipelineConfig}`, `manta_testkit::scene::{render_scene, SignalSpec}` (same as Task 5; this is a separate binary target so the scene helper is duplicated, matching this repo's existing per-test-file helper duplication convention — see `golden_v2_v3.rs` vs `golden_v7_v9_v10.rs` each having their own `decode_report`).

- [ ] **Step 1: Write the test**

Create `crates/manta-engine/tests/cpu_budget.rs`:

```rust
//! ROADMAP.md M2 CPU-budget accept criterion, Mac leg only (see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md and
//! benches/cpu_budget.rs's module doc). Raspberry Pi 4 leg (< 1 core) is
//! an explicitly flagged outstanding manual step -- same pattern as M1's
//! still-outstanding W1AW live-copy run (see CLAUDE.md Status).

use num_complex::Complex32;
use manta_engine::PipelineConfig;
use manta_testkit::scene::{render_scene, SignalSpec};

fn cpu_budget_scene() -> (Vec<Complex32>, f64, f64, PipelineConfig) {
    const FS: f64 = 192_000.0;
    const CENTER_FREQ_HZ: f64 = 14_000_000.0;
    const DURATION_S: f64 = 15.0;
    const N_SIGNALS: usize = 300;

    let signals: Vec<SignalSpec> = (0..N_SIGNALS)
        .map(|i| {
            let offset_hz = -90_000.0 + i as f64 * (180_000.0 / (N_SIGNALS - 1) as f64);
            SignalSpec {
                text: "CQ CQ DE K1BNC K1BNC K".into(),
                loop_text: true,
                wpm: 20.0,
                offset_hz,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            }
        })
        .collect();
    let (samples, _texts) =
        render_scene(&signals, FS, DURATION_S, Some(0x4350_555F_4250_5431)).unwrap();
    (samples, FS, CENTER_FREQ_HZ, PipelineConfig::default())
}

#[test]
#[ignore]
fn cpu_budget_mac_under_half_core() {
    let (iq, fs, center_freq_hz, cfg) = cpu_budget_scene();
    let audio_duration_s = iq.len() as f64 / fs;
    let start = std::time::Instant::now();
    let report = manta_engine::decode_samples(&iq, fs, center_freq_hz, &cfg);
    let elapsed = start.elapsed().as_secs_f64();
    assert!(report.is_ok(), "decode_samples failed: {:?}", report.err());
    let ratio = elapsed / audio_duration_s;
    println!(
        "cpu_budget: {elapsed:.2}s wall / {audio_duration_s:.2}s audio = {ratio:.3}x realtime (Mac budget: < 0.5x)"
    );
    assert!(
        ratio < 0.5,
        "192 kS/s / 300-track pipeline used {ratio:.3}x realtime, Mac budget is < 0.5x (< 50% of one core)"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p manta-engine --test cpu_budget -- --ignored --nocapture`

- [ ] **Step 3: Report the real number**

Note the printed `cpu_budget: ...x realtime` line and whether the assertion passed. This exact number goes into Task 7's DECISIONS doc — do not estimate it, use the real measured value from this run.

- [ ] **Step 4: If it fails**

If the ratio is >= 0.5 (over budget) on this Mac: this is a real perf finding, not a test bug. Do not loosen the assertion. Report the actual ratio and profile (re-run the Task 5 criterion bench with `--profile-time` or inspect with `cargo flamegraph` if available) to identify the bottleneck, and report back before deciding next steps — this may mean the CPU-budget work itself needs a follow-up optimization task, which is out of scope for this plan to pre-specify since it depends on what the profile shows.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-engine/tests/cpu_budget.rs
git commit -m "test(engine): CPU-budget wall-clock assertion (Mac leg)"
```

---

### Task 7: Final integration

**Files:**
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Create: `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`

**Interfaces:**
- Consumes: the real pass/fail outcomes and measured numbers reported by Tasks 3, 4, and 6.

- [ ] **Step 1: Write the DECISIONS pin doc**

Create `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`, following the existing style of `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` (numbered pinned decisions, each with a one-line summary and rationale). Must include, using the REAL values from Tasks 3/4/6 (not placeholders):

1. Pileup fixture callsigns are synthetic/deterministic (ChaCha8-seeded prefix+suffix composition), not real operator calls.
2. V8/V8w "callsign validated"/"bogus"/"ghost decode" are approximated via decoded-text substring/token heuristics (no real `manta-spot` validator exists yet — M3 scope), same convention as V5/V6, but upgraded to match tracks to signals by nearest `TrackMeta.freq_hz` rather than a bare substring search.
3. V8 result: pass/fail, and if `#[ignore]`d, the issue number and root-cause summary.
4. V8w result: same.
5. CPU-budget Mac measurement: the actual `cpu_budget: X.XXs wall / 15.00s audio = X.XXXx realtime` line from Task 6, and whether it cleared the < 0.5x budget.
6. Raspberry Pi 4 leg (< 1 core / 1.0x realtime): explicitly flagged outstanding, pending Tony running `cargo test -p manta-engine --test cpu_budget -- --ignored --nocapture` on real Pi4 hardware — same pattern as M1's still-outstanding W1AW live-copy run.

- [ ] **Step 2: Update ROADMAP.md**

In the M2 section, following the existing pattern ("M2 sub-project 1 ... is complete", "M2 sub-project 2 ... is complete"), add a sentence noting this sub-project's status (complete, or complete-with-documented-`#[ignore]`s matching V2/V5/V6's precedent) and pointing at the new DECISIONS doc. Update "Remaining M2 sub-projects" to drop the completed item, leaving SoapySDR input and KiwiSDR input.

- [ ] **Step 3: Update CLAUDE.md Status section**

Mirror the existing Status paragraph's style (which vectors are green/ignored, which issues are filed, what's next). Keep it under the file's ~100-line budget — trim if needed, don't let it grow unbounded (per global instructions: CLAUDE.md stays under ~100 lines, narratives belong in `docs/DECISIONS/`).

- [ ] **Step 4: Full workspace verification**

Run, in order:
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
All three must be clean before proceeding. Fix any issues and re-run.

- [ ] **Step 5: Commit**

```bash
git add ROADMAP.md CLAUDE.md docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md
git commit -m "docs: M2 pileup + CPU-budget close-out, pin real measurements"
```

- [ ] **Step 6: Push and open the PR**

```bash
git push -u origin feat/m2-pileup-cpu-budget
gh pr create --title "feat(testkit,engine): M2 pileup validation (V8/V8w) + CPU-budget bench" --body "$(cat <<'EOF'
## Summary

- SPEC-decode-core.md §7 V8/V8w pileup golden vectors (50-signal AWGN/Watterson CCIR-poor scenes).
- ROADMAP.md M2 CPU-budget criterion bench (192 kS/s, 300 active tracks).
- Real measured numbers pinned in docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md.

See docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md for the full design.

## Test plan

- [x] cargo fmt --all --check
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo test --workspace

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Report the PR URL.
