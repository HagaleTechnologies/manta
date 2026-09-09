//! MAN-72: guard the Serena project configuration that gives catalyst-dev's
//! codebase-analyzer / codebase-locator / codebase-pattern-finder subagents real
//! symbol search in this repo.
//!
//! Serena activation itself cannot be exercised from `cargo test` — it needs the
//! `mcp__serena__*` MCP server, which CI does not run. What CI *can* guard is the
//! shape of the committed config: that it exists, parses, names this repo, uses
//! the CURRENT schema key for the language list, spells out every field Serena
//! would otherwise backfill (a backfill re-serializes the file and strips every
//! comment in it), keeps the load-bearing ignores, keeps every crate's source
//! root visible to the indexer, and ships the `codebase_map` memory the analyzer
//! reads first.
//!
//! Unlike the sibling fleet repos, this file is NOT what makes CI run: manta's
//! `.github/workflows/ci.yml` has no paths filter, so a `.serena/`-only PR
//! already triggers the full two-OS matrix. This test is regression insurance
//! only — a rename or a tidy-up could otherwise silently drop the repo back into
//! grep-fallback with nothing to notice.
//!
//! Parsed with a purpose-built reader rather than a YAML crate on purpose: this
//! workspace declares no YAML parser (Cargo.toml:21-45 — serde, serde_json, toml
//! and nothing else), and the file being read is one we hand-author and whose
//! shape this very test pins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Every non-underscore field of Serena's `ProjectConfig` dataclass (22 as of
/// Serena 1.7.1.dev0). Any field missing from the committed YAML makes
/// `_load_yaml_dict` set `was_complete = False`, which re-serializes the file
/// on activation and deletes every comment in it.
const REQUIRED_KEYS: [&str; 22] = [
    "project_name",
    "language_servers",
    "encoding",
    "activation_command",
    "activation_command_timeout",
    "line_ending",
    "language_backend",
    "ignore_all_files_in_gitignore",
    "ls_specific_settings",
    "ls_workspace_folders",
    "ls_additional_workspace_folders",
    "ignored_paths",
    "read_only",
    "excluded_tools",
    "included_optional_tools",
    "fixed_tools",
    "default_modes",
    "added_modes",
    "initial_prompt",
    "symbol_info_budget",
    "read_only_memory_patterns",
    "ignored_memory_patterns",
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/manta-decode, so two hops up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has two ancestors")
        .to_path_buf()
}

fn serena_dir() -> PathBuf {
    repo_root().join(".serena")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Drops a valid YAML inline comment (` # ...` to end of line) that starts
/// *outside* any quoted scalar, so documenting an entry — `read_only: true #
/// navigation only`, `- rust # language server` — does not turn into a CI
/// failure. A `#` inside quotes, or one not preceded by whitespace (`a#b`), is
/// part of the value and is kept.
fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &c) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
                    return &value[..i];
                }
            }
        }
    }
    value
}

fn unquote(value: &str) -> String {
    strip_inline_comment(value.trim())
        .trim()
        .trim_matches(['\'', '"'])
        .to_string()
}

#[derive(Default)]
struct FlatYaml {
    scalars: HashMap<String, String>,
    lists: HashMap<String, Vec<String>>,
}

