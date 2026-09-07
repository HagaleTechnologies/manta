# MAN-101: V8w's "0 bogus callsigns" criterion is now its own always-run gate

SPEC-decode-core.md §7 states three independent pass criteria for the V8w
fading-pileup golden vector (CER accuracy, 0 bogus callsigns, 0
cross-channel ghost decodes). Until this change all three lived inside one
`#[ignore]`d test function,
`v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`
(`crates/manta-cli/tests/golden_v8_v8w.rs`), and CI never passes
`--ignored` (`.github/workflows/ci.yml:207,227,245`), so none of the three
ever executed. This records why "0 bogus callsigns" is now split into its
own non-ignored test, `v8w_pileup_fading_spots_no_bogus_callsigns`, and why
that is safe under this repo's CI-cost and auto-merge conventions.

## The reachability finding: `#[ignore]` was never the whole story

A Rust `assert!` unwinds its enclosing function on failure. The CER
assertion (`golden_v8_v8w.rs:214` before this change) is the *first* of the
three assertions in the old function body, and it fails today: 1/34
(2.9%) of strong signals decode at CER < 10% against the required 90%.
Reproduced directly in this session:

```
$ cargo test -p manta-cli --test golden_v8_v8w -- --ignored
thread '...' panicked at crates/manta-cli/tests/golden_v8_v8w.rs:214:5:
V8w must decode >= 90% of >= +6 dB signals at CER < 10%, got 1/34 (2.9%)
```

This means simply removing `#[ignore]` would not have made the bogus-call
check run at all — it would only turn today's silent skip into a loud CER
failure, with the bogus block (previously seven lines further down) never
reached. Splitting the bogus-call assertion into a function of its own was
therefore a structural requirement to ever run it independently, not a CI
wiring nicety.

## Measured CI cost

