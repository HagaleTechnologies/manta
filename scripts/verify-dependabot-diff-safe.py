#!/usr/bin/env python3
"""Verify a Dependabot PR's diff is confined to a safe Cargo dependency-version bump (MAN-218).

Manta analog of widdershins' scripts/verify-dependabot-diff-safe.mjs: re-derives the actual
dependency-version diff from Cargo.toml/Cargo.lock, independent of Dependabot's own self-reported
update-type classification, before dependabot-auto-merge.yml uses a code-owner-identity credential
to auto-approve the PR. Structurally simpler than the npm case in two ways that matter: Cargo
requirement strings have no OR-alternatives at all (comma-separated pieces are always AND'd, never
OR'd, per the Cargo Book's own version-requirement grammar), and Cargo.lock's [[package]] entries
have no hoisting/aliasing equivalent (no nested node_modules-style tree to walk).

Python, stdlib-only (tomllib, 3.11+), matching this repo's existing scripts/ convention
(confidence-separation.py, score-against-rbn.py are also stdlib-only) -- deliberately not Rust
(this session has no local Rust toolchain to compile/test it against) and not Node.js (would
introduce an entirely new CI toolchain to a currently 100%-Rust+Python-scripts project).

Usage: verify-dependabot-diff-safe.py <base_ref> <head_ref>
Run from a checkout of the manta repo; exits 0 (SAFE) or 1 (NOT SAFE), matching widdershins'
script's exit-code contract exactly, for a caller workflow to key off via steps.verify.outcome.
"""
import copy
import re
import subprocess
import sys
import tomllib

# Every workspace member's own Cargo.toml can declare per-crate [dependencies]/
# [dev-dependencies]/[build-dependencies] -- the root Cargo.toml additionally carries
# [workspace.dependencies], which is where most of manta's actual version pins live today.
WORKSPACE_MEMBERS = [
    "crates/manta-decode",
    "crates/manta-dsp",
    "crates/manta-input",
    "crates/manta-testkit",
    "crates/manta-engine",
    "crates/manta-spot",
    "crates/manta-server",
    "crates/manta-cli",
    "crates/manta-soak-harness",
]

DEPENDENCY_TABLES = ["dependencies", "dev-dependencies", "build-dependencies"]

# Cargo requirement comparator kinds. LT/LE are explicit, independently-editable ceiling data a
# compromised bump could widen and so must stay byte-identical; every other kind (CARET, TILDE,
# EXACT, GT, GE, WILDCARD) carries only a floor, and is checked against a major-version-boundary
# rule in compare_requirements (CARET/TILDE have their own kind-specific rule; EXACT/GT/GE share
# a generic one; WILDCARD admits everything already).
CARET, TILDE, EXACT, GT, GE, LT, LE, WILDCARD = (
    "caret", "tilde", "exact", "gt", "ge", "lt", "le", "wildcard",
)


class VerifyError(Exception):
    """Raised with a human-readable reason whenever a diff is judged NOT SAFE."""


def git_show(ref, path):
    """Read `path` as it existed at `ref`, or None if the path didn't exist at that ref.

    Never checks out `ref` -- only ever reads it as a git object via `git show`, matching
    widdershins' verifier's own trust model: this script itself always executes from the
    TRUSTED base checkout (the caller workflow never checks out the PR's own head tree), so a
    compromised head commit can't replace this script with one that always reports SAFE.
    """
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return None
    return result.stdout


def parse_toml(text):
    if text is None:
        return {}
    return tomllib.loads(text)


def normalize_dependency(value):
    """Reduce a Cargo.toml dependency's declared value to a comparable canonical shape.

    A dependency can be declared as a bare string ("1.2.3") or an inline table
    ({ version = "1.2.3", features = [...] }, { git = "...", ... }, { path = "..." },
    { workspace = true }, ...). Returns a tuple (kind, detail):
      - kind "version": detail is (requirement_string, rest) where rest is every OTHER key in the
        table (features, default-features, optional, package, registry, ...), or {} for a bare
        string. `rest` must be preserved and compared byte-for-byte by check_manifest_diff (Codex
        P1, manta#194 round 1): an earlier version discarded everything but the requirement
        string, so a head that added a feature, flipped `optional`, renamed via `package`
        (Cargo's own alias mechanism -- pointing an unchanged key at a DIFFERENT actual crate,
        directly analogous to npm's alias-swap attacks), or added a `registry` override while
        still bumping the version safely would have been silently waved through as a plain
        version bump.
      - every other kind: detail is the full raw table (never discarded -- check_manifest_diff
        needs the complete value to detect a same-kind change, e.g. a git dependency's rev/tag/
        branch pin, not just a kind change).
    Everything needed to detect a SOURCE change (kind changes) separately from a
    version-requirement change (kind stays "version", requirement or rest changes).
    """
    if isinstance(value, str):
        return ("version", (value, {}))
    if isinstance(value, dict):
        if "git" in value:
            return ("git", value)
        if "path" in value:
            return ("path", value)
        if value.get("workspace") is True:
            return ("workspace", value)
        if "version" in value:
            rest = {k: v for k, v in value.items() if k != "version"}
            return ("version", (value["version"], rest))
        # registry override with no explicit version (rare, but not a version-requirement
        # change we can safely characterize) -- treat as its own opaque kind.
        return ("registry-only", value)
    # Anything else (e.g. a bare table with neither version/git/path/workspace) is unrecognized;
    # treat as opaque so a real change here is never silently waved through.
    return ("opaque", value)


def parse_requirement(req):
    """Parse a Cargo version-requirement string into an ordered list of comparators.

    Each comparator is (kind, major, minor, patch, pre) with minor/patch/pre possibly None
    (Cargo allows partial versions like "1" or "1.2", and a prerelease suffix is optional).
    Comma-separated pieces are ALWAYS AND'd in Cargo's own grammar -- there is no OR syntax at
    all, unlike npm's "||" -- so this is just a flat list, no OR-alternatives to track
    (https://doc.rust-lang.org/cargo/reference/resolver.html).
    """
    pieces = [p.strip() for p in req.split(",") if p.strip()]
    if not pieces:
        raise VerifyError(f'empty version requirement "{req}"')
    return [parse_comparator(p, req) for p in pieces]


_COMPARATOR_RE = re.compile(
    r"^(?P<op>\^|~|=|>=|<=|>|<)?\s*(?P<major>\d+|\*)(?:\.(?P<minor>\d+|\*))?(?:\.(?P<patch>\d+|\*))?"
    r"(?:-(?P<pre>[0-9A-Za-z.\-]+))?$"
)


