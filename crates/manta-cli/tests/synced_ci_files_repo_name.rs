//! MAN-25: the fleet-synced CI/merge-config files must never carry the
//! pre-rename project name.
//!
//! These files are ported by hand from an upstream template (see
//! `.github/workflows/wait-for-codex.yml`'s own note: "this repo's copy has no
//! auto-sync, so port future template fixes here manually too"). A future port
//! that pastes a block predating the rename would silently reintroduce a repo
//! name that no longer exists. This test is the tripwire for that; it runs in
//! the required `cargo test --workspace` leg and deliberately lives outside the
//! synced files themselves, so the guard adds no drift against the template.
//!
//! `.mergify.yml` was a third guarded file until the native-merge-queue cutover
//! (#185) deleted it; the two workflow files below are what remains. Its absence
//! is itself asserted by `retired_mergify_config_is_not_resurrected` at the
//! bottom of this file -- prose alone said so three times and merge tooling still
//! went looking for the path, so the invariant is executable here instead.
//!
//! Scope is those files only -- the generic category phrase "CW skimmer"
//! stays legal everywhere else (see docs/DECISIONS/2026-09-01-rename-to-manta.md).

use std::path::{Path, PathBuf};

/// Workspace root, derived from this crate's manifest dir (`crates/manta-cli`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/manta-cli sits two levels below the workspace root")
        .to_path_buf()
}

const SYNCED_FILES: [&str; 2] = [
    ".github/workflows/ci.yml",
    ".github/workflows/wait-for-codex.yml",
];

/// The pre-rename project name. See docs/DECISIONS/2026-09-01-rename-to-manta.md.
const OLD_PROJECT_NAME: &str = "skimmer";

#[test]
fn synced_ci_files_never_carry_the_pre_rename_project_name() {
    let root = repo_root();
    let mut offenders = Vec::new();

    for rel in SYNCED_FILES {
        let path = root.join(rel);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{rel}: could not read {} ({err})", path.display()));
        for (idx, line) in body.lines().enumerate() {
            if line.to_ascii_lowercase().contains(OLD_PROJECT_NAME) {
                offenders.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "fleet-synced CI config still names the pre-rename project -- a manual \
         port from the upstream template most likely reintroduced it. Fix it here AND \
         upstream (see docs/DECISIONS/2026-09-01-rename-to-manta.md):\n{}",
        offenders.join("\n")
    );
}

/// The retired Mergify config, asserted absent rather than merely described as
/// absent.
///
/// `.mergify.yml` governed this repo's merges until the native-merge-queue
/// cutover (#185, 2026-09-11) deleted it. Its replacement is
/// `.github/workflows/auto-merge-trigger.yml` (admission / trusted-author
/// boundary) plus GitHub's native `merge_queue` ruleset rule on
/// `main-protection` (the merge itself). Nothing reads `.mergify.yml` any more,
/// and MAN-25's merge phase failed repeatedly by opening that path anyway
/// (`could not read .mergify.yml`, ENOENT) after following a stale prose
/// pointer -- so the "it is gone, do not recreate it, do not go read it" rule
/// that `CLAUDE.md` states in words is pinned here in a form CI can enforce.
///
/// If Mergify is ever deliberately reinstated, delete this test in the same PR
/// that adds the file back, and amend
/// `docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md` -- do not silence it.
#[test]
fn retired_mergify_config_is_not_resurrected() {
    let path = repo_root().join(".mergify.yml");
    assert!(
        !path.exists(),
        "{} exists. Mergify was retired from this repo by the native-merge-queue \
         cutover (#185); merge policy now lives in \
         .github/workflows/auto-merge-trigger.yml plus the main-protection \
         merge_queue ruleset rule (see \
         docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md, 2026-09-11 \
         amendment). Reinstating this file needs that ADR amended and this test \
         removed in the same change.",
        path.display()
    );
}

/// The replacement the test above points at must actually be there.
///
/// A bare "`.mergify.yml` must not exist" assertion is satisfied just as well by
/// a repo with no merge automation at all, which is the failure mode that would
/// send the next reader back to looking for a Mergify config. Pinning the
/// successor's existence makes the pair say "merge policy moved here", not
/// merely "merge policy left".
#[test]
fn merge_policy_successor_to_mergify_exists() {
    let rel = ".github/workflows/auto-merge-trigger.yml";
    let path = repo_root().join(rel);
    let body = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{rel}: could not read {} ({err}) -- this workflow is what replaced \
             the retired .mergify.yml as the trusted-author admission boundary \
             (#185). If it moved, update this test and \
             docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md together.",
            path.display()
        )
    });
    // The shell a step actually executes, and the command that shell actually
    // runs -- not the prose about it, not another scalar that happens to quote
    // it, and not a string that merely contains its words. This workflow's
    // header comment says `gh pr merge --auto` too, so a bare `body.contains(..)`
    // stayed green even with the arming step deleted (PR #93 review); a
    // comment-stripping scan of *every* line would still be satisfied by a step
    // `name:` or an `env:` value carrying the same text (PR #93 review, round
    // 2); scanning `run:` shell for two independent substrings would still be
    // satisfied by `echo "gh pr merge --auto"` or `CMD="gh pr merge --auto"`,
    // which invoke nothing (PR #93 review, round 3); and taking *every* `run:`
    // key as shell would still be satisfied by a mapping merely named `run`
    // (an `env:` child, a `with:` input, `jobs.<id>.defaults.run`), which
    // executes nothing either (PR #93 review, round 4).
    assert!(
        arms_auto_merge(&body),
        "{rel} no longer arms auto-merge: no `run:` shell in it *invokes* \
         `gh pr merge ... --auto` as a command (a header comment, a \
         `name:`/`env:` scalar, an `echo` of the command text or an assignment \
         of it to a variable all fail this check, as does naming it only inside \
         a conditional -- see `arms_auto_merge`). That call IS the post-#185 \
         admission boundary that replaced .mergify.yml's pull_request_rules; \
         losing it silently leaves the repo with no automated merge path at all."
    );
}

