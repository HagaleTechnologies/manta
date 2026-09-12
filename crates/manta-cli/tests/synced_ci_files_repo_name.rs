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
    // shell function that nothing ever calls (PR #93 review, round 7).
    assert!(
        arms_auto_merge(&body),
        "{rel} no longer arms auto-merge: no `run:` shell in it *invokes* \
         `gh pr merge ... --auto` as a command (a header comment, a \
         `name:`/`env:` scalar, an `echo` of the command text, an assignment \
         of it to a variable or a here-document body all fail this check, as \
         does naming it only behind `&&`/`||`, inside an `if`/`for` block, or \
         in the body of a function -- see `arms_auto_merge`). That call IS the post-#185 \
         admission boundary that replaced .mergify.yml's pull_request_rules; \
         losing it silently leaves the repo with no automated merge path at all."
    );
}

/// Does `body` -- a GitHub Actions workflow -- actually *execute* `gh pr merge
/// ... --auto` from a step's `run:` shell?
///
/// Four conditions, all necessary:
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
///    the shell will reach. Lines inside a here-document (`cat <<EOF ... EOF`)
///    are *data* on another command's stdin and are never executed at all; a
///    command behind `&&`/`||` or inside an `if`/`while`/`until`/`for`/`case`
///    block runs only if something else went a particular way; and a command
///    in a function body runs only when something calls that function, which
///    a `run:` block need never do. Treating every newline and `&&` as an
///    unconditional boundary let the first two certify a workflow whose real
///    arming call had been deleted (PR #93 review, round 6), and tracking only
///    those five block keywords let an uncalled `arm() { ... }` definition do
///    the same (PR #93 review, round 7). See `run_shell` for the
///    here-document, `simple_commands` for the operators and keywords, and
///    `FunctionScope` for the function body.
///
/// Tokens are compared after quote removal, so `"gh" pr merge --auto` counts
/// while `echo "gh pr merge --auto"` does not -- in the latter the whole
/// command text is a single argument token of `echo`.
///
/// Deliberately strict in one more direction: deferred execution is never
/// resolved, only refused. `gh` reached through a shell keyword (`if ...; then
/// gh pr merge --auto; fi`, or the same thing spelled across lines), gated by
/// `&&`/`||` (`git fetch && gh pr merge --auto`), or wrapped in a function
/// (`arm() { gh pr merge --auto; }` plus a real `arm` call) is NOT recognised,
/// even where a human can see the guard is always taken or the call is plainly
/// there. That is the safe error -- it fails the guard loudly and asks a human
/// to look, rather than certifying a merge path that a statically-dead branch,
/// or a definition nobody calls, could have turned off.
fn arms_auto_merge(body: &str) -> bool {
    simple_commands(&run_shell(body))
        .iter()
        .any(|command| !command.conditional && is_gh_auto_merge(&command.words))
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
fn run_shell(body: &str) -> String {
    let mut shell = String::new();
    // Here-documents opened but not yet closed, in the order their bodies
    // follow: `cat <<A <<B` reads A's body first, then B's.
    let mut open_heredocs: Vec<Heredoc> = Vec::new();
    for line in folded_run_lines(body) {
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
    shell
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
    /// The terminator is the delimiter alone on its line. Lines arrive here with
    /// the YAML block's own content indentation already removed but their
    /// *relative* indentation intact, so an indented `EOF` correctly fails to
    /// close a plain `<<EOF` -- and correctly does close a `<<-EOF`.
    fn terminated_by(&self, line: &str) -> bool {
        let Some(delimiter) = self.delimiter.as_deref() else {
            return false;
        };
        let candidate = if self.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        candidate.trim_end() == delimiter
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
/// A line that *starts* a shell line keeps its leading whitespace (the block's
/// own content indentation is already gone; what is left is the indentation the
/// shell itself sees). `executable_part` trims it back off for tokenising, but
/// `Heredoc::terminated_by` needs it: an indented `EOF` does not close a plain
/// `<<EOF`. Only a line *folded onto* a previous one is trimmed, because that is
/// what YAML folding does to it.
fn folded_run_lines(body: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for run_line in run_shell_lines(body) {
        match lines.last_mut() {
            Some(last) if run_line.folded_onto_previous => {
                last.push(' ');
                last.push_str(run_line.text.trim());
            }
            _ => lines.push(run_line.text.trim_end().to_owned()),
        }
    }
    lines
}

/// One simple command read out of `run:` shell.
struct SimpleCommand {
    /// The command's words, already unquoted.
    words: Vec<String>,
    /// True when the shell may not reach this command at all: it sits after an
    /// `&&`/`||` operator, inside an `if`/`while`/`until`/`for`/`case` block, or
    /// inside a function body nothing here need ever call. Such a command is
    /// text that *may* run, which is not the same thing as a merge path --
    /// `false && gh pr merge --auto` arms nothing (PR #93 review, round 6), and
    /// neither does `arm() { gh pr merge --auto; }` with no `arm` after it (PR
    /// #93 review, round 7).
    conditional: bool,
}

/// Shell keywords that open a compound command whose body may never run.
const BLOCK_OPENERS: [&str; 5] = ["if", "while", "until", "for", "case"];

/// The keywords that close those blocks. Between an opener and its closer every
/// command is conditional, which is what makes the multi-line spelling of
/// `if false<newline>then<newline>gh pr merge --auto<newline>fi` refuse just as
/// the one-line spelling already did.
const BLOCK_CLOSERS: [&str; 3] = ["fi", "done", "esac"];

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
/// there. That is the same refusal this reader already makes for `&&` and
/// `if` -- fail the guard loudly and ask a human to look, rather than model
/// function dispatch badly and certify a merge path that is not one.
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
    /// Braces are counted as whole words, so `${VAR}` or a `{` inside a quoted
    /// string never opens or closes a body. An unbalanced body swallows the
    /// rest of the shell, which -- as with an unterminated here-document --
    /// only makes the guard harder to satisfy.
    fn absorb(&mut self, words: &[String]) -> bool {
        let inside = self.open;
        if !self.open && is_function_definition(words) {
            self.open = true;
        }
        if self.open {
            for word in words {
                match word.as_str() {
                    "{" => self.brace_depth += 1,
                    "}" => {
                        self.brace_depth = self.brace_depth.saturating_sub(1);
                        if self.brace_depth == 0 {
                            self.open = false;
                        }
                    }
                    _ => {}
                }
            }
        }
        inside || self.open
    }
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
    let mut words = words.iter().map(String::as_str);
    let Some(first) = words.next() else {
        return false;
    };
    // `function name`, `function name()`, `function name {`.
    if first == "function" {
        return words.next().is_some_and(|name| name != "{");
    }
    let head = first.strip_suffix('{').unwrap_or(first);
    // `name()`, `name(){`, and the half-tokenised `name(` of `name( ) {`.
    if head.strip_suffix("()").is_some_and(|name| !name.is_empty())
        || (head.len() > 1 && head.ends_with('('))
    {
        return true;
    }
    // `name ()`, `name () {`, `name (){`.
    !head.is_empty() && words.next().is_some_and(|next| next.starts_with('('))
}

/// Record `words` as a simple command, and let a block keyword at its head --
/// or a function-definition header anywhere in it -- open or close a
/// conditional region around the commands that follow it.
fn push_simple_command(
    commands: &mut Vec<SimpleCommand>,
    words: Vec<String>,
    gated: bool,
    block_depth: &mut usize,
    functions: &mut FunctionScope,
) {
    let Some(head) = words.first() else {
        return;
    };
    // Unconditionally: the scope is a state machine and every command has to
    // pass through it, not just the ones that end up conditional.
    let in_function_body = functions.absorb(&words);
    let conditional = gated || *block_depth > 0 || in_function_body;
    if BLOCK_OPENERS.contains(&head.as_str()) {
        *block_depth += 1;
    } else if BLOCK_CLOSERS.contains(&head.as_str()) {
        *block_depth = block_depth.saturating_sub(1);
    }
    commands.push(SimpleCommand { words, conditional });
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
/// Boundaries are not all alike, and reading them as if they were is what let a
/// dead branch certify this guard (PR #93 review, round 6). `;`, `&` and a
/// newline end a command and the next one runs regardless; `&&` and `||` end a
/// command and make the next one *conditional* on how this one exited. A
/// pipeline's `|` inherits whatever gate the pipeline itself sits behind, so
/// `false && echo x | gh pr merge --auto` gates the `gh` too. Together with the
/// block-keyword depth and the function-body scope tracked by
/// `push_simple_command`, that is every way this reader knows of for shell text
/// to sit in the file without running.
fn simple_commands(shell: &str) -> Vec<SimpleCommand> {
    let mut commands: Vec<SimpleCommand> = Vec::new();
    let mut command: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    // Set by `&&`/`||`, cleared by the next `;`, `&` or newline: whether the
    // command currently being read is one the shell may skip.
    let mut gated = false;
    // How many `if`/`while`/`until`/`for`/`case` blocks enclose it.
    let mut block_depth: usize = 0;
    // Whether it sits in a function body, which runs only when something calls
    // the function.
    let mut functions = FunctionScope::default();
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
                    // `&&` and `||` are two characters, and the only boundaries
                    // that gate what comes after them.
                    let doubled = (c == '&' || c == '|') && chars.peek() == Some(&c);
                    if doubled {
                        chars.next();
                    }
                    push_simple_command(
                        &mut commands,
                        std::mem::take(&mut command),
                        gated,
                        &mut block_depth,
                        &mut functions,
                    );
                    gated = match c {
                        _ if doubled => true,
                        // A pipeline member shares the pipeline's own gate.
                        '|' => gated,
                        // `;`, `&` and a newline start an unconditional list.
                        _ => false,
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
    }
    push_simple_command(
        &mut commands,
        command,
        gated,
        &mut block_depth,
        &mut functions,
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
/// 3. **A folded block must be folded.** `>` and `|` delimit the same content
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
fn run_shell_lines(body: &str) -> Vec<RunLine<'_>> {
    let mut shell = Vec::new();
    let mut block: Option<BlockScalar> = None;
    // The indent of the `steps:` key currently in effect, and the column at
    // which the current step's own keys sit. A `run:` counts only at that
    // column.
    let mut steps_indent: Option<usize> = None;
    let mut step_key_indent: Option<usize> = None;

    for line in body.lines() {
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
                    continue;
                }
                Some(_) => block_ended = true,
            }
        }
        if block_ended {
            block = None;
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
        // `>`/`|` may carry chomping and explicit-indentation indicators
        // (`>-`, `|+`, `>2`); those affect trailing newlines, not whether the
        // block folds, so only the first character is read here.
        let folded = value.starts_with('>');
        if folded || value.starts_with('|') {
            block = Some(BlockScalar {
                content_indent: None,
                keep: is_step_run,
                folded,
                prev_line_foldable: false,
            });
        } else if is_step_run && !value.is_empty() {
            shell.push(RunLine {
                text: value,
                folded_onto_previous: false,
            });
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
    // Same reason, one operator down: `&&` only *may* reach what follows it.
    // See `arms_auto_merge_rejects_shell_text_that_never_executes`.
    assert!(!arms_auto_merge(&workflow(
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
    assert!(!arms_auto_merge(&workflow(
        "          for pr in $PRS\n          do\n            gh pr merge \"$pr\" --auto\n          done\n"
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