def parse_comparator(piece, full_req):
    m = _COMPARATOR_RE.match(piece)
    if not m:
        raise VerifyError(f'could not parse version-requirement piece "{piece}" in "{full_req}"')
    op = m.group("op")
    pre = m.group("pre")
    major_raw = m.group("major")
    minor_raw = m.group("minor")
    patch_raw = m.group("patch")
    if major_raw == "*":
        # Bare "*" -- Cargo Book: `* := >=0.0.0`, no ceiling at all. No numeric prefix survives a
        # wildcard at this position (there can't be one).
        return (WILDCARD, None, None, None, pre)
    major = int(major_raw)
    if minor_raw == "*":
        # "N.*" -- Cargo Book: `1.* := >=1.0.0, <2.0.0` (Codex P1, manta#194 round 2: an earlier
        # version collapsed EVERY wildcard shape to (WILDCARD, None, None, None), discarding the
        # numeric prefix entirely and skipping comparison outright -- "1.*" -> "2.*" reported
        # safe despite crossing a major boundary). Preserves major so compare_requirements can
        # apply the same major-boundary check every other kind gets.
        return (WILDCARD, major, None, None, pre)
    minor = int(minor_raw) if minor_raw is not None else None
    if patch_raw == "*":
        # "N.M.*" -- Cargo Book: `1.2.* := >=1.2.0, <1.3.0`.
        return (WILDCARD, major, minor, None, pre)
    patch = int(patch_raw) if patch_raw is not None else None
    kind_by_op = {"=": EXACT, ">": GT, ">=": GE, "<": LT, "<=": LE, "~": TILDE, "^": CARET}
    # No operator prefix at all is Cargo's own default, and it is caret -- NOT exact like npm's
    # bare default -- confirmed against the Cargo Book: `bar = "1.2.3"` means `^1.2.3`, and this
    # extends to caret's own 0.x special-casing (`"0.2.3"` behaves like `^0.2.3`, i.e.
    # >=0.2.3,<0.3.0; `"0.0.3"` behaves like `^0.0.3`, i.e. >=0.0.3,<0.0.4).
    kind = kind_by_op.get(op, CARET)
    return (kind, major, minor, patch, pre)


def _prerelease_sort_key(pre):
    """A sortable key for ONE dot-separated SemVer prerelease string, per SemVer 2.0.0 section 11:
    numeric identifiers compare numerically and always sort BEFORE alphanumeric ones at the same
    position; a prerelease with more identifiers is greater than an otherwise-equal, shorter one
    (Python tuple comparison already gives us that for free once each identifier is comparable).
    """
    return tuple((0, int(part)) if part.isdigit() else (1, part) for part in pre.split("."))


def version_sort_key(major, minor, patch, pre):
    """A fully-ordered sort key for one (major, minor, patch, pre) version, including SemVer's
    prerelease-precedence rule: a plain release is ALWAYS greater than any prerelease of the
    same major.minor.patch (Codex P1, manta#194 round 2: the previous floor_tuple ignored `pre`
    entirely, so "=1.0.0" -> "=1.0.0-alpha" -- a real downgrade per SemVer precedence -- compared
    as unchanged and passed the downgrade check).
    """
    base = (major, minor if minor is not None else 0, patch if patch is not None else 0)
    if pre is None:
        return (base, 1, ())
    return (base, 0, _prerelease_sort_key(pre))


def floor_tuple(comparator):
    _kind, major, minor, patch, pre = comparator
    return version_sort_key(major, minor, patch, pre)


def caret_ceiling_component(comparator):
    """The (position, value) pair identifying a caret comparator's ceiling: WHICH component
    bounds it, and that component's value.

    Cargo's own special-casing: the ceiling tracks the LEFTMOST significant (non-major-zero)
    component -- major normally, but minor for a 0.x major, and patch for a 0.0.x major/minor.
    Any change to that specific component is a ceiling change; changes to components to its
    right are pure floor movement within the same ceiling.

    Returning the bare numeric value alone, with no record of WHICH position it came from
    (Codex P1, manta#194 round 6), let two comparators whose ceilings sit at DIFFERENT positions
    collide on the same number: 0.2.3's ceiling is its minor (2), 2.0.0's ceiling is its major
    (2) -- both returned plain `2`, so every caller comparing them with `!=` (compare_requirements'
    CARET and EXACT/GT/GE branches, and check_lockfile_diff's resolved-version boundary check)
    read "0.2.3 -> 2.0.0" as unchanged and silently approved a full major bump. The position tag
    (0=major, 1=minor, 2=patch) makes a cross-position pair always compare unequal.
    """
    _kind, major, minor, patch, _pre = comparator
    if major != 0:
        return (0, major)
    if minor is None or minor != 0:
        return (1, minor)
    return (2, patch)


def compare_requirements(base_req, head_req):
    """Return None if head_req is a safe (non-downgrade, non-ceiling-widening) bump of
    base_req, or a string reason if not.
    """
    try:
        base_comparators = parse_requirement(base_req)
        head_comparators = parse_requirement(head_req)
    except VerifyError as e:
        return str(e)

    if len(base_comparators) != len(head_comparators):
        return (
            f'requirement structure changed ("{base_req}" -> "{head_req}") -- different number '
            "of comparators, this is not a version bump"
        )

    for base_cmp, head_cmp in zip(base_comparators, head_comparators):
        base_kind = base_cmp[0]
        head_kind = head_cmp[0]
        if base_kind != head_kind:
            return (
                f'requirement changed operator ("{base_req}" -> "{head_req}") -- '
                f"{base_kind} became {head_kind}, this is not a version bump"
            )

        if base_kind == WILDCARD:
            if base_cmp[1] is None:
                # Bare "*" -- >=0.0.0, no ceiling at all; nothing more permissive to widen into.
                continue
            if base_cmp[2] is None:
                # "N.*" -- >=N.0.0, <(N+1).0.0 (Cargo's own literal formula -- no 0.x
                # special-casing the way caret has; confirmed against the Cargo Book's own
                # `1.* := >=1.0.0, <2.0.0` template, which doesn't vary by N).
                if base_cmp[1] != head_cmp[1]:
                    return (
                        f'wildcard requirement crossed its own major-version boundary '
                        f'("{base_req}" -> "{head_req}") -- this is a major bump, not a safe '
                        "auto-approvable version change"
                    )
                continue
            # "N.M.*" -- >=N.M.0, <N.(M+1).0.
            if base_cmp[1] != head_cmp[1] or base_cmp[2] != head_cmp[2]:
                return (
                    f'wildcard requirement crossed its own minor-version boundary '
                    f'("{base_req}" -> "{head_req}") -- this is not a safe auto-approvable '
                    "version change"
                )
            continue

        if base_kind in (LT, LE):
            # Independent, explicitly-editable ceiling data -- Dependabot never touches a
            # range's ceiling, only its floor. Must stay byte-identical.
            if base_cmp != head_cmp:
                return (
                    f'requirement changed its upper bound ("{base_req}" -> "{head_req}") -- an '
                    f"explicit {base_kind} ceiling must stay identical, this is not a version bump"
                )
            continue

        base_floor = floor_tuple(base_cmp)
        head_floor = floor_tuple(head_cmp)
        if head_floor < base_floor:
            return (
                f'requirement decreased its version ("{base_req}" -> "{head_req}") -- a '
                "downgrade is never a safe auto-approvable bump"
            )

        if base_kind == CARET:
            # The ceiling is a MECHANICAL FUNCTION of the floor for caret (matching the exact
            # reasoning widdershins#421 rounds 19-21 converged on for npm's ^) -- but unlike npm,
            # Cargo's grammar keeps caret as a single comparator with the bound components
            # directly on it, so there is no separate desugared ceiling comparator to compare:
            # the ceiling-bounding component itself (see caret_ceiling_component) must be
            # unchanged, which IS the check that a caret bump stays within its own major (or
            # 0.x-adjusted) boundary.
            if caret_ceiling_component(base_cmp) != caret_ceiling_component(head_cmp):
                return (
                    f'caret requirement crossed its own major-version boundary '
                    f'("{base_req}" -> "{head_req}") -- this is a major bump, not a safe '
                    "auto-approvable version change"
                )
        elif base_kind == TILDE:
            if base_cmp[1] != head_cmp[1] or base_cmp[2] != head_cmp[2]:
                return (
                    f'tilde requirement crossed its own minor-version boundary '
                    f'("{base_req}" -> "{head_req}") -- this is not a safe auto-approvable '
                    "version change"
                )
        else:
            # EXACT/GT/GE (Codex P1-by-substance, manta#194 round 1): these have no explicit
            # ceiling comparator of their own, but that doesn't mean any floor increase is safe
            # regardless of size -- `=1.2.3` -> `=2.0.0` or `>=1.2.3` -> `>=2.0.0` both passed
            # the downgrade check above with no further scrutiny, silently accepting a major
            # bump this verifier exists specifically to catch (Dependabot's own update-type
            # classification is the thing this whole verifier distrusts, so a compromised/wrong
            # classification here would sail straight through). Reuse the same leading-
            # significant-component check caret already uses -- the concept of "does this cross
            # a major boundary" is identical regardless of which comparator kind is carrying the
            # floor.
            if caret_ceiling_component(base_cmp) != caret_ceiling_component(head_cmp):
                return (
                    f'{base_kind} requirement crossed a major-version boundary '
                    f'("{base_req}" -> "{head_req}") -- this is a major bump, not a safe '
                    "auto-approvable version change"
                )

    return None