/// Does `body` -- a GitHub Actions workflow -- actually *execute* `gh pr merge
/// ... --auto` from a step's `run:` shell?
///
/// Two conditions, both necessary:
///
/// 1. **Position in the file.** Only a *step's own* `run:` content counts. A
///    header comment, a step `name:`, an `env:` value or a `with:` input can
///    all carry the exact command text while the workflow does nothing at all
///    -- and so can a mapping whose key happens to be `run` but which is not a
///    step's `run:` field (`jobs.<id>.defaults.run`, an `env:` child called
///    `run`). See `run_shell_lines`.
/// 2. **Position in the command.** Within that shell, `gh` must be the *command
///    word* of a simple command, followed by the `pr merge` subcommand path,
///    with `--auto` as one of its own argument tokens. Substring matching is not
///    enough: `echo "gh pr merge --auto"`, `CMD="gh pr merge --auto"` and
///    `# gh pr merge --auto` all contain both fragments and arm nothing (PR #93
///    review, round 3).
///
/// Tokens are compared after quote removal, so `"gh" pr merge --auto` counts
/// while `echo "gh pr merge --auto"` does not -- in the latter the whole
/// command text is a single argument token of `echo`.
///
/// Deliberately strict in one more direction: a command word is taken as-is,
/// so `gh` reached through a shell keyword (`if ...; then gh pr merge --auto;
/// fi`) is NOT recognised. That is the safe error -- it fails the guard loudly
/// and asks a human to look, rather than certifying a merge path that a
/// statically-dead branch could have turned off.
fn arms_auto_merge(body: &str) -> bool {
    simple_commands(&run_shell(body))
        .iter()
        .any(|command| is_gh_auto_merge(command))
}

/// Is `command` -- one simple command, already tokenised and unquoted -- an
/// invocation of `gh pr merge ... --auto`?
fn is_gh_auto_merge(command: &[String]) -> bool {
    let Some(argv0) = command.first() else {
        return false;
    };
    // `gh`, or any path ending in it (`/usr/bin/gh`).
    let program = argv0.rsplit('/').next().unwrap_or(argv0);
    if program != "gh" {
        return false;
    }
    let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
    args.first() == Some(&"pr") && args.get(1) == Some(&"merge") && args[2..].contains(&"--auto")
}

/// The `run:` shell of `body`, comment-stripped and with backslash-newline
/// continuations rejoined, as one string.
fn run_shell(body: &str) -> String {
    let mut shell = String::new();
    for line in run_shell_lines(body) {
        shell.push_str(executable_part(line));
        shell.push('\n');
    }
    shell
}

