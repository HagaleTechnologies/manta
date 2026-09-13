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
        self.assertEqual(v.normalize_dependency("1.2.3"), ("version", "1.2.3"))

    def test_version_table_is_version_kind(self):
        self.assertEqual(
            v.normalize_dependency({"version": "1.2.3", "features": ["derive"]}),
            ("version", "1.2.3"),
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
             mock.patch.object(v, "WORKSPACE_MEMBERS", []):
            exit_code = v.main("base", "head")
        self.assertEqual(exit_code, 1)


if __name__ == "__main__":
    unittest.main()