def requirement_is_satisfied_by(req, version_str):
    """True if the concrete resolved version `version_str` satisfies every AND'd comparator in
    the Cargo requirement string `req` -- a full comparator-satisfaction predicate, not just a
    floor check.

    Consolidates three rounds of narrow, ad-hoc approximations in
    check_manifest_lockfile_consistency into one correctly-designed predicate, built entirely
    from primitives already tested across rounds 3-9 (floor_tuple, caret_ceiling_component,
    version_sort_key -- the SAME bucket logic check_lockfile_diff's own major/0.x-boundary check
    already relies on): round 8's first cut checked only the FLOOR (missing that a strict `>`
    excludes its own floor value -- fixed in round 9), and neither ever handled a dependency with
    MULTIPLE simultaneous lockfile candidates (a real diamond dependency, e.g. manta's own
    rand_core 0.9.5 AND 0.10.1 coexisting today) -- those names were skipped wholesale as
    "ambiguous," so raising a requirement while the wrong stale candidate remained locked passed
    unnoticed (Codex P1, manta#194 round 10). Rather than patch that skip with a fourth special
    case, this replaces the floor-only approximation outright: the caller now checks whether ANY
    of a name's lockfile candidates satisfies the full requirement, which is also the exact fix
    Codex asked for ("filter candidates by the declared requirement... reject when none
    satisfies it").

    Returns False (never raises) if `req` fails to parse -- fails toward "does not satisfy".
    """
    try:
        comparators = parse_requirement(req)
    except VerifyError:
        return False
    v_major, v_minor, v_patch, v_pre = _parse_bare_version(version_str)
    v_key = version_sort_key(v_major, v_minor, v_patch, v_pre)
    for comparator in comparators:
        kind, major, minor, _patch, _pre = comparator
        if kind == LT:
            if v_key >= floor_tuple(comparator):
                return False
        elif kind == LE:
            if v_key > floor_tuple(comparator):
                return False
        elif kind == GT:
            if v_key <= floor_tuple(comparator):
                return False
        elif kind == GE:
            if v_key < floor_tuple(comparator):
                return False
        elif kind == EXACT:
            if v_key != floor_tuple(comparator):
                return False
        elif kind == CARET:
            if v_key < floor_tuple(comparator):
                return False
            v_as_caret = (CARET, v_major, v_minor, v_patch, v_pre)
            if caret_ceiling_component(v_as_caret) != caret_ceiling_component(comparator):
                return False
        elif kind == TILDE:
            if v_key < floor_tuple(comparator):
                return False
            if v_major != major or (minor is not None and v_minor != minor):
                return False
        elif kind == WILDCARD:
            if major is not None and v_major != major:
                return False
            if minor is not None and v_minor != minor:
                return False
        else:
            return False
    if v_pre is not None:
        # Cargo's prerelease ELIGIBILITY rule (Codex P1, manta#194 round 13, Finding Q) -- a
        # policy Cargo layers ON TOP of SemVer, distinct from the numeric precedence comparisons
        # above (which already correctly rank a prerelease below its own release, per SemVer
        # 2.0.0 section 11 -- the same version_sort_key logic used throughout this file). SemVer
        # precedence alone would let "1.1.0-alpha" satisfy ">=1.0.0" (1.1.0-alpha ranks above
        # 1.0.0), but Cargo EXCLUDES a prerelease unless the requirement itself explicitly opts
        # in by naming a comparator with the exact SAME major.minor.patch that also carries a
        # prerelease tag -- ">=1.1.0-alpha" would opt in for 1.1.0's own prereleases, but plain
        # ">=1.0.0" never does, for any prerelease of any version.
        opted_in = any(
            c[1] == v_major and c[2] == v_minor and c[3] == v_patch and c[4] is not None
            for c in comparators
        )
        if not opted_in:
            return False
    return True


def collect_dependency_declarations(pkg_toml):
    """Flatten every dependency table in a parsed Cargo.toml into {name: normalized value}."""
    out = {}
    for table_name in DEPENDENCY_TABLES:
        for name, value in pkg_toml.get(table_name, {}).items():
            out[(table_name, name)] = normalize_dependency(value)
    workspace = pkg_toml.get("workspace", {})
    for name, value in workspace.get("dependencies", {}).items():
        out[("workspace.dependencies", name)] = normalize_dependency(value)
    # `[target.'cfg(...)'.dependencies]` (and its dev-/build- siblings) is a real, currently-used
    # shape in this repo (Codex P1, manta#194 round 1: windows-sys under
    # crates/manta-engine/Cargo.toml's `[target.'cfg(windows)'.dependencies]`) -- ignoring it
    # entirely meant a dependency declared ONLY under a target selector could bump across a major
    # boundary with no reason ever raised. Keyed by (target.<selector>.<table>, name) so a
    # dependency moving between selectors, or between a target table and a top-level one, shows
    # up as an add+remove (routed to human review by the existing added/removed handling) rather
    # than silently comparing across two different scopes.
    for selector, tables in pkg_toml.get("target", {}).items():
        for table_name in DEPENDENCY_TABLES:
            for name, value in tables.get(table_name, {}).items():
                out[(f"target.{selector}.{table_name}", name)] = normalize_dependency(value)
    return out


