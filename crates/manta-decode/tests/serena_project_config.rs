//! MAN-274: guards the Serena project config that gives catalyst-dev's
//! codebase-analyzer, codebase-locator and codebase-pattern-finder subagents
//! symbol search in this repo.
//!
//! `cargo test` cannot activate Serena: that needs a running MCP server, which
//! CI does not have. This file guards the committed config's shape instead.
//! It checks that the config exists, names this repo, spells every field
//! Serena would otherwise backfill (a backfill re-saves the file and drops its
//! comments), runs no activation command, keeps the load-bearing ignores, hides
//! no workspace `.rs` file from the index, and ships the `codebase_map` memory.
//!
//! CI runs `cargo test --workspace` on every PR with no paths filter, so this
//! is regression insurance, not what makes CI run on a `.serena/`-only change.
//!
//! The workspace has no YAML parser, so a small reader handles the subset of
//! YAML `project.yml` uses. Its own tests are at the bottom of this file.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Every `ProjectConfig` field of Serena 2.0 (25), from
/// `dataclasses.fields(ProjectConfig)` on oraios/serena 1de556f (2.0.0.dev0)
/// and d9e4c79 (2.0.1.dev0). A missing field, or one under a legacy name,
/// makes Serena re-save the file on load and drop every comment in it.
/// Serena 1.7 lacks the last three and ignores them.
const FIELDS: [&str; 25] = [
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
    "agent_interface",
    "excluded_apis",
    "included_apis",
];

/// Keys Serena's `RENAMED_FIELDS` migrates, which triggers the same re-save.
const LEGACY_KEYS: [&str; 2] = ["languages", "additional_workspace_folders"];

/// The CLAUDE.md section `initial_prompt` sends readers to.
const DOCUMENTS_HEADING: &str = "Documents (read in this order)";

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

fn project_yml() -> FlatYaml {
    parse_flat_yaml(&read(&serena_dir().join("project.yml")))
}

/// Drops a YAML inline comment (` # ...` to end of line) that starts outside
/// any quoted scalar. A `#` inside quotes, or one not preceded by whitespace
/// (`a#b`), is part of the value. Quote tracking honours YAML's two escapes:
/// `\"` inside double quotes and `''` inside single quotes.
fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(b'"') if c == b'\\' => {
                i += 2;
                continue;
            }
            Some(b'\'') if c == b'\'' && bytes.get(i + 1) == Some(&b'\'') => {
                i += 2;
                continue;
            }
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
        i += 1;
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
    /// Top-level keys in file order, repeats included.
    keys: Vec<String>,
    scalars: HashMap<String, String>,
    lists: HashMap<String, Vec<String>>,
}

impl FlatYaml {
    fn scalar(&self, key: &str) -> &str {
        self.scalars
            .get(key)
            .unwrap_or_else(|| panic!("project.yml has no `{key}:` key"))
    }

    fn list(&self, key: &str) -> &[String] {
        self.lists
            .get(key)
            .unwrap_or_else(|| panic!("project.yml has no `{key}:` key"))
    }
}

