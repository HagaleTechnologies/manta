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

use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;

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
    // which invoke nothing (PR #93 review, round 3); taking *every* `run:` key
    // as shell would still be satisfied by a mapping merely named `run` (an
    // `env:` child, a `with:` input, `jobs.<id>.defaults.run`), which executes
    // nothing either (PR #93 review, round 4); reading a *folded* `run: >`
    // block as though it were a literal `run: |` one would still be satisfied
    // by a block YAML folds into a single `echo` command, with `gh` and its
    // flags demoted to words of that `echo` (PR #93 review, round 5); and
    // treating every newline and `&&` as an unconditional command boundary
    // would still be satisfied by shell text the shell never executes -- a
    // here-document body handed to `cat` on stdin, or a branch behind
    // `false &&` (PR #93 review, round 6); and tracking only the five
    // compound-command keywords would still be satisfied by the body of a
    // shell function that nothing ever calls (PR #93 review, round 7); and
    // letting every boundary start a *reachable* command would still be
    // satisfied by leftover text below an unconditional `exit 0`, which the
    // shell is already gone before it reads (PR #93 review, round 8); and
    // reading quotes off a word before balancing braces, or treating every
    // block body as skippable, would still be satisfied by a `gh pr merge`
    // an `echo "}"` had falsely freed from its uncalled function, or by one
    // written below `if true; then exit 0; fi` (PR #93 review, round 9); and
    // counting only a *standalone* opening brace, or reading a settled block
    // only in its `true`/`false` spelling, would still be satisfied by a call
    // a nested group's `}` had freed from an `arm(){ ... }` nothing calls, or
    // by one written below `if :; then exit 0; fi` or
    // `for f in a b; do exit 0; done` (PR #93 review, round 10); and reading a
    // brace off a word without asking where in the command it stands, or
    // applying a settled `if`'s verdict to every one of its arms, would still
    // be satisfied by a call a bare `echo }` had freed from that same uncalled
    // function, or by one written in the `else` of `if true` -- an arm the
    // shell never executes (PR #93 review, round 11); and closing a
    // here-document at a delimiter line carrying trailing blanks, reading a
    // `break` as an ordinary command, or resetting reachability at every
    // newline with no account of `bash -e` would still be satisfied by a
    // here-document body `EOF   ` had falsely freed, by a call written under a
    // `break` in `while true; do ...; done`, or by one written under a bare
    // `false` (PR #93 review, round 12); and counting a redirection as an
    // argument of the command it is written on, or reading every `run:` block
    // as bash whatever interpreter its own step names, would still be satisfied
    // by a call written under `false >/dev/null` -- which `bash -e` exits on
    // exactly as it does a bare `false` -- or by one written in a
    // `shell: python` step, whose Python raises before `gh` is ever reached
    // (PR #93 review, round 14); and reading that redirection only where a
    // space sets it apart from the command would still be satisfied by a call
    // written under `false>/dev/null`, which bash splits at the operator and
    // runs as the same builtin (PR #93 review, round 14, second reading); and
    // reading a step's `run:` without asking whether Actions runs the step at
    // all would still be satisfied by a call inside a step -- or a job -- that
    // an `if: false` parks, whose shell is never handed to a shell (PR #93
    // review, round 15); and splitting a command at every unquoted `&`, or
    // reading a pipeline's status off its last member alone, would still be
    // satisfied by a call written under `false &>/dev/null` -- whose `&` is a
    // redirection, not the background operator that would spare that `false`
    // from errexit -- or under the `false | true` an explicit `shell: bash`
    // step runs with `-o pipefail` (PR #93 review, round 16); and reading the
    // command word as the program it names, without asking whether the shell
    // has a *function* by that name, would still be satisfied by a step that
    // writes `gh() { echo disabled; }` and then calls `gh pr merge "$PR_URL"
    // --auto` -- bash searches its function table before `$PATH`, so that call
    // reaches the stub and merges nothing (PR #93 review, round 17).
    //
    // The reachability rules those rounds built up ask whether the shell *can*
    // reach the call, not whether it is guaranteed to: this workflow arms from
    // inside an `if`/`else` that stands down while a live `needs-human` label
    // covers the current head, and reading "may be skipped" as "never runs"
    // made this guard fail on the real, working file.
    assert!(
        arms_auto_merge(&body),
        "{rel} no longer arms auto-merge: no `run:` shell in it *invokes* \
         `gh pr merge ... --auto` as a command (a header comment, a \
         `name:`/`env:` scalar, an `echo` of the command text, an assignment \
         of it to a variable or a here-document body all fail this check, as \
         does naming it only behind a constant gate (`false &&`, the body of \
         `if false`), in the body of a function nothing calls, below an \
         `exit`, below a `break` in the same loop body, or below a bare \
         `false` the step's own `bash -e` exits on -- in any spelling of its \
         redirections, `false &>/dev/null` included -- or below a pipeline \
         `pipefail` fails in an explicit `shell: bash` step, or writing it in a step \
         (or job) a statically false `if:` parks, or shadowing the name with a \
         shell function of its own (`gh() {{ echo disabled; }}`) -- see \
         `arms_auto_merge`). \
         That call IS the post-#185 \
         admission boundary that replaced .mergify.yml's pull_request_rules; \
         losing it silently leaves the repo with no automated merge path at all."
    );
}

/// Does `body` -- a GitHub Actions workflow -- actually *execute* `gh pr merge
/// ... --auto` from a step's `run:` shell?
///
/// Six conditions, all necessary:
///
/// 1. **Position in the file.** Only a *step's own* `run:` content counts. A
///    header comment, a step `name:`, an `env:` value or a `with:` input can
///    all carry the exact command text while the workflow does nothing at all
///    -- and so can a mapping whose key happens to be `run` but which is not a
///    step's `run:` field (`jobs.<id>.defaults.run`, an `env:` child called
///    `run`). See `run_shell_lines`.
/// 2. **Line structure as YAML defines it.** A literal block (`run: |`) hands
///    the shell every line break; a *folded* block (`run: >`) folds the break
///    between two plain lines into a single space. So `echo disabled` followed
///    by `gh pr merge ... --auto` is two commands under `|` and *one* `echo`
///    command under `>`. Reading the second as the first would certify a
///    workflow that invokes nothing (PR #93 review, round 5). See
///    `folded_run_lines`.
/// 3. **Position in the command.** Within that shell, `gh` must be the *command
///    word* of a simple command, followed by the `pr merge` subcommand path,
///    with `--auto` as one of its own argument tokens. Substring matching is not
///    enough: `echo "gh pr merge --auto"`, `CMD="gh pr merge --auto"` and
///    `# gh pr merge --auto` all contain both fragments and arm nothing (PR #93
///    review, round 3).
///
/// 4. **Reachability.** A command boundary is not the same thing as a command
///    the shell can reach. Lines inside a here-document (`cat <<EOF ... EOF`)
///    are *data* on another command's stdin and are never executed at all; a
///    command behind a constant gate (`false && ...`, `true || ...`, the body
///    of `if false`) is a branch the shell can never take; a command in a
///    function body runs only when something calls that function, which a
///    `run:` block need never do; and a command written below an `exit` the
///    shell actually reaches never runs at all, because there is no shell left
///    to read it. Treating every newline and `&&` as an unconditional boundary
///    let the first two certify a workflow whose real arming call had been
///    deleted (PR #93 review, round 6), tracking only the block keywords let an
///    uncalled `arm() { ... }` definition do the same (round 7), and resetting
///    reachability at every boundary let `exit 0` with the arming call still
///    written beneath it do the same again (round 8). See `run_shell` for the
///    here-document, `simple_commands` for the operators and keywords,
///    `FunctionScope` for the function body, and `TERMINATING_COMMANDS` for the
///    `exit`. Round 9 closed the two ways that last one leaked: an `exit` in a
///    body the condition settles (`if true; then exit 0; fi`) now ends the
///    shell for the text below the block, and a quoted brace (`echo "}"`) no
///    longer closes a function body -- see `Block` and `FunctionScope::absorb`.
///    Round 10 closed the residual spelling of each: a header carrying its own
///    opening brace (`arm(){`) now opens the body at depth one, so a nested
///    group's `}` no longer ends it early (`opening_braces`), and a block is
///    settled by `:` as well as by `true`, and by a `for` over a list of plain
///    literal words (`constant_condition`, `for_list_is_certain`). Round 11
///    closed the last spelling of each: `}` is the shell's body-closing
///    reserved word only in *command position*, so a bare `echo }` no longer
///    balances a body the way `echo "}"` once did (`FunctionScope::absorb`),
///    and a settled `if` now inverts its verdict at `elif`/`else` instead of
///    applying the opener's to the whole block, so the `else` of `if true` is
///    read as the dead text it is -- and the `else` of `if false` as the branch
///    the shell takes (`Block::enter_arm`). Round 12 added the three ways the
///    shell leaves text behind that had no reader here at all: a here-document
///    delimiter is now matched exactly, so `EOF   ` no longer closes `<<EOF` a
///    line early and hands the body under it back as commands
///    (`Heredoc::terminated_by`); a reached `break`/`continue` now ends the
///    enclosing loop's body, so `while true; do break; gh ...; done` arms
///    nothing (`LOOP_CONTROL_COMMANDS`, `ShellState::end_loop_body`); and a
///    bare `false` now ends the step under the `bash -e` an Actions `run:`
///    defaults to, with each of bash's own errexit exemptions checked first
///    (`fails_under_errexit`, `Follower`). Round 14 closed the two ways a step
///    could still look like shell it is not: a redirection is syntax rather
///    than an argument, so `false >/dev/null` ends the step exactly as a bare
///    `false` does (`without_redirections`), and a step that names its own
///    interpreter (`shell: python`) is not handed to this bash reader at all
///    (`shell_is_modelled`). Reading that round again closed the spelling it
///    left: an operator needs no space in front of it to be one, so
///    `false>/dev/null` is split at the `>` and ends the step too
///    (`redirection_split`). Round 16 closed the two ways a *status* could
///    still be misread: an `&` that belongs to a redirection (`false
///    &>/dev/null`, `false 2>&1`) is no longer split off as a background
///    operator that would exempt the `false` from errexit
///    (`simple_commands`, `REDIRECTION_OPERATORS`), and a pipeline in a step
///    that names `shell: bash` -- which Actions runs with `-o pipefail` -- now
///    fails when any member does, so `false | true` ends the step
///    (`WorkflowShell::pipefail`, `ShellState::pipeline_failed`).
///
/// 5. **Which program the name resolves to.** `gh` in command position is the
///    GitHub CLI only while nothing in the same shell has defined a *function*
///    by that name. Bash resolves a command word against its function table
///    before `$PATH`, so a `run:` block spelling `gh() { echo disabled; }` and
///    then `gh pr merge "$PR_URL" --auto` executes the stub -- a reachable
///    command, correctly, but not an invocation of the CLI -- while a check
///    that reads only the word's basename certified it as the merge path (PR
///    #93 review, round 17). Function names defined in the text are tracked as
///    it is read (`ShellState::shadowing_functions`,
///    `defined_function_name`) and a call to one is marked `shadowed`, which
///    only ever refuses more text: a name shadowed here may be perfectly
///    ordinary in the real shell if the definition sits in a *different* step,
///    since `run_shell` reads the whole workflow's `run:` text as one stream.
///    Round 18 closed the spelling that left: commands break at `;`, `&`, `|`
///    and newline only, so a header sharing a line with the compound command
///    enclosing it (`{ gh() { echo disabled; }; }`, `if true; then gh() { echo
///    disabled; }; fi`) reached `defined_function_name` behind the shell's own
///    `{`/`then`/`else`/`do`, named nothing, and left the call below it
///    unshadowed -- so the header is now read off `significant_words`, which
///    drops exactly those structural tokens, the same words `command_head` is
///    read from. Shadowing is deliberately not undone by anything -- `unset -f
///    gh`, a subshell that ends, `command gh` -- because resolving those is the
///    same dispatch modelling this reader refuses everywhere else.
///
/// 6. **The step runs at all.** Everything above reads shell; Actions decides
///    separately whether the step is handed to a shell in the first place. A
///    step -- or the job enclosing it -- carrying a statically false `if:`
///    (`if: false`, `if: ${{ false }}`, `if: 0`) is skipped outright, so its
///    `run:` text arms nothing however that text reads. Checking only the field
///    position, the interpreter and the shell-default latch left such a step
///    certifying a successor workflow with no executable auto-merge path (PR
///    #93 review, round 15). See `condition_is_statically_false`, and note that
///    only a *statically* false condition counts: an expression over real
///    context is a branch the workflow can take, and this file's own job
///    condition is one.
///
///    What this does *not* refuse is a call the shell merely may not reach on a
///    given run. `gh pr merge --auto` inside `if ... else ... fi`, inside a
///    `for` loop, or after `grep -q ... &&` is a branch the shell can take, and
///    a branch it can take is an automated merge path -- which is how
///    auto-merge-trigger.yml arms one now that it stands down on a live
///    `needs-human` label. Refusing those refused the working workflow itself.
///
/// Tokens are compared after quote removal, so `"gh" pr merge --auto` counts
/// while `echo "gh pr merge --auto"` does not -- in the latter the whole
/// command text is a single argument token of `echo`.
///
/// Deliberately strict in one direction still: *deferred* execution is never
/// resolved, only refused. `gh` wrapped in a function (`arm() { gh pr merge
/// --auto; }` plus a real `arm` call) is NOT recognised, even where the call is
/// plainly there, because resolving dispatch badly is how a guard certifies a
/// merge path that is not one. Same for a gate this reader cannot evaluate but
/// can see is constant (`true && gh pr merge --auto` is read as dead). Both
/// errors point the same way: the guard fails loudly and a human looks, rather
/// than a definition nobody calls standing in for a merge path.
fn arms_auto_merge(body: &str) -> bool {
    let shell = run_shell(body);
    simple_commands(&shell.text, shell.pipefail)
        .iter()
        .any(|command| {
            !command.unreachable && !command.shadowed && is_gh_auto_merge(&command.words)
        })
}

/// Is `command` -- one simple command, already tokenised and unquoted -- an
/// invocation of `gh pr merge ... --auto`?
///
/// Whether the word `gh` reaches the GitHub CLI at all is a question about the
/// *shell*, not about this one command, so it is answered where the shell is
/// read: `SimpleCommand::shadowed` records a command word a function definition
/// earlier in the text has taken over, and `arms_auto_merge` refuses those
/// before this predicate is consulted.
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

/// The `run:` shell of `body` -- one *logical* line per line the shell receives,
/// comment-stripped, with here-document bodies removed and with
/// backslash-newline continuations rejoined -- as one string.
///
/// The here-document part is why this works line by line rather than on the
/// joined text. `cat <<EOF` hands every following line, up to a line holding the
/// delimiter alone, to `cat` on stdin: that text is *data*, and the shell never
/// looks at it for commands. Keeping it made a block like
///
/// ```sh
/// cat <<EOF
/// gh pr merge "$PR_URL" --auto --squash
/// EOF
/// ```
///
/// satisfy this guard while the workflow arms nothing (PR #93 review, round 6).
///
/// A here-document whose delimiter never arrives swallows the rest of the shell,
/// exactly as a real shell would, and so does a `<<` this reader cannot find a
/// delimiter word for. Both make the guard *harder* to satisfy, which is the
/// direction it has to err in.
fn run_shell(body: &str) -> WorkflowShellText {
    let mut shell = String::new();
    // Here-documents opened but not yet closed, in the order their bodies
    // follow: `cat <<A <<B` reads A's body first, then B's.
    let mut open_heredocs: Vec<Heredoc> = Vec::new();
    let (lines, pipefail) = folded_run_lines(body);
    for line in lines {
        if let Some(current) = open_heredocs.first() {
            if current.terminated_by(&line) {
                open_heredocs.remove(0);
            }
            // Body line or delimiter line, it is never shell either way.
            continue;
        }
        let executable = executable_part(&line);
        shell.push_str(executable);
        shell.push('\n');
        open_heredocs.extend(heredocs_opened(executable));
    }
    WorkflowShellText {
        text: shell,
        pipefail,
    }
}

/// A workflow's whole `run:` shell text, plus how the shell it was written for
/// treats a failed pipeline member. See `WorkflowShell::pipefail`.
struct WorkflowShellText {
    text: String,
    pipefail: bool,
}