def strip_dependency_tables(pkg_toml):
    """A deep copy of a parsed Cargo.toml with every table this verifier inspects removed.

    Leaves everything else -- [features], [profile.*], [patch.*], [package] metadata, and so on
    -- for the caller to compare byte-for-byte against the other side of the diff (Codex P1,
    manta#194 round 2): check_manifest_diff/collect_dependency_declarations only ever validate
    the dependency tables they enumerate, so a change anywhere ELSE in the manifest -- e.g. a
    new `[patch.crates-io]` entry, which can redirect a dependency's resolution without ever
    touching that dependency's own `[dependencies]` entry at all -- previously produced no
    reason whatsoever. `main()` rejects any diff where this residual structure differs.
    """
    stripped = copy.deepcopy(pkg_toml)
    for table_name in DEPENDENCY_TABLES:
        stripped.pop(table_name, None)
    if "workspace" in stripped:
        stripped["workspace"] = {
            k: v for k, v in stripped["workspace"].items() if k != "dependencies"
        }
    if "target" in stripped:
        filtered_targets = {}
        for selector, tables in stripped["target"].items():
            remaining = {k: v for k, v in tables.items() if k not in DEPENDENCY_TABLES}
            if remaining:
                filtered_targets[selector] = remaining
        if filtered_targets:
            stripped["target"] = filtered_targets
        else:
            stripped.pop("target", None)
    return stripped


def check_manifest_diff(path, base_toml, head_toml):
    """Compare one Cargo.toml's dependency declarations between base and head.

    Returns a list of reason strings (empty if the diff at this path is entirely safe).
    """
    if strip_dependency_tables(base_toml) != strip_dependency_tables(head_toml):
        return [
            f'"{path}" changed outside its dependency declarations (e.g. [features], '
            "[profile.*], [patch.*], or [package] metadata) -- not a plain version-requirement "
            "bump"
        ]
    base_decls = collect_dependency_declarations(base_toml)
    head_decls = collect_dependency_declarations(head_toml)
    reasons = []
    for key in set(base_decls) | set(head_decls):
        table_name, dep_name = key
        base_decl = base_decls.get(key)
        head_decl = head_decls.get(key)
        if base_decl is None or head_decl is None:
            # A dependency being added or removed entirely is not a "version bump" -- Dependabot
            # itself never does this, so treat any appearance/disappearance as out of scope for
            # this verifier and let it fall through to human review.
            reasons.append(
                f'"{path}" {table_name}.{dep_name} was added or removed -- not a version bump'
            )
            continue
        base_kind, base_detail = base_decl
        head_kind, head_detail = head_decl
        if base_kind != head_kind:
            reasons.append(
                f'"{path}" {table_name}.{dep_name} changed declaration kind '
                f"({base_kind} -> {head_kind}) -- a source-type change, not a version bump"
            )
            continue
        if base_kind != "version":
            # git/path/workspace/registry-only/opaque: kind is unchanged, but there is no bare
            # requirement string for THIS verifier to compare further (a git ref/rev change, for
            # instance, is a real content change this verifier doesn't attempt to adjudicate --
            # left to human review rather than waved through).
            if base_decl != head_decl:
                reasons.append(
                    f'"{path}" {table_name}.{dep_name} changed its {base_kind} declaration -- '
                    "not a plain version-requirement bump"
                )
            continue
        base_req, base_rest = base_detail
        head_req, head_rest = head_detail
        if base_rest != head_rest:
            # Codex P1, manta#194 round 1: features/default-features/optional/package/registry
            # (everything in an inline version-table besides the version string itself) must
            # stay byte-identical -- `package` in particular is Cargo's own alias mechanism
            # (this key stays the same, but the crate it resolves to changes), directly
            # analogous to npm's alias-swap attacks this verifier's widdershins counterpart
            # already defends against.
            reasons.append(
                f'"{path}" {table_name}.{dep_name} changed a non-version field '
                f"({base_rest} -> {head_rest}) -- not a plain version-requirement bump"
            )
            continue
        if base_req == head_req:
            continue
        reason = compare_requirements(base_req, head_req)
        if reason is not None:
            reasons.append(f'"{path}" {table_name}.{dep_name} {reason}')
    return reasons


def parse_lockfile_packages(lock_toml):
    """Return {(name, source_kind, version, source): entry} for every [[package]] in a parsed
    Cargo.lock.

    Keyed by the package's FULL identity -- name, source_kind, version, AND the raw `source`
    string itself (Codex P1, manta#194 round 5): Cargo's own uniqueness invariant for a
    [[package]] entry is name+version+source together, not name+kind+version. A diamond
    dependency can legitimately pull in the SAME name and version from two different sources of
    the same broad kind (e.g. two different git URLs) -- each gets its own [[package]] entry.
    Keying on (name, kind, version) alone made those two entries collide on the same dict key, so
    Python's plain assignment silently dropped one of them during parsing -- it never reached any
    check below, however careful, because it was discarded before any of them ran. Including the
    full source string in the key means two colliding entries always land on distinct keys, so
    neither is lost; a change to only ONE of them then surfaces normally as a removed+added pair
    (caught by the source-identity check below) or, if the source string itself stays byte-for-
    byte identical, as a retained-entry field change (checksum/dependencies).
    """
    out = {}
    for entry in lock_toml.get("package", []):
        name = entry.get("name")
        source = entry.get("source")
        if source is None:
            kind = "workspace"
        elif source.startswith("registry+"):
            kind = "registry"
        elif source.startswith("git+"):
            kind = "git"
        else:
            kind = "unknown"
        out[(name, kind, entry.get("version"), source)] = entry
    return out


def _source_identity(kind, source):
    """The part of a lockfile entry's `source` string that identifies WHERE it comes from,
    excluding the part a legitimate version bump is expected to change.

    Registry sources never embed a version -- the whole string IS the identity, so any change at
    all is a source swap (Codex P1, manta#194 round 4: comparing only `kind` treated any two
    "registry" sources, or any two "git" sources, as equivalent regardless of which actual URL/ref
    they named). Git sources embed the resolved commit as a trailing `#<sha>` fragment, which is
    exactly what changes on a normal Dependabot bump (a new commit on the same pinned
    branch/tag/rev) -- everything BEFORE that fragment (host, path, and any branch/tag/rev query
    parameters) is the identity that must stay fixed.
    """
    if source is None:
        return None
    if kind == "git":
        base_part, _, _ = source.rpartition("#")
        return base_part or source
    return source


def _index_pkgs_by_name(pkgs):
    """{name: [full (name, kind, version, source) keys]} for one side of a parsed Cargo.lock."""
    out = {}
    for key in pkgs:
        out.setdefault(key[0], []).append(key)
    return out


def _resolve_dependency_ref(ref, candidates):
    """Resolve one `dependencies` list entry ("name", "name version", or "name version (source)")
    to the exact (name, kind, version, source) key it points to, among `candidates` (every key on
    this same side of the diff sharing that name). Returns None if resolution is ambiguous or
    impossible -- callers must treat None as "could not confirm unchanged," never as "unchanged."

    Cargo's own lockfile encoding (confirmed against cargo's resolver/encode.rs and the Cargo
    Book): version/source are appended ONLY to disambiguate multiple resolved instances of a name
    elsewhere in the graph. A crate name can't contain a space, so splitting on the first one is
    exact.
    """
    name, _, rest = ref.partition(" ")
    if not rest:
        return candidates[0] if len(candidates) == 1 else None
    if rest.endswith(")") and " (" in rest:
        version_part, _, source_part = rest.partition(" (")
        source_part = source_part[:-1]
    else:
        version_part, source_part = rest, None
    matches = [
        key
        for key in candidates
        if key[2] == version_part and (source_part is None or key[3] == source_part)
    ]
    return matches[0] if len(matches) == 1 else None


