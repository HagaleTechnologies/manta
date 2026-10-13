//! MAN-133: `manta completions <SHELL>` and `manta man-pages --out <DIR>`,
//! tested against the binary's own `--help` output rather than the
//! generators, so the assertions hold under every feature set. Real-shell
//! checks run where the shell is installed and skip with a notice where it
//! is not; the shell-free tests also run on Windows.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // MAN-261: inherited MANTA_* variables must never reach the child.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run(args: &[&str]) -> Output {
    manta().args(args).output().unwrap()
}

/// The long help of `manta <path...>`.
fn help(path: &[&str]) -> String {
    let out = manta().args(path).arg("--help").output().unwrap();
    assert!(out.status.success(), "`manta {path:?} --help` failed");
    String::from_utf8(out.stdout).unwrap()
}

/// The subcommands `manta <path...> --help` lists, `help` excluded. A leaf
/// command lists none.
fn commands(path: &[&str]) -> Vec<String> {
    let text = help(path);
    let Some(start) = text.find("Commands:\n") else {
        return Vec::new();
    };
    let rest = &text[start + "Commands:\n".len()..];
    let rest = &rest[..rest.find("\n\n").unwrap_or(rest.len())];
    rest.lines()
        .filter_map(|l| l.strip_prefix("  "))
        // A wrapped about-text continuation is indented to the description
        // column, so it still starts with spaces after the two-name indent.
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_string)
        .collect()
}

/// Every command path in the tree, root first, each parent before its
/// children (the order `manta.1`, `manta-run.1`, ... is derived from it).
fn walk() -> Vec<Vec<String>> {
    let mut paths = vec![Vec::new()];
    let mut i = 0;
    while i < paths.len() {
        let current = paths[i].clone();
        let refs: Vec<&str> = current.iter().map(String::as_str).collect();
        for name in commands(&refs) {
            let mut child = current.clone();
            child.push(name);
            paths.push(child);
        }
        i += 1;
    }
    paths
}

/// The long flags `manta <path...> --help` defines on option-definition
/// lines (`  -d, --device <DEVICE>` / `      --device <DEVICE>`), which is
/// every heading's format in long help.
fn flags(path: &[&str]) -> BTreeSet<String> {
    let re = regex::Regex::new(r"^\s{2,}(?:-[A-Za-z], )?(--[a-z][a-z0-9-]*)").unwrap();
    help(path)
        .lines()
        .filter_map(|l| re.captures(l))
        .map(|c| c[1].to_string())
        .collect()
}

