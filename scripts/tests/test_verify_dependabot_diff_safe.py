"""Tests for verify-dependabot-diff-safe.py (MAN-218).

stdlib unittest only, matching this file's own no-third-party-dependencies design -- run via
`python3 -m unittest scripts.tests.test_verify_dependabot_diff_safe` (or plain `unittest
discover`) with zero pip installs needed.
"""
import importlib.util
import os
import unittest
from unittest import mock

HERE = os.path.dirname(__file__)
spec = importlib.util.spec_from_file_location(
    "verify_dependabot_diff_safe", os.path.join(HERE, "..", "verify-dependabot-diff-safe.py")
)
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)


class CompareRequirementsTests(unittest.TestCase):
    def assert_safe(self, base, head):
        reason = v.compare_requirements(base, head)
        self.assertIsNone(reason, f"expected safe, got reason: {reason}")

    def assert_unsafe(self, base, head):
        reason = v.compare_requirements(base, head)
        self.assertIsNotNone(reason, "expected unsafe, got None")

    def test_caret_default_floor_bump_is_safe(self):
        self.assert_safe("1.2.3", "1.2.5")
        self.assert_safe("1.2.3", "1.9.0")

    def test_caret_crossing_major_is_unsafe(self):
        self.assert_unsafe("1.2.3", "2.0.0")

    def test_caret_zero_x_special_case_floor_bump_is_safe(self):
        self.assert_safe("0.2.3", "0.2.9")

    def test_caret_zero_x_crossing_minor_is_unsafe(self):
        self.assert_unsafe("0.2.3", "0.3.0")

    def test_caret_zero_zero_x_crossing_patch_is_unsafe(self):
        self.assert_unsafe("0.0.3", "0.0.4")

    def test_unchanged_requirement_is_safe(self):
        self.assert_safe("0.0.3", "0.0.3")

    def test_tilde_floor_bump_within_minor_is_safe(self):
        self.assert_safe("~1.3.0", "~1.3.5")

    def test_tilde_crossing_minor_is_unsafe(self):
        self.assert_unsafe("~1.3.0", "~1.4.0")

    def test_exact_repin_bump_is_safe(self):
        self.assert_safe("=0.4.11", "=0.4.12")

    def test_exact_downgrade_is_unsafe(self):
        self.assert_unsafe("=0.4.11", "=0.4.10")

    def test_explicit_range_floor_bump_same_ceiling_is_safe(self):
        self.assert_safe(">=0.6.0, <0.8.0", ">=0.6.5, <0.8.0")

    def test_explicit_range_ceiling_widened_is_unsafe(self):
        self.assert_unsafe(">=0.6.0, <0.8.0", ">=0.6.5, <0.9.0")

    def test_bare_downgrade_is_unsafe(self):
        self.assert_unsafe("1.2.3", "1.2.2")

    def test_compound_caret_with_extra_ceiling_widened_is_unsafe(self):
        # The Cargo analog of widdershins#421 round 21's finding: a caret comparator ALONGSIDE an
        # independent explicit ceiling comparator in the same (comma-separated, AND'd) requirement.
        # A leading-character/whole-string classification would incorrectly treat this as pure
        # caret and skip the ceiling check entirely -- this verifier's per-comparator-position
        # design never does that classification at all, so it's not exposed to this bug class.
        self.assert_unsafe("^1.0.0, <1.5.0", "^1.0.1, <1.9.0")

    def test_compound_caret_floor_bump_only_is_safe(self):
        self.assert_safe("^1.0.0, <1.5.0", "^1.0.5, <1.5.0")

    def test_operator_change_is_unsafe(self):
        self.assert_unsafe("^1.2.3", "~1.2.5")

    def test_comparator_count_change_is_unsafe(self):
        self.assert_unsafe("1.2.3", "1.2.3, <2.0.0")

    def test_wildcard_is_always_safe(self):
        self.assert_safe("*", "*")

    def test_unparseable_requirement_fails_closed_not_open(self):
        # Build-metadata suffixes ("+...") are rare in a [dependencies] requirement string (far
        # more common in Cargo.lock's own resolved `version`, which this doesn't touch) and
        # aren't handled by this parser -- confirms that an unparseable requirement is reported
        # as a reason (blocking auto-approval, routing to human review), never silently treated
        # as safe.
        reason = v.compare_requirements("1.2.3", "1.2.3+build.1")
        self.assertIsNotNone(reason)


