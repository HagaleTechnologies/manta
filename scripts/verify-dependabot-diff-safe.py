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

    Each comparator is (kind, major, minor, patch) with minor/patch possibly None (Cargo allows
    partial versions like "1" or "1.2"). Comma-separated pieces are ALWAYS AND'd in Cargo's own
    grammar -- there is no OR syntax at all, unlike npm's "||" -- so this is just a flat list, no
    OR-alternatives to track (https://doc.rust-lang.org/cargo/reference/resolver.html).
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
    major_raw = m.group("major")
    if major_raw == "*" or m.group("minor") == "*" or m.group("patch") == "*":
        return (WILDCARD, None, None, None)
    major = int(major_raw)
    minor = int(m.group("minor")) if m.group("minor") is not None else None
    patch = int(m.group("patch")) if m.group("patch") is not None else None
    kind_by_op = {"=": EXACT, ">": GT, ">=": GE, "<": LT, "<=": LE, "~": TILDE, "^": CARET}
    # No operator prefix at all is Cargo's own default, and it is caret -- NOT exact like npm's
    # bare default -- confirmed against the Cargo Book: `bar = "1.2.3"` means `^1.2.3`, and this
    # extends to caret's own 0.x special-casing (`"0.2.3"` behaves like `^0.2.3`, i.e.
    # >=0.2.3,<0.3.0; `"0.0.3"` behaves like `^0.0.3`, i.e. >=0.0.3,<0.0.4).
    kind = kind_by_op.get(op, CARET)
    return (kind, major, minor, patch)


def floor_tuple(comparator):
    _kind, major, minor, patch = comparator
    return (major, minor if minor is not None else 0, patch if patch is not None else 0)


def caret_ceiling_component(comparator):
    """The single component whose value bounds a caret comparator's ceiling.

    Cargo's own special-casing: the ceiling tracks the LEFTMOST significant (non-major-zero)
    component -- major normally, but minor for a 0.x major, and patch for a 0.0.x major/minor.
    Any change to that specific component is a ceiling change; changes to components to its
    right are pure floor movement within the same ceiling.
    """
    _kind, major, minor, patch = comparator
    if major != 0:
        return major
    if minor is None or minor != 0:
        return minor
    return patch


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
            # "*" admits everything already; nothing to compare, and nothing more permissive to
            # widen into.
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


def check_manifest_diff(path, base_toml, head_toml):
    """Compare one Cargo.toml's dependency declarations between base and head.

    Returns a list of reason strings (empty if the diff at this path is entirely safe).
    """
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
    """Return {(name, source_kind): entry} for every [[package]] in a parsed Cargo.lock.

    source_kind is one of "workspace" (no source field -- a local workspace member),
    "registry", or "git", derived from the entry's own `source` field. Keyed by (name,
    source_kind) rather than name alone since Cargo.lock CAN carry two same-named packages at
    different major versions (each gets its own [[package]] entry) -- source_kind in the key
    keeps a registry-vs-git identity swap visible as two different keys disappearing/appearing
    rather than one key's fields silently changing.
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
        out[(name, kind, entry.get("version"))] = entry
    return out


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
    reasons = []

    removed = set(base_pkgs) - set(head_pkgs)
    added = set(head_pkgs) - set(base_pkgs)

    # Group removed/added entries by (name, kind) to match up "old version of X removed, new
    # version of X added" pairs -- the common Dependabot-bump shape -- from genuinely new/removed
    # dependencies, which aren't this verifier's concern (see check_manifest_diff).
    def by_name_kind(keys):
        grouped = {}
        for name, kind, version in keys:
            grouped.setdefault((name, kind), []).append(version)
        return grouped

    removed_grouped = by_name_kind(removed)
    added_grouped = by_name_kind(added)

    for name_kind, removed_versions in removed_grouped.items():
        name, kind = name_kind
        added_versions = added_grouped.pop(name_kind, [])
        if not added_versions:
            # A version of this package disappeared with no same-(name,kind) replacement --
            # could be a legitimate dependency removal (out of scope, deferred) or a kind swap
            # already caught by the registry<->git checks below via the OTHER grouping.
            continue
        if len(removed_versions) != 1 or len(added_versions) != 1:
            reasons.append(
                f'Cargo.lock package "{name}" ({kind}) changed in a way with more than one '
                "version added/removed at once -- not a simple version bump"
            )
            continue
        old_v, new_v = removed_versions[0], added_versions[0]
        if new_v is not None and old_v is not None and _version_tuple(new_v) < _version_tuple(old_v):
            reasons.append(
                f'Cargo.lock package "{name}" ({kind}) downgraded ({old_v} -> {new_v})'
            )

    # Any (name, kind) that only ever appears in `added` (never matched to a `removed` peer) is a
    # brand-new dependency in the lockfile -- e.g. a fresh transitive pulled in by a manifest
    # bump. Not this verifier's concern per se (the manifest-level check already validated the
    # manifest-visible requirement change); Cargo itself already validated the lockfile's own
    # internal consistency via `cargo generate-lockfile`/`cargo update`, which is not something
    # this verifier re-derives from scratch.

    return reasons


def _version_tuple(version_str):
    parts = version_str.split(".")
    out = []
    for p in parts[:3]:
        digits = re.match(r"\d+", p)
        out.append(int(digits.group()) if digits else 0)
    while len(out) < 3:
        out.append(0)
    return tuple(out)


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
