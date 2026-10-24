# 2026-10-23 — MAN-220: the Dependabot diff verifier reads omitted requirement components as Cargo does; a lockfile edge may follow its target's own bump

**Status:** Implemented (branch `MAN-220`). It applies to Dependabot PRs
opened after it merges, because `dependabot-auto-merge.yml` runs
`scripts/verify-dependabot-diff-safe.py` from `main`.

**Ticket:** MAN-220. It collects Codex findings V, W and AK on manta#194
and AM on manta#195 (MAN-218's verifier), deferred under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Problem

**Finding V.** `requirement_is_satisfied_by` tested a caret requirement's
ceiling by comparing `caret_ceiling_component` identities. That function
returns `(1, None)` for `^0` and `(2, None)` for `^0.0`, and no concrete
version's identity can equal either. So `^0` rejected 0.5.0 and `^0.0`
rejected 0.0.5. The consistency pass checks every head declaration, so one
unrelated `^0` or `^0.0` in any manifest made every Dependabot PR
`NOT SAFE`.

**Finding AK.** `compare_requirements` compared zero-filled floors. Cargo
reads an omitted component in `>` as excluding that whole release line:
`>1.2` is `>=1.3.0`, and `>1` is `>=2.0.0`. Zero-filling read `>1.2` →
`>1.2.1` as a floor raise, although the head admits 1.2.2 and the base did
not. With the locked version unchanged (1.3.0 satisfies both), the
consistency and lockfile checks saw nothing either, and the verifier
approved a widened range. The same rule rejected `>1.2.1` → `>1.2`, which
narrows, as a downgrade.

**Finding AM.** A replacement entry's edges are compared by
`(name, kind, raw source)` when the target name has two or more
candidates; round 27 (Finding AL) kept the Git SHA in that source. When
`bar` 1.0.0 → 1.0.1 moved its edge from Git `foo 1.0.0@aaaa` to Git
`foo 1.0.1@bbbb`, a validated bump of `foo` itself, and a registry
`foo 2.0.0` stayed in the graph, the SHA change read as a retarget and
`bar` was reported as having changed its dependencies.

**The retained-entry analogue (found while planning).** The retained-entry
loop compares edges by full identity, version included, when the target
name has two candidates. Cargo writes such an edge as `"syn 3.0.4"`, so a
plain patch bump of a crate present at two versions rewrites the edge in
every retained consumer, and each one was reported. This is the only one
of the four that fires on manta's real `Cargo.lock`, which holds nine names
at two versions each (`syn` 2.x/3.x and `rand_core` 0.9/0.10 among them).

## Evidence

Measured 2026-10-23 against `cd79084` (`main`).

Cargo's own reading (cargo 1.98.0, offline). For each requirement, a path
dependency `dep = { path = "../dep", version = "<req>" }` against a path
crate of the given version, then `cargo generate-lockfile --offline`. Each
rejection was a `failed to select a version for the requirement` error:

| Requirement | Accepts | Rejects |
|---|---|---|
| `^0` | 0.5.0, 0.0.5 | 1.0.0 |
| `^0.0` | 0.0.5 | 0.1.0 |
| `^0.0.0` | 0.0.0 | 0.0.1 |
| `=1.2` | 1.2.5 | 1.3.0 |
| `>1.2` | 1.3.0 | 1.2.2 |
| `>1.2.1` | 1.2.2 | (none probed) |
| `>1` | (none probed) | 1.5.0 |
| `>1.5` | 1.6.0 | 1.5.9 |
| `~0` | 0.9.0 | (none probed) |
| `>1.2-alpha`, `^0-alpha` | none: `failed to parse manifest` | |

- **V:** `check_manifest_lockfile_consistency` with `demo = "^0"` and a
  locked `demo 0.5.0` returned a reason that no lock entry satisfies `^0`;
  likewise `^0.0` with 0.0.5.
- **AK:** a mocked `main('base', 'head')` with root manifests
  `demo = ">1.2"` → `demo = ">1.2.1"` and an unchanged lock at `demo 1.3.0`
  printed `SAFE: diff is confined to dependency-version value changes.`
  and returned 0.
- **AM:** the case above returned `Cargo.lock package "bar" (registry)
  changed its dependencies alongside its version bump (1.0.0 -> 1.0.1)`.
- **Retained analogue:** manta's `Cargo.lock` with `syn` 3.0.4 → 3.0.5 and
  every `"syn 3.0.4"` edge rewritten as Cargo writes it gave six reasons
  (`clap_derive`, `futures-macro`, `serde_derive`, `thiserror-impl`,
  `manta-cli`, `tokio-macros`); `rand_core` 0.10.1 → 0.10.2 gave five.
  After this change both give `[]`.