/// Reads the flat `key: value` / `key:` + `- item` / `key: |` subset this file uses.
fn parse_flat_yaml(src: &str) -> FlatYaml {
    let mut out = FlatYaml::default();
    let mut current_key: Option<String> = None;
    let mut in_block_scalar = false;

    for raw_line in src.lines() {
        if in_block_scalar {
            // A block scalar's body is indented; the first non-indented,
            // non-empty line ends it.
            if raw_line.trim().is_empty() || raw_line.starts_with(char::is_whitespace) {
                continue;
            }
            in_block_scalar = false;
        }
        let line = raw_line.trim_end();
        if line.is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let line = strip_inline_comment(line).trim_end();
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if let Some(key) = current_key.as_ref() {
                out.lists
                    .entry(key.clone())
                    .or_default()
                    .push(unquote(item));
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.starts_with(char::is_whitespace) || key.is_empty() {
            continue;
        }
        let key = key.to_string();
        let value = value.trim();
        out.lists.entry(key.clone()).or_default();
        if value == "|" || value == ">" {
            in_block_scalar = true;
            out.scalars.insert(key.clone(), String::new());
        } else {
            out.scalars.insert(key.clone(), unquote(value));
        }
        current_key = Some(key);
    }
    out
}

/// Workspace members from the root `Cargo.toml`'s `members = [ ... ]` array.
/// Read directly rather than via a TOML dependency: `toml` is not a dependency
/// of `manta-decode` and this is a three-line parse of an array we control.
fn workspace_members() -> Vec<String> {
    let manifest = read(&repo_root().join("Cargo.toml"));
    let start = manifest.find("members = [").expect("members array missing");
    let rest = &manifest[start + "members = [".len()..];
    let end = rest.find(']').expect("unterminated members array");
    rest[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Matches one gitignore pattern segment (may contain `*`, which never
/// crosses a `/`) against one path segment.
fn segment_matches(pattern: &[u8], segment: &[u8]) -> bool {
    match (pattern.first(), segment.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            segment_matches(&pattern[1..], segment)
                || (!segment.is_empty() && segment_matches(pattern, &segment[1..]))
        }
        (Some(p), Some(s)) if p == s => segment_matches(&pattern[1..], &segment[1..]),
        _ => false,
    }
}

/// Matches a full slash-split gitignore pattern (`*` and `**` wildcards)
/// against a full slash-split path.
fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((p, prest)), None) => *p == "**" && segments_match(prest, path),
        (None, Some(_)) => false,
        (Some((p, prest)), Some((s, srest))) => {
            if *p == "**" {
                segments_match(prest, path) || segments_match(pattern, srest)
            } else {
                segment_matches(p.as_bytes(), s.as_bytes()) && segments_match(prest, srest)
            }
        }
    }
}

/// True if an `ignored_paths` entry (real gitignore syntax, per
/// `pathspec.GitWildMatchPattern` — see `serena/project.py:106`) would exclude
/// `target` or any ancestor directory of it. A slash anywhere but the end
/// anchors the pattern at the repo root; otherwise it can match at any depth
/// (`src` behaves like `**/src`). Matching an ancestor directory (e.g.
/// `crates/**` matching `crates`) shadows everything below it too, so every
/// prefix of `target`'s segments is checked, not just the full path.
fn pattern_shadows(pattern: &str, target: &str) -> bool {
    let trimmed = pattern.trim_end_matches('/');
    let anchored = pattern.starts_with('/') || trimmed.contains('/');
    let unanchored = trimmed.trim_start_matches('/');
    let mut segs: Vec<&str> = unanchored.split('/').collect();
    if !anchored {
        segs.insert(0, "**");
    }
    let target_segs: Vec<&str> = target.split('/').collect();
    (0..=target_segs.len()).any(|i| segments_match(&segs, &target_segs[..i]))
}

#[test]
fn project_yml_is_committed_at_the_repo_root() {
    assert!(
        serena_dir().join("project.yml").exists(),
        ".serena/project.yml must exist at the repo root — without it Serena does \
         not error, it silently auto-generates a config named after the checkout \
         directory with empty ignored_paths and read_only: false (MAN-72)"
    );
}

#[test]
fn project_yml_identifies_this_repo_and_stays_navigation_only() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    assert_eq!(
        cfg.scalars.get("project_name").map(String::as_str),
        Some("manta")
    );
    assert_eq!(
        cfg.scalars.get("read_only").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        cfg.scalars
            .get("ignore_all_files_in_gitignore")
            .map(String::as_str),
        Some("true")
    );
}

#[test]
fn project_yml_declares_rust_under_the_current_schema_key() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    assert!(
        !cfg.scalars.contains_key("languages"),
        "`languages:` is the LEGACY key; Serena's RENAMED_FIELDS migrates it to \
         `language_servers:` and re-serializes the file, stripping every comment"
    );
    let ls = cfg
        .lists
        .get("language_servers")
        .expect("language_servers key missing");
    assert!(
        ls.iter().any(|l| l == "rust"),
        "dropping `rust` silently disables symbol search across all 9 crates; got {ls:?}"
    );
}

#[test]
fn project_yml_spells_every_field_serena_would_otherwise_backfill() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    for key in REQUIRED_KEYS {
        assert!(
            cfg.scalars.contains_key(key) || cfg.lists.contains_key(key),
            "missing key {key:?}: any absent field makes Serena rewrite the file on \
             activation and strip its comments"
        );
    }
}

