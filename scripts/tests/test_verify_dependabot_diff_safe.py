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

    def test_caret_cross_position_ceiling_collision_is_unsafe(self):
        # Codex P1, manta#194 round 6: 0.2.3's ceiling is its MINOR component (value 2); 2.0.0's
        # ceiling is its MAJOR component (also value 2). Comparing bare values alone made these
        # collide and read as an unchanged ceiling -- a full major bump passing as safe.
        self.assert_unsafe("0.2.3", "2.0.0")

    def test_exact_cross_position_ceiling_collision_is_unsafe(self):
        self.assert_unsafe("=0.2.3", "=2.0.0")

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

    def test_referenced_added_transitive_is_not_flagged_as_unreachable(self):
        # A real Cargo.lock never carries an addition nothing references (Cargo's own
        # `generate-lockfile` wouldn't produce one) -- serde_core is added and immediately
        # referenced by serde's own (now-changed) dependencies list, exactly as the real
        # serde/serde_core split looks in practice, with serde itself reachable from a workspace
        # root. Finding U/X's reachability check must not fire on serde_core; whether serde's own
        # dependencies-list change is separately flagged (Finding I's retained-entry check, since
        # serde's version here is unchanged) is a different, orthogonal concern this test doesn't
        # assert on either way.
        root = {"name": "root", "version": "0.1.0", "dependencies": ["serde"]}
        base = {
            "package": [
                root,
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["serde_core"],
                },
                {
                    "name": "serde_core",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "ccc",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertFalse(any("not reachable" in r for r in reasons))


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

    def test_cross_position_ceiling_collision_is_flagged(self):
        # Codex P1, manta#194 round 6: 0.2.3's ceiling is its minor (2); 2.0.0's ceiling is its
        # major (also 2) -- this exact resolved-version jump is what the round-6 fix targets.
        reasons = v.check_lockfile_diff(self._pkg("0.2.3"), self._pkg("2.0.0"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("major/0.x boundary", reasons[0])


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

    def test_git_commit_swap_at_the_same_version_is_flagged(self):
        # Codex P1, manta#194 round 11 -- the exact reproduction: only the commit fragment
        # changes, same branch/tag URL, same self-reported version. Unlike the "advancing"
        # case above, there is zero independent evidence (no version change, no source-identity
        # change) that this swap is a legitimate update rather than a malicious commit
        # substitution -- the version field is entirely self-reported by the pinned commit's own
        # Cargo.toml, so it carries no safety signal on its own.
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
                    "version": "1.2.3",
                    "source": "git+https://good.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("SAME version", reasons[0])

    def test_git_commit_swap_disguised_by_build_metadata_is_flagged(self):
        # Codex P1, manta#194 round 12, Finding N: round 11's `old_v == new_v` raw-string
        # comparison missed that SemVer build metadata never affects version identity --
        # "1.2.3+old" and "1.2.3+new" ARE the same version, but compare unequal as strings,
        # letting a commit swap disguised by a cosmetic build-tag change slip past.
        base = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.3+old",
                    "source": "git+https://good.example/repo?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                }
            ]
        }
        head = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.2.3+new",
                    "source": "git+https://good.example/repo?branch=main#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("SAME version", reasons[0])

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


class LockfileUnmatchedRemovalTests(unittest.TestCase):
    """Codex P1, manta#194 round 7: a transitive package disappearing from Cargo.lock with no
    same-name replacement was previously treated as always-safe, on the theory that "a real
    removal is visible at the manifest level too" -- true only for a DIRECTLY declared
    dependency. A transitive dependency has no manifest declaration for check_manifest_diff to
    ever see, so an unmatched transitive removal reached neither check and was silently accepted.
    """

    def test_unmatched_transitive_removal_is_flagged(self):
        base = {
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
                    "checksum": "bbb",
                },
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("removed with no same-name replacement", reasons[0])

    def test_referenced_added_transitive_with_no_removal_triggers_no_removal_reason(self):
        # A brand-new, REFERENCED dependency appearing (never matched to a removed peer) does
        # not trigger the unmatched-REMOVAL check this test class covers (round 7) -- that check
        # is one-directional by design. Whether a different, orthogonal check (Finding I/O's
        # dependencies-edge validation, or round 15's Finding U for an unreferenced addition)
        # has something to say about the addition itself is not this test's concern.
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
                    "dependencies": ["serde_core"],
                },
                {
                    "name": "serde_core",
                    "version": "1.0.200",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertFalse(any("removed with no same-name replacement" in r for r in reasons))


class LockfileDependencyDisambiguationSuffixTests(unittest.TestCase):
    """Found empirically while verifying round 7's fix against manta's real Dependabot history
    (not a Codex comment): Cargo's own lockfile encoding writes a `dependencies` entry as "name",
    "name version", or "name version (source)", appending version/source ONLY to disambiguate
    multiple resolved instances of that name elsewhere in the graph. An unrelated bump shifting
    how many versions of some OTHER crate exist can flip every entry that depends on it between
    "name" and "name X.Y.Z" with no actual change to what code runs -- manta's own commit
    fb1c7f3 (a plain clap patch bump) hit this on ~10 unrelated proc-macro crates.
    """

    def _syn_entry(self):
        # A real Cargo.lock always carries an entry for every crate named in a `dependencies`
        # list -- these tests must include it so _resolve_dependency_ref has something to
        # resolve "syn"/"syn 2.0.118" against (an earlier version of these fixtures omitted it,
        # which made every ref silently unresolvable and passed these tests for the wrong
        # reason -- caught while implementing round 8's Finding I fix).
        return {
            "name": "syn",
            "version": "2.0.118",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "checksum": "syn-checksum",
        }

    def test_disambiguation_suffix_appearing_is_safe(self):
        base = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn"],
                },
                self._syn_entry(),
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn 2.0.118"],
                },
                self._syn_entry(),
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_disambiguation_suffix_disappearing_is_safe(self):
        base = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn 2.0.118"],
                },
                self._syn_entry(),
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn"],
                },
                self._syn_entry(),
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_actual_dependency_name_addition_alongside_disambiguation_noise_is_flagged(self):
        # Confirms tolerating disambiguation-suffix churn doesn't mask an actual add/remove
        # riding along with it -- `syn`'s own identity is unchanged and correctly tolerated, but
        # `malicious-crate` is a genuinely new edge with no resolution at all.
        base = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn"],
                },
                self._syn_entry(),
            ]
        }
        head = {
            "package": [
                {
                    "name": "serde_derive",
                    "version": "1.0.228",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                    "dependencies": ["proc-macro2", "quote", "syn 2.0.118", "malicious-crate"],
                },
                self._syn_entry(),
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("dependencies", reasons[0])

    def test_edge_retargeted_to_newly_added_major_version_is_flagged(self):
        # Codex P1, manta#194 round 8, Finding I -- the exact reproduction: a diamond dependency
        # where consumer_a keeps depending on foo 1.0.0 (so foo 1.0.0's own [[package]] entry is
        # RETAINED, not removed) while consumer_b's edge is retargeted to a newly ADDED foo 2.0.0
        # (added with no matching removal, since foo 1.0.0 is still needed elsewhere -- so the
        # major-boundary check on matched removed/added pairs never runs for it either).
        root = {"name": "root", "version": "0.1.0", "dependencies": ["consumer_a", "consumer_b"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
                {
                    "name": "consumer_a",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "ca",
                    "dependencies": ["foo 1.0.0"],
                },
                {
                    "name": "consumer_b",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "cb",
                    "dependencies": ["foo 1.0.0"],
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
                {
                    "name": "foo",
                    "version": "2.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                },
                {
                    "name": "consumer_a",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "ca",
                    "dependencies": ["foo 1.0.0"],
                },
                {
                    "name": "consumer_b",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "cb",
                    "dependencies": ["foo 2.0.0"],
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("consumer_b", reasons[0])
        self.assertIn("dependencies", reasons[0])


class RequirementIsSatisfiedByTests(unittest.TestCase):
    """Codex P1, manta#194 round 10: replaces the floor-only approximation (rounds 8-9) with a
    full comparator-satisfaction predicate, built from primitives already tested elsewhere in
    this file (floor_tuple, caret_ceiling_component, version_sort_key).
    """

    def test_caret_within_floor_is_satisfied(self):
        self.assertTrue(v.requirement_is_satisfied_by("1.1", "1.1.0"))
        self.assertTrue(v.requirement_is_satisfied_by("1.1", "1.9.9"))

    def test_caret_below_floor_is_not_satisfied(self):
        self.assertFalse(v.requirement_is_satisfied_by("1.1", "1.0.5"))

    def test_caret_crossing_major_is_not_satisfied(self):
        self.assertFalse(v.requirement_is_satisfied_by("^1.2.3", "2.0.0"))

    def test_zero_x_caret_minor_mismatch_is_not_satisfied(self):
        self.assertFalse(v.requirement_is_satisfied_by("0.10", "0.9.5"))
        self.assertTrue(v.requirement_is_satisfied_by("0.10", "0.10.1"))

    def test_strict_gt_excludes_its_own_floor(self):
        self.assertFalse(v.requirement_is_satisfied_by(">1.0.5", "1.0.5"))
        self.assertTrue(v.requirement_is_satisfied_by(">1.0.5", "1.0.6"))

    def test_ge_includes_its_own_floor(self):
        self.assertTrue(v.requirement_is_satisfied_by(">=1.0.5", "1.0.5"))

    def test_exact_requires_an_exact_match(self):
        self.assertTrue(v.requirement_is_satisfied_by("=1.2.3", "1.2.3"))
        self.assertFalse(v.requirement_is_satisfied_by("=1.2.3", "1.2.4"))

    def test_explicit_range_respects_its_ceiling(self):
        self.assertTrue(v.requirement_is_satisfied_by(">=1.2.3, <2.0.0", "1.9.9"))
        self.assertFalse(v.requirement_is_satisfied_by(">=1.2.3, <2.0.0", "2.0.0"))
        self.assertFalse(v.requirement_is_satisfied_by(">=1.2.3, <2.0.0", "1.2.2"))

    def test_tilde_respects_its_minor_ceiling(self):
        self.assertTrue(v.requirement_is_satisfied_by("~1.3.0", "1.3.9"))
        self.assertFalse(v.requirement_is_satisfied_by("~1.3.0", "1.4.0"))

    def test_wildcard_respects_its_prefix(self):
        self.assertTrue(v.requirement_is_satisfied_by("1.*", "1.9.9"))
        self.assertFalse(v.requirement_is_satisfied_by("1.*", "2.0.0"))
        self.assertTrue(v.requirement_is_satisfied_by("*", "9.9.9"))

    def test_unparseable_requirement_is_not_satisfied(self):
        self.assertFalse(v.requirement_is_satisfied_by("not a version", "1.0.0"))


class ManifestLockfileConsistencyTests(unittest.TestCase):
    """Codex P1, manta#194 round 8, Finding J: a manifest floor raised beyond an already-
    unchanged locked version triggers neither compare_requirements (sees a non-major bump) nor
    check_lockfile_diff (the lockfile entry didn't change at all) -- this check validates the
    HEAD manifest/lock combination directly, independent of what changed in the diff.
    """

    def _lock(self, version):
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

    def test_floor_raised_beyond_stale_locked_version_is_flagged(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": "1.1"}}}
        reasons = v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.0.5"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("does not satisfy the head manifest", reasons[0])

    def test_locked_version_satisfying_the_raised_floor_is_safe(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": "1.1"}}}
        self.assertEqual(
            v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.1.0")), []
        )

    def test_unchanged_requirement_with_satisfying_lock_is_safe(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": "1.0"}}}
        self.assertEqual(
            v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.0.5")), []
        )

    def test_no_lockfile_entry_is_out_of_scope(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": "1.1"}}}
        self.assertEqual(v.check_manifest_lockfile_consistency(head_tomls, {"package": []}), [])

    def test_diamond_where_one_candidate_satisfies_is_safe(self):
        # A real diamond dependency: two coexisting versions is fine as long as at least one of
        # them actually satisfies this declaration's own requirement.
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": "1.5"}}}
        lock = {
            "package": [
                {
                    "name": "demo",
                    "version": "1.0.5",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
                {
                    "name": "demo",
                    "version": "1.5.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                },
            ]
        }
        self.assertEqual(v.check_manifest_lockfile_consistency(head_tomls, lock), [])

    def test_diamond_where_no_candidate_satisfies_is_flagged(self):
        # Codex P1, manta#194 round 10, Finding L -- the exact reproduction shape: manta's own
        # real rand_core 0.9.5-and-0.10.1 diamond, with the requirement raised to something
        # NEITHER coexisting candidate satisfies. Rounds 8-9 skipped this name outright as
        # "ambiguous" and never caught it.
        head_tomls = {"Cargo.toml": {"dependencies": {"rand_core": "0.10.2"}}}
        lock = {
            "package": [
                {
                    "name": "rand_core",
                    "version": "0.9.5",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
                {
                    "name": "rand_core",
                    "version": "0.10.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                },
            ]
        }
        reasons = v.check_manifest_lockfile_consistency(head_tomls, lock)
        self.assertEqual(len(reasons), 1)
        self.assertIn("does not satisfy the head manifest", reasons[0])

    def test_strict_gt_bound_equal_to_locked_version_is_flagged(self):
        # Codex P1, manta#194 round 9: `demo = ">1.0.5"` requires STRICTLY more than 1.0.5 --
        # a locked "1.0.5" does not satisfy it, even though it isn't BELOW the floor value.
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": ">1.0.5"}}}
        reasons = v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.0.5"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("does not satisfy the head manifest", reasons[0])

    def test_strict_gt_bound_below_locked_version_is_safe(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": ">1.0.5"}}}
        self.assertEqual(
            v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.0.6")), []
        )

    def test_inclusive_ge_bound_equal_to_locked_version_is_safe(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"demo": ">=1.0.5"}}}
        self.assertEqual(
            v.check_manifest_lockfile_consistency(head_tomls, self._lock("1.0.5")), []
        )


class ManifestLockfileConsistencyEndToEndTests(unittest.TestCase):
    def test_stale_lock_against_raised_floor_fails_end_to_end(self):
        base_files = {
            "Cargo.toml": '[workspace]\nmembers = []\n\n[workspace.dependencies]\ndemo = "1.0"\n',
            "Cargo.lock": (
                "[[package]]\n"
                'name = "demo"\n'
                'version = "1.0.5"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                'checksum = "aaa"\n'
            ),
        }
        head_files = {
            "Cargo.toml": '[workspace]\nmembers = []\n\n[workspace.dependencies]\ndemo = "1.1"\n',
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


class LockfileReplacementEntryDependencyEdgeTests(unittest.TestCase):
    """Codex P1, manta#194 round 12, Finding O: the matched removed/added branch never looked at
    the NEW entry's own `dependencies` field -- only the retained-entries loop does that, which
    by definition never runs for a matched pair (its key changed). A version bump could ride
    along with an edge redirected to a brand-new, otherwise-unvalidated dependency.
    """

    def test_dependencies_change_alongside_a_version_bump_is_flagged(self):
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                    "dependencies": ["bar"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["evil"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
                {
                    "name": "evil",
                    "version": "1.0.0",
                    "source": "registry+https://malicious.example/fake-index",
                    "checksum": "evil1",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("foo", reasons[0])
        self.assertIn("dependencies", reasons[0])

    def test_unchanged_dependencies_alongside_a_version_bump_is_safe(self):
        base = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                    "dependencies": ["bar"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        head = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["bar"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])

    def test_edge_kind_swap_at_same_name_and_version_is_flagged(self):
        # Codex P1, manta#194 round 13, Finding P -- the exact reproduction: foo's edge to "bar"
        # changes from a registry-sourced instance to a newly-added git-sourced instance of the
        # SAME name and version. Round 12's bare-name-set comparison saw {"bar"} == {"bar"} and
        # missed this entirely, since the name itself never changed.
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                    "dependencies": ["bar 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["bar 1.0.0 (git+https://malicious.example/bar.git?rev=deadbeef#deadbeefdeadbeefdeadbeefdeadbeefdeadbeef)"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "git+https://malicious.example/bar.git?rev=deadbeef#deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("foo", reasons[0])
        self.assertIn("dependencies", reasons[0])

    def test_edge_version_only_retarget_within_a_diamond_is_safe(self):
        # The clap_derive/syn case (round 12) restated as a diamond: foo's edge to "bar" shifts
        # from bar 1.0.0 to a newly-available bar 2.0.0 of the SAME kind/source -- both syn
        # instances coexist in head, same as clap_derive's real syn 2.x/3.x transition.
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                    "dependencies": ["bar"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["bar 2.0.0"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
                {
                    "name": "bar",
                    "version": "2.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar2",
                },
            ]
        }
        self.assertEqual(v.check_lockfile_diff(base, head), [])


class RequirementPrereleaseEligibilityTests(unittest.TestCase):
    """Codex P1, manta#194 round 13, Finding Q: Cargo excludes a prerelease from satisfying a
    requirement unless the requirement itself explicitly opts in with a same-major.minor.patch
    prerelease comparator -- a policy layered on top of (not replaced by) SemVer precedence.
    """

    def test_unopted_prerelease_above_the_floor_is_not_satisfied(self):
        self.assertFalse(v.requirement_is_satisfied_by(">=1.0.0", "1.1.0-alpha"))

    def test_opted_in_prerelease_at_the_same_base_version_is_satisfied(self):
        self.assertTrue(v.requirement_is_satisfied_by(">=1.1.0-alpha", "1.1.0-alpha.2"))

    def test_stable_version_is_unaffected_by_the_eligibility_rule(self):
        self.assertTrue(v.requirement_is_satisfied_by(">=1.0.0", "1.1.0"))

    def test_prerelease_of_a_different_base_version_still_excluded_even_if_caret_matches(self):
        # ^1.0.0's floor is 1.0.0 and ceiling is <2.0.0 -- 1.1.0-alpha's numeric precedence falls
        # inside that range, but no comparator names 1.1.0 with a prerelease, so it's still
        # excluded.
        self.assertFalse(v.requirement_is_satisfied_by("^1.0.0", "1.1.0-alpha"))


class ManifestLockfileConsistencyRenameTests(unittest.TestCase):
    """Codex P1, manta#194 round 13, Finding R: Cargo's rename mechanism
    (`alias = { package = "foo", version = "1.1" }`) declares under key "alias" but Cargo.lock
    records the actual crate under its real name "foo" -- looking up "alias" in the lockfile
    silently found nothing and skipped the whole check for every renamed dependency.
    """

    def _lock(self, name, version):
        return {
            "package": [
                {
                    "name": name,
                    "version": version,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }

    def test_renamed_dependency_is_resolved_via_its_package_name(self):
        head_tomls = {
            "Cargo.toml": {"dependencies": {"alias": {"package": "foo", "version": "1.1"}}}
        }
        reasons = v.check_manifest_lockfile_consistency(head_tomls, self._lock("foo", "1.0.5"))
        self.assertEqual(len(reasons), 1)
        self.assertIn("alias", reasons[0])
        self.assertIn("does not satisfy the head manifest", reasons[0])

    def test_renamed_dependency_satisfying_its_requirement_is_safe(self):
        head_tomls = {
            "Cargo.toml": {"dependencies": {"alias": {"package": "foo", "version": "1.1"}}}
        }
        self.assertEqual(
            v.check_manifest_lockfile_consistency(head_tomls, self._lock("foo", "1.1.0")), []
        )


class ManifestLockfileConsistencySourceKindTests(unittest.TestCase):
    """Codex P1, manta#194 round 14, Finding S: candidates were filtered by name (and, since
    round 10, checked for satisfaction) but never by SOURCE KIND -- an unrelated same-named git-
    sourced entry could satisfy a plain registry version declaration whose own stale registry
    entry didn't.
    """

    def test_unrelated_git_entry_does_not_mask_a_stale_registry_entry(self):
        # Codex's exact reproduction: `foo = "1.0"` -> `"1.1"`, the registry entry stays stale at
        # 1.0, and an unrelated git-sourced "foo 1.1" (pulled in by something else entirely)
        # coexists. The git entry must not be allowed to satisfy the registry declaration.
        head_tomls = {"Cargo.toml": {"dependencies": {"foo": "1.1"}}}
        lock = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
                {
                    "name": "foo",
                    "version": "1.1.0",
                    "source": "git+https://unrelated.example/foo.git?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                },
            ]
        }
        reasons = v.check_manifest_lockfile_consistency(head_tomls, lock)
        self.assertEqual(len(reasons), 1)
        self.assertIn("foo", reasons[0])
        self.assertIn("does not satisfy the head manifest", reasons[0])

    def test_matching_registry_entry_still_satisfies_alongside_an_unrelated_git_entry(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"foo": "1.1"}}}
        lock = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.1.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                },
                {
                    "name": "foo",
                    "version": "1.5.0",
                    "source": "git+https://unrelated.example/foo.git?branch=main#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                },
            ]
        }
        self.assertEqual(v.check_manifest_lockfile_consistency(head_tomls, lock), [])


class ManifestLockfileConsistencyRegistryIdentityTests(unittest.TestCase):
    """Codex P1, manta#194 round 15, Finding T: round 14's kind filter excludes git/workspace but
    doesn't distinguish two DIFFERENT registry sources for the same name -- a stale private-
    registry entry could be masked by an unrelated crates.io entry of the same name/version.
    """

    def test_two_distinct_registry_sources_fail_closed(self):
        # Codex's exact reproduction: a private-registry declaration whose own entry is stale,
        # masked by an unrelated crates.io entry of the same name.
        head_tomls = {"Cargo.toml": {"dependencies": {"foo": {"version": "1.1", "registry": "private"}}}}
        lock = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://my-private-registry.example/index",
                    "checksum": "aaa",
                },
                {
                    "name": "foo",
                    "version": "1.1.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bbb",
                },
            ]
        }
        reasons = v.check_manifest_lockfile_consistency(head_tomls, lock)
        self.assertEqual(len(reasons), 1)
        self.assertIn("distinct", reasons[0])

    def test_single_registry_source_is_unaffected(self):
        head_tomls = {"Cargo.toml": {"dependencies": {"foo": "1.1"}}}
        lock = {
            "package": [
                {
                    "name": "foo",
                    "version": "1.1.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "aaa",
                }
            ]
        }
        self.assertEqual(v.check_manifest_lockfile_consistency(head_tomls, lock), [])


class LockfileUnreferencedAdditionTests(unittest.TestCase):
    """Codex P1, manta#194 round 15, Finding U: an added-only [[package]] entry that NOTHING in
    the head lockfile's own dependency graph references at all is not explainable as a
    consequence of anything else in the diff -- Cargo would never produce such an orphaned
    package via a normal update.
    """

    def test_wholly_unreferenced_addition_is_flagged(self):
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                },
                {
                    "name": "orphan",
                    "version": "1.0.0",
                    "source": "registry+https://malicious.example/fake-index",
                    "checksum": "orphan1",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 1)
        self.assertIn("orphan", reasons[0])
        self.assertIn("not reachable", reasons[0])

    def test_referenced_addition_is_not_flagged_as_unreachable(self):
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["bar"],
                },
                {
                    "name": "bar",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "bar1",
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertFalse(any("not reachable" in r for r in reasons))


class LockfileDisconnectedCycleTests(unittest.TestCase):
    """Codex P1, manta#194 round 16, Finding X -- the exact reproduction: two added-only
    entries referencing only EACH OTHER (`evil-a -> evil-b -> evil-a`), disconnected from every
    real workspace root. Round 15's name-occurrence check saw both names mentioned in some
    dependencies list and flagged neither.
    """

    def test_disconnected_referencing_cycle_is_flagged(self):
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
                {
                    "name": "evil-a",
                    "version": "1.0.0",
                    "source": "registry+https://malicious.example/fake-index",
                    "checksum": "evila1",
                    "dependencies": ["evil-b"],
                },
                {
                    "name": "evil-b",
                    "version": "1.0.0",
                    "source": "registry+https://malicious.example/fake-index",
                    "checksum": "evilb1",
                    "dependencies": ["evil-a"],
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertEqual(len(reasons), 2)
        joined = " ".join(reasons)
        self.assertIn("evil-a", joined)
        self.assertIn("evil-b", joined)
        for r in reasons:
            self.assertIn("not reachable", r)

    def test_cycle_reachable_from_a_real_root_is_not_flagged(self):
        root = {"name": "root", "version": "0.1.0", "dependencies": ["foo"]}
        base = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo1",
                },
            ]
        }
        head = {
            "package": [
                root,
                {
                    "name": "foo",
                    "version": "1.0.1",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "foo2",
                    "dependencies": ["real-a"],
                },
                {
                    "name": "real-a",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "reala1",
                    "dependencies": ["real-b"],
                },
                {
                    "name": "real-b",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "checksum": "realb1",
                    "dependencies": ["real-a"],
                },
            ]
        }
        reasons = v.check_lockfile_diff(base, head)
        self.assertFalse(any("not reachable" in r for r in reasons))


if __name__ == "__main__":
    unittest.main()