/// A here-document opened by `<<WORD` and still swallowing lines.
struct Heredoc {
    /// The delimiter word with its quotes removed, or `None` when the `<<`
    /// operator had no word after it on its own line -- a spelling this reader
    /// does not model, which then consumes the rest of the shell rather than
    /// letting text of unknown status be read as commands.
    delimiter: Option<String>,
    /// True for `<<-`, the only spelling whose terminator line may be indented
    /// (with tabs).
    strip_tabs: bool,
}

impl Heredoc {
    /// Does `line` end this here-document?
    ///
    /// The terminator is the delimiter *alone* on its line -- nothing before it
    /// and nothing after it, trailing blanks included. Lines arrive here with
    /// the YAML block's own content indentation already removed but the rest of
    /// their whitespace intact, so an indented `EOF` correctly fails to close a
    /// plain `<<EOF` -- and correctly does close a `<<-EOF`, whose leading tabs
    /// (and only whose leading tabs) the shell strips first.
    ///
    /// Trailing blanks are just as significant, and comparing past them was the
    /// last way a here-document body could be read as shell: `EOF   ` does not
    /// close `<<EOF` in bash, so in
    ///
    /// ```sh
    /// cat <<EOF
    /// EOF␠␠␠
    /// gh pr merge "$PR_URL" --auto
    /// EOF
    /// ```
    ///
    /// the `gh` line is still *data* on `cat`'s stdin, while a `trim_end()` here
    /// closed the here-document a line early and handed that data back to the
    /// reader as an executed merge path (PR #93 review, round 12). Matching the
    /// delimiter exactly keeps such a block swallowed to its real terminator,
    /// which -- as with an unterminated here-document -- only makes this guard
    /// harder to satisfy. Which blanks the line still carries when it gets here
    /// is `source_lines`' business -- a trailing `\r` is one of them, and
    /// `str::lines` would have eaten it before this comparison ever ran.
    fn terminated_by(&self, line: &str) -> bool {
        let Some(delimiter) = self.delimiter.as_deref() else {
            return false;
        };
        let candidate = if self.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        candidate == delimiter
    }
}

/// The here-documents opened on one shell line, in the order their bodies
/// follow it.
///
/// `cat <<EOF`, `cat << EOF`, `cat <<-EOF`, `cat <<'EOF'` and `cat <<"EOF"` all
/// open one. `<<<` is a here-*string*: a single-word redirection with no body,
/// so it opens nothing and must not swallow the lines after it. A `<<` inside
/// quotes (`echo "<<EOF"`) is not an operator either.
fn heredocs_opened(line: &str) -> Vec<Heredoc> {
    let mut opened = Vec::new();
    let mut chars = line.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    chars.next();
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    chars.next();
                }
                '<' => {
                    if chars.peek() != Some(&'<') {
                        continue;
                    }
                    chars.next();
                    // `<<<` is a here-string, not a here-document.
                    if chars.peek() == Some(&'<') {
                        chars.next();
                        continue;
                    }
                    let strip_tabs = chars.peek() == Some(&'-');
                    if strip_tabs {
                        chars.next();
                    }
                    opened.push(heredoc_delimiter(&mut chars, strip_tabs));
                }
                _ => {}
            },
        }
    }

    opened
}

/// Read the delimiter word that follows a `<<` operator, consuming it from
/// `chars`.
fn heredoc_delimiter(chars: &mut Peekable<Chars<'_>>, strip_tabs: bool) -> Heredoc {
    while matches!(chars.peek(), Some(' ' | '\t')) {
        chars.next();
    }

    let mut word = String::new();
    let mut quote: Option<char> = None;
    while let Some(&c) = chars.peek() {
        match quote {
            Some(q) => {
                chars.next();
                if c == q {
                    quote = None;
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    chars.next();
                    quote = Some(c);
                }
                '\\' => {
                    chars.next();
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                // The word ends where the shell's own word ends.
                c if c.is_whitespace() => break,
                ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                c => {
                    chars.next();
                    word.push(c);
                }
            },
        }
    }

    Heredoc {
        delimiter: (!word.is_empty()).then_some(word),
        strip_tabs,
    }
}

/// The step-`run:` lines of `body` after YAML block folding, so each element is
/// one line *as the shell receives it*.
///
/// This is the whole difference between the two block-scalar spellings Actions
/// allows. A literal block (`run: |`) preserves every line break, so each
/// physical line is its own shell line. A folded block (`run: >`) replaces the
/// break between two plain content lines with a single space, so
///
/// ```yaml
/// run: >
///   echo disabled
///   gh pr merge "$PR_URL" --auto
/// ```
///
/// reaches the shell as the single command `echo disabled gh pr merge "$PR_URL"
/// --auto` -- an `echo` of six words, arming nothing. Joining here (rather than
/// folding inside `run_shell_lines`) also means `executable_part` cuts `#`
/// comments from the folded line, which is where the shell would see them.
///
/// A line that *starts* a shell line keeps its whitespace at both ends (the
/// block's own content indentation is already gone; what is left is what the
/// shell itself sees). `executable_part` trims it back off for tokenising, but
/// `Heredoc::terminated_by` needs both halves: an indented `EOF` does not close
/// a plain `<<EOF`, and neither does `EOF   ` -- a delimiter line is the
/// delimiter *alone*. Trimming the tail here closed a here-document one line
/// early, so the body line under it was read as a command rather than as the
/// stdin data bash hands to `cat` (PR #93 review, round 12). Only a line
/// *folded onto* a previous one is trimmed, because that is what YAML folding
/// does to it.
///
/// The `pipefail` flag `run_shell_lines` decided is carried straight through:
/// folding is YAML's business and says nothing about the interpreter.
fn folded_run_lines(body: &str) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = Vec::new();
    let WorkflowShell {
        lines: run_lines,
        pipefail,
    } = run_shell_lines(body);
    for run_line in run_lines {
        match lines.last_mut() {
            Some(last) if run_line.folded_onto_previous => {
                last.push(' ');
                last.push_str(run_line.text.trim());
            }
            _ => lines.push(run_line.text.to_owned()),
        }
    }
    (lines, pipefail)
}

/// One simple command read out of `run:` shell.
struct SimpleCommand {
    /// The command's words, already unquoted.
    words: Vec<String>,
    /// True when the shell can *never* run this command, whatever happens at
    /// run time: it sits behind a constant-status gate (`false && ...`,
    /// `true || ...`, the body of `if false`), inside a function body nothing
    /// here ever calls, or below a command that ends the shell outright. Such
    /// a command is text, not a merge path -- `false && gh pr merge --auto`
    /// arms nothing (PR #93 review, round 6), neither does
    /// `arm() { gh pr merge --auto; }` with no `arm` after it (round 7), and
    /// neither does a call written below an unconditional `exit 0` (round 8)
    /// or below `if true; then exit 0; fi` (round 9).
    ///
    /// A command that merely *may* be skipped is a different thing and is NOT
    /// marked here. `gh pr merge --auto` inside `if ... then ... else ... fi`,
    /// inside a `for` loop, or after `cmd &&` is a branch the shell can take,
    /// so it is an automated merge path -- which is exactly how this repo's own
    /// auto-merge-trigger.yml arms one now that it stands down on a live
    /// `needs-human` label. Reading "may not run on this invocation" as "never
    /// runs" made this guard refuse the real, working workflow.
    unreachable: bool,
    /// True when this command's own command word names a *shell function*
    /// defined earlier in the same shell text. Such a command runs -- it is not
    /// `unreachable` -- but it runs that function, not the program of the same
    /// name: bash looks a function up before it ever consults `$PATH`, so
    /// `gh() { echo disabled; }` followed by `gh pr merge "$PR_URL" --auto`
    /// invokes the stub and merges nothing, while the basename check in
    /// `is_gh_auto_merge` read the call as the GitHub CLI's (PR #93 review,
    /// round 17). Only an unqualified name is shadowable: `/usr/bin/gh` and
    /// `command gh` reach the program whatever functions exist, and neither
    /// matches a function name here.
    shadowed: bool,
}

/// Shell keywords that open a compound command whose body runs only on some
/// paths -- and, when the condition is a constant, on none.
const BLOCK_OPENERS: [&str; 5] = ["if", "while", "until", "for", "case"];

/// The keywords that close those blocks.
const BLOCK_CLOSERS: [&str; 3] = ["fi", "done", "esac"];

/// Words that introduce a compound command's *body* rather than heading a
/// command of their own.
///
/// The shell reads `then`, `else`, `do` and `elif` as syntax, and what follows
/// one of them on the same line is an ordinary simple command. This reader
/// breaks commands at `;`, `&`, `|` and newline only, so `if true; then exit 0;
/// fi` hands it the three words `then exit 0` as one command -- whose head was
/// `then`, which is no keyword this reader knows, so the `exit` it introduces
/// went unrecognised and the shell was read as still running below the block
/// (PR #93 review, round 9).
const BODY_INTRODUCERS: [&str; 4] = ["then", "else", "elif", "do"];

/// The words of a simple command that carry meaning for this reader: its own
/// words with the shell's structural tokens dropped -- a brace-group `{` (whose
/// contents run in this same shell) and the body introducers above.
fn significant_words(words: &[String]) -> impl Iterator<Item = &str> {
    words
        .iter()
        .map(String::as_str)
        .filter(|word| *word != "{" && !BODY_INTRODUCERS.contains(word))
}

/// Builtins whose exit status is fixed before the shell ever runs, so a list
/// operator applied to one of them decides statically -- not at run time --
/// whether what follows it executes. `false && CMD` and `true || CMD` are the
/// round-6 finding's own spellings of dead text.
///
/// `:` is `true` under another name. Over-reading here is the safe direction:
/// `true && gh pr merge --auto` is treated as dead and refused, which costs a
/// loud failure rather than a wrong certification.
const CONSTANT_STATUS_COMMANDS: [&str; 3] = ["true", "false", ":"];

/// The word that heads a simple command, read past the shell's own structural
/// tokens.
///
/// A brace group is not a command of its own: the shell runs its contents in
/// this same shell, so `{ exit 0; }` ends it exactly as a bare `exit 0` does
/// and `{ if ...` opens a block exactly as a bare `if` does. `then`, `else`,
/// `elif` and `do` are skipped for the same reason -- they introduce a body,
/// and the command they precede is what the shell actually runs. A `}` closing
/// a function body is still `FunctionScope`'s business, and heads no keyword
/// either way.
///
/// A redirection is syntax the shell consumes before the command starts, so it
/// heads nothing and ends the word it is written on: `>log false` and
/// `false>/dev/null` both head the builtin `false`, exactly as a bare `false`
/// does. Reading the raw first word instead left the glued spelling heading a
/// command called `false>/dev/null`, which no rule here recognises -- so the
/// errexit latch skipped it and a `gh pr merge ... --auto` written below it
/// still counted (PR #93 review, round 14, second reading).
fn command_head(words: &[String]) -> Option<&str> {
    without_redirections(significant_words(words))
        .into_iter()
        .next()
}

/// Is this whole simple command one of the constant-status builtins, with no
/// arguments of its own? `false` is; `[ -n "$PR_URL" ]` and `grep -q false f`
/// are not.
///
/// A *redirection* is not an argument. `false >/dev/null` runs the same builtin
/// to the same fixed status a bare `false` does -- the operator and the file
/// name it takes are syntax the shell consumes before the command itself ever
/// starts -- but they reach this reader as ordinary words, so counting them made
/// the command look like some other program that merely happens to be named
/// `false`. Neither the errexit latch nor a `&&`/`||` gate then recognised it,
/// and a `gh pr merge ... --auto` written below one was handed back as a live
/// merge path (PR #93 review, round 14). `without_redirections` drops them
/// before the count -- in every spelling, including the one glued straight onto
/// the builtin's own word (`false>/dev/null`), which bash splits at the
/// operator exactly as it splits the spaced form (`redirection_split`).
fn is_constant_status(words: &[String]) -> bool {
    matches!(
        without_redirections(significant_words(words)).as_slice(),
        [head] if CONSTANT_STATUS_COMMANDS.contains(head)
    )
}

/// `words` with its redirections removed -- each operator token and whatever
/// target is glued behind it, plus the following word when the target is not
/// glued (`> /dev/null` is two words, `>/dev/null` is one), and the command
/// text a redirection is glued *onto* is handed back without it
/// (`false>/dev/null` is one word carrying both).
///
/// Over-reading is the safe direction and is taken on purpose: a word this
/// wrongly drops can only make a command look *more* like a bare
/// `true`/`false`/`:`, which either ends the step earlier under errexit or kills
/// a gate's right-hand side -- both of which make the guard harder to satisfy.
/// Quote provenance is deliberately not consulted for the same reason: `false
/// ">"` is read as a redirection here, and `false` fails either way.
fn without_redirections<'a>(words: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut kept = Vec::new();
    let mut target_pending = false;
    for word in words {
        if target_pending {
            // The file name of a redirection whose operator stood alone.
            target_pending = false;
            continue;
        }
        match redirection_split(word) {
            Some((command_part, target)) => {
                // What was written before the operator is still a command --
                // the `false` of `false>/dev/null` -- unless the word was
                // nothing but the redirection itself.
                if !command_part.is_empty() {
                    kept.push(command_part);
                }
                target_pending = target.is_empty();
            }
            None => kept.push(word),
        }
    }
    kept
}

/// `word` split at the redirection written in it: the command text standing
/// before the operator (empty when the word is nothing but the redirection),
/// and the target glued behind it (`/dev/null` for `>/dev/null`, empty for a
/// bare `>` or `2>`, whose target is the next word). `None` when the word
/// carries no redirection at all.
///
/// A redirection operator is not a word character. Bash ends the word it is
/// written on the moment it reads one, so `false>/dev/null` is the builtin
/// `false` with its output redirected, exactly as `false >/dev/null` is.
/// Recognising the operator only at the *start* of a word caught the spaced
/// spelling and missed the glued one, which was read as some program named
/// `false>/dev/null`: errexit never latched on it, and a `gh pr merge ...
/// --auto` written below it was handed back as a live merge path (PR #93
/// review, round 14, second reading).
///
/// An optional file-descriptor number may precede the operator and belongs to
/// the redirection rather than to the command -- but only where the digits are
/// the whole word before it, which is bash's own rule: `2>err` redirects fd 2,
/// while `log2>err` runs `log2`.
///
/// The ampersand spellings (`&>f`, `&>>f`, `2>&1`, `>&2`) are redirections too,
/// and they reach here whole because `simple_commands` now reads them before it
/// splits at `&`. Splitting first was not the harmless permissive shortcut the
/// note here used to claim: `false &>/dev/null` was cut into a `false` the
/// manufactured `&` boundary then exempted from errexit as a *backgrounded*
/// command (`Follower::Background`), so the step read as still running and a
/// `gh pr merge ... --auto` written below it was handed back as a live merge
/// path -- while the real `bash -e` had already exited on that `false` (PR #93
/// review, round 16).
fn redirection_split(word: &str) -> Option<(&str, &str)> {
    let (at, operator) = word.char_indices().find_map(|(at, _)| {
        REDIRECTION_OPERATORS
            .iter()
            .find(|operator| word[at..].starts_with(**operator))
            .map(|operator| (at, *operator))
    })?;
    let before = &word[..at];
    let command_part = if !before.is_empty() && before.bytes().all(|b| b.is_ascii_digit()) {
        // A file-descriptor number, not a command of its own.
        ""
    } else {
        before
    };
    Some((command_part, &word[at + operator.len()..]))
}

/// The redirection operators, longest spelling first so `<<-` is never read as
/// a `<<` taking `-` as its delimiter, and `&>>` never as an `&>` writing to a
/// file called `>log`.
const REDIRECTION_OPERATORS: [&str; 12] = [
    "&>>", "&>", "<<<", "<<-", "<<", "<&", "<>", "<", ">>", ">|", ">&", ">",
];