/// The shells `manta completions` accepts, from its own help.
fn shells() -> Vec<String> {
    let completions_help = help(&["completions"]);
    let line = completions_help
        .lines()
        .find(|l| l.contains("[possible values: "))
        .expect("completions --help lists no possible values");
    line.split("[possible values: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(']')
        .split(", ")
        .map(str::to_string)
        .collect()
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
        .unwrap_or(false)
}

/// Runs `manta man-pages --out dir` and returns the directory.
fn write_man_pages() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["man-pages", "--out"])
        .arg(dir.path().join("man1"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "man-pages failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

// ---- `manta completions <SHELL>` ----

#[test]
fn completions_lists_exactly_five_shells() {
    assert_eq!(shells(), ["bash", "elvish", "fish", "powershell", "zsh"]);
}

#[test]
fn completions_prints_a_script_for_every_shell() {
    for shell in shells() {
        let a = run(&["completions", &shell]);
        assert!(a.status.success(), "completions {shell} failed");
        assert!(!a.stdout.is_empty(), "completions {shell} printed nothing");
        assert!(a.stderr.is_empty(), "completions {shell} wrote stderr");
        let b = run(&["completions", &shell]);
        assert_eq!(
            a.stdout, b.stdout,
            "completions {shell} is not deterministic"
        );
    }
}

#[test]
fn completions_rejects_an_unknown_shell() {
    let out = run(&["completions", "tcsh"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("possible values"), "{stderr}");
    assert!(stderr.contains("zsh"), "{stderr}");
}

#[test]
fn completions_requires_a_shell() {
    let out = run(&["completions"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("<SHELL>"),
        "the usage error does not name <SHELL>"
    );
}

#[cfg(unix)]
#[test]
fn bash_completion_offers_every_command_and_flag() {
    if !on_path("bash") {
        eprintln!("skipping: bash is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("manta.bash");
    let script = script.to_str().unwrap().to_string();
    let out = run(&["completions", "bash"]);
    assert!(out.status.success());
    std::fs::write(&script, &out.stdout).unwrap();

    // Drives the script's _manta directly: the last word is the one being
    // completed, the second-to-last is what bash would pass as `prev`.
    let complete = |words: &[String]| -> Vec<String> {
        let prog = format!(
            "source \"{script}\"\n\
             COMP_WORDS=(\"$@\"); COMP_CWORD=$(( $# - 1 )); COMP_LINE=\"$*\"; \
             COMP_POINT=${{#COMP_LINE}}\n\
             _manta manta \"${{COMP_WORDS[COMP_CWORD]}}\" \"${{COMP_WORDS[COMP_CWORD-1]}}\"\n\
             printf '%s\\n' \"${{COMPREPLY[@]}}\""
        );
        let out = Command::new("bash")
            .args(["--norc", "--noprofile", "-c", &prog])
            .arg("bash")
            .args(words)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "bash probe {:?} failed: {}",
            words,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    };

    for path in walk() {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        let mut prefix: Vec<String> = vec!["manta".into()];
        prefix.extend(path.iter().cloned());

        let mut words = prefix.clone();
        words.push("--".into());
        let got: BTreeSet<String> = complete(&words).into_iter().collect();
        for flag in flags(&refs) {
            assert!(
                got.contains(&flag),
                "completing `manta {}--` does not offer {flag}; got {:?}",
                path.iter().map(|p| format!("{p} ")).collect::<String>(),
                got
            );
        }

        let subs = commands(&refs);
        if !subs.is_empty() {
            let mut words = prefix.clone();
            words.push(String::new());
            let got: BTreeSet<String> = complete(&words).into_iter().collect();
            for sub in &subs {
                assert!(
                    got.contains(sub),
                    "completing `manta {}` does not offer {sub}; got {:?}",
                    path.iter().map(|p| format!("{p} ")).collect::<String>(),
                    got
                );
            }
        }
    }

    // The visible alias `listen` completes; the hidden `--server-config`
    // alias (clap_complete writes visible aliases only) does not.
    let root: BTreeSet<String> = complete(&["manta".into(), String::new()])
        .into_iter()
        .collect();
    assert!(root.contains("listen"), "no `listen` alias: {root:?}");
    let run_flags: BTreeSet<String> = complete(&["manta".into(), "run".into(), "--".into()])
        .into_iter()
        .collect();
    assert!(
        !run_flags.contains("--server-config"),
        "the hidden --server-config alias is offered: {run_flags:?}"
    );
    let levels: BTreeSet<String> = complete(&[
        "manta".into(),
        "run".into(),
        "--log-level".into(),
        String::new(),
    ])
    .into_iter()
    .collect();
    let want: BTreeSet<String> = ["off", "error", "warn", "info", "debug", "trace"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(levels, want, "completing --log-level's value");
}

#[cfg(unix)]
#[test]
fn zsh_completion_script_parses_and_registers() {
    if !on_path("zsh") {
        eprintln!("skipping: zsh is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("_manta");
    let script = script.to_str().unwrap().to_string();
    let out = run(&["completions", "zsh"]);
    assert!(out.status.success());
    std::fs::write(&script, &out.stdout).unwrap();

    let syntax = Command::new("zsh").arg("-n").arg(&script).output().unwrap();
    assert!(
        syntax.status.success(),
        "zsh -n rejected the script: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    // zsh's completion widgets only run inside an interactive line editor,
    // so the interactive check is manual (docs/DECISIONS/-man133). Here the
    // script only has to load and register the _manta widget.
    let prog = format!(
        "autoload -Uz compinit && compinit -u -D; eval \"$(cat {script})\"; \
         print -r -- ${{_comps[manta]}}"
    );
    let out = Command::new("zsh")
        .args(["-f", "-c", &prog])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "loading the zsh script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "_manta");
}

#[cfg(unix)]
#[test]
fn fish_completion_offers_every_command_and_flag() {
    if !on_path("fish") {
        eprintln!("skipping: fish is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("manta.fish");
    let script = script.to_str().unwrap().to_string();
    let out = run(&["completions", "fish"]);
    assert!(out.status.success());
    std::fs::write(&script, &out.stdout).unwrap();

    let probe = |line: &str| -> String {
        let prog = format!("source {script}; complete -C \"{line}\"");
        let out = Command::new("fish")
            .args(["--no-config", "-c", &prog])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fish probe `{line}` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let root = probe("manta ");
    for name in commands(&[]) {
        assert!(
            root.lines()
                .any(|l| l.split('\t').next() == Some(name.as_str())),
            "fish does not offer `{name}` for `manta `:\n{root}"
        );
    }
    let kiwi = probe("manta run --kiwi");
    for flag in flags(&["run"]) {
        if flag.starts_with("--kiwi") {
            assert!(
                kiwi.lines()
                    .any(|l| l.split('\t').next() == Some(flag.as_str())),
                "fish does not offer {flag} for `manta run --kiwi`:\n{kiwi}"
            );
        }
    }
}

// ---- `manta man-pages --out <DIR>` ----

#[test]
fn man_pages_writes_one_page_per_command() {
    let dir = write_man_pages();
    let out_dir = dir.path().join("man1");
    let out = manta()
        .args(["man-pages", "--out"])
        .arg(&out_dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "man-pages wrote to stdout");

    let mut expected: Vec<String> = vec!["manta.1".into()];
    for path in walk() {
        if !path.is_empty() {
            expected.push(format!("manta-{}.1", path.join("-")));
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        stderr,
        format!(
            "wrote {} man pages to {}\n",
            expected.len(),
            out_dir.display()
        ),
        "stderr must be exactly the one confirmation line (MAN-270)"
    );
    let mut names: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn every_man_page_documents_every_flag_its_help_defines() {
    let dir = write_man_pages();
    let out_dir = dir.path().join("man1");
    for path in walk() {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        let name = if path.is_empty() {
            "manta.1".to_string()
        } else {
            format!("manta-{}.1", path.join("-"))
        };
        let page = std::fs::read_to_string(out_dir.join(&name))
            .unwrap_or_else(|e| panic!("reading {name}: {e}"));
        for flag in flags(&refs) {
            // roff escapes `-` as `\-`.
            let escaped = flag.replace('-', "\\-");
            assert!(
                page.contains(&escaped),
                "{name} does not document {flag} from `manta {refs:?} --help`"
            );
        }
    }
}

#[test]
fn man_page_title_lines_carry_the_release_version() {
    let version_line = String::from_utf8_lossy(&run(&["--version"]).stdout)
        .trim()
        .to_string();
    let version = version_line.split_whitespace().nth(1).unwrap();
    let after = version_line.strip_prefix("manta ").unwrap();

    let dir = write_man_pages();
    let out_dir = dir.path().join("man1");
    let mut names: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let page = std::fs::read_to_string(out_dir.join(&name)).unwrap();
        let stem = name.trim_end_matches(".1");
        let th = page
            .lines()
            .find(|l| l.starts_with(".TH "))
            .unwrap_or_else(|| panic!("{name} has no .TH line"));
        assert_eq!(
            th,
            format!(".TH {stem} 1 \"\" \"manta {version}\" \"User Commands\""),
            "{name}"
        );
    }
    let root = std::fs::read_to_string(out_dir.join("manta.1")).unwrap();
    let v = root
        .find(".SH VERSION")
        .expect("manta.1 has no VERSION section");
    assert!(
        root[v..].contains(after),
        "manta.1's VERSION section lacks `{after}` (from `manta --version`)"
    );
}

#[test]
fn man_pages_render_without_errors() {
    let dir = write_man_pages();
    let out_dir = dir.path().join("man1");
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    pages.sort();
    if on_path("mandoc") {
        let mut cmd = Command::new("mandoc");
        cmd.args(["-T", "lint", "-W", "error"]).args(&pages);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "mandoc -T lint -W error failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if on_path("groff") {
        let out = Command::new("groff")
            .args(["-man", "-Tutf8", "-ww", "-z"])
            .args(&pages)
            .output()
            .unwrap();
        assert!(
            out.stderr.is_empty(),
            "groff warned: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if !on_path("mandoc") && !on_path("groff") {
        eprintln!("skipping: neither mandoc nor groff is on PATH");
    }
}

#[test]
fn man_pages_replaces_old_pages_and_keeps_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("man1");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("manta.1"), "junk from an older copy").unwrap();
    std::fs::write(out_dir.join("notes.txt"), "operator notes").unwrap();
    let out = manta()
        .args(["man-pages", "--out"])
        .arg(&out_dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    let page = std::fs::read_to_string(out_dir.join("manta.1")).unwrap();
    assert!(
        page.starts_with(".ie"),
        "manta.1 was not replaced:\n{}",
        &page[..page.len().min(200)]
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("notes.txt")).unwrap(),
        "operator notes"
    );
}

#[test]
fn man_pages_rejects_a_file_as_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    let out = manta()
        .args(["man-pages", "--out"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("error: {} is not a directory\n", file.display())
    );
}

#[test]
fn man_pages_requires_out() {
    let out = run(&["man-pages"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--out <DIR>"),
        "the usage error does not name --out <DIR>"
    );
}