def _resolved_dependency_identities(deps, own_by_name, other_by_name):
    """Resolve a `dependencies` list to the set of identities it references, for comparison
    against the SAME package's dependency list on the other side of the diff.

    For a name that never has more than one simultaneous candidate on EITHER side (`own_by_name`
    or `other_by_name`), resolves to the bare NAME alone regardless of version/source (Codex P1,
    manta#194 round 8, fixing a regression this round's own first cut introduced -- caught while
    re-verifying against manta's real Dependabot history, not from a Codex comment): when a
    name's sole resolved instance simply bumps version between base and head (the ordinary case,
    e.g. skimmer-cli's edge to "clap" when clap itself bumps), that instance's own version-level
    safety is ALREADY independently verified via clap's own [[package]] entry checks elsewhere in
    check_lockfile_diff -- resolving the edge to its full identity re-flagged it as "changed" on
    every such bump, which is pure noise, not a coverage gap.

    Only when a name has 2+ simultaneous candidates on EITHER side (round 8, Finding I's actual
    diamond-dependency scenario) does this fall through to full identity resolution, since only
    then can an edge meaningfully be redirected between two coexisting versions with no
    accompanying removed+added pair to catch it. An unresolvable ref in that case (ambiguous, or
    naming a crate with no matching [[package]] entry at all) falls back to a sentinel keyed on
    its own raw text -- two unresolvable refs only compare equal if their raw text is identical,
    so this never silently treats a real change as unchanged, it only ever fails toward "flag it."
    """
    resolved = set()
    for ref in deps:
        name = ref.split(" ", 1)[0]
        own_candidates = own_by_name.get(name, [])
        other_candidates = other_by_name.get(name, [])
        if len(own_candidates) <= 1 and len(other_candidates) <= 1:
            resolved.add(name)
            continue
        key = _resolve_dependency_ref(ref, own_candidates)
        resolved.add(key if key is not None else ("__unresolved__", ref))
    return resolved


def _resolved_dependency_source_identities(deps, own_by_name, other_by_name):
    """Like _resolved_dependency_identities, but resolves each edge to (name, kind,
    source_identity) rather than the full (name, kind, version, source) key -- used for a
    MATCHED REPLACEMENT entry's own dependencies field (round 12/13), where the full-identity
    comparison is too strict: it flagged manta's own real fb1c7f3 commit, where clap_derive
    legitimately shifted which VERSION of its existing `syn` dependency it needs between its own
    two published releases (round 12). Dropping version from the comparison tolerates that
    ordinary crate evolution while still catching a KIND or SOURCE swap on the same name (Codex
    P1, manta#194 round 13, Finding P: an edge redirected from a registry-sourced "bar" to a
    newly added git-sourced "bar" of the same name and version -- round 12's bare-name-only
    comparison couldn't see this at all, since the name never changed).

    Same ambiguity-aware fallback as _resolved_dependency_identities: a name with at most one
    simultaneous candidate on either side resolves to its bare name (no coverage loss, since
    kind/source can't meaningfully "swap" without either an old entry disappearing -- caught by
    the kind/source-identity checks on that name's own matched pair -- or a genuine diamond
    appearing, which is exactly the >1-candidate case this falls through to below).
    """
    resolved = set()
    for ref in deps:
        name = ref.split(" ", 1)[0]
        own_candidates = own_by_name.get(name, [])
        other_candidates = other_by_name.get(name, [])
        if len(own_candidates) <= 1 and len(other_candidates) <= 1:
            resolved.add(name)
            continue
        key = _resolve_dependency_ref(ref, own_candidates)
        if key is None:
            resolved.add(("__unresolved__", ref))
        else:
            _rname, rkind, _rversion, rsource = key
            resolved.add((name, rkind, _source_identity(rkind, rsource)))
    return resolved