/// One `if`/`while`/`until`/`for`/`case` block enclosing the command being
/// read, and what the block's own condition settles statically about its body.
struct Block {
    /// True when the condition can never hold, so the body is dead text:
    /// `if false`, `while false`, `until true`.
    body_is_dead: bool,
    /// True when the condition always holds, so the body is certain to run at
    /// least once: `if true`, `while true`, `until false`.
    ///
    /// Only a guaranteed body lets an `exit` written inside it end the shell
    /// for everything *after* the block. Reading every block's body as
    /// skippable left `if true; then exit 0; fi` followed by
    /// `gh pr merge "$PR_URL" --auto` classified as a reachable merge path,
    /// although the shell was already gone before it (PR #93 review, round 9).
    body_is_guaranteed: bool,
    /// True for an `if`, the one block whose body is split into *arms* by
    /// `elif` and `else`. A `while`/`until`/`for`/`case` body is one region,
    /// and nothing in it inverts what the opener settled.
    is_if: bool,
    /// True once an *earlier* arm of this `if` is certain to run, so every arm
    /// after it is dead however its own condition reads: the shell has already
    /// committed to a branch by the time it reaches them.
    earlier_arm_is_certain: bool,
    /// True while *every* earlier arm of this `if` is dead, which is what makes
    /// a closing `else` certain to run. Vacuously true at the opener, since an
    /// `if` has no arms before its first one.
    every_earlier_arm_is_dead: bool,
    /// True for `while`/`until`/`for`, the blocks `break` and `continue` act
    /// on. An `if` or a `case` is not a loop, so a `break` written inside one
    /// leaves through it to whatever loop encloses *it*.
    is_loop: bool,
    /// True once a reached `break`/`continue` has ended this loop's body, so
    /// every command written below it and still inside the block is text the
    /// shell never runs -- `while true; do break; gh pr merge "$PR_URL"
    /// --auto; done` arms nothing, because `break` leaves the loop before the
    /// `gh` line is ever reached (PR #93 review, round 12). Cleared with the
    /// block itself: shell below the matching `done` is ordinary shell again.
    body_terminated: bool,
}

impl Block {
    /// Step this block onto the arm that `elif`/`else` opens, re-deciding what
    /// is settled about the text that follows.
    ///
    /// The opener's verdict describes the *first* arm only, and applying it to
    /// the whole block read the `else` of `if true` -- text the shell can never
    /// reach, since the `then` arm always wins -- as an ordinary live branch.
    /// A `gh pr merge ... --auto` written there satisfied this guard with no
    /// executed auto-merge path in the file at all (PR #93 review, round 11).
    ///
    /// The inversion is exact in both directions, and each one matters:
    /// `if true`'s later arms are dead, and `if false`'s `else` is *certain* --
    /// which is what lets an `exit` written in it end the shell for the text
    /// below the block, exactly as one in the body of `if true` does.
    /// `elif` re-reads its own condition, but only while every arm before it is
    /// dead; once one is merely uncertain, so is everything after it.
    fn enter_arm(&mut self, introducer: &str, condition: &[&str]) {
        if !self.is_if {
            return;
        }
        self.earlier_arm_is_certain |= self.body_is_guaranteed;
        self.every_earlier_arm_is_dead &= self.body_is_dead;
        let settled = if self.earlier_arm_is_certain {
            // An earlier branch always wins, so this one never runs.
            Some(false)
        } else if self.every_earlier_arm_is_dead {
            match introducer {
                // Every condition before it failed statically, so the `else` is
                // the branch the shell takes.
                "else" => Some(true),
                // `elif`: its own condition decides, on the same terms the
                // opener's did.
                _ => constant_condition(condition),
            }
        } else {
            // An earlier arm may or may not have been taken, so nothing about
            // this one is settled.
            None
        };
        (self.body_is_dead, self.body_is_guaranteed) = match settled {
            Some(true) => (false, true),
            Some(false) => (true, false),
            None => (false, false),
        };
    }
}

/// The `elif`/`else` arm this simple command opens, with the condition words
/// that follow an `elif`, or `None` when it opens no arm at all.
///
/// This reader breaks commands at `;`, `&`, `|` and newline only, so an arm's
/// introducer arrives at the head of the command it introduces (`else`, or
/// `else gh pr merge ... --auto` written on one line) rather than as a command
/// of its own -- the same reason `BODY_INTRODUCERS` exists. A brace group's `{`
/// is skipped ahead of it for the same reason `command_head` skips it.
fn arm_introducer(words: &[String]) -> Option<(&str, Vec<&str>)> {
    let mut words = words
        .iter()
        .map(String::as_str)
        .skip_while(|word| *word == "{");
    let introducer = words.next()?;
    if introducer != "else" && introducer != "elif" {
        return None;
    }
    // `elif false; then ...` splits at the `;`, so the condition is what is
    // left of this command -- but stop at a `then` that stayed on it anyway.
    let condition = words
        .take_while(|word| !BODY_INTRODUCERS.contains(word))
        .collect();
    Some((introducer, condition))
}

/// What `words` -- a block opener -- settles statically about the body that
/// follows it.
///
/// Only the constant spellings settle anything: `if false`/`while false`/
/// `until true` make the body dead, `if true`/`while true`/`until false` make
/// it certain. A real test command or a `case` subject is a branch the shell
/// may or may not take, so neither flag is set for it.
///
/// Two spellings of a settled block leaked past the round-9 reading of this,
/// each one letting an `exit` that really does run be filed as skippable and
/// so leaving a `gh pr merge ... --auto` written below the block classified as
/// a live merge path (PR #93 review, round 10):
///
/// - `:` is `true` under another name -- `CONSTANT_STATUS_COMMANDS` has always
///   said so -- but the condition was compared against the literal word
///   `true`, so `if :; then exit 0; fi` and `while :; do exit 0; done` settled
///   nothing. `constant_condition` now reads both names.
/// - A `for` over a list of plain literal words runs its body at least once,
///   as surely as `while true` does. `for f in a b; do exit 0; done` is a
///   guaranteed exit; `for f in $LIST` (which may expand to nothing) and
///   `for f in *.log` (which may match nothing) are not, and stay skippable.
///
/// What this decides is the *first* arm of the block, and nothing more. An
/// `if` splits its body at `elif`/`else`, and applying the opener's verdict to
/// every arm read the `else` of `if true` -- which the shell can never reach --
/// as a live branch, so a `gh pr merge ... --auto` written there certified this
/// guard with no executed merge path in the file (PR #93 review, round 11).
/// `Block::enter_arm` steps the verdict onto each later arm; this function only
/// seeds it. A `case` whose subject is a literal that one of its patterns must match is
/// the one settled shape still read as skippable: matching shell patterns to
/// decide it is more machinery than this tripwire should carry, and the error
/// it leaves is bounded by every other rule here.
fn block_from_opener(words: &[String]) -> Block {
    let mut significant = significant_words(words);
    let Some(head) = significant.next() else {
        return Block {
            body_is_dead: false,
            body_is_guaranteed: false,
            is_if: false,
            earlier_arm_is_certain: false,
            every_earlier_arm_is_dead: true,
            is_loop: false,
            body_terminated: false,
        };
    };
    let condition: Vec<&str> = significant.collect();
    let (body_is_dead, body_is_guaranteed) = match head {
        "if" | "while" => match constant_condition(&condition) {
            Some(status) => (!status, status),
            None => (false, false),
        },
        "until" => match constant_condition(&condition) {
            Some(status) => (status, !status),
            None => (false, false),
        },
        "for" => (false, for_list_is_certain(&condition)),
        _ => (false, false),
    };
    Block {
        body_is_dead,
        body_is_guaranteed,
        is_if: head == "if",
        earlier_arm_is_certain: false,
        every_earlier_arm_is_dead: true,
        is_loop: matches!(head, "while" | "until" | "for"),
        body_terminated: false,
    }
}

/// The status a block's condition settles before the shell ever runs, if it
/// settles one at all.
///
/// Only a bare constant-status builtin settles anything: `true` and its other
/// name `:` are always success, `false` is always failure. Anything else --
/// `[ -n "$PR_URL" ]`, `grep -q false f`, `true && false` -- is a run-time
/// answer this reader must not pretend to know.
fn constant_condition(condition: &[&str]) -> Option<bool> {
    // A redirection on the condition is syntax, not a word of it: `if false
    // >/dev/null` settles its body exactly as `if false` does. Same reading, and
    // same safe direction, as `is_constant_status` -- see `without_redirections`.
    match without_redirections(condition.iter().copied()).as_slice() {
        ["true"] | [":"] => Some(true),
        ["false"] => Some(false),
        _ => None,
    }
}

/// Is this `for` header's word list certain to yield at least one iteration,
/// so that its body runs on every pass through the block?
///
/// Only an explicit `in` list of plain literal words is: every word after `in`
/// must be non-empty, free of `$` (a parameter or command substitution can
/// expand to nothing, or to nothing but whitespace) and free of the glob
/// metacharacters `*`, `?` and `[` (a pattern that matches no file leaves the
/// list empty under the default `nullglob`-off behaviour too, since the word
/// then survives unexpanded -- but reading that as certain would mean reading
/// shell pattern semantics, which this tripwire does not). `for f` with no
/// `in` iterates `"$@"`, which may be empty, and settles nothing either.
fn for_list_is_certain(condition: &[&str]) -> bool {
    let Some(in_at) = condition.iter().position(|word| *word == "in") else {
        return false;
    };
    let list = &condition[in_at + 1..];
    !list.is_empty()
        && list
            .iter()
            .all(|word| !word.is_empty() && !word.contains(['$', '*', '?', '[']))
}

/// Commands that end the shell where they stand, so that nothing written after
/// one of them is ever reached.
///
/// `exit` ends the shell; `exec CMD` replaces it with `CMD` and never returns.
/// A command boundary past either is still a boundary -- the shell just never
/// gets there -- so reading the text after it as an unconditional command let
/// `exit 0` followed by a deleted step's leftover `gh pr merge ... --auto` keep
/// this guard green with no merge path left (PR #93 review, round 8).
///
/// Only a *reached* one counts: `cmd || exit 1`, an `exit` inside an
/// `if`/`for` block, and one in a function body are all commands the shell may
/// skip, and none of them ends anything by being written down. Two spellings
/// are over-read on purpose, both in the direction of refusing more text:
/// redirection-only `exec >log` (which does *not* replace the shell) and an
/// `exit` reached only as a member of a pipeline (which ends a subshell, not
/// this one). Refusing text a human can see is live costs a loud failure;
/// certifying text the shell never reaches costs the merge path itself.
const TERMINATING_COMMANDS: [&str; 2] = ["exit", "exec"];

/// Commands that end the enclosing *loop body* where they stand, so that
/// nothing written after one of them -- up to that loop's own `done` -- is ever
/// reached.
///
/// `break` leaves the loop; `continue` jumps back to its head. Either way the
/// shell never reads the rest of the body on that pass, and never on any later
/// pass either, because every pass arrives at the same `break`. Treating the
/// line after one as an ordinary reachable command let
/// `while true; do break; gh pr merge "$PR_URL" --auto; done` certify this
/// guard with no merge path the shell can take (PR #93 review, round 12).
///
/// Unlike `exit` this is *scoped*: `Block::body_terminated` carries it, and the
/// matching `done` pops it, so shell below the loop is ordinary shell again.
const LOOP_CONTROL_COMMANDS: [&str; 2] = ["break", "continue"];

/// The `false` of a bare `false`, and the only command whose *failure* this
/// reader can settle before the shell runs.
///
/// An Actions step's default shell is `bash -e {0}`, and `help set` defines
/// `-e` as "Exit immediately if a command exits with a non-zero status". So a
/// reached, ungated `false` ends the step exactly as `exit 1` does, and a
/// `gh pr merge ... --auto` written below one is dead text -- which the reader
/// certified as a live merge path while it reset reachability at every newline
/// (PR #93 review, round 12).
///
/// Only bash's own errexit *exemptions* keep a `false` from ending the shell,
/// and each is checked before this latches: a `false` whose status is consumed
/// by `&&`/`||`, one that is a non-final member of a pipeline, one run in the
/// background with `&`, one that is the condition of an `if`/`elif`/`while`/
/// `until` (which reaches this reader inside the opener's own command, or
/// behind an `elif`), and one whose status is inverted with `!` (which heads
/// the command instead, so `is_constant_status` never sees `false` at all).
/// Anything else this reader cannot settle -- `grep -q x f`, `[ -n "$X" ]` --
/// stays a run-time answer and latches nothing.
const STATICALLY_FAILING_COMMAND: &str = "false";

/// What the boundary *after* a simple command does to its exit status -- which
/// is the whole of what decides whether errexit acts on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follower {
    /// `;`, a newline, or the end of the shell: nothing consumes the status, so
    /// `set -e` sees a failure and ends the step.
    List,
    /// `&&` or `||`: the status is the operator's own input, and bash exempts
    /// every member of such a list but the last from errexit. This reader does
    /// not track which member is last, so it exempts them all -- the permissive
    /// direction, but the only one that cannot mis-read `false && cmd`.
    Conditional,
    /// `|`: a non-final pipeline member's status is discarded -- *unless* the
    /// shell runs with `pipefail`, where any member's failure becomes the
    /// pipeline's own status and errexit reads it at the pipeline's end. (The
    /// *final* member is followed by something else, so it arrives here as
    /// `List`.) See `ShellState::pipefail`.
    Pipe,
    /// `&`: the command runs asynchronously and its status never reaches
    /// errexit at all.
    Background,
}

/// Does `words` -- one simple command -- fail statically under errexit, ending
/// the step where it stands?
///
/// The command must be a bare `false` *and* stand where bash lets errexit see
/// its status. `elif false` is the one exempt spelling that still reaches
/// `is_constant_status` as a bare `false`, because `significant_words` drops
/// the `elif` that introduces it; `if false`/`while false`/`until false` keep
/// their keyword at the head and never look like one.
///
/// `pipeline_failed` carries the other way a status this reader can settle
/// arrives at a `List` boundary: an earlier member of the pipeline this command
/// ends failed, and under `pipefail` that failure *is* the pipeline's status
/// however the last member exited. So `false | true` ends a `shell: bash` step
/// exactly as a bare `false` does -- reading only the final member left it
/// certifying a workflow whose `gh pr merge ... --auto` the shell is already
/// gone before it reads (PR #93 review, round 16). See `ShellState::pipefail`.
fn fails_under_errexit(words: &[String], follower: Follower, pipeline_failed: bool) -> bool {
    if follower != Follower::List {
        return false;
    }
    pipeline_failed || is_statically_failing(words)
}

/// Is `words` -- one simple command -- a bare `false` whose status is its own?
///
/// Split out of `fails_under_errexit` because a pipeline member's failure has
/// to be recognised at a `Pipe` boundary, where errexit itself does not act
/// yet: under `pipefail` it is remembered and read at the pipeline's end
/// instead (`ShellState::pipeline_failed`).
fn is_statically_failing(words: &[String]) -> bool {
    if command_head(words) != Some(STATICALLY_FAILING_COMMAND) || !is_constant_status(words) {
        return false;
    }
    // The condition of an `elif` is a test, not a command errexit acts on.
    !words
        .iter()
        .map(String::as_str)
        .any(|word| word == "elif" || word == "!")
}

/// Whether the reader is inside a shell *function body* -- text the shell reads
/// but does not run until something calls the function.
///
/// A definition is not an invocation. `arm() { gh pr merge "$PR_URL" --auto; }`
/// describes a merge path; only a later `arm` takes one. The block keywords
/// above are the only compound commands this reader tracked, so a workflow
/// keeping such a definition while its single real call was deleted satisfied
/// the guard with no automated merge path left at all (PR #93 review, round 7).
///
/// Calls are deliberately not resolved: a `gh pr merge --auto` reachable only
/// through a function is simply not recognised, even where the call is plainly
/// there. Unlike an `if` branch or a `&&` -- both of which this reader now
/// counts, because the shell can reach either without anything else being
/// written -- a function body is reached only through a dispatch this reader
/// would have to model, and modelling it badly is how a guard certifies a merge
/// path that is not one. Failing loudly and asking a human to look is the safe
/// error here.
#[derive(Default)]
struct FunctionScope {
    /// True from a definition header (`name()`, `name ()`, `function name`)
    /// until its body's braces balance again.
    open: bool,
    /// Unclosed `{` words seen since that header. A body whose opening brace
    /// sits on the *next* line leaves this at zero for one command, which is
    /// why `open` is tracked separately rather than inferred from it.
    brace_depth: usize,
}