- **History replay:** the verifier before and after this change, run as
  `<sha>^ <sha>` on the 24 `chore(deps)` commits on `main`, gives the same
  verdict on every one: 8 `SAFE`, 16 `NOT SAFE`. #203 (`clap 4.6.6 →
  4.6.7`, `e861fb76`) is `NOT SAFE` under both: it moves five retained
  entries from `windows-sys 0.61.2` to `0.59.0`, a real retarget between
  coexisting instances (round 8, Finding I).

## Decisions

### D1 (V). Caret satisfaction uses an exclusive numeric upper bound

`caret_upper_bound` follows the Cargo Book's caret table: increment the
leftmost nonzero specified component, or the last specified one when every
specified component is zero. `^0` is `<1.0.0`, `^0.0` is `<0.1.0`, and
`^0.0.0` is `<0.0.1`. The floor check is unchanged. The bound compares
numeric parts only, so `2.0.0-alpha` stays outside `^1.2.3-alpha`, as in
the `semver` crate. Every fully-specified caret gives the same answer as
before.

### D2 (AK). Floors compare by their effective bound

`lower_bound_key` gives `(version_sort_key, 0)` for an inclusive floor and
`(version_sort_key, 1)` for a strict one, so a larger key is the stricter
floor. A partial `>` becomes Cargo's inclusive equivalent first: `>1.2` is
`>=1.3.0`, and `>1` is `>=2.0.0`. Every other kind keeps the zero-filled
floor, which is already exact for it (`>=1.2`, `=1.2`, `^1.2` and `~1.2`
all start at 1.2.0). A lowered strict floor gets its own reason, `strict
requirement lowered its effective floor`, because "decreased its version"
reads wrongly for `>1` → `>1.5`.

### D3 (AM). Edge targets map through matched replacement pairs; SHAs stay

A matched pair is a name with exactly one removed and one added lockfile
entry. The replacement loop validates each pair on its own (kind, source
identity, same-version Git commit swap, downgrade, major boundary,
checksum, its own edges) and reports one that fails. When a head-side edge
resolves to a pair's added entry, it is compared as the removed entry it
replaced. An edge that followed its target's own validated bump then
compares equal; every other edge change keeps its comparison, raw Git
source included.

This departs from the ticket's suggested rule, "retain the SHA only when
the old target is retained and the new target is an unmatched addition".
That rule reopens Finding AL. Case AL′: `x` keeps `foo 1.5.0@aaaa`;
`foo 1.4.0@cccc` is "replaced" by `foo 1.5.0@bbbb`, a pair that passes its
own checks; `bar` moves its edge from `aaaa` to `bbbb`. The new target is
not an unmatched addition, so the suggested rule strips the SHA and
accepts the same-version commit swap. While planning, a prototype of that
rule passed all 163 existing tests and returned `[]` for AL′. Under D3,
`bar`'s head edge
maps to `cccc`, which differs from its base edge `aaaa`, so `bar` is
reported. `test_retarget_laundered_through_another_instances_replacement_is_flagged`
pins this.

### D4. D3 applies to the retained-entry loop too

The ticket names V, W, AK and AM, but its title asks the verifier not to
over-reject, and the retained analogue is the one over-rejection that
manta's real lock will hit on the next patch bump of `syn` 3.x or
`rand_core` 0.10.x. The fix is the same map. Finding I stays covered: a
retarget to a target added with no matching removal, or to a retained
coexisting instance, is not a pair, so the edge change is still visible
(`test_edge_retargeted_to_newly_added_major_version_is_flagged` and
`test_retained_edge_moving_to_a_different_instance_is_still_flagged`).

## Finding W

Already resolved. Round 17's `_comparator_boundary` (Finding Z) made the
`EXACT` branch compare only the specified components, so `=1.2` accepts
1.2.5. `test_exact_with_omitted_patch_matches_the_whole_line` covers it.
No change here.

## Not done

These stay as they were and are not gaps this ticket left open:

- **Partial EXACT edits** such as `=1.2.0` → `=1.2` are accepted. The floor
  is unchanged and the range stays inside one major, which is the existing
  EXACT/GT/GE policy (no floor decrease, no major crossing); the same policy
  accepts `>=1.2.3` → `>=1.2.5`, which has no ceiling at all.
- **The GT major-boundary check** still reads the raw comparator, so
  `>1` → `>2.0.0` is rejected although it narrows. That fails closed.
- **A prerelease on a partial version** (`>1.2-alpha`) still parses here,
  although cargo 1.98.0 refuses it. `lower_bound_key` ignores `pre` for a
  partial `>`.
- **An edge moving between two coexisting instances of one Git source
  identity** (both retained, or retained → another pair's replacement)
  stays `NOT SAFE`. In real Cargo output one source identity locks one
  commit, so only a crafted lockfile produces it.
- **`caret_ceiling_component`** keeps its other callers:
  `compare_requirements`' CARET and EXACT/GT/GE branches and
  `check_lockfile_diff`'s resolved-version boundary check. With `^0` or
  `^0.0` it only fails closed, and only fully-specified versions reach the
  lockfile check.

The ticket also says manta has no Git-sourced dependencies. It has three
(`coppa-dsp`, `coppa-audio`, `coppa-channel`, pinned by `rev`). A coppa bump
changes the manifest `rev` and the lock source's `?rev=` query, so it is
already `NOT SAFE` both as a changed Git declaration and as a changed
source identity. This change does not alter that.