def check_lockfile_diff(base_lock, head_lock):
    """Compare Cargo.lock's [[package]] entries between base and head.

    Cargo.lock has no hoisting/aliasing equivalent (unlike npm's package-lock.json) -- there is
    exactly one flat array of packages, each self-describing its own name/version/source/
    checksum, so this is a much narrower check than widdershins' own lockfile-identity work: a
    package entry disappearing/appearing is either a legitimate version-bump replacement (an old
    (name, kind, version) key removed, a new one with a HIGHER version and the SAME kind added)
    or something this verifier defers to human review.
    """
    base_pkgs = parse_lockfile_packages(base_lock)
    head_pkgs = parse_lockfile_packages(head_lock)
    base_by_name = _index_pkgs_by_name(base_pkgs)
    head_by_name = _index_pkgs_by_name(head_pkgs)
    reasons = []

    removed = set(base_pkgs) - set(head_pkgs)
    added = set(head_pkgs) - set(base_pkgs)

    # Group removed/added entries by NAME ALONE -- not (name, kind) (Codex P1, manta#194 round
    # 2): a same-name registry-to-git swap produces one removed (name, "registry", old_version)
    # key and one added (name, "git", new_version) key. Grouping by (name, kind) put those in
    # two DIFFERENT buckets that never got matched to each other -- the removed entry's bucket
    # had no added peer and was silently skipped (a stale comment here claimed this was "caught
    # by the registry<->git checks below," but no such check existed). Grouping by name alone
    # puts both in the SAME bucket, where the kind mismatch is directly visible and rejected.
    def by_name(keys):
        grouped = {}
        for name, kind, version, source in keys:
            grouped.setdefault(name, []).append((kind, version, source))
        return grouped

    removed_grouped = by_name(removed)
    added_grouped = by_name(added)

    for name, removed_entries in removed_grouped.items():
        added_entries = added_grouped.pop(name, [])
        if not added_entries:
            # Disappeared with no same-name replacement at all (Codex P1, manta#194 round 7):
            # this was PREVIOUSLY treated as an out-of-scope legitimate removal on the theory
            # that "a real removal is visible at the manifest level too" -- true for a DIRECTLY
            # declared dependency, but false for a TRANSITIVE one, which has no manifest
            # declaration for check_manifest_diff to ever see. An unmatched transitive removal
            # therefore reached neither check at all and was silently accepted. This verifier has
            # no independent way to confirm a disappearance is a legitimate consequence of the
            # rest of the dependency graph changing (that requires actually re-resolving the
            # graph, which is exactly what this Python/tomllib-only verifier deliberately does
            # NOT attempt) versus an unexplained drop riding along with the diff -- so, matching
            # this file's existing bias elsewhere (see the len()!=1 branch just below), route it
            # to human review rather than assume it's safe.
            reasons.append(
                f'Cargo.lock package "{name}" was removed with no same-name replacement -- not '
                "verifiable as a safe consequence of the rest of the dependency-graph change"
            )
            continue
        if len(removed_entries) != 1 or len(added_entries) != 1:
            # Also the (safe-direction) outcome for two independent major-version-lines of the
            # same crate (e.g. serde v1 and v2 both present via a diamond dependency) both
            # bumping in the same diff -- routes to human review rather than risk conflating
            # them, same tradeoff this check already made before this fix.
            reasons.append(
                f'Cargo.lock package "{name}" changed in a way with more than one version '
                "added/removed at once -- not a simple version bump"
            )
            continue
        (old_kind, old_v, old_source), (new_kind, new_v, new_source) = (
            removed_entries[0],
            added_entries[0],
        )
        if old_kind != new_kind:
            reasons.append(
                f'Cargo.lock package "{name}" changed source kind ({old_kind} -> {new_kind}) -- '
                "not a plain version bump"
            )
            continue
        # Source identity check (Codex P1, manta#194 round 4): `old_kind == new_kind` alone
        # treats any two "git" (or "registry") sources as equivalent regardless of which URL/ref
        # they actually name, so a same-kind source swap riding along with a version bump (e.g.
        # a git dependency's URL silently changed to a different host) passed unnoticed.
        old_identity = _source_identity(old_kind, old_source)
        new_identity = _source_identity(new_kind, new_source)
        if old_identity != new_identity:
            reasons.append(
                f'Cargo.lock package "{name}" ({old_kind}) changed source identity '
                f"({old_identity} -> {new_identity}) -- not a verified version bump"
            )
            continue
        # Same-version git commit swap (Codex P1, manta#194 round 11): _source_identity
        # deliberately strips a git source's `#<sha>` fragment so an ordinary branch/tag-pinned
        # bump (a new commit, a version bump reported by that commit's own Cargo.toml) doesn't
        # get rejected as a "source identity change." But a git dependency's `version` field is
        # entirely SELF-reported by the pinned commit's own Cargo.toml -- unlike a registry
        # crate, nothing polices it, so it carries none of the safety signal the downgrade/
        # major-boundary checks below assume. A malicious commit swap on the SAME branch/tag,
        # crafted to report the SAME version string as before, sails through every check that
        # follows (no downgrade, no boundary crossed) with zero independent evidence the swap is
        # legitimate. Reject it outright rather than trust a self-reported version that didn't
        # even change.
        # Codex P1, manta#194 round 12, Finding N: comparing `old_v == new_v` as raw STRINGS
        # missed that SemVer 2.0.0 build metadata (a trailing `+...`) never affects version
        # identity/precedence -- "1.2.3+old" and "1.2.3+new" are the SAME version, but compare
        # unequal as strings, so a malicious commit swap reporting a cosmetically different build
        # tag would have slipped past the very check round 11 added to catch exactly this.
        # _parse_bare_version already strips build metadata (used everywhere else in this file
        # for exactly that reason) -- compare through it instead of the raw string.
        old_bare = _parse_bare_version(old_v) if old_v is not None else None
        new_bare = _parse_bare_version(new_v) if new_v is not None else None
        if old_kind == "git" and old_source != new_source and old_bare == new_bare:
            reasons.append(
                f'Cargo.lock package "{name}" (git) changed its pinned commit '
                f"({old_source} -> {new_source}) while reporting the SAME version ({old_v}) -- "
                "not independently verifiable as a safe update"
            )
            continue
        if new_v is None or old_v is None:
            continue
        if _version_tuple(new_v) < _version_tuple(old_v):
            reasons.append(
                f'Cargo.lock package "{name}" ({old_kind}) downgraded ({old_v} -> {new_v})'
            )
            continue
        # Major/0.x-boundary check on the RESOLVED version itself (Codex P1, manta#194 round 3,
        # Finding E): the manifest-level check (compare_requirements) only ever sees an edit to
        # the manifest's OWN requirement string -- when that requirement is already maximally
        # permissive (e.g. `demo = "*"`), a resolved-version jump from 1.x to 2.x reaches this
        # point with no manifest edit at all to catch it, and the downgrade check above passes it
        # (it's an increase, not a downgrade). Reuses the same ceiling rule compare_requirements
        # applies to a manifest-level caret requirement, applied here to synthetic caret
        # comparators built from the two resolved versions.
        old_major, old_minor, old_patch, old_pre = _parse_bare_version(old_v)
        new_major, new_minor, new_patch, new_pre = _parse_bare_version(new_v)
        old_ceiling = caret_ceiling_component((CARET, old_major, old_minor, old_patch, old_pre))
        new_ceiling = caret_ceiling_component((CARET, new_major, new_minor, new_patch, new_pre))
        if old_ceiling != new_ceiling:
            reasons.append(
                f'Cargo.lock package "{name}" ({old_kind}) crossed a major/0.x boundary in its '
                f"resolved version ({old_v} -> {new_v}) -- a permissive manifest requirement can "
                "mask this, so it is checked independently here"
            )
        # Dependency-edge validation on a REPLACEMENT entry (Codex P1, manta#194 round 12,
        # Finding O; refined round 13, Finding P): none of the checks above ever look at the NEW
        # entry's own `dependencies` field -- only the retained-entries loop below does that,
        # which by definition never runs for a matched removed/added pair (its key, by
        # definition, changed). A version bump can ride along with an edge redirected to a
        # brand-new, or differently-sourced, otherwise-unvalidated dependency with no reason ever
        # raised.
        #
        # Compares by (name, kind, source_identity) here via
        # _resolved_dependency_source_identities -- NOT the full (name, kind, version, source)
        # identity the retained-entries loop uses for round 8's Finding I, and NOT bare name
        # alone either (round 12's first cut, which round 13's Finding P showed missed a kind/
        # source swap: "bar" registry -> "bar" git keeps the same name, so a bare-name set
        # comparison saw no change at all). Dropping VERSION specifically (unlike Finding I's
        # comparison) tolerates a version-only retarget: a MATCHED REPLACEMENT entry's own
        # version already changed and passed the downgrade/major-boundary checks above, and a
        # published crate legitimately shifting which version of an EXISTING same-kind/source
        # dependency it needs between its own two releases is routine, not suspicious -- manta's
        # own real commit fb1c7f3 hit exactly this (clap_derive 4.6.1 -> 4.6.4 shifted its own
        # `syn` edge from a 2.x instance to a newly-available 3.x one, both registry-sourced) and
        # would have been wrongly rejected by full identity comparison.
        old_entry = base_pkgs[(name, old_kind, old_v, old_source)]
        new_entry = head_pkgs[(name, new_kind, new_v, new_source)]
        old_dep_ids = _resolved_dependency_source_identities(
            old_entry.get("dependencies", []), base_by_name, head_by_name
        )
        new_dep_ids = _resolved_dependency_source_identities(
            new_entry.get("dependencies", []), head_by_name, base_by_name
        )
        if old_dep_ids != new_dep_ids:
            reasons.append(
                f'Cargo.lock package "{name}" ({old_kind}) changed its dependencies alongside '
                f"its version bump ({old_v} -> {new_v}) -- not a plain version bump"
            )

    # A name that only ever appears in `added` (never matched to a `removed` peer, and never
    # popped from `added_grouped` by the loop above) is a brand-new dependency in the lockfile --
    # ordinarily a fresh transitive pulled in by a manifest bump, and not this verifier's concern
    # per se (Cargo itself already validated the lockfile's own internal consistency via `cargo
    # generate-lockfile`/`cargo update` when it produced this diff). But Codex P1, manta#194
    # round 15, Finding U: an added entry that NOTHING in the head lockfile's own dependency graph
    # references at all is not explainable as a consequence of anything else in the diff -- Cargo
    # itself would never add such an orphaned package via a normal update, so its presence here
    # is unverifiable. Checked by bare NAME (not exact identity) against every OTHER head entry's
    # `dependencies` list, deliberately erring toward NOT flagging when the name is referenced by
    # anything at all, even ambiguously -- this is a narrow "wholly unreferenced" check, not a
    # full graph-reachability validation.
    referenced_names = set()
    for entry in head_pkgs.values():
        for ref in entry.get("dependencies", []):
            referenced_names.add(ref.split(" ", 1)[0])
    for added_name, added_entries in added_grouped.items():
        if added_name in referenced_names:
            continue
        for kind, version, _source in added_entries:
            reasons.append(
                f'Cargo.lock package "{added_name}" ({kind}, {version}) was added but is not '
                "referenced by any dependency edge in the head lockfile -- not verifiable as a "
                "safe consequence of the rest of the dependency-graph change"
            )

    # Retained entries -- same (name, kind, version, source) key present in BOTH base and head --
    # are invisible to the removed/added set difference above (Codex P1, manta#194 round 3,
    # Finding F). Since round 5's fix folded the raw `source` string into the key itself, a
    # retained key now also means `source` stayed byte-for-byte identical -- what's left that can
    # still differ underneath an identical key is `checksum`/`dependencies` (a git rev swap now
    # surfaces earlier, as a removed+added pair the source-identity check above catches, because
    # a rev change changes `source` and therefore the key). Compare the full entry dicts for
    # every retained key regardless.
    for key in set(base_pkgs) & set(head_pkgs):
        base_entry = base_pkgs[key]
        head_entry = head_pkgs[key]
        # `dependencies` needs its own comparison, not byte equality (found empirically while
        # verifying round 7's fix against manta's real Dependabot history, not from a Codex
        # comment): Cargo's own lockfile encoding writes each entry as "name", "name version", or
        # "name version (source)" -- appending version/source ONLY when needed to disambiguate
        # multiple resolved instances of that name elsewhere in the graph (confirmed against
        # cargo's resolver/encode.rs and the Cargo Book). A completely unrelated bump that shifts
        # how many versions of some OTHER crate (e.g. `syn`) are present can flip every entry
        # that depends on it between "syn" and "syn 2.0.118" with no actual change to what code
        # runs -- manta's own real bump commit fb1c7f3 (clap 4.6.1 -> 4.6.4, otherwise a plain
        # patch bump) hit exactly this on ~10 unrelated proc-macro crates and would have been
        # rejected outright by a byte-for-byte comparison.
        #
        # Comparing by bare crate NAME ALONE (round 7's first attempt) was itself a real gap
        # (Codex P1, manta#194 round 8, Finding I): when two versions of the same crate coexist
        # (a diamond dependency), a retained entry's edge changing from "foo 1.0.0" to a newly
        # ADDED "foo 2.0.0" -- while another consumer keeps "foo 1.0.0", so foo 1.0.0's own
        # [[package]] entry is RETAINED, not removed -- collapsed both refs to the bare name
        # "foo" and read as unchanged; the new "foo 2.0.0" entry, added with no matching removal,
        # is separately out of scope (see the comment above this loop). A real major-version or
        # source transition on that one edge passed unnoticed. Resolving each ref to the exact
        # package identity it points to (via _resolved_dependency_identities) closes this: "syn"
        # and "syn 2.0.118" still resolve to the SAME single syn entry when only one exists (the
        # benign case above), but "foo 1.0.0" and "foo 2.0.0" resolve to two DIFFERENT entries
        # when both exist, so the edge change is correctly visible.
        base_fields = dict(base_entry)
        head_fields = dict(head_entry)
        base_dep_ids = _resolved_dependency_identities(
            base_fields.pop("dependencies", []), base_by_name, head_by_name
        )
        head_dep_ids = _resolved_dependency_identities(
            head_fields.pop("dependencies", []), head_by_name, base_by_name
        )
        changed_fields = sorted(
            f
            for f in set(base_fields) | set(head_fields)
            if base_fields.get(f) != head_fields.get(f)
        )
        if base_dep_ids != head_dep_ids:
            changed_fields.append("dependencies")
        if changed_fields:
            name, kind, version, _source = key
            reasons.append(
                f'Cargo.lock package "{name}" ({kind}, {version}) has an unchanged name/kind/'
                f"version/source but its {', '.join(changed_fields)} field(s) changed -- not a "
                "verified version bump"
            )

    return reasons