/// Split shell source into simple commands, each a list of unquoted tokens.
///
/// A deliberately small shell reader rather than a dependency: word splitting on
/// unquoted whitespace, single/double quote removal, backslash escapes
/// (including line continuation), and command boundaries at unquoted `;`, `&`,
/// `|` and newline. Constructs it does not model (`$(...)`, `{ ...; }`) are left
/// in whatever token they appear in, which can only ever make a match *harder* to
/// achieve -- the direction this guard needs to err in.
fn simple_commands(shell: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut command: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = shell.chars().peekable();

    // Close off the token being built, if any.
    macro_rules! end_token {
        () => {
            if started {
                command.push(std::mem::take(&mut token));
                started = false;
            }
        };
    }

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    // Inside double quotes a backslash only escapes a few
                    // characters; passing the next one through verbatim is close
                    // enough for a tripwire and never invents a token.
                    if let Some(next) = chars.next() {
                        token.push(next);
                    }
                } else {
                    token.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '\\' => match chars.next() {
                    // Line continuation: the newline disappears and the command
                    // continues, so `gh pr merge \<newline>  --auto` is one
                    // command.
                    Some('\n') | None => {}
                    Some(next) => {
                        token.push(next);
                        started = true;
                    }
                },
                ';' | '&' | '|' | '\n' => {
                    end_token!();
                    if !command.is_empty() {
                        commands.push(std::mem::take(&mut command));
                    }
                }
                c if c.is_whitespace() => end_token!(),
                c => {
                    token.push(c);
                    started = true;
                }
            },
        }
    }

    // Not `end_token!()`: this is the last use of `token`/`started`, and the
    // macro's reset of `started` would be flagged as a dead assignment under
    // `-D warnings`.
    if started {
        command.push(token);
    }
    if !command.is_empty() {
        commands.push(command);
    }
    commands
}

/// The shell lines of every *step's* `run:` in `body`, in file order.
///
/// A deliberately small YAML reader rather than a dependency. Two things have
/// to be right for a line to count as executable shell:
///
/// 1. **The key must be a step's own `run:`** -- a key at the column a step's
///    keys sit at, inside the `steps:` sequence of a job. A mapping named `run`
///    anywhere else (`jobs.<id>.defaults.run`, an `env:` child literally called
///    `run`, a `with:` input of that name) carries no shell at all; matching
///    every `run:` key by prefix let such a value satisfy the guard while the
///    real arming step was gone (PR #93 review, round 4).
/// 2. **The value must be one of the two spellings Actions allows** -- an
///    inline scalar (`run: cmd`) or a block scalar (`run: |` / `run: >`, whose
///    body is every following line indented deeper than its first content
///    line).
///
/// Block scalars opened by *other* keys are tracked too, and their content
/// discarded, so a `description: |` paragraph can never be re-read as keys.
/// Anything else this reader cannot recognise is simply not returned, so an
/// unhandled spelling makes the guard fail loudly rather than pass vacuously;
/// that is the direction this guard needs to err in.
fn run_shell_lines(body: &str) -> Vec<&str> {
    let mut shell = Vec::new();
    // `Some((None, keep))` = inside a block scalar whose content indentation is
    // not pinned yet (no non-blank line seen); `Some((Some(n), keep))` = pinned
    // at column `n`; `None` = not inside one. `keep` is true only for a block
    // opened by a step's own `run:` -- every other block is consumed and
    // dropped.
    let mut block: Option<(Option<usize>, bool)> = None;
    // The indent of the `steps:` key currently in effect, and the column at
    // which the current step's own keys sit. A `run:` counts only at that
    // column.
    let mut steps_indent: Option<usize> = None;
    let mut step_key_indent: Option<usize> = None;

    for line in body.lines() {
        let indent = line.len() - line.trim_start().len();

        if let Some((content_indent, keep)) = block {
            // A block scalar's indentation is set by its first non-blank line
            // and it ends at the first non-blank line indented less than that.
            // Sibling keys of `run:` sit one level shallower, so this is what
            // keeps a following `name:`/`env:` out of the shell.
            if line.trim().is_empty() {
                if keep {
                    shell.push(line);
                }
                continue;
            }
            match content_indent {
                None => {
                    block = Some((Some(indent), keep));
                    if keep {
                        shell.push(line);
                    }
                    continue;
                }
                Some(base) if indent >= base => {
                    if keep {
                        shell.push(line);
                    }
                    continue;
                }
                Some(_) => block = None,
            }
        }

        let trimmed = line.trim_start();
        // Blank lines and YAML comments carry no key and must not disturb the
        // step context tracked below.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A step is a sequence item, so its first key arrives as `- run: ...`,
        // two columns right of the dash. Items may be indented deeper than
        // their `steps:` key or sit at the same column as it; both spellings
        // are legal YAML.
        let item = trimmed.starts_with("- ");
        if steps_indent.is_some_and(|base| indent < base || (indent == base && !item)) {
            // Dedented out of the `steps:` sequence entirely.
            steps_indent = None;
            step_key_indent = None;
        }

        let (key, key_indent) = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                let key_indent = indent + 2;
                // A new item directly under `steps:` starts a new step; a
                // sequence item anywhere else is not a step and has no `run:`.
                step_key_indent = steps_indent.map(|_| key_indent);
                (rest, key_indent)
            }
            None => (trimmed, indent),
        };

        if key
            .strip_prefix("steps:")
            .is_some_and(|rest| rest.trim().is_empty())
        {
            steps_indent = Some(key_indent);
            step_key_indent = None;
            continue;
        }

        let Some((name, value)) = key.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let is_step_run = name == "run" && Some(key_indent) == step_key_indent;
        if value.starts_with('|') || value.starts_with('>') {
            block = Some((None, is_step_run));
        } else if is_step_run && !value.is_empty() {
            shell.push(value);
        }
    }

    shell
}