#[test]
fn project_yml_ignores_the_noise_that_would_dominate_an_index_pass() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    let ignored = cfg
        .lists
        .get("ignored_paths")
        .expect("ignored_paths key missing");
    // `/.catalyst-cache` is the load-bearing one: Serena's GitignoreParser reads
    // only files named `.gitignore`, never `.git/info/exclude`, so a relocated
    // CARGO_HOME under the repo root is invisible to `ignore_all_files_in_gitignore`.
    for required in [".serena/cache", "/target", "/.catalyst-cache", "/thoughts"] {
        assert!(
            ignored.iter().any(|p| p == required),
            "ignored_paths must contain {required:?}; got {ignored:?}"
        );
    }
}

/// Every `.rs` file beneath `dir`, as repo-root-relative slash-separated paths.
/// Directory ancestors alone are not enough to guard the index: a file-selecting
/// pattern (`*.rs`, `crates/**/*.rs`) shadows no directory at all, yet would
/// still take the whole workspace source out of Serena's symbol index.
fn rust_files_under(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            rust_files_under(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let rel = path
                .strip_prefix(root)
                .expect("path is under the repo root");
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[test]
fn no_ignored_path_shadows_a_workspace_member_source_root() {
    let cfg = parse_flat_yaml(&read(&serena_dir().join("project.yml")));
    let ignored = cfg
        .lists
        .get("ignored_paths")
        .expect("ignored_paths key missing");
    let members = workspace_members();
    assert_eq!(
        members.len(),
        9,
        "workspace member count changed; got {members:?}"
    );
    for member in &members {
        let src = format!("{member}/src");
        let src_dir = repo_root().join(&src);
        assert!(src_dir.is_dir(), "{src} should exist");
        for pattern in ignored {
            assert!(
                !pattern_shadows(pattern, &src),
                "ignored_paths entry {pattern:?} shadows workspace source root {src}"
            );
        }
        // The directory checks above miss file-selecting patterns (`*.rs`,
        // `crates/**/*.rs`) — those shadow no directory yet still empty the
        // symbol index, so every source file is checked too, not just its
        // ancestors.
        let mut sources = Vec::new();
        rust_files_under(&repo_root(), &src_dir, &mut sources);
        sources.sort();
        assert!(!sources.is_empty(), "{src} contains no .rs files");
        for source in &sources {
            for pattern in ignored {
                assert!(
                    !pattern_shadows(pattern, source),
                    "ignored_paths entry {pattern:?} shadows workspace source file {source}"
                );
            }
        }
    }
}

#[test]
fn serena_ignore_wiring_keeps_cache_and_local_override_out_of_git() {
    let nested = read(&serena_dir().join(".gitignore"));
    assert!(nested.lines().any(|l| l.trim() == "/cache"), "{nested}");
    assert!(
        nested.lines().any(|l| l.trim() == "/project.local.yml"),
        "{nested}"
    );
}

#[test]
fn serena_ships_the_codebase_map_memory_the_analyzer_reads_first() {
    // The filename is a contract: codebase-analyzer's orientation step calls
    // read_memory("codebase_map"). Note the memory NAME has no `.md` suffix even
    // though the file does.
    let memory = serena_dir().join("memories").join("codebase_map.md");
    assert!(memory.exists(), "{} missing", memory.display());
    let body = read(&memory);
    for member in workspace_members() {
        let crate_name = member.rsplit('/').next().unwrap();
        assert!(
            body.contains(crate_name),
            "codebase_map.md does not mention {crate_name}"
        );
    }
}

#[test]
fn inline_comments_are_stripped_outside_quoted_scalars() {
    // Documenting an entry the way YAML allows must not fail the assertions
    // above; a `#` inside a quoted scalar is data, not a comment.
    let cfg = parse_flat_yaml(concat!(
        "read_only: true # navigation only\n",
        "language_servers:\n",
        "- rust # language server\n",
        "project_name: \"man # ta\"\n",
    ));
    assert_eq!(
        cfg.scalars.get("read_only").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        cfg.lists.get("language_servers").map(Vec::as_slice),
        Some(["rust".to_string()].as_slice())
    );
    assert_eq!(
        cfg.scalars.get("project_name").map(String::as_str),
        Some("man # ta")
    );
}

#[test]
fn pattern_shadows_catches_file_selecting_globs() {
    let source = "crates/manta-decode/src/lib.rs";
    assert!(pattern_shadows("*.rs", source));
    assert!(pattern_shadows("crates/**/*.rs", source));
    // ...without flagging patterns that leave the source visible.
    assert!(!pattern_shadows("*.rs", "crates/manta-decode/src"));
    assert!(!pattern_shadows("/target", source));
}