impl FunctionScope {
    /// Fold one simple command into the scope, and report whether the shell
    /// reaches that command only by calling a function.
    ///
    /// Braces are counted as whole words, so `${VAR}` never opens or closes a
    /// body -- and only *unquoted* words count, because the shell's own brace
    /// is a syntax token while a quoted or backslash-escaped one is ordinary
    /// text. Words reach this reader with their quotes already removed, so
    /// `echo "}"` inside a body was indistinguishable from the body's own
    /// closing brace: it ended the scope early and re-opened a later
    /// `gh pr merge ... --auto` in the same uncalled function to an
    /// unconditional reading (PR #93 review, round 9). `quoted` carries that
    /// lost provenance alongside the words, one flag per word, so the two stay
    /// apart. An unbalanced body swallows the rest of the shell, which -- as
    /// with an unterminated here-document -- only makes the guard harder to
    /// satisfy.
    ///
    /// Quote provenance was only half of that leak. A brace is *usually* a word
    /// of its own, but a function header may carry the body's opening brace
    /// glued to its own token -- `arm(){` is one word, not two -- and counting
    /// only a standalone `{` left such a body sitting at depth zero. The first
    /// `}` inside it then closed the scope one brace early, exactly as
    /// `echo "}"` used to: `arm(){ { echo x; }; gh pr merge "$PR_URL" --auto; }`
    /// handed the arming call back to an unconditional reading with nothing
    /// calling `arm` (PR #93 review, round 10). `opening_braces` reads that
    /// glued brace off the tail of a longer word; a `}` glued to one is
    /// deliberately *not* read, since leaving it uncounted only keeps a body
    /// open for longer -- the direction this reader is allowed to err in.
    ///
    /// Quoting is not the only way a brace reaches this reader as *text*, which
    /// is the last spelling of that leak (PR #93 review, round 11, asking for
    /// quote provenance "or fail closed on ambiguous brace tokens"). `}` is a
    /// reserved word only in **command position**; `echo }` passes a bare,
    /// unquoted, entirely ordinary argument, and counting it closed an uncalled
    /// function one brace early just as `echo "}"` used to. Only the command's
    /// own first word is read as a closer now -- which is exactly the rule the
    /// shell itself applies -- and every other brace token is left uncounted,
    /// keeping the body open rather than freeing what follows it.
    fn absorb(&mut self, words: &[String], quoted: &[bool]) -> bool {
        let inside = self.open;
        if !self.open && is_function_definition(words) {
            self.open = true;
        }
        if self.open {
            // Where this command's own first word stands. A `}` is the reserved
            // word that closes a body only in *command position*; anywhere else
            // the shell reads it as an ordinary argument, which is what `echo }`
            // and `printf '%s\n' }` are.
            let command_position = words
                .iter()
                .position(|word| word != "{" && !BODY_INTRODUCERS.contains(&word.as_str()));
            for (idx, word) in words.iter().enumerate() {
                // Text that came out of quotes or a backslash escape is an
                // argument, never the shell's own brace. Missing provenance is
                // read as unquoted, which is how every word behaved before.
                if quoted.get(idx).copied().unwrap_or(false) {
                    continue;
                }
                if word == "}" && Some(idx) != command_position {
                    // An argument that happens to be a brace. Leaving it
                    // uncounted only keeps the body open for longer, which is
                    // the direction this reader is allowed to err in.
                    continue;
                }
                if word == "}" {
                    self.brace_depth = self.brace_depth.saturating_sub(1);
                    if self.brace_depth == 0 {
                        self.open = false;
                    }
                } else {
                    self.brace_depth += opening_braces(word);
                }
            }
        }
        inside || self.open
    }
}

/// How many body-opening braces an unquoted word carries: one for a bare `{`,
/// and one for a brace glued to the end of a longer token (`arm(){`, the whole
/// header of a one-line function definition).
///
/// Over-reading is the safe direction, and is taken on purpose: a stray
/// unquoted `echo x{` inside a body bumps the depth too, so the body simply
/// never closes and everything below it is refused. Under-reading is the
/// failure this exists to stop -- a body still at depth zero is closed by the
/// first `}` it meets, which frees the rest of an uncalled function to be read
/// as an executed merge path.
fn opening_braces(word: &str) -> usize {
    usize::from(word.ends_with('{'))
}

/// Does this simple command open a function definition?
///
/// Every spelling POSIX and bash allow for the header: `name()`, `name ()`,
/// `name () {`, `name(){`, and the `function` keyword with or without the
/// parentheses. A brace on the same word is stripped first, since this reader
/// does not treat `{` as a token separator.
///
/// Over-recognising here is harmless -- it can only mark more text
/// conditional, never less -- so the shapes are matched loosely on purpose.
fn is_function_definition(words: &[String]) -> bool {
    defined_function_name(words).is_some()
}

/// The *name* a function-definition header defines, or `None` when this command
/// is no definition at all.
///
/// Same shapes as `is_function_definition`, which is now this function asked a
/// coarser question -- one reader, so the two can never drift into disagreeing
/// about what a header is. The name is what makes a later call to it resolve to
/// the function rather than to the program of that name, which is why it has to
/// be pulled out at all (PR #93 review, round 17).
///
/// `Some("")` is a header this reader recognises but cannot name -- a degenerate
/// `function ()`, say. It keeps the definition's *body* conditional exactly as
/// before while matching no command word, since a shell command word is never
/// empty. That is the safe pair of answers: refusing to name something never
/// silently un-shadows a real `gh`, because a header that really does define one
/// spells the name out.
///
/// The header is read from the command's `significant_words`, not from its raw
/// first token, for the same reason `command_head` is: this reader breaks
/// commands at `;`, `&`, `|` and newline only, so a definition written on the
/// same line as the compound command that encloses it arrives with the shell's
/// own structural token still in front of it -- `{ gh() { echo disabled; }; }`
/// hands over the words `{ gh() { echo disabled`, and `if true; then gh() { echo
/// disabled; }; fi` hands over `then gh() { echo disabled`. Reading only the
/// raw first word, those headers named nothing, no shadow was recorded, and the
/// `gh pr merge "$PR_URL" --auto` written below one -- which bash resolves to
/// the stub a brace group or a taken branch really did define in this same
/// shell -- was handed back as the GitHub CLI's own arming call (PR #93 review,
/// round 18; the multi-line spellings of both were already refused, because
/// there the header starts its own line).
fn defined_function_name(words: &[String]) -> Option<&str> {
    let mut words = significant_words(words);
    let first = words.next()?;
    // `function name`, `function name()`, `function name {`. The body's opening
    // brace is not among these words -- `significant_words` drops it -- so a
    // degenerate `function {` runs out of words here and names nothing.
    if first == "function" {
        return words.next().map(header_name);
    }
    let head = first.strip_suffix('{').unwrap_or(first);
    // `name()`, `name(){`, and the half-tokenised `name(` of `name( ) {`.
    if let Some(name) = head.strip_suffix("()").filter(|name| !name.is_empty()) {
        return Some(name);
    }
    if head.len() > 1 && head.ends_with('(') {
        return Some(&head[..head.len() - 1]);
    }
    // `name ()`, `name () {`, `name (){`.
    if !head.is_empty() && words.next().is_some_and(|next| next.starts_with('(')) {
        return Some(head);
    }
    None
}

/// The bare name inside a definition header's own word: `gh(){`, `gh()` and
/// `gh(` all name `gh`, and a word that is only punctuation names nothing.
fn header_name(word: &str) -> &str {
    let word = word.strip_suffix('{').unwrap_or(word);
    let word = word.strip_suffix("()").unwrap_or(word);
    word.strip_suffix('(').unwrap_or(word)
}

/// Record `words` as a simple command, decide whether the shell can ever reach
/// it, and let a block keyword at its head -- or a function-definition header
/// anywhere in it -- open or close a region around the commands that follow it.
/// A head that ends the shell (`exit`, `exec`) closes the *rest* of it the same
/// way, by setting `terminated`.
///
/// Two different questions are answered here, and conflating them is what made
/// this guard refuse a working workflow:
///
/// - **Can the shell ever reach this command?** That is `unreachable`, and it
///   is what decides whether a `gh pr merge ... --auto` here counts as a merge
///   path. Only statically dead text says no: a constant-status gate, the body
///   of `if false`, a function body nothing calls, anything below a reached
///   `exit`.
/// - **Is the shell *guaranteed* to reach it on this run?** That is
///   `skippable`, and it is only used to decide whether an `exit` here may latch
///   `terminated` for everything below it. An `exit` inside an `if` block whose
///   condition is a real test, or behind `&&`, ends nothing on the runs that
///   skip it, so it must not. An `exit` inside a block the condition settles --
///   `if true`, `while true`, `until false` -- is reached on every run, so it
///   does (PR #93 review, round 9).
///
/// A third question is settled here too, and it is about neither reachability
/// nor skipping: **which program does the command word name?** A word an
/// earlier definition in this same text turned into a shell function is a call
/// to that function, since bash searches its function table before `$PATH`.
/// Such a command is perfectly reachable -- it just is not the program it looks
/// like -- so it gets its own flag, `SimpleCommand::shadowed`, rather than
/// being folded into `unreachable` (PR #93 review, round 17).
///
/// `quoted` runs parallel to `words`, one flag per word, recording which of
/// them came out of quotes or a backslash escape. Only `FunctionScope::absorb`
/// needs it, to tell a body-closing `}` from an `echo "}"`.
fn push_simple_command(
    commands: &mut Vec<SimpleCommand>,
    words: Vec<String>,
    quoted: Vec<bool>,
    gate: Gate,
    follower: Follower,
    state: &mut ShellState,
) {
    if words.is_empty() {
        return;
    }
    // An `elif`/`else` re-decides what the enclosing `if` settles, before
    // anything about *this* command is read: it is the first command of the new
    // arm, not the last of the old one.
    if let Some((introducer, condition)) = arm_introducer(&words) {
        if let Some(block) = state.blocks.last_mut() {
            block.enter_arm(introducer, &condition);
        }
    }
    let head = command_head(&words).unwrap_or("{");
    let opens_block = BLOCK_OPENERS.contains(&head);
    let closes_block = BLOCK_CLOSERS.contains(&head);
    // `exit`/`exec` end the shell outright; a bare `false` ends it just as
    // surely under the `bash -e` an Actions step runs by default -- and so does
    // a pipeline any member of which failed, once `pipefail` is on.
    let ends_shell = TERMINATING_COMMANDS.contains(&head)
        || fails_under_errexit(&words, follower, state.pipeline_failed);
    // What this command leaves for the rest of its pipeline, read before
    // anything below can end the shell: a `Pipe` boundary carries the failure
    // forward under `pipefail`, and every other boundary ends the pipeline.
    state.pipeline_failed = follower == Follower::Pipe
        && state.pipefail
        && (state.pipeline_failed || is_statically_failing(&words));
    let ends_loop_body = LOOP_CONTROL_COMMANDS.contains(&head);
    // Which program this command's own word reaches: a name some earlier
    // definition took over is a call to that function, whatever program shares
    // its name. Read before this command's own header can add to the table, so
    // a definition never shadows itself.
    let shadowed = command_head(&words)
        .is_some_and(|word| state.shadowing_functions.iter().any(|name| name == word));
    if let Some(name) = defined_function_name(&words).filter(|name| !name.is_empty()) {
        state.shadowing_functions.push(name.to_owned());
    }
    // Unconditionally: the scope is a state machine and every command has to
    // pass through it, not just the ones that end up unreachable.
    let in_function_body = state.functions.absorb(&words, &quoted);
    let in_dead_block = state.blocks.iter().any(|block| block.body_is_dead);
    let past_loop_control = state.blocks.iter().any(|block| block.body_terminated);
    let unreachable =
        gate.dead || in_dead_block || in_function_body || state.terminated || past_loop_control;
    // A block whose condition is settled is entered on every run, so it does
    // not make its body skippable; every other block does.
    let skippable =
        unreachable || gate.gated || state.blocks.iter().any(|block| !block.body_is_guaranteed);
    if opens_block {
        state.blocks.push(block_from_opener(&words));
    } else if closes_block {
        state.blocks.pop();
    } else if !skippable && ends_shell {
        // The `exit` itself runs -- `skippable` above is already decided -- and
        // everything after it does not.
        state.terminated = true;
    } else if ends_loop_body && !unreachable && !gate.gated {
        state.end_loop_body(loop_control_levels(&words));
    }
    commands.push(SimpleCommand {
        words,
        unreachable,
        shadowed,
    });
}

/// How many loops a `break`/`continue` leaves: its optional literal count, or
/// `None` when there is none to read.
///
/// `break 2` leaves two. A count this reader cannot evaluate (`break "$n"`)
/// reads as `None`, which `ShellState::end_loop_body` treats as *every*
/// enclosing loop -- refusing more text rather than less, the direction this
/// reader is allowed to err in.
fn loop_control_levels(words: &[String]) -> Option<usize> {
    let mut significant = significant_words(words);
    significant.next()?;
    match significant.next() {
        None => Some(1),
        Some(count) => count.parse().ok().filter(|levels| *levels > 0),
    }
}

/// What the boundary *before* a simple command settled about reaching it.
///
/// `;`, `&` and a newline settle nothing and reset both flags; `&&` and `||`
/// make the next command depend on how this one exited, and do so statically
/// when the left-hand side is a constant-status builtin.
#[derive(Clone, Copy, Default)]
struct Gate {
    /// The shell may skip the command, depending on a status only known at run
    /// time.
    gated: bool,
    /// Stronger: the gate is a constant, so the shell can never reach the
    /// command at all.
    dead: bool,
}

/// The reader state that carries from one simple command to the next.
///
/// Bundled rather than passed as five separate `&mut` parameters, which is both
/// what `clippy::too_many_arguments` asks for and the honest shape: these three
/// are one state machine, stepped by every command in file order.
#[derive(Default)]
struct ShellState {
    /// The `if`/`while`/`until`/`for`/`case` blocks enclosing the command being
    /// read, innermost last, each recording what its own condition settles
    /// about its body.
    blocks: Vec<Block>,
    /// Whether that command sits in a function body, which runs only when
    /// something calls the function.
    functions: FunctionScope,
    /// The names every function-definition header read so far has defined, in
    /// file order. A command word matching one of them is a call to that
    /// function, not to the program of the same name -- bash consults its
    /// function table before `$PATH` -- so `gh() { echo disabled; }` above a
    /// `gh pr merge "$PR_URL" --auto` leaves the workflow arming nothing (PR
    /// #93 review, round 17).
    ///
    /// Names are recorded as the text is read, so a call written *above* the
    /// definition that shadows it still counts: at that point in a real run the
    /// function does not exist yet. Everything else about the table errs
    /// towards refusing -- a definition inside dead text, inside another
    /// function's body, or in a different step of the same workflow all shadow
    /// the name here, and nothing ever removes one.
    shadowing_functions: Vec<String>,
    /// Set by a reached `exit`/`exec` -- or by a bare `false` under the `bash
    /// -e` an Actions step runs by default -- and never cleared: the shell is
    /// gone, so every command after one is text nothing executes.
    terminated: bool,
    /// Whether the shell this text was written for runs with `pipefail`, so a
    /// failed member anywhere in a pipeline is the pipeline's own status.
    ///
    /// Actions runs an unqualified `run:` under `bash -e {0}` and a `shell: sh`
    /// one under `sh -e {0}` -- neither sets `pipefail`, so there
    /// `false | true` succeeds and the step continues. An explicit `shell:
    /// bash` is `bash --noprofile --norc -eo pipefail {0}`, where the same
    /// pipeline fails and errexit ends the step. `run_shell_lines` decides
    /// which reading applies.
    pipefail: bool,
    /// Whether a member of the pipeline currently being read has already
    /// failed. Set at a `Pipe` boundary under `pipefail`, read by the command
    /// that ends the pipeline, and cleared by every other boundary.
    pipeline_failed: bool,
}

impl ShellState {
    /// Record a reached `break`/`continue`, ending the body of the loop it
    /// leaves so that the rest of that body reads as the dead text it is.
    ///
    /// `levels` counts loops outward from the innermost, as `break N` does;
    /// `None` -- a count this reader could not evaluate -- leaves the outermost
    /// one, which is the widest region and so refuses the most text. A
    /// `break` with no loop around it is the shell's own no-op and ends
    /// nothing.
    ///
    /// The latch only fires when the `break` is reached on *every* pass through
    /// that loop's body: a `break` nested in an `if` the condition does not
    /// settle is skipped on some runs, and the command below it is then a
    /// branch the shell can take. Note that this is a weaker requirement than
    /// the one an `exit` has to meet -- the loop's own body need not be
    /// guaranteed, because the text this kills sits in that same body and is
    /// reached on exactly the passes the `break` is.
    fn end_loop_body(&mut self, levels: Option<usize>) {
        let loops: Vec<usize> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_loop)
            .map(|(idx, _)| idx)
            .collect();
        // A count larger than the nesting depth exits every enclosing loop,
        // which is what `None` means here too.
        let Some(&target) = (match levels {
            Some(levels) => loops.iter().rev().nth(levels - 1).or(loops.first()),
            None => loops.first(),
        }) else {
            return;
        };
        if self.blocks[target + 1..]
            .iter()
            .any(|block| !block.body_is_guaranteed)
        {
            return;
        }
        self.blocks[target].body_terminated = true;
    }
}