def _parse_bare_version(version_str):
    """Parse major/minor/patch/pre out of a resolved Cargo.lock `version` string (real SemVer
    2.0.0 syntax, not a Cargo requirement). Build metadata is discarded outright (per SemVer
    2.0.0 section 10, it never affects ordering or a major-boundary determination).
    """
    without_build = version_str.split("+", 1)[0]
    core, _, pre = without_build.partition("-")
    pre = pre or None
    parts = core.split(".")
    nums = []
    for p in parts[:3]:
        digits = re.match(r"\d+", p)
        nums.append(int(digits.group()) if digits else 0)
    while len(nums) < 3:
        nums.append(0)
    major, minor, patch = nums
    return major, minor, patch, pre


def _version_tuple(version_str):
    """A fully-ordered sort key for a resolved Cargo.lock `version` string.

    Delegates to version_sort_key for the prerelease-precedence rule (Codex P1, manta#194 round
    2: an earlier version ignored the prerelease suffix entirely, so a Cargo.lock entry going
    from "1.0.0" to "1.0.0-alpha" -- a real downgrade per SemVer precedence -- compared as
    unchanged and passed the downgrade check).
    """
    major, minor, patch, pre = _parse_bare_version(version_str)
    return version_sort_key(major, minor, patch, pre)