class NormalizeDependencyTests(unittest.TestCase):
    def test_bare_string_is_version_kind(self):
        self.assertEqual(v.normalize_dependency("1.2.3"), ("version", ("1.2.3", {})))

    def test_version_table_is_version_kind(self):
        self.assertEqual(
            v.normalize_dependency({"version": "1.2.3", "features": ["derive"]}),
            ("version", ("1.2.3", {"features": ["derive"]})),
        )

    def test_git_table_is_git_kind(self):
        value = {"git": "https://example.com/x.git", "rev": "abc"}
        kind, detail = v.normalize_dependency(value)
        self.assertEqual(kind, "git")
        self.assertEqual(detail, value)

    def test_path_table_is_path_kind(self):
        kind, _ = v.normalize_dependency({"path": "../local-crate"})
        self.assertEqual(kind, "path")

    def test_workspace_true_is_workspace_kind(self):
        kind, _ = v.normalize_dependency({"workspace": True})
        self.assertEqual(kind, "workspace")


class CheckManifestDiffTests(unittest.TestCase):
    def test_safe_version_bump_produces_no_reasons(self):
        base = {"dependencies": {"serde": "1.0.200"}}
        head = {"dependencies": {"serde": "1.0.210"}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])

    def test_source_change_from_version_to_git_is_flagged(self):
        base = {"dependencies": {"serde": "1.0.200"}}
        head = {"dependencies": {"serde": {"git": "https://example.com/serde.git"}}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("source-type change", reasons[0])

    def test_major_bump_is_flagged(self):
        base = {"dependencies": {"serde": "1.0.200"}}
        head = {"dependencies": {"serde": "2.0.0"}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("major", reasons[0])

    def test_unrelated_unchanged_dependency_produces_no_reasons(self):
        base = {"dependencies": {"serde": "1.0.200", "anyhow": "1.0"}}
        head = {"dependencies": {"serde": "1.0.210", "anyhow": "1.0"}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])

    def test_workspace_dependencies_table_is_checked(self):
        base = {"workspace": {"dependencies": {"serde": "1.0.200"}}}
        head = {"workspace": {"dependencies": {"serde": "1.0.210"}}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])

    def test_git_rev_change_with_same_kind_is_flagged_not_waved_through(self):
        base = {"dependencies": {"coppa-dsp": {"git": "https://x.git", "rev": "aaa"}}}
        head = {"dependencies": {"coppa-dsp": {"git": "https://x.git", "rev": "bbb"}}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("git", reasons[0])


class CheckLockfileDiffTests(unittest.TestCase):
    def test_safe_registry_version_bump(self):
        base = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.210",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                }
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_downgrade_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.210",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("downgraded", reasons[0])

    def test_workspace_member_entries_are_ignored(self):
        base = {"package": [{"name": "manta-cli", "version": "0.1.0"}]}
        head = {"package": [{"name": "manta-cli", "version": "0.1.0"}]}
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_unrelated_added_transitive_produces_no_reasons(self):
        base = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
                {
                    "name": "serde_core",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "ccc",
                },
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])


class MainEndToEndTests(unittest.TestCase):
    def test_safe_diff_across_manifest_and_lockfile(self):
        base_files = {
            "Cargo.toml": '[workspace]\nmembers = []\n\n[workspace.dependencies]\nserde = "1.0.200"\n',
            "Cargo.lock": (
                "[[package]]\n"
                'name = "serde"\n'
                'version = "1.0.200"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                'checksum = "aaa"\n'
            ),
        }
        head_files = {
            "Cargo.toml": '[workspace]\nmembers = []\n\n[workspace.dependencies]\nserde = "1.0.210"\n',
            "Cargo.lock": (
                "[[package]]\n"
                'name = "serde"\n'
                'version = "1.0.210"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                'checksum = "bbb"\n'
            ),
        }

        def fake_git_show(ref, path):
            files = base_files if ref == "base" else head_files
            return files.get(path)

        with mock.patch.object(v, "git_show", side_effect=fake_git_show), \
             mock.patch.object(v, "check_diff_scope", return_value=[]), \
             mock.patch.object(v, "WORKSPACE_MEMBERS", []):
            exit_code = v.main("base", "head")
        self.assertEqual(exit_code, 0)

    def test_unsafe_source_change_end_to_end(self):
        base_files = {
            "Cargo.toml": '[workspace]\nmembers = []\n\n[workspace.dependencies]\nserde = "1.0.200"\n',
            "Cargo.lock": (
                "[[package]]\n"
                'name = "serde"\n'
                'version = "1.0.200"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                'checksum = "aaa"\n'
            ),
        }
        head_files = {
            "Cargo.toml": (
                "[workspace]\nmembers = []\n\n[workspace.dependencies]\n"
                'serde = { git = "https://example.com/serde.git" }\n'
            ),
            "Cargo.lock": base_files["Cargo.lock"],
        }

        def fake_git_show(ref, path):
            files = base_files if ref == "base" else head_files
            return files.get(path)

        with mock.patch.object(v, "git_show", side_effect=fake_git_show), \
             mock.patch.object(v, "check_diff_scope", return_value=[]), \
             mock.patch.object(v, "WORKSPACE_MEMBERS", []):
            exit_code = v.main("base", "head")
        self.assertEqual(exit_code, 1)