/// Split shell source into simple commands, each a list of unquoted tokens plus
/// whether the shell is guaranteed to reach it.
///
/// A deliberately small shell reader rather than a dependency: word splitting on
/// unquoted whitespace, single/double quote removal, backslash escapes
/// (including line continuation), and command boundaries at unquoted `;`, `&`,
/// `|` and newline. Constructs it does not model (`$(...)`, `{ ...; }`) are left
/// in whatever token they appear in, which can only ever make a match *harder* to
/// achieve -- the direction this guard needs to err in.
///
/// Not every `&` is a boundary, though: bash reads `&>`, `&>>`, `2>&1` and
/// `>&2` as redirection *syntax* of the command they are written on, and the
/// operator is recognised here before the split. Splitting first manufactured a
/// background boundary out of `false &>/dev/null`, which then exempted that
/// `false` from errexit and handed back the dead text under it as a live merge
/// path (PR #93 review, round 16).
///
/// `pipefail` says whether the shell this text was written for turns a failed
/// pipeline member into the pipeline's own status; it reaches errexit through
/// `ShellState::pipeline_failed`.
///
/// Boundaries are not all alike, and reading them as if they were is what let a
/// dead branch certify this guard (PR #93 review, round 6). `;`, `&` and a
/// newline end a command and start a fresh, ungated list; `&&` and `||` end a
/// command and make the next one depend on how this one exited. That dependence
/// is only *dead* when the left-hand side settles it statically -- `false &&`
/// and `true ||` can never reach what follows them, while `grep -q x f &&` can
/// -- so only the constant-status spellings mark what follows unreachable. A
/// pipeline's `|` inherits whatever the pipeline itself sits behind, so
/// `false && echo x | gh pr merge --auto` kills the `gh` too. A boundary the
/// shell never arrives at is not a boundary either: past a reached `exit`, the
/// text below it is as dead as a branch behind `false &&` (PR #93 review, round
/// 8). Together with the block stack, the function-body scope and the
/// terminating-command latch tracked by `push_simple_command`, that is every
/// way this reader knows of for shell text to sit in the file without ever
/// running.
fn simple_commands(shell: &str, pipefail: bool) -> Vec<SimpleCommand> {
    let mut commands: Vec<SimpleCommand> = Vec::new();
    let mut command: Vec<String> = Vec::new();
    // One flag per word of `command`: did any of that word's text come out of
    // quotes or a backslash escape? Only the brace bookkeeping in
    // `FunctionScope::absorb` reads it.
    let mut quoted: Vec<bool> = Vec::new();
    let mut token = String::new();
    let mut token_quoted = false;
    let mut started = false;
    let mut quote: Option<char> = None;
    // What the boundary before the command currently being read settled about
    // reaching it.
    let mut gate = Gate::default();
    // The block stack, function scope and termination latch, stepped by every
    // command `push_simple_command` records.
    let mut state = ShellState {
        pipefail,
        ..ShellState::default()
    };
    let mut chars = shell.chars().peekable();

    // Close off the token being built, if any.
    macro_rules! end_token {
        () => {
            if started {
                command.push(std::mem::take(&mut token));
                quoted.push(std::mem::take(&mut token_quoted));
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
                    token_quoted = true;
                }
                '\\' => match chars.next() {
                    // Line continuation: the newline disappears and the command
                    // continues, so `gh pr merge \<newline>  --auto` is one
                    // command.
                    Some('\n') | None => {}
                    Some(next) => {
                        token.push(next);
                        started = true;
                        // `\}` is text, exactly as `"}"` is.
                        token_quoted = true;
                    }
                },
                // An `&` written as part of a redirection is syntax of the
                // command being read, not a boundary after it: `&>`/`&>>` open
                // one, and an `&` immediately behind a `>` or `<` names a file
                // descriptor (`2>&1`, `>&2`, `<&0`). Reading them as background
                // operators cut `false &>/dev/null` into a `false` this reader
                // then exempted from errexit, so the step read as still running
                // (PR #93 review, round 16). Anything else keeps its old
                // meaning: `cmd &` still backgrounds, `a && b` still gates.
                '&' if chars.peek() == Some(&'>') || (started && token.ends_with(['>', '<'])) => {
                    token.push(c);
                    started = true;
                }
                ';' | '&' | '|' | '\n' => {
                    end_token!();
                    // `&&` and `||` are two characters, and the only boundaries
                    // that gate what comes after them.
                    let doubled = (c == '&' || c == '|') && chars.peek() == Some(&c);
                    if doubled {
                        chars.next();
                    }
                    // Whether *this* command decides statically what the `&&`
                    // or `||` after it reaches -- read before the words move.
                    let constant_status = is_constant_status(&command);
                    // What this boundary does to the command's own status,
                    // which is what decides whether errexit acts on it.
                    let follower = match c {
                        _ if doubled => Follower::Conditional,
                        '|' => Follower::Pipe,
                        '&' => Follower::Background,
                        _ => Follower::List,
                    };
                    push_simple_command(
                        &mut commands,
                        std::mem::take(&mut command),
                        std::mem::take(&mut quoted),
                        gate,
                        follower,
                        &mut state,
                    );
                    gate = match c {
                        _ if doubled => Gate {
                            gated: true,
                            dead: gate.dead || constant_status,
                        },
                        // A pipeline member shares the pipeline's own gate.
                        '|' => gate,
                        // `;`, `&` and a newline start an unconditional list.
                        _ => Gate::default(),
                    };
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
        quoted.push(token_quoted);
    }
    // End of the shell text: nothing consumes the last command's status, so
    // errexit sees it exactly as it would before a `;`.
    push_simple_command(
        &mut commands,
        command,
        quoted,
        gate,
        Follower::List,
        &mut state,
    );
    commands
}

/// A block scalar (`|` or `>`) that is currently open, and what to do with the
/// content lines it swallows.
struct BlockScalar {
    /// Content indentation, pinned by the block's first non-blank line.
    content_indent: Option<usize>,
    /// True only for a block opened by a step's own `run:`; every other block's
    /// content is consumed and dropped.
    keep: bool,
    /// True for a *folded* block (`>`), where YAML turns the break between two
    /// plain content lines into a single space. False for a literal one (`|`),
    /// which keeps every break.
    folded: bool,
    /// True when the previous content line was non-blank and sat exactly at
    /// `content_indent` -- the only kind of line a following one may fold onto.
    prev_line_foldable: bool,
    /// Set when the block is the value of an `if:` key rather than of a `run:`
    /// one, naming the scope that condition governs. `if: |` and `if: >` are
    /// both legal, and `auto-merge-trigger.yml`'s own job condition is written
    /// that way, so a block-scalar condition has to be read rather than
    /// discarded like every other non-`run:` block.
    condition: Option<ConditionScope>,
    /// The condition's content, gathered while the block is open and read by
    /// `record_condition` when it closes. Empty for every other block.
    text: String,
}

impl BlockScalar {
    /// Gather one content line of an `if:` block scalar. Both spellings of the
    /// block hand Actions the same expression once whitespace is normalised --
    /// `|` keeps the breaks, `>` folds them to spaces, and an expression is
    /// whitespace-insensitive either way -- so the lines are joined by spaces
    /// whichever it is. Nothing is gathered for a block that is not an `if:`.
    fn gather_condition(&mut self, line: &str) {
        if self.condition.is_none() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(line.trim());
    }
}

/// One physical line of a step's `run:`, plus how YAML joins it to the line
/// before it.
struct RunLine<'a> {
    /// The line as the *shell* receives it: the YAML block's content
    /// indentation removed, any deeper indentation kept.
    text: &'a str,
    /// True when YAML folds the *preceding* line break into a space instead of
    /// keeping it: a folded (`>`) block with this line and the one before it
    /// both non-blank and sitting at the block's own content indentation. YAML
    /// preserves the break beside a blank line and beside a *more-indented*
    /// line, and preserves every break inside a literal (`|`) block, so all of
    /// those stay separate shell lines.
    folded_onto_previous: bool,
}

/// Which steps' `run:` an `if:` key governs.
#[derive(Clone, Copy, PartialEq)]
enum ConditionScope {
    /// The current step's own `if:` -- a key at the column that step's keys sit
    /// at. It governs exactly that step.
    Step,
    /// An `if:` that is not a step's own field. In a workflow that is a *job*
    /// condition, and a job that never runs runs none of its steps.
    Outer,
}

/// Which `run:` blocks a statically false `if:` has taken out of play.
#[derive(Default)]
struct Disablement {
    /// True once the current step's own `if:` cannot hold. Cleared with the
    /// step, like `shell:`: the field is per-step and so is its verdict.
    step: bool,
    /// True once an `if:` *outside* a step's own keys cannot hold. Never
    /// cleared, for exactly the reason `unmodelled_shell_default` is not: this
    /// reader tracks step scope but not job scope, so from such a key on it
    /// refuses every step rather than guess which ones the condition covered.
    /// Over-refusing costs a loud failure; guessing costs a wrong
    /// certification.
    outer: bool,
}

/// Record what an `if:` settles about the `run:` text around it.
///
/// A step GitHub Actions skips executes nothing, so its `run:` block is not a
/// merge path however the shell in it reads. Checking only the field position,
/// the interpreter and the shell-default latch left `if: false` on the arming
/// step -- or on its enclosing job -- handing that dead text back as a live
/// `gh pr merge ... --auto` call, so the guard would have stayed green on a
/// successor workflow with no executable auto-merge path at all (PR #93
/// review, round 15).
///
/// Only a *statically* false condition counts (see
/// `condition_is_statically_false`); an expression over real context is a
/// branch the workflow can take, and refusing those would refuse the working
/// file, whose `auto-merge-trigger.yml` job condition is exactly such an
/// expression.
fn record_condition<'a>(
    scope: ConditionScope,
    text: &str,
    disabled: &mut Disablement,
    shell: &mut Vec<RunLine<'a>>,
    step_run_start: Option<usize>,
) {
    if !condition_is_statically_false(text) {
        return;
    }
    match scope {
        ConditionScope::Step => disabled.step = true,
        ConditionScope::Outer => disabled.outer = true,
    }
    // Actions lets the field follow the `run:` it governs, so by now that
    // block may already have been collected as shell -- the same ordering
    // `shell:` has to undo.
    if let Some(start) = step_run_start {
        shell.truncate(start);
    }
}