/// The executable part of `line` -- everything before its first `#`.
///
/// Shell comments inside a `run:` block start with `#`, so this drops them.
/// Cutting unconditionally can only ever discard *more* than a real comment
/// (a `#` inside a quoted string, say), which fails loudly rather than passing
/// vacuously -- again the safe direction here.
fn executable_part(line: &str) -> &str {
    let line = line.trim();
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// `arms_auto_merge` must read `run:` shell and only `run:` shell -- the
/// property PR #93's review asked for, pinned on synthetic workflows so it
/// holds independently of what the real file happens to look like today.
#[test]
fn arms_auto_merge_counts_run_shell_only() {
    // Both spellings of an executing step.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto --squash\n"
    ));
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - name: Enable auto-merge\n        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    ));

    // The command named anywhere that does not execute it.
    assert!(!arms_auto_merge(
        "# calls gh pr merge --auto per PR\non: push\n"
    ));
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - name: gh pr merge --auto\n        uses: actions/checkout@v4\n"
    ));
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - env:\n          CMD: gh pr merge --auto --squash\n        uses: actions/checkout@v4\n"
    ));
    // A shell comment inside a real `run:` block is still prose.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: |\n          # gh pr merge \"$PR_URL\" --auto --squash\n          echo skipped\n"
    ));
    // The block scalar ends where indentation returns to the key's level.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: |\n          echo hi\n        name: gh pr merge --auto\n"
    ));
}

/// A key spelled `run` is only shell when it is a *step's* `run:` field (PR #93
/// review, round 4: any `run:` key at any depth was being read as shell, so an
/// `env:` child named `run` kept the guard green with the arming step deleted).
#[test]
fn arms_auto_merge_ignores_run_keys_that_are_not_step_fields() {
    // An `env:` child literally named `run`, inline and as a block scalar.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - env:\n          run: gh pr merge \"$PR_URL\" --auto --squash\n        uses: actions/checkout@v4\n"
    ));
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - env:\n          run: |\n            gh pr merge \"$PR_URL\" --auto --squash\n        uses: actions/checkout@v4\n"
    ));
    // A `with:` input of that name, handed to an action that may ignore it.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - uses: actions/github-script@v7\n        with:\n          run: gh pr merge \"$PR_URL\" --auto --squash\n"
    ));
    // `jobs.<id>.defaults.run`, which configures a shell rather than running one.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    defaults:\n      run: gh pr merge \"$PR_URL\" --auto --squash\n    steps:\n      - uses: actions/checkout@v4\n"
    ));
    // Not a step at all: a `run:` under some other sequence.
    assert!(!arms_auto_merge(
        "on:\n  workflow_call:\nx:\n  - run: gh pr merge \"$PR_URL\" --auto --squash\n"
    ));

    // A real step still counts with its items at the `steps:` key's own column,
    // the other legal YAML sequence indentation.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n    - env:\n        PR_URL: x\n      run: gh pr merge \"$PR_URL\" --auto --squash\n"
    ));
}

/// Inside `run:` shell, the command text must be *the command* -- not an
/// argument of some other one, and not a string assigned to a variable (PR #93
/// review, round 3: two independent substring matches were treated as
/// execution).
#[test]
fn arms_auto_merge_requires_an_actual_invocation() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // Named as data, executed by something else (or by nothing at all).
    assert!(!arms_auto_merge(&workflow(
        "          echo \"gh pr merge --auto --squash\"\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          CMD=\"gh pr merge $PR_URL --auto --squash\"\n          echo \"$CMD\"\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          printf '%s\\n' 'gh pr merge --auto'\n"
    )));
    // Reachable only through a shell keyword: not recognised on purpose, so the
    // guard fails loudly rather than certifying a branch that may be dead.
    assert!(!arms_auto_merge(&workflow(
        "          if false; then gh pr merge \"$PR_URL\" --auto; fi\n"
    )));
    // Right words, wrong command: `gh` must head the command and `pr merge`
    // must be its subcommand path, with `--auto` its own token.
    assert!(!arms_auto_merge(&workflow(
        "          gh pr comment \"$PR_URL\" --body 'gh pr merge --auto'\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --squash\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          gh pr merge --auto-x\n"
    )));

    // Real invocations, in the spellings a workflow plausibly uses.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" \\\n            --auto --squash\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          echo arming && gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge --auto --squash \"$PR_URL\"; echo done\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          /usr/bin/gh pr merge \"$PR_URL\" --auto\n"
    )));
}