Debug profile (what CI's `cargo test --workspace` uses), 2 vCPU container:

| Command | before | after |
|---|---|---|
| `cargo test -p manta-cli --test golden_v8_v8w` | 13.99 s | 196.76 s |
| V8w bogus test alone | n/a | 193.40 s |
| Ignored CER test alone | 196.85 s | 196.85 s (unchanged) |

`cargo test --workspace` runs one test binary at a time, so the
workspace-level delta equals the per-binary delta: **+182.8 s per CI leg**.
That leg is not paid twice — it is paid **six** times per push.
`golden_v8_v8w.rs` lives in `manta-cli`, and three jobs build and run
`manta-cli`'s test binaries, each across `strategy.matrix.os:
[ubuntu-latest, macos-latest]`: `test` (`cargo test --workspace`,
`ci.yml:207`), `test-soapy` (`cargo test -p manta-input -p manta-cli
--features soapy`, `ci.yml:227`), and `test-hpsdr` (`cargo test -p
manta-input -p manta-cli --features hpsdr`, `ci.yml:245`). Only `test`
(`ubuntu-latest`/`macos-latest`) is a **required** context under
`docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md:26-27,64-66`;
`test-soapy` and `test-hpsdr` are not — a red run on either does not block
auto-merge. None of the three feature-gates `golden_v8_v8w` away, though,
so `cargo test -p manta-cli` builds and runs every integration test in the
package on all six legs regardless of which are required. Those six legs
run **concurrently** as independent GitHub Actions jobs, so added CI
**wall-clock** is therefore approximately **one leg, ~3-4 minutes**, not
six times that. Added **billed runner-time**, summed across all six legs,
is **~6 x 183 s ≈ 18-19 minutes** (before any macOS runner-minute
multiplier) — a real cost, but not a serial wall-clock one. The single-sample 193.40 s/196.76 s
figures above are not perfectly stable run to run: three independent
measurements on this container class (same code, same machine) produced
193 s, 229 s, and 237 s, so treat "~183-240 s per leg" as the honest range
rather than a fixed constant. The V8w (fading) decode is roughly 14x
slower than the sibling AWGN-only V8 decode of the identical 50-signal
scene (13.99 s) — Watterson fading is the entire cost, not test-harness
overhead.

## Why this test is deliberately exempt from MAN-9's 180 s ignore rule

MAN-9's plan (`crates/manta-cli/tests/golden_v8_v8w.rs`'s companion CER
work, tracked separately as MAN-9 / MAN-107–MAN-113 / issue #28) set a
180 s CI-cost rule for itself and `#[ignore]`d a regression ratchet because
the V8w decode measured ~367 s against it in that session. On its face,
landing a ~197 s always-run test in the same file looks like it
contradicts that precedent. It does not: MAN-9's 180 s rule was scoped to
a *diagnostic ratchet* — a test whose only job is to notice if a number
nobody gates on moves. The bogus-call test added here is a *normative SPEC
§7 pass criterion* for the failure mode `wiki/pages/spot-validation.md`
calls the one that "discredits the whole network" (false spots reaching
the RBN-compatible output). Different artifact, different cost/benefit;
applying MAN-9's rule here would make this ticket unimplementable as
written. (MAN-9's own pin doc and ratchet runbook — expected at
`docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md` and
`docs/RUNBOOKS/man9-v8w-baseline-ratchet.md` — arrive with its PR #106,
not yet merged into `main` as of this change; this note exists so the two
read as a deliberate contrast once both are on `main`, not as an
inconsistency.)

A cheaper alternative was considered and rejected: replaying a committed
`DecoderEvent` fixture through `manta-spot::Validator` directly (MAN-100's
harness makes this ~14 ms) would be fast, but it would stop being an
end-to-end gate and would require freezing a large generated artifact into
the repo. The sibling V8 (AWGN) test already establishes that a full scene
decode is an accepted CI cost in this file, so the same shape was kept
here rather than introducing a second, narrower kind of gate.

## The anti-vacuity floor is not a SPEC criterion

"0 bogus callsigns" is vacuously satisfied by a change that stops emitting
spots entirely — a real hazard given that the companion fix (MAN-100,
tightening `manta-spot`'s candidate arbitration and repetition gate) is
subtractive by design. The new test therefore also asserts
`MIN_V8W_VALIDATED = 15`: measured baseline at this change is 22/50
genuine calls validated; MAN-100's plan measures 20/50 after its fix. 15
leaves headroom for a legitimate precision/recall trade while still
catching a collapse to near-zero. SPEC-decode-core.md §7's V8w row states
no recall bar, so this floor is documented in the test source as a guard
on the test's own meaning, not as a normative gate to tune against the
spec.

## Both conditions reported together, not as two sequential asserts

The new test collects both failure conditions (bogus non-empty, recall
below floor) into one list and reports them with a single `assert!`. Two
sequential asserts would make the second unreachable whenever the first
fails — exactly the defect this ticket exists to fix, reintroduced one
level down.

## What did not change

- The ghost-decode assertion (0 cross-channel ghost decodes) stays inside
  the still-`#[ignore]`d CER test. The ticket's Gherkin and technical note
  name only the bogus-call check; SPEC §7 lists ghosts as a separate
  criterion; and this session measured 0 ghost decodes today, so it is
  not currently a live failure either way. If it needs its own gate later,
  that is a new ticket — the pattern established here makes it a small,
  well-precedented change.
- No production code changed. This is a test-only split; the fix that
  turns the new test green is the companion ticket, MAN-100, in
  `manta-spot`.
- The CER gate and its `#[ignore]` are untouched — that remains MAN-9 /
  MAN-107–MAN-113 / issue #28's scope.

## No shared decode cache, and the PR #106 rebase rule

MAN-9's PR #106 is open against this same file (`golden_v8_v8w.rs`) and,
as of this change, is not yet on `main`. Its diff adds a `OnceLock` decode
cache to the file. This change deliberately introduces no such cache of
its own: with the CER test still `#[ignore]`d, a plain CI run decodes V8w
exactly once either way, so a second cache would save nothing on the CI
path today and would only guarantee a merge conflict with #106.

**Rebase rule, either ordering:** whichever of MAN-101 or PR #106 lands
second on `main` rebases and, in the newly-added
`v8w_pileup_fading_spots_no_bogus_callsigns`, changes only the line `let
(report, manifest) = decode_report(&spec);` to call #106's cached
accessor instead of `decode_report` directly. Nothing else in this test
needs to change for that rebase. This is recorded here — rather than left
only in the PR body — so it survives independently of PR #132's own
lifecycle.

## Expected sequencing

This test is expected to land red (5 of 27 distinct spotted callsigns are
bogus at the commit this was written against) and merge only once MAN-100
lands. Under `docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md`, a red
required check simply means auto-merge does not fire — `main` is never
made red by this test existing on an open PR. That is the intended
behavior: the ticket's own framing is that this check should start
"failing (visibly)" the moment the busted-spot fix is being worked on,
rather than staying silently unreachable.