/// Is this `if:` value false before the workflow ever runs, whatever the event
/// that triggered it?
///
/// Actions evaluates an `if:` value as an expression whether or not it is
/// wrapped in `${{ }}`, so `if: false` and `if: ${{ false }}` are the same
/// disabled step; a YAML-quoted scalar is still that expression, so `if:
/// 'false'` is the boolean literal and not the truthy string; and the values
/// Actions casts to false -- `0` and the empty string -- disable a step just as
/// the literal does.
///
/// Nothing else is read. An expression this cannot evaluate (`github.actor ==
/// 'x'`, `!true`, a `${{ }}` with text around it) is left alone as a branch the
/// workflow may well take: refusing those would refuse the real
/// `auto-merge-trigger.yml`, whose job condition is such an expression. The one
/// place this deliberately over-reads is quoting *inside* an expression --
/// `${{ 'false' }}` is a truthy string to Actions and dead text here -- which
/// costs a loud failure rather than a wrong certification, the direction this
/// guard errs in everywhere else.
fn condition_is_statically_false(value: &str) -> bool {
    // YAML's own inline comment (` #`), as `shell_is_modelled` reads it.
    let value = match value.split_once(" #") {
        Some((head, _)) => head.trim(),
        None => value.trim(),
    };
    if value.is_empty() {
        // No scalar on the key's own line: the value is elsewhere (a block
        // scalar, a nested node) and nothing is settled here.
        return false;
    }
    let inner = value
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .map_or(value, str::trim);
    let unquoted = inner
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| {
            inner
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .map_or(inner, str::trim);
    // `unquoted` is empty only where the scalar was a quoted empty string --
    // the unquoted empty value returned above already.
    unquoted.is_empty() || unquoted.eq_ignore_ascii_case("false") || unquoted == "0"
}

/// `body` split into lines the way the *shell* is handed them: on `\n` alone.
///
/// `str::lines` would be the obvious call here, and it is wrong for one byte:
/// it also strips a trailing `\r`, which bash does not. A here-document's
/// terminator is the delimiter alone on its line, and `\r` is as much a
/// trailing blank as a space is -- `EOF\r` no more closes `<<EOF` than `EOF `
/// does. Leaving that `\r` on the line keeps such a block swallowed to its real
/// terminator instead of handing its body back to the reader as shell (PR #93
/// review, round 13, generalising the round-12 exact-delimiter fix past the
/// whitespace `str::lines` was quietly eating first). A file written wholly in
/// CRLF then has no line this reader can read as a terminator at all, so its
/// here-documents swallow everything after them and the guard fails loudly --
/// the direction it has to err in. Every other consumer of these lines
/// (`executable_part`, the indent and key parsing below) trims before it reads,
/// so the `\r` reaches only the comparison that needs it.
fn source_lines(body: &str) -> impl Iterator<Item = &str> {
    // A final `\n` terminates the last line rather than opening an empty one,
    // and an empty body has no lines at all -- both are `str::lines`' own
    // behaviour, which is the only part of it that is right here.
    let body = body.strip_suffix('\n').unwrap_or(body);
    (!body.is_empty())
        .then(|| body.split('\n'))
        .into_iter()
        .flatten()
}

/// The shell lines of every *step's* `run:` in `body`, in file order.
///
/// A deliberately small YAML reader rather than a dependency. Three things have
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
/// 3. **The step must hand that value to a shell this reader models.** A step
///    may name its own interpreter (`shell: python`, `shell: pwsh`, a custom
///    `{0}` template), and its `run:` text is then not shell at all -- reading
///    it as bash certified this guard for a step whose Python raises before
///    `gh` is ever invoked (PR #93 review, round 14). The step's sibling
///    `shell:` field is tracked, in either order relative to the `run:` it
///    governs, and only `bash`/`sh` (and the absent field, which Actions runs
///    under `bash -e {0}`) are kept; see `shell_is_modelled`. A `shell:` key
///    that is *not* a step's own field -- `defaults: run: shell:` at job or
///    workflow level -- has no scope tracking here, so an unmodelled one drops
///    every step after it rather than guess which steps it governs.
/// 5. **The step must be one Actions runs.** A step's own `if:`, and any `if:`
///    outside a step's keys (a job's, most often), are read for a *statically*
///    false condition -- `if: false` and the spellings around it -- and the
///    `run:` they govern is then not executable text at all. A step's own field
///    is tracked in either order relative to its `run:`, exactly as `shell:`
///    is; an `if:` this reader cannot scope drops every step after it rather
///    than guess which ones it covered. Block-scalar conditions are read too,
///    since that is how the real `auto-merge-trigger.yml` writes its job's.
///    See `record_condition` and `condition_is_statically_false`.
/// 4. **A folded block must be folded.** `>` and `|` delimit the same content
///    but hand the shell different *lines*: under `>`, the break between two
///    lines at the block's own indentation becomes a space. Each returned line
///    carries `folded_onto_previous` so `folded_run_lines` can rebuild what the
///    shell actually sees; treating `>` like `|` manufactured a command boundary
///    the shell never gets, which let a folded block that only ever runs `echo`
///    certify an auto-merge call (PR #93 review, round 5).
///
/// Block scalars opened by *other* keys are tracked too, and their content
/// discarded, so a `description: |` paragraph can never be re-read as keys.
/// Anything else this reader cannot recognise is simply not returned, so an
/// unhandled spelling makes the guard fail loudly rather than pass vacuously;
/// that is the direction this guard needs to err in.
fn run_shell_lines(body: &str) -> WorkflowShell<'_> {
    let mut shell = Vec::new();
    // Set by any `shell: bash` key in the file, at step level or in a
    // `defaults: run:` -- see `WorkflowShell::pipefail` for why one key decides
    // it for the whole file.
    let mut pipefail = false;
    let mut block: Option<BlockScalar> = None;
    // The indent of the `steps:` key currently in effect, and the column at
    // which the current step's own keys sit. A `run:` counts only at that
    // column.
    let mut steps_indent: Option<usize> = None;
    let mut step_key_indent: Option<usize> = None;
    // What the current step's own `shell:` field says about the interpreter its
    // `run:` text is handed to, and where that step's run lines start in
    // `shell` -- the field may be written *after* the `run:` it governs, so what
    // has already been collected for the step has to stay removable.
    let mut step_shell_is_modelled = true;
    let mut step_run_start: Option<usize> = None;
    // Latched by a `shell:` key that is *not* a step's own field -- a
    // `defaults: run: shell:` at job or workflow level, which governs steps this
    // reader has no scope tracking for. Never cleared: from that key on, no step
    // in the file is read as shell, so an unmodelled default makes the guard
    // fail loudly rather than certify a step whose interpreter it cannot read.
    let mut unmodelled_shell_default = false;
    // What a statically false `if:` has already ruled out -- the current step,
    // or (from an `if:` this reader cannot scope) every step after it.
    let mut disabled = Disablement::default();

    for line in source_lines(body) {
        let indent = line.len() - line.trim_start().len();

        // Set when the open block scalar ends on this line, so the line itself
        // still falls through to the key handling below.
        let mut block_ended = false;
        if let Some(open) = block.as_mut() {
            // A block scalar's indentation is set by its first non-blank line
            // and it ends at the first non-blank line indented less than that.
            // Sibling keys of `run:` sit one level shallower, so this is what
            // keeps a following `name:`/`env:` out of the shell.
            if line.trim().is_empty() {
                // A blank line is a break YAML always preserves, in either
                // flavour of block.
                open.prev_line_foldable = false;
                if open.keep {
                    shell.push(RunLine {
                        text: "",
                        folded_onto_previous: false,
                    });
                }
                continue;
            }
            match open.content_indent {
                None => {
                    open.content_indent = Some(indent);
                    open.prev_line_foldable = open.folded;
                    if open.keep {
                        shell.push(RunLine {
                            text: line.get(indent..).unwrap_or(""),
                            folded_onto_previous: false,
                        });
                    }
                    open.gather_condition(line);
                    continue;
                }
                Some(base) if indent >= base => {
                    // Only a line at the block's own indentation folds. A
                    // *more-indented* line keeps the breaks on both sides of it
                    // -- that is how a folded scalar carries a pre-formatted
                    // chunk -- so it starts a fresh shell line and the next
                    // plain line cannot fold onto it either.
                    let plain = indent == base;
                    let folded_onto_previous = open.folded && plain && open.prev_line_foldable;
                    open.prev_line_foldable = open.folded && plain;
                    if open.keep {
                        shell.push(RunLine {
                            // The block's own content indentation is YAML's, not
                            // the shell's: strip exactly that much and no more,
                            // so `Heredoc::terminated_by` still sees whatever
                            // indentation the shell would.
                            text: line.get(base..).unwrap_or(""),
                            folded_onto_previous,
                        });
                    }
                    open.gather_condition(line);
                    continue;
                }
                Some(_) => block_ended = true,
            }
        }
        if block_ended {
            close_block(block.take(), &mut disabled, &mut shell, step_run_start);
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
            step_shell_is_modelled = true;
            step_run_start = None;
            disabled.step = false;
        }

        let (key, key_indent) = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                let key_indent = indent + 2;
                // A new item directly under `steps:` starts a new step; a
                // sequence item anywhere else is not a step and has no `run:`.
                step_key_indent = steps_indent.map(|_| key_indent);
                // A new step asks the interpreter question again from scratch:
                // `shell:` is a per-step field, and the run lines it governs are
                // this step's own. So is `if:`.
                step_shell_is_modelled = true;
                step_run_start = None;
                disabled.step = false;
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
        if name == "shell" {
            pipefail = pipefail || shell_enables_pipefail(value);
            let modelled = shell_is_modelled(value);
            if Some(key_indent) == step_key_indent {
                step_shell_is_modelled = modelled;
                if !modelled {
                    // Actions lets the field follow the `run:` it governs, so by
                    // now that block may already have been collected as shell.
                    if let Some(start) = step_run_start {
                        shell.truncate(start);
                    }
                }
            } else if !modelled {
                unmodelled_shell_default = true;
            }
        }
        let condition_scope = (name == "if").then(|| {
            if Some(key_indent) == step_key_indent {
                ConditionScope::Step
            } else {
                ConditionScope::Outer
            }
        });
        if let Some(scope) = condition_scope {
            // An inline condition is settled here; a block-scalar one is read
            // when its block closes, below.
            record_condition(scope, value, &mut disabled, &mut shell, step_run_start);
        }
        // A step's `run:` is shell only if the step actually hands it to a
        // shell, and only if the workflow can reach the step at all.
        let is_step_run = name == "run"
            && Some(key_indent) == step_key_indent
            && step_shell_is_modelled
            && !unmodelled_shell_default
            && !disabled.step
            && !disabled.outer;
        // `>`/`|` may carry chomping and explicit-indentation indicators
        // (`>-`, `|+`, `>2`); those affect trailing newlines, not whether the
        // block folds, so only the first character is read here.
        let folded = value.starts_with('>');
        if folded || value.starts_with('|') {
            if is_step_run {
                step_run_start = Some(shell.len());
            }
            block = Some(BlockScalar {
                content_indent: None,
                keep: is_step_run,
                folded,
                prev_line_foldable: false,
                condition: condition_scope,
                text: String::new(),
            });
        } else if is_step_run && !value.is_empty() {
            step_run_start = Some(shell.len());
            shell.push(RunLine {
                text: value,
                folded_onto_previous: false,
            });
        }
    }
    // A block scalar the file simply ends inside still closes there.
    close_block(block.take(), &mut disabled, &mut shell, step_run_start);

    WorkflowShell {
        lines: shell,
        pipefail,
    }
}

/// Every step's `run:` shell in one workflow, plus how the shell it is handed
/// to treats a failed pipeline member.
struct WorkflowShell<'a> {
    lines: Vec<RunLine<'a>>,
    /// True when any `shell:` key in the file names bash explicitly, so
    /// `pipefail` is on: Actions runs `shell: bash` as `bash --noprofile --norc
    /// -eo pipefail {0}`, while the *absent* field is `bash -e {0}` and
    /// `shell: sh` is `sh -e {0}` -- neither of those sets it. Under `pipefail`
    /// a failed member anywhere in a pipeline is the pipeline's own status, so
    /// `false | true` ends the step; without it only the last member's status
    /// counts and the step runs on. Reading every step as though `pipefail`
    /// were off let an explicit-bash step arm this guard with a `gh pr merge
    /// ... --auto` its own shell is already gone before it reads (PR #93
    /// review, round 16).
    ///
    /// One key decides it for the *whole file* rather than per step, for the
    /// reason `unmodelled_shell_default` and `Disablement::outer` are file-wide
    /// too: this reader concatenates every step's `run:` into one shell text
    /// and has no per-step scope to hang the flag on. That over-reads -- a
    /// default-shell step's `false | cat` is read under `pipefail` because some
    /// *other* step named bash -- and over-reading refuses text a human can see
    /// is live, which costs a loud failure, while under-reading would certify a
    /// merge path that is not one.
    pipefail: bool,
}

/// Finish a block scalar, reading an `if:` verdict out of the content it
/// gathered. Every other block's content was already dropped as it arrived.
fn close_block<'a>(
    block: Option<BlockScalar>,
    disabled: &mut Disablement,
    shell: &mut Vec<RunLine<'a>>,
    step_run_start: Option<usize>,
) {
    let Some(finished) = block else { return };
    if let Some(scope) = finished.condition {
        record_condition(scope, &finished.text, disabled, shell, step_run_start);
    }
}

/// Does this `shell:` value name an interpreter whose execution semantics
/// everything below `simple_commands` actually models?
///
/// Only bash and `sh`. An Actions step with no `shell:` runs under `bash -e
/// {0}`, `shell: bash` under `bash --noprofile --norc -eo pipefail {0}` and
/// `shell: sh` under `sh -e {0}`: three POSIX-ish shells with errexit on, which
/// is exactly what this reader's word splitting, quoting, here-documents and
/// `false`-ends-the-step latch assume. Every other value Actions accepts --
/// `python`, `pwsh`, `powershell`, `cmd`, or a custom `command ... {0}`
/// template -- hands the `run:` text to a language this reader cannot read at
/// all, and reading `gh pr merge "$PR_URL" --auto` out of a Python block
/// certified this guard for a step that raises a `SyntaxError` long before any
/// merge is armed (PR #93 review, round 14).
///
/// Unrecognised means *not shell*, never "assume bash": an unmodelled
/// interpreter drops the run block it governs, so the guard fails loudly and a
/// human looks -- the direction every other rule in this reader errs in too.
fn shell_is_modelled(value: &str) -> bool {
    matches!(shell_name(value), "bash" | "sh")
}

/// Does this `shell:` value name the one interpreter Actions runs with
/// `pipefail` on?
///
/// `shell: bash` is `bash --noprofile --norc -eo pipefail {0}`; the absent
/// field (`bash -e {0}`) and `shell: sh` (`sh -e {0}`) are not. GitHub
/// documents the difference, and `auto-merge-trigger.yml`'s own step comment
/// relies on it ("this step's default shell has no `pipefail`").
fn shell_enables_pipefail(value: &str) -> bool {
    shell_name(value) == "bash"
}