/// Reads the flat `key: value` / `key:` + `- item` / `key: |` subset this
/// file uses. A block scalar's body is kept verbatim, one trimmed line each.
fn parse_flat_yaml(src: &str) -> FlatYaml {
    let mut out = FlatYaml::default();
    let mut current_key: Option<String> = None;
    let mut block_key: Option<String> = None;

    for raw_line in src.lines() {
        if let Some(key) = block_key.clone() {
            // The body is indented; the first unindented, non-empty line ends
            // it. Inside a block scalar a `#` is prose, not a comment.
            if raw_line.trim().is_empty() || raw_line.starts_with(char::is_whitespace) {
                let body = out.scalars.entry(key).or_default();
                body.push_str(raw_line.trim());
                body.push('\n');
                continue;
            }
            block_key = None;
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
        if key.is_empty() || key.starts_with(char::is_whitespace) {
            continue;
        }
        let key = key.to_string();
        let value = value.trim();
        out.keys.push(key.clone());
        out.lists.entry(key.clone()).or_default();
        if value == "|" || value == ">" {
            block_key = Some(key.clone());
            out.scalars.insert(key.clone(), String::new());
        } else {
            out.scalars.insert(key.clone(), unquote(value));
        }
        current_key = Some(key);
    }
    out
}

/// Workspace member paths (`crates/<name>`) from the root `Cargo.toml`.
fn workspace_members() -> Vec<String> {
    let manifest: toml::Value =
        toml::from_str(&read(&repo_root().join("Cargo.toml"))).expect("parse Cargo.toml");
    let members: Vec<String> = manifest["workspace"]["members"]
        .as_array()
        .expect("workspace.members array")
        .iter()
        .map(|m| m.as_str().expect("member path string").to_string())
        .collect();
    assert!(!members.is_empty(), "Cargo.toml lists no workspace members");
    members
}

/// Matches one gitignore pattern segment (`*` never crosses a `/`) against one
/// path segment.
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

/// Matches a slash-split gitignore pattern (`*` and `**`) against a
/// slash-split path.
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

/// True if an `ignored_paths` entry (gitignore syntax) would exclude `target`
/// or any ancestor directory of it. A slash anywhere but the end anchors the
/// pattern at the repo root; otherwise it matches at any depth (`src` behaves
/// like `**/src`).
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

/// Every `.rs` file beneath `dir`, as repo-relative slash-separated paths.
/// The whole member directory is walked, not only `src/`: `tests/`,
/// `benches/`, `examples/` and `manta-cli/build.rs` are first-party Rust the
/// index must see, and a file-selecting pattern (`*.rs`) shadows no directory.
fn rust_files_under(dir: &Path, out: &mut Vec<String>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files_under(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let rel = path
                .strip_prefix(repo_root())
                .expect("path is under the repo root");
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[test]
fn project_yml_is_committed_at_the_repo_root() {
    assert!(
        serena_dir().join("project.yml").is_file(),
        ".serena/project.yml must exist. Without it Serena does not fail: it writes a \
         config named after the checkout directory, with no ignores and read_only: false"
    );
}

#[test]
fn project_yml_spells_exactly_the_current_serena_fields() {
    let cfg = project_yml();
    for legacy in LEGACY_KEYS {
        assert!(
            !cfg.keys.iter().any(|k| k == legacy),
            "`{legacy}:` is a legacy key; Serena migrates it and re-saves the file"
        );
    }
    let mut seen = BTreeSet::new();
    for key in &cfg.keys {
        assert!(seen.insert(key.as_str()), "key {key:?} appears twice");
    }
    let want: BTreeSet<&str> = FIELDS.into_iter().collect();
    assert_eq!(
        seen, want,
        "project.yml's top-level keys must be exactly Serena's ProjectConfig fields: \
         a missing one makes Serena re-save the file and drop its comments"
    );
}

#[test]
fn project_yml_identifies_manta_with_the_classic_read_only_tools() {
    let cfg = project_yml();
    for (key, want) in [
        ("project_name", "manta"),
        // The tool names the catalyst-dev agents call exist only under the
        // `tools` interface; Serena 2.0.1 defaults new installs to REPL.
        ("agent_interface", "tools"),
        ("read_only", "true"),
        ("ignore_all_files_in_gitignore", "true"),
    ] {
        assert_eq!(cfg.scalar(key), want, "project.yml `{key}`");
    }
    let servers = cfg.list("language_servers");
    assert!(
        servers.iter().any(|s| s == "rust"),
        "language_servers must include rust, or symbol search covers no crate; got {servers:?}"
    );
}

#[test]
fn project_yml_runs_no_activation_command() {
    // Serena runs activation_command as a shell command whenever an agent
    // activates the project.
    let cfg = project_yml();
    assert_eq!(
        cfg.scalar("activation_command"),
        "",
        "activation_command must stay empty"
    );
    assert!(cfg.list("activation_command").is_empty());
}

#[test]
fn project_yml_ignores_what_gitignore_does_not_cover() {
    let cfg = project_yml();
    let ignored = cfg.list("ignored_paths");
    // Serena reads only files named `.gitignore`, never `.git/info/exclude`,
    // which is all that hides `.catalyst-cache` on some runners.
    for required in [
        ".serena/cache",
        "/target",
        "/.catalyst-cache",
        "/.catalyst",
        "/thoughts",
    ] {
        assert!(
            ignored.iter().any(|p| p == required),
            "ignored_paths must contain {required:?}; got {ignored:?}"
        );
    }
}

#[test]
fn no_ignored_path_hides_a_workspace_rust_file() {
    let cfg = project_yml();
    let ignored = cfg.list("ignored_paths");
    for member in workspace_members() {
        let src = format!("{member}/src");
        assert!(repo_root().join(&src).is_dir(), "{src} should exist");
        let mut sources = Vec::new();
        rust_files_under(&repo_root().join(&member), &mut sources);
        sources.sort();
        assert!(
            sources.iter().any(|s| s.starts_with(&format!("{src}/"))),
            "{src} holds no .rs file"
        );
        for source in &sources {
            for pattern in ignored {
                assert!(
                    !pattern_shadows(pattern, source),
                    "ignored_paths entry {pattern:?} hides workspace source file {source}"
                );
            }
        }
    }
}

#[test]
fn serena_gitignore_keeps_the_cache_and_local_override_out_of_git() {
    let nested = read(&serena_dir().join(".gitignore"));
    for line in ["/cache", "/project.local.yml"] {
        assert!(nested.lines().any(|l| l.trim() == line), "{nested}");
    }
}

#[test]
fn codebase_map_memory_names_every_workspace_crate() {
    // codebase-analyzer reads it with read_memory("codebase_map"): the memory
    // name has no `.md` even though the file does.
    let body = read(&serena_dir().join("memories").join("codebase_map.md"));
    for member in workspace_members() {
        let name = member.rsplit('/').next().expect("member has a name");
        assert!(
            body.contains(name),
            ".serena/memories/codebase_map.md does not mention {name}"
        );
    }
}

#[test]
fn initial_prompt_points_at_claude_md_reading_order() {
    // The prompt points at CLAUDE.md's list instead of copying it, so the two
    // cannot disagree about which documents to read or in what order.
    let cfg = project_yml();
    let prompt = cfg.scalar("initial_prompt").replace('\n', " ");
    assert!(
        prompt.contains("CLAUDE.md") && prompt.contains(&format!("\"{DOCUMENTS_HEADING}\"")),
        "initial_prompt must send readers to CLAUDE.md's \"{DOCUMENTS_HEADING}\" section: \
         {prompt}"
    );
    let claude_md = read(&repo_root().join("CLAUDE.md"));
    assert!(
        claude_md
            .lines()
            .any(|l| l.trim_end() == format!("## {DOCUMENTS_HEADING}")),
        "CLAUDE.md has no `## {DOCUMENTS_HEADING}` heading; update DOCUMENTS_HEADING here \
         and initial_prompt in .serena/project.yml to the new heading"
    );
}

#[test]
fn parser_strips_inline_comments_outside_quotes() {
    let cfg = parse_flat_yaml(concat!(
        "read_only: true # navigation only\n",
        "language_servers:\n",
        "- rust # language server\n",
        "project_name: \"man # ta\"\n",
        "double: \"a \\\" # b\" # trailing\n",
        "single: 'it''s # fine' # trailing\n",
    ));
    assert_eq!(cfg.scalar("read_only"), "true");
    assert_eq!(cfg.list("language_servers"), ["rust"]);
    assert_eq!(cfg.scalar("project_name"), "man # ta");
    assert_eq!(cfg.scalar("double"), "a \\\" # b");
    assert_eq!(cfg.scalar("single"), "it''s # fine");
}

#[test]
fn parser_keeps_block_scalars_and_records_repeated_keys() {
    let cfg = parse_flat_yaml("a: |\n  one # two\n  three\nb:\nc: x\nb:\n");
    assert_eq!(cfg.scalar("a"), "one # two\nthree\n");
    assert_eq!(cfg.keys, ["a", "b", "c", "b"]);
}

#[test]
fn pattern_shadows_catches_file_globs_and_spares_visible_sources() {
    let source = "crates/manta-decode/src/lib.rs";
    assert!(pattern_shadows("*.rs", source));
    assert!(pattern_shadows("crates/**/*.rs", source));
    assert!(pattern_shadows("src", source));
    assert!(pattern_shadows("/crates", source));
    assert!(pattern_shadows("build.rs", "crates/manta-cli/build.rs"));
    assert!(!pattern_shadows("*.rs", "crates/manta-decode/src"));
    assert!(!pattern_shadows("/target", source));
    assert!(!pattern_shadows("/src", source));
}