class CheckDiffScopeTests(unittest.TestCase):
    def _run(self, changed_files, allowed_paths):
        fake_result = mock.Mock(returncode=0, stdout="\n".join(changed_files) + "\n")
        with mock.patch.object(v.subprocess, "run", return_value=fake_result):
            return v.check_diff_scope("base", "head", allowed_paths)

    def test_diff_confined_to_allowed_paths_is_safe(self):
        reasons = self._run(["Cargo.toml", "Cargo.lock"], {"Cargo.toml", "Cargo.lock"})
        self.assertEqual(reasons, [])

    def test_diff_touching_an_extra_file_is_flagged(self):
        # The exact reproduction Codex gave: a diff that touches only workflow/script files (no
        # Cargo.toml/Cargo.lock change at all) must not be reported safe just because none of the
        # files THIS verifier inspects happen to differ.
        reasons = self._run(
            [".github/workflows/dependabot-auto-merge.yml", "scripts/verify-dependabot-diff-safe.py"],
            {"Cargo.toml", "Cargo.lock"},
        )
        self.assertEqual(len(reasons), 1)
        self.assertIn("outside the enumerated", reasons[0])

    def test_diff_mixing_allowed_and_extra_files_is_flagged(self):
        reasons = self._run(
            ["Cargo.toml", "Cargo.lock", "crates/manta-cli/src/main.rs"],
            {"Cargo.toml", "Cargo.lock"},
        )
        self.assertEqual(len(reasons), 1)
        self.assertIn("main.rs", reasons[0])


class TargetSpecificDependencyTests(unittest.TestCase):
    def test_target_dependency_table_is_collected(self):
        pkg_toml = {
            "target": {
                "cfg(windows)": {"dependencies": {"windows-sys": "0.59"}},
            }
        }
        decls = v.collect_dependency_declarations(pkg_toml)
        self.assertIn(("target.cfg(windows).dependencies", "windows-sys"), decls)

    def test_target_dependency_major_bump_is_flagged(self):
        base = {"target": {"cfg(windows)": {"dependencies": {"windows-sys": "0.59"}}}}
        head = {"target": {"cfg(windows)": {"dependencies": {"windows-sys": "0.60"}}}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("major", reasons[0])

    def test_target_dependency_patch_bump_is_safe(self):
        base = {"target": {"cfg(windows)": {"dependencies": {"windows-sys": "0.59.1"}}}}
        head = {"target": {"cfg(windows)": {"dependencies": {"windows-sys": "0.59.2"}}}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])


class NonVersionFieldPreservedTests(unittest.TestCase):
    def test_added_feature_alongside_safe_version_bump_is_flagged(self):
        base = {"dependencies": {"serde": {"version": "1.0.200", "features": ["derive"]}}}
        head = {
            "dependencies": {
                "serde": {"version": "1.0.210", "features": ["derive", "rc"]}
            }
        }
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("non-version field", reasons[0])

    def test_package_rename_alongside_safe_version_bump_is_flagged(self):
        # Cargo's own alias mechanism: the dependency key stays the same, but `package` points it
        # at a DIFFERENT actual crate -- directly analogous to npm's alias-swap attacks.
        base = {"dependencies": {"compat": {"package": "real-crate", "version": "1.0.0"}}}
        head = {"dependencies": {"compat": {"package": "malicious-crate", "version": "1.0.1"}}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("non-version field", reasons[0])

    def test_unchanged_non_version_fields_with_safe_bump_is_safe(self):
        base = {"dependencies": {"serde": {"version": "1.0.200", "features": ["derive"]}}}
        head = {"dependencies": {"serde": {"version": "1.0.210", "features": ["derive"]}}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])


class ExactAndLowerBoundMajorBoundaryTests(unittest.TestCase):
    def test_exact_major_bump_is_unsafe(self):
        reason = v.compare_requirements("=1.2.3", "=2.0.0")
        self.assertIsNotNone(reason)

    def test_ge_major_bump_is_unsafe(self):
        reason = v.compare_requirements(">=1.2.3", ">=2.0.0")
        self.assertIsNotNone(reason)

    def test_gt_zero_x_minor_crossing_is_unsafe(self):
        reason = v.compare_requirements(">0.2.3", ">0.3.0")
        self.assertIsNotNone(reason)

    def test_ge_patch_bump_within_major_is_safe(self):
        reason = v.compare_requirements(">=1.2.3", ">=1.9.0")
        self.assertIsNone(reason)