def check_manifest_lockfile_consistency(head_tomls, head_lock):
    """Confirm the HEAD lockfile actually satisfies the HEAD manifest's own requirements (Codex
    P1, manta#194 round 8, Finding J; refined rounds 9-10).

    compare_requirements only ever sees an EDIT to a requirement string; check_lockfile_diff only
    ever sees an EDIT to a lockfile entry. A manifest requirement raised beyond an already-
    unchanged locked version -- e.g. `demo = "1.0"` -> `"1.1"` while Cargo.lock retains "1.0.5" --
    triggers neither: compare_requirements sees a non-major bump and passes it, and
    check_lockfile_diff sees no diff at all for "demo" (its lockfile entry didn't change). The
    verifier previously reported SAFE without ever confirming the head lock and head manifest are
    mutually consistent, which the head lock genuinely fails here (1.0.5 doesn't satisfy 1.1).

    Checks every HEAD-declared version-kind dependency against ALL of its lockfile candidates via
    requirement_is_satisfied_by, not just ones whose requirement changed -- this is a validity
    check on the resulting state, not a diff (matching Codex's own suggested remedy: validate the
    combined manifest/lock state). A name is only flagged when NONE of its candidates satisfy the
    requirement -- e.g. manta's own real rand_core 0.9.5-and-0.10.1 diamond is fine as long as
    0.10.1 itself satisfies whatever rand_core's own requirement currently is; round 8/9 skipped
    any name with more than one candidate outright, which is exactly what let a genuine
    inconsistency hide behind an unrelated coexisting version (round 10, Finding L).

    Looks up lockfile candidates by the declared `package` rename field when present, falling
    back to the dependency's own key otherwise (Codex P1, manta#194 round 13, Finding R): Cargo's
    rename mechanism (`alias = { package = "foo", version = "1.1" }`) declares the dependency
    under key "alias" but Cargo.lock records the ACTUAL crate under its real name "foo" -- looking
    up "alias" in the lockfile finds nothing, so every renamed dependency's requirement silently
    skipped this whole check (the `if not versions: continue` guard below, meant for a genuinely
    out-of-scope path/workspace-only dep, was quietly absorbing every rename too).
    """
    reasons = []
    pkgs_by_name = _index_pkgs_by_name(parse_lockfile_packages(head_lock))
    for path, head_toml in head_tomls.items():
        for (table_name, dep_name), (kind, detail) in collect_dependency_declarations(
            head_toml
        ).items():
            if kind != "version":
                continue
            req, rest = detail
            lockfile_name = rest.get("package", dep_name)
            # Restrict candidates to a source KIND consistent with a bare version-requirement
            # declaration -- "registry" (crates.io or an alternate registry override) or
            # "unknown" (a non-git/registry source prefix this file doesn't further classify),
            # never "git" or "workspace" (Codex P1, manta#194 round 14): filtering by name alone
            # let an UNRELATED same-named git-sourced diamond entry's version satisfy a plain
            # registry declaration whose own (stale) registry entry didn't -- e.g. `foo = "1.0"`
            # -> `"1.1"` with a stale registry `foo 1.0` retained alongside a coincidentally
            # same-named `foo 1.1` git entry pulled in by something else entirely.
            candidates = [
                key for key in pkgs_by_name.get(lockfile_name, []) if key[1] in ("registry", "unknown")
            ]
            # Fail closed on 2+ DISTINCT registry sources among the candidates (Codex P1,
            # manta#194 round 15, Finding T): round 14's kind filter excludes git/workspace, but
            # "registry" alone doesn't distinguish crates.io from a NAMED alternate registry
            # (`{ registry = "private" }`) -- resolving a registry NAME to its actual index URL
            # would need this repo's `.cargo/config.toml`, which this verifier doesn't read. A
            # stale private-registry `foo 1.0` could otherwise be masked by an unrelated
            # crates.io `foo 1.1` of the same name. Two distinct registry sources for the same
            # name is rare for a genuine single-registry project (manta's own real Cargo.lock has
            # none) -- when it happens, this verifier can't safely tell a legitimate multi-
            # registry setup apart from the exact attack Codex describes, so it doesn't guess.
            registry_sources = {key[3] for key in candidates}
            if len(registry_sources) > 1:
                reasons.append(
                    f'"{path}" {table_name}.{dep_name} has {len(registry_sources)} distinct '
                    "registry sources among its Cargo.lock candidates -- cannot verify which one "
                    "the manifest's own declaration resolves to"
                )
                continue
            versions = sorted({key[2] for key in candidates if key[2] is not None})
            if not versions:
                continue
            if not any(requirement_is_satisfied_by(req, version) for version in versions):
                reasons.append(
                    f'"{path}" {table_name}.{dep_name} requires "{req}" but no Cargo.lock entry '
                    f"for it ({', '.join(versions)}) satisfies that requirement -- the head lock "
                    "does not satisfy the head manifest"
                )
    return reasons


def check_diff_scope(base_ref, head_ref, allowed_paths):
    """Reject any diff that touches a path outside `allowed_paths` (Codex P1, manta#194 round 1).

    Every other check in this file only ever inspects the enumerated Cargo.toml/Cargo.lock paths
    -- reporting SAFE never meant "the whole diff is safe," only "the files I looked at changed
    safely." A compromised Dependabot commit could add arbitrary Rust source or workflow changes
    alongside an acceptable dependency bump and this verifier would never even look at them.
    Confirmed reproducible: running this verifier from this very PR's own parent commit to this
    commit (which changes only .github/workflows/ and scripts/, no Cargo.toml/Cargo.lock at all)
    returned SAFE before this check existed.
    """
    result = subprocess.run(
        ["git", "diff", "--name-only", base_ref, head_ref],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        return [f"could not compute the full diff between {base_ref} and {head_ref}"]
    changed = [line for line in result.stdout.splitlines() if line]
    out_of_scope = [path for path in changed if path not in allowed_paths]
    if out_of_scope:
        listed = ", ".join(f'"{p}"' for p in out_of_scope)
        return [
            f"the diff touches {listed}, outside the enumerated Cargo.toml/Cargo.lock paths -- "
            "not confined to a dependency-version bump"
        ]
    return []


def main(base_ref, head_ref):
    reasons = []

    manifest_paths = ["Cargo.toml"] + [f"{m}/Cargo.toml" for m in WORKSPACE_MEMBERS]
    reasons.extend(
        check_diff_scope(base_ref, head_ref, set(manifest_paths) | {"Cargo.lock"})
    )

    head_tomls = {}
    for path in manifest_paths:
        base_text = git_show(base_ref, path)
        head_text = git_show(head_ref, path)
        if base_text is None and head_text is None:
            continue
        if base_text is None or head_text is None:
            reasons.append(f'"{path}" was added or removed entirely -- not a version bump')
            continue
        try:
            base_toml = parse_toml(base_text)
            head_toml = parse_toml(head_text)
        except tomllib.TOMLDecodeError as e:
            reasons.append(f'"{path}" could not be parsed as TOML: {e}')
            continue
        head_tomls[path] = head_toml
        reasons.extend(check_manifest_diff(path, base_toml, head_toml))

    base_lock_text = git_show(base_ref, "Cargo.lock")
    head_lock_text = git_show(head_ref, "Cargo.lock")
    if base_lock_text is None or head_lock_text is None:
        reasons.append('"Cargo.lock" is missing at base or head -- cannot verify the lockfile diff')
    else:
        try:
            base_lock = parse_toml(base_lock_text)
            head_lock = parse_toml(head_lock_text)
        except tomllib.TOMLDecodeError as e:
            reasons.append(f'"Cargo.lock" could not be parsed as TOML: {e}')
        else:
            reasons.extend(check_lockfile_diff(base_lock, head_lock))
            reasons.extend(check_manifest_lockfile_consistency(head_tomls, head_lock))

    if reasons:
        sys.stderr.write("NOT SAFE:\n" + "\n".join(f"  - {r}" for r in reasons) + "\n")
        return 1
    sys.stdout.write("SAFE: diff is confined to dependency-version value changes.\n")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.stderr.write("usage: verify-dependabot-diff-safe.py <base_ref> <head_ref>\n")
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2]))