/// The interpreter named by a `shell:` value, with YAML's inline comment and
/// either quoting style removed.
fn shell_name(value: &str) -> &str {
    // YAML's own inline comment (` #`), then either quoting style.
    let value = match value.split_once(" #") {
        Some((head, _)) => head.trim(),
        None => value.trim(),
    };
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(value)
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
    // Behind a *constant* gate the shell can never take: dead text, refused.
    // See `arms_auto_merge_rejects_shell_text_that_never_executes`.
    assert!(!arms_auto_merge(&workflow(
        "          if false; then gh pr merge \"$PR_URL\" --auto; fi\n"
    )));
    // Behind a gate decided at run time, it is a branch the shell can take --
    // a real merge path, and counted as one.
    assert!(arms_auto_merge(&workflow(
        "          echo arming && gh pr merge \"$PR_URL\" --auto --squash\n"
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
        "          gh pr merge --auto --squash \"$PR_URL\"; echo done\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          /usr/bin/gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// Text that sits in a `run:` block without the shell ever *executing* it must
/// not satisfy the guard (PR #93 review, round 6: every newline and `&&` was
/// read as an unconditional command boundary, so a here-document body and a
/// statically-dead `false && ...` branch both certified a workflow whose real
/// arming call could have been deleted).
#[test]
fn arms_auto_merge_rejects_shell_text_that_never_executes() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // --- here-document data: stdin for another command, never shell ------
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n          gh pr merge \"$PR_URL\" --auto --squash\n          EOF\n"
    )));
    // The spellings that open one all have to be recognised, or the guard is
    // only as strict as its least-covered syntax.
    assert!(!arms_auto_merge(&workflow(
        "          cat << EOF\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          cat <<'EOF'\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          cat <<\"EOF\"\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          cat <<-EOF\n          gh pr merge \"$PR_URL\" --auto\n          \tEOF\n"
    )));
    // A plain `<<EOF` is closed only by an *unindented* delimiter, so an
    // indented one leaves the here-document open and the rest of the block is
    // still data -- which is what a real shell does with it too.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n            EOF\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Redirecting into a file rather than a pipe changes nothing about it.
    assert!(!arms_auto_merge(&workflow(
        "          cat > body.txt <<EOF\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));

    // --- conditional execution -------------------------------------------
    // The review's own case: the left-hand command decides whether `gh` runs
    // at all, so this arms nothing.
    assert!(!arms_auto_merge(&workflow(
        "          false && gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false || gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    // The gate survives a pipe, because a pipeline runs as a unit.
    assert!(!arms_auto_merge(&workflow(
        "          false && echo x | gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The multi-line spelling of the `if`/`then` case the one-line test above
    // already covers: `then` and `fi` on their own lines used to put `gh` at
    // the head of its own unconditional command.
    assert!(!arms_auto_merge(&workflow(
        "          if false\n          then\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    // A loop whose condition is constant never runs its body either.
    assert!(!arms_auto_merge(&workflow(
        "          while false\n          do\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          until true\n          do\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));

    // --- what must still count --------------------------------------------
    // A here-document after a real invocation does not retroactively unarm it,
    // and its delimiter ends it: shell resumes on the next line.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto --squash\n          cat <<EOF\n          nothing here\n          EOF\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          cat <<EOF\n          nothing here\n          EOF\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The invocation itself may take a here-document on stdin.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto --body-file - <<EOF\n          merged by CI\n          EOF\n"
    )));
    // `<<<` is a here-string: one word, no body, so it swallows no lines.
    assert!(arms_auto_merge(&workflow(
        "          cat <<<\"gh pr merge --auto\"\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A branch the shell decides at run time is a merge path, not dead text --
    // the distinction this whole function turns on. The first two are the shape
    // auto-merge-trigger.yml itself now uses: its arming call sits inside an
    // `if`/`else` that stands down while a live `needs-human` label covers the
    // current head, so refusing these refuses the real, working workflow.
    assert!(arms_auto_merge(&workflow(
        "          if grep -qxF needs-human labels\n          then\n            echo standing down\n          else\n            gh pr merge \"$PR_URL\" --auto --squash\n          fi\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          if needs_human_is_stale \"$SHA\"\n          then\n            gh pr merge \"$PR_URL\" --auto --squash\n          fi\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for pr in $PRS\n          do\n            gh pr merge \"$pr\" --auto\n          done\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          gh pr view \"$PR\" --json state && gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Gating is per-list, not per-block: `;` starts an unconditional one again,
    // and so does the end of an `if`/`fi`.
    assert!(arms_auto_merge(&workflow(
        "          false && echo skipped; gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          if false\n          then\n            echo skipped\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // `gh` first, gate after: the invocation is unconditional, the fallback is
    // the conditional half.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto --squash || echo failed\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          echo x | gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// A shell *function definition* is not an invocation (PR #93 review, round 7:
/// the reader tracked only `if`/`while`/`until`/`for`/`case`, so `arm() { gh pr
/// merge "$PR_URL" --auto; }` satisfied the guard with the one real `arm` call
/// deleted and no automated merge path left).
#[test]
fn arms_auto_merge_rejects_uncalled_function_bodies() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case: a definition, and nothing that calls it.
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            gh pr merge \"$PR_URL\" --auto --squash\n          }\n"
    )));
    // The call is not resolved even when it is right there -- refusing is the
    // safe error, and it is what makes the definition above refuse at all.
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            gh pr merge \"$PR_URL\" --auto\n          }\n          arm\n"
    )));
    // Every header spelling has to be recognised, or the guard is only as
    // strict as its least-covered syntax.
    assert!(!arms_auto_merge(&workflow(
        "          arm () {\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm(){\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function arm {\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function arm() {\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // The brace on its own line, which leaves the body's depth at zero for one
    // command.
    assert!(!arms_auto_merge(&workflow(
        "          arm()\n          {\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // One-line, and with a nested brace group inside -- the inner `}` must not
    // end the body early and re-open the file to an unconditional reading.
    assert!(!arms_auto_merge(&workflow(
        "          arm() { gh pr merge \"$PR_URL\" --auto; }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            { echo x; }\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // A *quoted* brace is an argument, not the body's closing brace. Words
    // reach `FunctionScope::absorb` with their quotes already stripped, so
    // `echo "}"` used to balance the body early and hand the call below it
    // back to an unconditional reading (PR #93 review, round 9); all three
    // spellings of a quoted brace have to stay out of the count.
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            echo \"}\"\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            echo '}'\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            echo \\}\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // Quoting is not the only way a brace arrives as ordinary text: `}` is the
    // reserved word that closes a body only in *command position*, so a bare
    // `echo }` passes an argument exactly as `echo "}"` does and must not
    // balance the body either (PR #93 review, round 11).
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            echo }\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            printf '%s' }\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // ...and the same trick with the *opening* brace must not smuggle the call
    // out of the body either.
    assert!(!arms_auto_merge(&workflow(
        "          arm() {\n            echo \"{\"\n            echo \"}\"\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    // A header carrying the body's opening brace glued to its own word
    // (`arm(){`) is the other half of the same leak: the body started at depth
    // zero, so a nested group's perfectly ordinary `}` closed the scope one
    // brace early and freed the call below it (PR #93 review, round 10). Both
    // spellings of the body, and the `function` keyword's, have to hold.
    assert!(!arms_auto_merge(&workflow(
        "          arm(){\n            { echo x; }\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          arm(){ { echo x; }; gh pr merge \"$PR_URL\" --auto; }\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function arm(){\n            { echo x; }\n            gh pr merge \"$PR_URL\" --auto\n          }\n"
    )));

    // --- what must still count --------------------------------------------
    // A body that ends is over: shell after the closing brace is ordinary
    // unconditional shell again.
    assert!(arms_auto_merge(&workflow(
        "          arm() {\n            echo nothing\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Over-recognition is the deliberate direction, pinned here rather than
    // left to chance: tokens reach `is_function_definition` already unquoted,
    // so `echo "()"` is indistinguishable from the `arm ()` header above and
    // opens a body that never closes. That costs a false *refusal* of the
    // arming call below it -- the guard fails loudly and a human looks, which
    // is the error this reader is allowed to make.
    assert!(!arms_auto_merge(&workflow(
        "          echo \"()\"\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// Shell written *below* a command that ends the shell is never executed (PR
/// #93 review, round 8: every newline and `;` reset reachability, so a step
/// spelled `exit 0` and then `gh pr merge "$PR_URL" --auto` satisfied the guard
/// while the workflow's real arming call could have been deleted out from
/// under it).
#[test]
fn arms_auto_merge_rejects_shell_below_a_terminating_command() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case, in both spellings of the boundary.
    assert!(!arms_auto_merge(&workflow(
        "          exit 0\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          exit 0; gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A bare `exit` (no status) ends the shell just as thoroughly, and so does
    // an `exit` several commands up.
    assert!(!arms_auto_merge(&workflow(
        "          exit\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          echo done\n          exit 0\n          echo unreachable\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // `exec CMD` replaces the shell and never comes back.
    assert!(!arms_auto_merge(&workflow(
        "          exec ./merge.sh\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Closing an `if` does not resurrect the shell: the `exit` ran at the top
    // level before it.
    assert!(!arms_auto_merge(&workflow(
        "          exit 0\n          if true\n          then\n            echo x\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A brace group is not a subshell: `{ ... }` runs in *this* shell, so an
    // `exit` inside one ends it just as a bare `exit` does, in either spelling.
    assert!(!arms_auto_merge(&workflow(
        "          { exit 0; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          {\n            exit 0\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A block whose condition is a constant is entered on every run, so the
    // `exit` inside it is reached on every run too. Reading every block body as
    // merely skippable let `if true; then exit 0; fi` leave the call below it
    // classified as a live merge path (PR #93 review, round 9).
    assert!(!arms_auto_merge(&workflow(
        "          if true; then exit 0; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          if true\n          then\n            exit 0\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          while true; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          until false; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // `:` is `true` under another name, and `for` over a list of literal words
    // iterates at least once -- both settle their body just as `if true` does,
    // and both were still read as skippable, so the `exit` inside them latched
    // nothing and the call below stayed a live merge path (PR #93 review,
    // round 10).
    assert!(!arms_auto_merge(&workflow(
        "          if :; then exit 0; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          while :; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The same second name on the dead side: `until :` never enters its body
    // at all, so a call written inside one arms nothing.
    assert!(!arms_auto_merge(&workflow(
        "          until :; do gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          for f in a b; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          for f in only; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));

    // --- what must still count --------------------------------------------
    // Only an `exit` the shell actually *reaches* ends anything. The `|| exit 1`
    // idiom is the common one and runs only on failure, so the call below it is
    // still a merge path.
    assert!(arms_auto_merge(&workflow(
        "          gh auth status || exit 1\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          [ -n \"$PR_URL\" ] && exit 0\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // An `exit` inside a block runs only if the block is entered -- and a real
    // test command, a `for` list or a `case` subject settles nothing, so those
    // blocks stay skippable and the call below them stays a merge path.
    assert!(arms_auto_merge(&workflow(
        "          if [ -z \"$PR_URL\" ]\n          then\n            exit 0\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for f in $LIST; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A `for` list settles its body only when every word is a plain literal:
    // an expansion can yield nothing and a glob can match nothing, so neither
    // guarantees the `exit` inside runs, and the call below them stays a merge
    // path.
    assert!(arms_auto_merge(&workflow(
        "          for f in *.log; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for f in \"$@\"; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for f; do exit 0; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Nesting a settled block inside an unsettled one does not settle it.
    assert!(arms_auto_merge(&workflow(
        "          if [ -z \"$PR_URL\" ]\n          then\n            if true; then exit 0; fi\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...and one in a function body only when something calls the function.
    assert!(arms_auto_merge(&workflow(
        "          bail() {\n            exit 1\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The word has to be the command, not an argument of one: `echo exit` and
    // `VAR=exit` end nothing.
    assert!(arms_auto_merge(&workflow(
        "          echo exit 0\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          ACTION=exit\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // An `exit` *after* the invocation unarms nothing -- the call already ran.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto --squash\n          exit 0\n"
    )));
    // A brace group that ends nothing leaves the shell running, and the call
    // below it is still a merge path -- reading past the brace must not turn
    // every group into a terminator.
    assert!(arms_auto_merge(&workflow(
        "          { echo x; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The braced spelling of a function body is still a definition, not a call:
    // its `exit` runs only when something calls `bail`.
    assert!(arms_auto_merge(&workflow(
        "          bail() { exit 1; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// An `if` whose condition is settled statically splits into arms the shell
/// can and cannot reach, and the opener settles only the *first* of them (PR
/// #93 review, round 11: the opener's verdict was applied to the whole block,
/// so `if true; then ...; else gh pr merge "$PR_URL" --auto; fi` -- an `else`
/// arm the shell never executes -- was read as a live automated merge path).
#[test]
fn arms_auto_merge_rejects_dead_arms_of_a_settled_if() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case: `if true` always takes its `then` arm, so the
    // `else` below it is text, not a merge path.
    assert!(!arms_auto_merge(&workflow(
        "          if true\n          then\n            echo standing down\n          else\n            gh pr merge \"$PR_URL\" --auto --squash\n          fi\n"
    )));
    // `:` is `true` under another name here, exactly as it is in the opener.
    assert!(!arms_auto_merge(&workflow(
        "          if :; then\n            echo standing down\n          else\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    // Once an arm is certain, every arm after it is dead whatever its own
    // condition reads -- the shell has already committed to a branch.
    assert!(!arms_auto_merge(&workflow(
        "          if true\n          then\n            echo ok\n          elif [ -n \"$PR_URL\" ]\n          then\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          if true\n          then\n            echo ok\n          elif [ -n \"$PR_URL\" ]\n          then\n            echo x\n          else\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    // An `elif` re-reads its own condition, so a constantly false one is dead
    // on its own terms even where every arm before it failed too.
    assert!(!arms_auto_merge(&workflow(
        "          if false\n          then\n            echo x\n          elif false\n          then\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));

    // --- what must still count --------------------------------------------
    // The mirror image, and the reason the inversion is exact rather than
    // one-directional: the `else` of `if false` is the branch the shell *takes*.
    assert!(arms_auto_merge(&workflow(
        "          if false\n          then\n            echo x\n          else\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    // A real condition settles nothing about either arm -- which is the shape
    // auto-merge-trigger.yml itself arms from, so refusing it refuses the
    // working workflow.
    assert!(arms_auto_merge(&workflow(
        "          if grep -qxF needs-human labels\n          then\n            echo standing down\n          else\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          if false\n          then\n            echo x\n          elif grep -qxF ready labels\n          then\n            gh pr merge \"$PR_URL\" --auto\n          fi\n"
    )));
    // An `else` belongs to the innermost `if`, not to the settled one around it.
    assert!(arms_auto_merge(&workflow(
        "          if true\n          then\n            if grep -q x f\n            then\n              echo x\n            else\n              gh pr merge \"$PR_URL\" --auto\n            fi\n          fi\n"
    )));
    // `fi` ends the block: shell below it is unconditional again, whatever the
    // arms settled.
    assert!(arms_auto_merge(&workflow(
        "          if true\n          then\n            echo ok\n          else\n            echo never\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Certainty follows the arm the shell must take: an `exit` in the `else` of
    // `if false` really does run, so the call below the block is dead...
    assert!(!arms_auto_merge(&workflow(
        "          if false\n          then\n            echo x\n          else\n            exit 0\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...while one in the `else` of `if true` ends nothing, because the shell
    // never reaches it -- so the call below that block is a merge path.
    assert!(arms_auto_merge(&workflow(
        "          if true\n          then\n            echo ok\n          else\n            exit 0\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// A folded block scalar (`run: >`) must be folded the way YAML folds it before
/// its content is read as shell (PR #93 review, round 5: `>` was accepted but
/// processed exactly like `|`, so a folded block whose lines join into a single
/// `echo` command was certified as an auto-merge invocation).
#[test]
fn arms_auto_merge_folds_folded_block_scalars() {
    // The review's own case. YAML joins these two lines with a space, so the
    // shell runs `echo disabled gh pr merge "$PR_URL" --auto` -- one `echo`,
    // with `gh` and its flags as mere arguments, and no `gh` invocation at all.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          echo disabled\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    // Chomping indicators belong to the header, not the content, so `>-` folds
    // exactly the same way.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >-\n          echo disabled\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    // The very same lines under `|` are two commands, the second of which does
    // arm auto-merge -- the contrast the previous revision could not draw.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: |\n          echo disabled\n          gh pr merge \"$PR_URL\" --auto\n"
    ));

    // Folding cuts both ways: an invocation split across plain lines of a
    // folded block is one real command once YAML is through with it.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          gh pr merge \"$PR_URL\"\n          --auto --squash\n"
    ));
    // A single-line folded block has no break to fold.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    ));

    // Breaks YAML *preserves* inside a folded block are still breaks, so the
    // `gh` line is its own command in both of these: beside a blank line, and
    // on a line indented deeper than the block's own content indentation.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          echo disabled\n\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          echo disabled\n            gh pr merge \"$PR_URL\" --auto\n"
    ));

    // Folding does not widen what counts as shell: a folded block under a key
    // that is not a step's `run:` is still dropped, and the block still ends
    // where indentation returns to the key's level.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - env:\n          run: >\n            gh pr merge \"$PR_URL\" --auto\n        uses: actions/checkout@v4\n"
    ));
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: >\n          echo hi\n        name: gh pr merge --auto\n"
    ));
}

/// A here-document's terminator is the delimiter *alone* on its line, trailing
/// blanks included (PR #93 review, round 12: a `trim_end()` on the line closed
/// `<<EOF` at a line spelled `EOF   `, which bash reads as ordinary body data,
/// so the here-document body under it was handed back to the reader as
/// executable shell).
#[test]
fn arms_auto_merge_closes_here_documents_only_on_an_exact_delimiter() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case: `EOF   ` is body text, so `cat` still owns every
    // line up to the real terminator and the `gh` line never reaches the shell.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n          EOF   \n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    // A tab is whitespace the delimiter line may not carry either.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n          EOF\t\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    // `<<-` strips *leading* tabs from the terminator and nothing else, so a
    // trailing blank still leaves it open.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<-EOF\n          \tEOF \n          gh pr merge \"$PR_URL\" --auto\n          \tEOF\n"
    )));
    // Anything after the delimiter on its line is the same story.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n          EOF nope\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    // A carriage return is a trailing blank too, and the one that got past the
    // comparison above for free: `str::lines` strips it, bash does not, so
    // `EOF\r` left this here-document open and the `gh` line under it was read
    // as shell (PR #93 review, round 13 -- see `source_lines`).
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\n          EOF\r\n          gh pr merge \"$PR_URL\" --auto\n          EOF\n"
    )));
    // The same line ending throughout, which is what a CRLF-checked-out file
    // actually looks like: no line here is a terminator bash would accept, so
    // the here-document swallows the rest and nothing is certified.
    assert!(!arms_auto_merge(&workflow(
        "          cat <<EOF\r\n          EOF\r\n          gh pr merge \"$PR_URL\" --auto\r\n          EOF\r\n"
    )));

    // --- what must still count --------------------------------------------
    // The exact delimiter still closes it, and shell resumes on the next line.
    assert!(arms_auto_merge(&workflow(
        "          cat <<EOF\n          nothing here\n          EOF\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          cat <<-EOF\n          nothing here\n          \tEOF\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Keeping `\r` on the line narrows nothing outside a delimiter comparison:
    // an ordinary command still reads as one, because `executable_part` trims
    // the line before it is tokenised.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto --squash\r\n"
    )));
}

/// `break` and `continue` end the enclosing loop's body where they stand, so
/// the rest of that body is never executed (PR #93 review, round 12: only
/// `exit`/`exec` affected reachability, so `while true; do break; gh pr merge
/// "$PR_URL" --auto; done` satisfied the guard although bash leaves the loop
/// before it ever reads the `gh` line).
#[test]
fn arms_auto_merge_rejects_shell_below_loop_control() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case, in both spellings of the boundary.
    assert!(!arms_auto_merge(&workflow(
        "          while true; do break; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          while true\n          do\n            break\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));
    // `continue` jumps to the loop head, so the rest of the body is just as
    // unreachable -- on this pass and on every later one, which all arrive at
    // the same `continue`.
    assert!(!arms_auto_merge(&workflow(
        "          while true; do continue; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    // Every loop keyword, not just `while`.
    assert!(!arms_auto_merge(&workflow(
        "          for f in a b; do break; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          until false; do break; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    // The loop's own condition need not be settled: the text this kills sits in
    // the same body and is reached on exactly the passes the `break` is.
    assert!(!arms_auto_merge(&workflow(
        "          for pr in $PRS; do break; gh pr merge \"$pr\" --auto; done\n"
    )));
    // A `break` inside a settled `if` inside the loop still runs on every pass.
    assert!(!arms_auto_merge(&workflow(
        "          while true\n          do\n            if true; then break; fi\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));
    // `break 2` leaves both loops, so the text after it in either body is dead.
    assert!(!arms_auto_merge(&workflow(
        "          while true\n          do\n            for f in a b\n            do\n              break 2\n            done\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));
    // A count past the nesting depth exits every enclosing loop, exactly as
    // bash does with it.
    assert!(!arms_auto_merge(&workflow(
        "          while true; do break 9; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    // A count this reader cannot evaluate is read as "all of them" -- refusing
    // more text, the direction this reader is allowed to err in.
    assert!(!arms_auto_merge(&workflow(
        "          while true; do break \"$n\"; gh pr merge \"$PR_URL\" --auto; done\n"
    )));

    // --- what must still count --------------------------------------------
    // The latch is scoped to the loop: `done` ends it and the shell below is
    // ordinary shell again.
    assert!(arms_auto_merge(&workflow(
        "          while true; do break; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for f in a b; do continue; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A call *above* the `break` still runs before the loop is left.
    assert!(arms_auto_merge(&workflow(
        "          while true\n          do\n            gh pr merge \"$PR_URL\" --auto\n            break\n          done\n"
    )));
    // A `break` the shell may skip ends nothing: the call below it is a branch
    // the shell can take, which is a merge path.
    assert!(arms_auto_merge(&workflow(
        "          while true\n          do\n            if [ -z \"$PR_URL\" ]; then break; fi\n            gh pr merge \"$PR_URL\" --auto\n          done\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for pr in $PRS; do [ -z \"$pr\" ] && break\n            gh pr merge \"$pr\" --auto\n          done\n"
    )));
    // `break 2` from a nested loop leaves the *outer* body dead, but the inner
    // loop's own text above it -- and the shell below the outer `done` -- is not.
    assert!(arms_auto_merge(&workflow(
        "          while true\n          do\n            for f in a b\n            do\n              break 2\n            done\n          done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The word has to be the command: `echo break` and `VAR=break` end nothing.
    assert!(arms_auto_merge(&workflow(
        "          while true; do echo break; gh pr merge \"$PR_URL\" --auto; done\n"
    )));
    // A `break` in an uncalled function body is not a reached `break`.
    assert!(arms_auto_merge(&workflow(
        "          stop() { break; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...and one with no loop around it is the shell's own no-op.
    assert!(arms_auto_merge(&workflow(
        "          break\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// An Actions step runs under `bash -e`, so a statically failing command ends
/// the step exactly as `exit 1` does (PR #93 review, round 12: reachability was
/// reset at every newline with no account of errexit, so a bare `false`
/// followed by `gh pr merge "$PR_URL" --auto` satisfied the guard although the
/// shell is already gone before it reads the `gh` line).
#[test]
fn arms_auto_merge_rejects_shell_below_a_statically_failing_command() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case, in both spellings of the boundary.
    assert!(!arms_auto_merge(&workflow(
        "          false\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false; gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A `false` several commands up ends the step just as thoroughly.
    assert!(!arms_auto_merge(&workflow(
        "          echo checking\n          false\n          echo unreachable\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The last member of a pipeline is the one whose status errexit reads.
    assert!(!arms_auto_merge(&workflow(
        "          echo x | false\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A block the condition settles is entered on every run, so the `false`
    // inside it is reached on every run too.
    assert!(!arms_auto_merge(&workflow(
        "          if true; then false; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A redirection is syntax the shell consumes before the command starts, not
    // an argument that would make this some other program called `false`: every
    // spelling of one still ends the step (PR #93 review, round 14).
    assert!(!arms_auto_merge(&workflow(
        "          false >/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false > /dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false >>log 2>>err\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...including the spelling with no space in front of the operator, which
    // bash splits at the `>` exactly as it splits the spaced one (PR #93
    // review, round 14, second reading).
    assert!(!arms_auto_merge(&workflow(
        "          false>/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false>>log\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false>/dev/null 2>>err\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false>/dev/null && gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...and including every spelling that writes the operator with an `&` in
    // it, which this reader used to cut into a command boundary: the `&` of
    // `&>`/`&>>` and of an fd duplication (`2>&1`, `>&2`) is redirection
    // syntax, not the background operator, so errexit still sees the `false`
    // (PR #93 review, round 16).
    assert!(!arms_auto_merge(&workflow(
        "          false &>/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false &> /dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false &>>log\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false 2>&1\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false >/dev/null 2>&1\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false>&2\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          false &>/dev/null && gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The same word is not an argument to a *gate* either, so what a redirected
    // constant status settles statically stays settled.
    assert!(!arms_auto_merge(&workflow(
        "          false >/dev/null && gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          if false >/dev/null; then gh pr merge \"$PR_URL\" --auto; fi\n"
    )));

    // --- what must still count --------------------------------------------
    // Every errexit exemption bash defines. `&&`/`||` consume the status...
    assert!(arms_auto_merge(&workflow(
        "          false && echo skipped\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          grep -q x f || false\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...a non-final pipeline member's status is discarded...
    assert!(arms_auto_merge(&workflow(
        "          false | cat\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...a backgrounded command's never reaches errexit at all -- the real `&`,
    // which is still a boundary wherever it is not part of a redirection...
    assert!(arms_auto_merge(&workflow(
        "          false &\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          false >/dev/null &\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          false& gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...`!` inverts it...
    assert!(arms_auto_merge(&workflow(
        "          ! false\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...and a condition is a test, not a command errexit acts on, in every
    // keyword that takes one.
    assert!(arms_auto_merge(&workflow(
        "          if false; then echo x; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          if [ -n \"$PR_URL\" ]\n          then\n            echo x\n          elif false\n          then\n            echo y\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          while false; do echo x; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A status only known at run time settles nothing, whatever it turns out
    // to be -- this reader must not pretend to know it.
    assert!(arms_auto_merge(&workflow(
        "          grep -q needs-human labels\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          [ -n \"$PR_URL\" ]\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A `false` the shell may skip ends nothing either.
    assert!(arms_auto_merge(&workflow(
        "          if [ -z \"$PR_URL\" ]; then false; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The word has to be the command: `echo false`, `VAR=false` and a `false`
    // that takes arguments (so it is some other program) end nothing.
    assert!(arms_auto_merge(&workflow(
        "          echo false\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          ACTION=false\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // ...and one in an uncalled function body is not a reached one.
    assert!(arms_auto_merge(&workflow(
        "          bail() { false; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Dropping redirections must not promote a real command to a constant one:
    // only the builtin's own word may be left standing.
    assert!(arms_auto_merge(&workflow(
        "          grep -q x f >/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          falsify >/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Splitting a word at its operator must not promote one either: what stands
    // in front of the `>` is the command, and only the builtin's own word is a
    // constant status. A leading file-descriptor number is the redirection's
    // (`2>err`), but digits glued to a command name are that name's (`log2>err`
    // runs `log2`).
    assert!(arms_auto_merge(&workflow(
        "          falsify>/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          grep -q x f>/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          echo false>/dev/null\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          log2>err\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// An explicit `shell: bash` step runs under `-o pipefail`, so a failed member
/// anywhere in a pipeline is the pipeline's own status and errexit ends the
/// step there -- the property PR #93's review asked for in round 16. Reading
/// only the pipeline's *last* member let `false | true` certify a step whose
/// shell is gone before it reaches the `gh` line below.
#[test]
fn arms_auto_merge_models_pipefail_for_explicit_bash_steps() {
    let bash = |shell: &str| {
        format!("jobs:\n  m:\n    steps:\n      - shell: bash\n        run: |\n{shell}")
    };
    let default_shell = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case: under `pipefail` the pipeline fails although its
    // last member succeeded.
    assert!(!arms_auto_merge(&bash(
        "          false | true\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&bash(
        "          false | cat | wc -l\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A `defaults: run: shell: bash` turns it on for the steps it governs just
    // as a step's own field does.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    defaults:\n      run:\n        shell: bash\n    steps:\n      - run: |\n          false | true\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    // The flag is file-wide, not per step: a step that names bash puts every
    // pipeline in the file under `pipefail`. That over-reads on purpose -- see
    // `WorkflowShell::pipefail` -- and over-reading costs a loud failure, which
    // is the direction this guard errs in everywhere else.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - shell: bash\n        run: echo one\n      - run: |\n          false | cat\n          gh pr merge \"$PR_URL\" --auto\n"
    ));

    // --- what must still count --------------------------------------------
    // Without an explicit bash there is no `pipefail`: Actions runs a bare
    // `run:` under `bash -e {0}` and `shell: sh` under `sh -e {0}`, where only
    // the pipeline's last member decides its status.
    assert!(arms_auto_merge(&default_shell(
        "          false | true\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - shell: sh\n        run: |\n          false | cat\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    // `pipefail` does not make a pipeline out of a `||`, and it does not reach
    // past the pipeline it belongs to either.
    assert!(arms_auto_merge(&bash(
        "          false || true\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&bash(
        "          grep -q x f | cat\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A pipeline whose failure is consumed -- by `&&`/`||`, by `&`, by `!`, or
    // by standing where the shell need not reach it -- ends nothing, exactly as
    // a bare `false` in the same place does.
    assert!(arms_auto_merge(&bash(
        "          false | true && echo armed\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&bash(
        "          false | true &\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&bash(
        "          if [ -z \"$PR_URL\" ]; then false | true; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}

/// A `run:` block is shell only when its step hands it to a shell -- the
/// property PR #93's review asked for in round 14. A step may name its own
/// interpreter, and `shell: python` means the block's text is Python: it raises
/// before any `gh` in it is reached, so reading it as bash certifies a merge
/// path the workflow does not have.
#[test]
fn arms_auto_merge_reads_run_blocks_only_for_modelled_interpreters() {
    let step = |fields: &str| format!("jobs:\n  m:\n    steps:\n      - name: arm\n{fields}");

    // The review's own case, in both orders -- the field may be written after
    // the `run:` it governs, which is only known once the block has been read.
    assert!(!arms_auto_merge(&step(
        "        shell: python\n        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n        shell: python\n"
    )));
    // Every other interpreter Actions accepts, and an inline `run:` too.
    assert!(!arms_auto_merge(&step(
        "        shell: pwsh\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        shell: cmd\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A custom `{0}` template is an interpreter this reader models no better
    // for being spelled with the word `bash` in it.
    assert!(!arms_auto_merge(&step(
        "        shell: python3 {0}\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A `shell:` this reader cannot scope -- `defaults: run: shell:` at job
    // level -- drops every step after it rather than guess which it governs.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    defaults:\n      run:\n        shell: python\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n"
    ));

    // --- what must still count --------------------------------------------
    // The absent field: Actions runs an unqualified `run:` under `bash -e {0}`.
    assert!(arms_auto_merge(&step(
        "        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Both modelled interpreters, quoted or not, with or without a comment.
    assert!(arms_auto_merge(&step(
        "        shell: bash\n        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(arms_auto_merge(&step(
        "        shell: sh\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&step(
        "        shell: \"bash\"\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&step(
        "        shell: bash # the Actions default, spelled out\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    defaults:\n      run:\n        shell: bash\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n"
    ));
    // A *later* step's interpreter is that step's own business: it must not
    // reach back and disarm a bash step that already ran the call.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n      - shell: python\n        run: print(\"done\")\n"
    ));
}

/// A step Actions never runs arms nothing -- the property PR #93's review asked
/// for in round 15. `if: false` on the arming step, or on the job that encloses
/// it, skips the command outright; reading the `run:` text anyway certifies a
/// successor workflow with no executable auto-merge path at all.
#[test]
fn arms_auto_merge_rejects_statically_disabled_steps() {
    let step = |fields: &str| format!("jobs:\n  m:\n    steps:\n      - name: arm\n{fields}");
    let job = |condition: &str| {
        format!("jobs:\n  m:\n{condition}    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n")
    };

    // The review's own case, in both orders -- Actions lets `if:` follow the
    // `run:` it governs, exactly as `shell:` may.
    assert!(!arms_auto_merge(&step(
        "        if: false\n        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        run: |\n          gh pr merge \"$PR_URL\" --auto --squash\n        if: false\n"
    )));
    // Every spelling of the same condition: wrapped in `${{ }}`, quoted (a
    // quoted scalar is still the expression Actions evaluates), commented, and
    // in the two other values Actions casts to false.
    assert!(!arms_auto_merge(&step(
        "        if: ${{ false }}\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        if: 'false'\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        if: false # parked until MAN-999\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        if: 0\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&step(
        "        if: ''\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The enclosing job's condition, which skips every step in it.
    assert!(!arms_auto_merge(&job("    if: false\n")));
    assert!(!arms_auto_merge(&job("    if: ${{ false }}\n")));
    // ...including the block-scalar spelling the real auto-merge-trigger.yml
    // writes its own job condition in.
    assert!(!arms_auto_merge(&job("    if: |\n      false\n")));
    assert!(!arms_auto_merge(&job("    if: >-\n      false\n")));
    // A step's own `if:` written as a block scalar, too.
    assert!(!arms_auto_merge(&step(
        "        if: |\n          false\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A block-scalar condition the file simply ends inside still closes.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n        if: |\n          false\n"
    ));

    // --- what must still count --------------------------------------------
    // The absent field, and a condition that holds.
    assert!(arms_auto_merge(&step(
        "        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&step(
        "        if: true\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // An expression over real context is a branch the workflow can take, not
    // dead text -- refusing these would refuse the real auto-merge-trigger.yml,
    // whose job condition is exactly such an expression, written as a block
    // scalar and mentioning `false` inside it.
    assert!(arms_auto_merge(&job(
        "    if: github.event.pull_request.user.login == 'thagale'\n"
    )));
    assert!(arms_auto_merge(&job(
        "    if: |\n      github.event.pull_request.draft == false &&\n      github.event.action != 'edited'\n"
    )));
    assert!(arms_auto_merge(&step(
        "        if: ${{ github.actor != 'dependabot[bot]' }}\n        run: gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A disabled step is that step's own business: it must not reach back and
    // disarm a live step that already ran the call, nor forward past its own.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: gh pr merge \"$PR_URL\" --auto\n      - if: false\n        run: echo parked\n"
    ));
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - if: false\n        run: echo parked\n      - run: gh pr merge \"$PR_URL\" --auto\n"
    ));
    // A mapping merely *named* `if` that is not a condition at all -- a `with:`
    // input, an `env:` value -- is scoped as an outer one, so it is read only
    // where it is statically false; a truthy one settles nothing.
    assert!(arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - with:\n          if: yes\n        run: gh pr merge \"$PR_URL\" --auto\n"
    ));
}

/// A command word a shell *function* has taken over does not reach the program
/// of that name (PR #93 review, round 17: a step could define
/// `gh() { echo disabled; }` and then run `gh pr merge "$PR_URL" --auto`, whose
/// basename this guard read as the GitHub CLI's while bash ran the stub and
/// merged nothing).
#[test]
fn arms_auto_merge_rejects_calls_shadowed_by_a_shell_function() {
    let workflow = |shell: &str| format!("jobs:\n  m:\n    steps:\n      - run: |\n{shell}");

    // The review's own case, and the one-line spelling of it.
    assert!(!arms_auto_merge(&workflow(
        "          gh() {\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto --squash\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          gh() { echo disabled; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Every header spelling defines the name, or the guard is only as strict as
    // its least-covered syntax -- the same list `is_function_definition` knows.
    assert!(!arms_auto_merge(&workflow(
        "          gh () {\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          gh(){\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function gh {\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function gh() {\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          gh()\n          {\n            echo disabled\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Quoting the command word does not escape the function table: bash strips
    // the quotes and looks the resulting word up exactly as it does a bare one
    // (unlike an alias, which quoting *does* defeat). Words reach this reader
    // already unquoted, so this holds by construction -- pinned so it stays so.
    assert!(!arms_auto_merge(&workflow(
        "          gh() { echo disabled; }\n          \"gh\" pr merge \"$PR_URL\" --auto\n"
    )));
    // A definition the shell reaches only on some runs, or never, still shadows
    // here. Deciding otherwise means modelling when the definition executes,
    // which is the dispatch resolution this reader refuses everywhere else --
    // and refusing more text is the direction it is allowed to err in.
    assert!(!arms_auto_merge(&workflow(
        "          if [ -n \"$STUB\" ]; then\n            gh() { echo disabled; }\n          fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // The definition in one step and the call in another: `run_shell` reads the
    // whole workflow's `run:` text as one stream, so this refuses too.
    assert!(!arms_auto_merge(
        "jobs:\n  m:\n    steps:\n      - run: |\n          gh() { echo disabled; }\n      - run: |\n          gh pr merge \"$PR_URL\" --auto\n"
    ));
    // A header sharing its line with the compound command that encloses it
    // still defines the name. This reader breaks commands at `;`, `&`, `|` and
    // newline only, so the shell's own structural token arrives in front of the
    // header -- and reading only the raw first word left every one-line
    // spelling below naming nothing, un-shadowing a call bash resolves to the
    // stub the brace group or the taken branch really did define (PR #93
    // review, round 18). The multi-line spellings above already refused.
    assert!(!arms_auto_merge(&workflow(
        "          { gh() { echo disabled; }; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          if true; then gh() { echo disabled; }; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          if false; then :; else gh() { echo disabled; }; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          for i in 1 2; do gh() { echo disabled; }; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(!arms_auto_merge(&workflow(
        "          function gh { echo disabled; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));

    // --- what must still count --------------------------------------------
    // A function by some *other* name shadows nothing -- the real
    // auto-merge-trigger.yml defines `needs_human_is_stale` right above its own
    // arming call, and reading any definition as a shadow would refuse the
    // working workflow.
    assert!(arms_auto_merge(&workflow(
        "          needs_human_is_stale() {\n            echo no\n          }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // A call written *above* the definition is not shadowed: at that point in a
    // real run the function does not exist yet, and the call reaches the CLI.
    assert!(arms_auto_merge(&workflow(
        "          gh pr merge \"$PR_URL\" --auto\n          gh() { echo disabled; }\n"
    )));
    // A word that merely mentions the name is no definition of it.
    assert!(arms_auto_merge(&workflow(
        "          echo \"gh() { echo disabled; }\"\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    // Reading the header past the shell's structural tokens must not make a
    // one-line definition of some *other* name shadow `gh`, nor leave the body
    // it opens hanging open over the arming call below it -- either would refuse
    // a workflow that arms one, which is the direction that costs a real merge
    // path. The one-line spellings of the shapes refused above, with the name
    // changed, all still count.
    assert!(arms_auto_merge(&workflow(
        "          { needs_human() { echo no; }; }\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          if true; then needs_human() { echo no; }; fi\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
    assert!(arms_auto_merge(&workflow(
        "          for i in 1 2; do needs_human() { echo no; }; done\n          gh pr merge \"$PR_URL\" --auto\n"
    )));
}