class WildcardPrefixPreservedTests(unittest.TestCase):
    def test_major_only_wildcard_crossing_major_is_unsafe(self):
        self.assertIsNotNone(v.compare_requirements("1.*", "2.*"))

    def test_major_only_wildcard_unchanged_is_safe(self):
        self.assertIsNone(v.compare_requirements("1.*", "1.*"))

    def test_major_minor_wildcard_crossing_minor_is_unsafe(self):
        self.assertIsNotNone(v.compare_requirements("1.2.*", "1.3.*"))

    def test_major_minor_wildcard_unchanged_is_safe(self):
        self.assertIsNone(v.compare_requirements("1.2.*", "1.2.*"))

    def test_bare_wildcard_is_always_safe(self):
        self.assertIsNone(v.compare_requirements("*", "*"))


class PrereleasePrecedenceTests(unittest.TestCase):
    def test_release_to_prerelease_is_a_downgrade(self):
        self.assertIsNotNone(v.compare_requirements("=1.0.0", "=1.0.0-alpha"))

    def test_prerelease_to_release_is_an_increase(self):
        self.assertIsNone(v.compare_requirements("=1.0.0-alpha", "=1.0.0"))

    def test_prerelease_to_higher_prerelease_is_safe(self):
        self.assertIsNone(v.compare_requirements("=1.0.0-alpha", "=1.0.0-beta"))

    def test_prerelease_to_lower_prerelease_is_a_downgrade(self):
        self.assertIsNotNone(v.compare_requirements("=1.0.0-beta", "=1.0.0-alpha"))

    def test_numeric_prerelease_identifiers_compare_numerically(self):
        self.assertIsNone(v.compare_requirements("=1.0.0-alpha.1", "=1.0.0-alpha.2"))
        self.assertIsNotNone(v.compare_requirements("=1.0.0-alpha.2", "=1.0.0-alpha.1"))

    def test_lockfile_prerelease_downgrade_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.0-alpha",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("downgraded", reasons[0])


class WholeManifestScopeTests(unittest.TestCase):
    def test_change_outside_dependency_tables_is_flagged(self):
        base = {"dependencies": {"serde": "1.0.200"}, "profile": {"release": {"opt-level": 3}}}
        head = {"dependencies": {"serde": "1.0.200"}, "profile": {"release": {"opt-level": 0}}}
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("outside its dependency declarations", reasons[0])

    def test_new_patch_table_is_flagged(self):
        base = {"dependencies": {"serde": "1.0.200"}}
        head = {
            "dependencies": {"serde": "1.0.200"},
            "patch": {"crates-io": {"serde": {"git": "https://malicious.example/serde.git"}}},
        }
        reasons = v.check_manifest_diff("Cargo.toml", base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("outside its dependency declarations", reasons[0])

    def test_safe_bump_with_no_other_changes_is_unaffected(self):
        base = {"dependencies": {"serde": "1.0.200"}, "package": {"name": "manta-cli"}}
        head = {"dependencies": {"serde": "1.0.210"}, "package": {"name": "manta-cli"}}
        self.assertEqual(v.check_manifest_diff("Cargo.toml", base, head), [])


class LockfileSourceKindSwapTests(unittest.TestCase):
    def test_registry_to_git_swap_same_name_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "git+https://malicious.example/serde.git?rev=deadbeef#deadbeef",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("changed source kind", reasons[0])


class LockfileMajorBoundaryOnIncreaseTests(unittest.TestCase):
    """Codex P1, manta#194 round 3, Finding E: the resolved version's own major/0.x boundary is
    checked independently of the manifest, since a permissive manifest requirement (e.g. "*")
    leaves the manifest-level check with nothing to compare.
    """

    def _pkg(self, version):
        return {
            "package": [
                {
                    "name": "demo",
                    "version": version,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }

    def test_major_version_increase_is_flagged(self):
        reasons = v.check_lockfile_diff(self._pkg("1.2.3"), self._pkg("2.0.0"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("major/0.x boundary", reasons[0])

    def test_zero_x_minor_increase_is_flagged(self):
        # Cargo treats a 0.x minor bump as breaking -- same ceiling rule as a manifest-level
        # caret requirement.
        reasons = v.check_lockfile_diff(self._pkg("0.2.3"), self._pkg("0.3.0"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("major/0.x boundary", reasons[0])

    def test_minor_and_patch_increase_within_same_major_is_safe(self):
        self.assertEqual(v.check_lockfile_diff(self._pkg("1.2.3"), self._pkg("1.9.9")), [])

    def test_zero_x_patch_increase_within_same_minor_is_safe(self):
        self.assertEqual(v.check_lockfile_diff(self._pkg("0.2.3"), self._pkg("0.2.9")), [])


class LockfileRetainedEntryTests(unittest.TestCase):
    """Codex P1, manta#194 round 3, Finding F: a package retained under an identical
    (name, kind, version) key is invisible to the removed/added set difference -- e.g. a git
    dependency's `version` field comes from the pinned crate's own Cargo.toml at that commit,
    not the git rev itself, so a rev swap can leave name/kind/version all unchanged.
    """

    def test_git_rev_swap_with_unchanged_version_is_flagged(self):
        # Since round 5 folded the raw `source` string into parse_lockfile_packages's key, this
        # rev swap changes the key itself -- it now surfaces as a removed+added pair caught by
        # the source-identity check, not as a same-key retained-entry field change. Kept as its
        # own regression case (distinct from the round-4 tests below) because the VERSION here
        # stays identical, only the source does.
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "0.1.0",
                    "source": "git+https://example.com/demo.git?rev=aaaaaaa#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "0.1.0",
                    "source": "git+https://malicious.example/demo.git?rev=bbbbbbb#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("source identity", reasons[0])

    def test_retained_dependencies_list_change_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["serde"],
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["serde", "libc"],
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("dependencies", reasons[0])

    def test_fully_unchanged_retained_entry_is_safe(self):
        pkg = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        self.assertEqual(v.check_lockfile_diff(pkg, pkg), [])


class LockfileSourceIdentityOnBumpTests(unittest.TestCase):
    """Codex P1, manta#194 round 4: a same-kind source swap riding along with a version bump
    changes the (name, kind, version) dict key, so it's invisible to the retained-entry check
    (round 3, Finding F) -- `old_kind == new_kind` alone treated any two git/registry sources as
    equivalent regardless of which URL/ref they actually named.
    """

    def test_git_url_swap_alongside_version_bump_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.3",
                    "source": "git+https://good.example/repo?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.4",
                    "source": "git+https://evil.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("source identity", reasons[0])

    def test_git_rev_advancing_on_same_url_is_safe(self):
        # A normal Dependabot bump for a branch-pinned git dependency: same URL/branch, a newer
        # commit sha. Only the part after the `#` fragment changes.
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.3",
                    "source": "git+https://good.example/repo?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.4",
                    "source": "git+https://good.example/repo?branch=main#cccccccccccccccccccccccccccccccccccccccc",
                }
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_registry_url_swap_alongside_version_bump_is_flagged(self):
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.1",
                    "source": "registry+https://evil.example/fake-index",
                    "checksum": "bbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("source identity", reasons[0])


class LockfileCollidingIdentityTests(unittest.TestCase):
    """Codex P1, manta#194 round 5: a diamond dependency can legitimately pull in the same name
    and version from two DIFFERENT sources of the same broad kind (e.g. two git URLs) -- each
    gets its own [[package]] entry. Keying parse_lockfile_packages by (name, kind, version) alone
    (rounds 1-4) collided those two entries onto one dict key, silently dropping one during
    parsing -- before ANY check below ever ran on it.
    """

    def _two_git_entries(self, second_source):
        return {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.3",
                    "source": "git+https://good-a.example/repo?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                },
                {
                    "name": "demo",
                    "version": "1.2.3",
                    "source": second_source,
                },
            ]
        }

    def test_both_colliding_entries_are_preserved_when_unchanged(self):
        pkg = self._two_git_entries(
            "git+https://good-b.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )
        self.assertEqual(len(v.parse_lockfile_packages(pkg)), 2)
        self.assertEqual(v.check_lockfile_diff(pkg, pkg), [])

    def test_source_swap_on_the_second_colliding_entry_is_flagged(self):
        base = self._two_git_entries(
            "git+https://good-b.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )
        head = self._two_git_entries(
            "git+https://evil.example/repo?branch=main#cccccccccccccccccccccccccccccccccccccccc"
        )
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("source identity", reasons[0])

    def test_dependencies_change_on_the_second_colliding_entry_is_flagged(self):
        second_source = "git+https://good-b.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        base = self._two_git_entries(second_source)
        base["package"][1]["dependencies"] = ["serde"]
        head = self._two_git_entries(second_source)
        head["package"][1]["dependencies"] = ["serde", "libc"]
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("dependencies", reasons[0])


if __name__ == "__main__":
    unittest.main()
