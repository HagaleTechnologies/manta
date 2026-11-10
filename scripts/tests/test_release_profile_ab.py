"""MAN-528: scripts/release-profile-ab.py -- the release-profile variant matrix, the commands it
runs, its timing arithmetic, the float-WAV reader and the decision rule `choose()` (decision D4
of docs/DECISIONS/2026-11-10-man528-release-profile.md), including the research run's
single-thread numbers as a fixture that must pick fat_cu1.

stdlib unittest only; nothing here builds or runs cargo.
Run: python3 -m unittest scripts.tests.test_release_profile_ab -v
"""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SCRIPT = os.path.join(ROOT, "scripts", "release-profile-ab.py")


def load(name="release_profile_ab", path=SCRIPT):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


ab = load()

REF = "00ef6b13fd582a48" + "0" * 48
OTHER = "ffffffffffffffff" + "0" * 48


def record(name, size, build_cpu, cpu_min, cpu_median, reps=7, digest=REF, mode="1"):
    """A result record whose runs have exactly the given best-of-N and median CPU."""
    lto, cgu, panic = ab.VARIANTS.get(name, ("false", "16", "unwind"))
    cpu = [cpu_min] + [cpu_median] * (reps - 1)
    return {
        "lto": lto, "codegen_units": cgu, "panic": panic,
        "size_bytes": size, "gzip_bytes": size // 3,
        "build_wall_s": build_cpu / 2, "build_cpu_s": build_cpu,
        "runs": {mode: [{"cpu_s": c, "wall_s": c, "sha256": digest} for c in cpu]},
    }


# Research run at de39a71 (AMD Ryzen 9 5950X, RAYON_NUM_THREADS=1 taskset -c 9): size, clean-build
# CPU seconds, single-thread CPU min and median.
RESEARCH = {
    "base": (25_486_488, 307.2, 14.27, 14.72),
    "thin": (25_678_744, 335.0, 14.68, 15.13),
    "fat": (18_092_160, 304.3, 13.93, 14.59),
    "cu1": (20_543_496, 306.0, 14.20, 15.80),
    "thin_cu1": (20_700_592, 326.2, 14.08, 15.18),
    "fat_cu1": (17_229_944, 379.2, 14.28, 14.92),
    "fat_cu1_abort": (14_866_624, 311.4, 14.46, 14.96),
    "abort": (21_585_336, 296.2, 14.05, 14.75),
}


def research_results():
    return {n: record(n, *v) for n, v in RESEARCH.items()}


class VariantTests(unittest.TestCase):
    def test_matrix_is_the_ticket_matrix(self):
        self.assertEqual(
            list(ab.VARIANTS),
            ["base", "thin", "fat", "cu1", "thin_cu1", "fat_cu1", "abort", "fat_cu1_abort"],
        )
        self.assertEqual(ab.VARIANTS["fat_cu1"], ("fat", "1", "unwind"))
        self.assertEqual(ab.VARIANTS["thin"], ("thin", "16", "unwind"))
        self.assertEqual(ab.VARIANTS["abort"], ("false", "16", "abort"))

    def test_base_resets_every_key_to_cargo_defaults(self):
        env = ab.variant_env("base", {"CARGO_PROFILE_RELEASE_LTO": "fat",
                                      "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
                                      "PATH": "/bin"})
        self.assertEqual(env["CARGO_PROFILE_RELEASE_LTO"], "false")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], "16")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_PANIC"], "unwind")
        self.assertEqual(env["PATH"], "/bin")

    def test_variant_env_does_not_mutate_its_input(self):
        given = {"CARGO_PROFILE_RELEASE_LTO": "fat"}
        ab.variant_env("base", given)
        self.assertEqual(given, {"CARGO_PROFILE_RELEASE_LTO": "fat"})

    def test_fat_cu1_abort_env(self):
        env = ab.variant_env("fat_cu1_abort", {})
        self.assertEqual(
            [env[k] for k in ab.ENV_KEYS], ["fat", "1", "abort"],
        )

    def test_unknown_variant_raises(self):
        with self.assertRaises(ValueError):
            ab.variant_env("lto_max", {})

    def test_build_command_is_the_shipped_build(self):
        self.assertEqual(ab.build_command(),
                         ["cargo", "build", "--release", "--locked", "-p", "manta-cli"])

    def test_scene_command_runs_the_harness_writer(self):
        self.assertEqual(
            ab.scene_command(),
            ["cargo", "test", "--release", "--locked", "-p", "manta-engine", "--test",
             "cpu_budget", "write_cpu_budget_scene", "--", "--ignored", "--exact", "--nocapture"],
        )

    def test_scene_writer_exists_in_the_harness(self):
        with open(os.path.join(ROOT, "crates", "manta-engine", "tests", "cpu_budget.rs")) as f:
            text = f.read()
        self.assertIn("fn write_cpu_budget_scene()", text)
        self.assertIn("MANTA_CPU_BUDGET_SCENE_DIR", text)
        self.assertIn(f'"{ab.SCENE_NAME}"', text)

    def test_single_thread_decode_is_pinned_and_multi_thread_is_not(self):
        self.assertEqual(ab.decode_command("m", "s.wav", "1", 3),
                         ["taskset", "-c", "3", "m", "decode", "s.wav", "--json"])
        self.assertEqual(ab.decode_command("m", "s.wav", "all", 3),
                         ["m", "decode", "s.wav", "--json"])
        self.assertEqual(ab.decode_env("1", {})["RAYON_NUM_THREADS"], "1")
        self.assertNotIn("RAYON_NUM_THREADS", ab.decode_env("all", {"RAYON_NUM_THREADS": "1"}))


def float_wav(frames, rate, channels=2):
    """A 32-bit float WAV (format tag 3), which Python's wave module refuses."""
    block = 4 * channels
    data = struct.pack(f"<{frames * channels}f", *([0.25] * frames * channels))
    fmt = struct.pack("<HHIIHH", 3, channels, rate, rate * block, block, 32)
    body = (b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt
            + b"LIST" + struct.pack("<I", 3) + b"abc\x00"  # odd-sized chunk, padded
            + b"data" + struct.pack("<I", len(data)) + data)
    return b"RIFF" + struct.pack("<I", len(body)) + body


class MathTests(unittest.TestCase):
    def test_hop_rate_is_the_decoder_hop_rate(self):
        with open(os.path.join(ROOT, "crates", "manta-decode", "src", "lib.rs")) as f:
            self.assertIn("pub const FO_HZ: f64 = 375.0;", f.read())
        self.assertEqual(ab.HOP_RATE_HZ, 375.0)

    def test_per_hop_us_matches_research(self):
        self.assertAlmostEqual(ab.per_hop_us(14.72, 60.0), 654.2, delta=0.1)

    def test_summarize_min_median_spread(self):
        s = ab.summarize([16.90, 18.29, 17.79, 17.5, 18.0])
        self.assertEqual(s["n"], 5)
        self.assertEqual(s["min"], 16.90)
        self.assertEqual(s["max"], 18.29)
        self.assertAlmostEqual(s["median"], 17.79)
        self.assertAlmostEqual(s["spread"], (18.29 - 16.90) / 16.90)
        with self.assertRaises(ValueError):
            ab.summarize([])

    def test_wav_duration_reads_a_float_wav_header(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "s.wav")
            with open(path, "wb") as f:
                f.write(float_wav(4800, 1600))
            info = ab.wav_info(path)
            self.assertEqual((info["format_tag"], info["channels"], info["bits"]), (3, 2, 32))
            self.assertEqual(info["rate"], 1600)
            self.assertEqual(info["frames"], 4800)
            self.assertAlmostEqual(ab.wav_duration_s(path), 3.0)

    def test_cpu_model_reads_x86_and_pi4_cpuinfo(self):
        x86 = "processor\t: 0\nmodel name\t: AMD Ryzen 9 5950X 16-Core Processor\n"
        self.assertEqual(ab.cpu_model_from_cpuinfo(x86), "AMD Ryzen 9 5950X 16-Core Processor")
        pi4 = ("processor\t: 0\nBogoMIPS\t: 108.00\nCPU part\t: 0xd08\n\n"
               "Hardware\t: BCM2835\nModel\t\t: Raspberry Pi 4 Model B Rev 1.4\n")
        self.assertEqual(ab.cpu_model_from_cpuinfo(pi4),
                         "Raspberry Pi 4 Model B Rev 1.4, Cortex-A72")
        self.assertEqual(ab.cpu_model_from_cpuinfo(""), "")

    def test_wav_reader_refuses_a_non_wav(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "x.wav")
            with open(path, "wb") as f:
                f.write(b"not a wav at all")
            with self.assertRaises(ValueError):
                ab.wav_info(path)


class ChooseTests(unittest.TestCase):
    def test_research_single_thread_numbers_pick_fat_cu1(self):
        winner, reason, ineligible = ab.choose(research_results())
        self.assertEqual(winner, "fat_cu1")
        self.assertIn("no speed winner", reason)
        self.assertIn("3.1%", reason)  # (14.72 - 14.27) / 14.72
        self.assertEqual(sorted(n for n, _ in ineligible), ["abort", "fat_cu1_abort"])

    def test_clear_speed_winner_is_chosen(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "thin": record("thin", 101, 320, 9.0, 9.18),
            "fat_cu1": record("fat_cu1", 70, 380, 10.0, 10.1),
        }
        winner, reason, _ = ab.choose(results)
        self.assertEqual(winner, "thin")
        self.assertIn("speed", reason)
        self.assertNotIn("no speed winner", reason)

    def test_speed_needs_both_median_and_best_of_n(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            # median 3.9% faster, best-of-N only 2.5%: not a speed win, so size decides
            "thin": record("thin", 101, 300, 9.75, 9.8),
            # best-of-N 10% faster, median no better: not a speed win either
            "cu1": record("cu1", 102, 300, 9.0, 10.2),
            "fat": record("fat", 80, 300, 10.0, 10.3),
        }
        self.assertEqual(ab.choose(results)[0], "fat")

    def test_noisy_base_widens_the_margin(self):
        results = {
            "base": record("base", 100, 300, 9.0, 10.0),  # margin 10%
            "thin": record("thin", 101, 300, 8.5, 9.2),   # 8% faster: inside the margin
            "fat": record("fat", 90, 300, 9.0, 10.0),
        }
        winner, reason, _ = ab.choose(results)
        self.assertEqual(winner, "fat")
        self.assertIn("10.0%", reason)

    def test_abort_is_never_chosen_even_if_fastest_and_smallest(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "abort": record("abort", 50, 280, 5.0, 5.1),
            "fat_cu1_abort": record("fat_cu1_abort", 40, 280, 5.0, 5.0),
        }
        winner, _, ineligible = ab.choose(results)
        self.assertEqual(winner, "base")
        self.assertEqual(dict(ineligible)["abort"], 'panic = "abort", decision D2')
        self.assertIn("fat_cu1_abort", dict(ineligible))

    def test_output_mismatch_is_excluded(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "fat": record("fat", 60, 300, 8.0, 8.1),
            "cu1": record("cu1", 90, 300, 10.0, 10.2),
        }
        results["fat"]["runs"]["1"][3]["sha256"] = OTHER  # one run out of seven differs
        winner, _, ineligible = ab.choose(results)
        self.assertEqual(winner, "cu1")
        self.assertEqual(dict(ineligible)["fat"], "decode output differs from base")

    def test_output_mismatch_in_any_collected_mode_is_excluded(self):
        # D4 rule 1 reads "any run": with --threads 1,all, a multi-thread digest that differs from
        # base drops the variant even though only the single-thread mode is timed for the verdict.
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "fat": record("fat", 60, 300, 8.0, 8.1),
            "cu1": record("cu1", 90, 300, 10.0, 10.2),
        }
        for rec in results.values():
            rec["runs"]["all"] = [{"cpu_s": 5.0, "wall_s": 2.0, "sha256": REF} for _ in range(5)]
        results["fat"]["runs"]["all"][2]["sha256"] = OTHER  # one multi-thread run differs
        winner, _, ineligible = ab.choose(results, "1")
        self.assertEqual(winner, "cu1")
        self.assertEqual(dict(ineligible)["fat"], "decode output differs from base")
        # Base must agree with itself across modes too, or there is no single reference digest.
        results["base"]["runs"]["all"] = [{"cpu_s": 5.0, "wall_s": 2.0, "sha256": OTHER}] * 5
        with self.assertRaisesRegex(ValueError, "base decode output differs"):
            ab.choose(results, "1")

    def test_nondeterministic_base_raises(self):
        results = {"base": record("base", 100, 300, 10.0, 10.2)}
        results["base"]["runs"]["1"][0]["sha256"] = OTHER
        with self.assertRaises(ValueError):
            ab.choose(results)

    def test_build_cpu_cap_excludes_an_expensive_smallest_variant(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "fat_cu1": record("fat_cu1", 50, 451, 10.0, 10.2),  # +50.3% build CPU
            "fat": record("fat", 70, 450, 10.0, 10.2),          # exactly +50%: allowed
        }
        self.assertEqual(ab.choose(results)[0], "fat")

    def test_ties_break_by_variant_order(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "fat_cu1": record("fat_cu1", 70, 300, 10.0, 10.2),
            "fat": record("fat", 70, 300, 10.0, 10.2),
        }
        self.assertEqual(ab.choose(results)[0], "fat")
        fast = {
            "base": record("base", 100, 300, 10.0, 10.2),
            "cu1": record("cu1", 90, 300, 8.0, 8.0),
            "thin": record("thin", 90, 300, 8.0, 8.0),
        }
        self.assertEqual(ab.choose(fast)[0], "thin")

    def test_missing_base_raises(self):
        with self.assertRaises(ValueError):
            ab.choose({"fat": record("fat", 70, 300, 10.0, 10.2)})

    def test_zero_base_cpu_raises(self):
        results = {"base": record("base", 100, 300, 0.0, 0.0),
                   "fat": record("fat", 70, 300, 0.0, 0.0)}
        with self.assertRaises(ValueError):
            ab.choose(results)

    def test_rule_reads_the_requested_mode(self):
        results = {
            "base": record("base", 100, 300, 10.0, 10.2, mode="all"),
            "fat": record("fat", 70, 300, 10.0, 10.2, mode="all"),
        }
        self.assertEqual(ab.choose(results, "all")[0], "fat")
        with self.assertRaises(ValueError):
            ab.choose(results, "1")


class CliTests(unittest.TestCase):
    def test_dry_run_prints_commands_and_runs_nothing(self):
        boom = mock.Mock(side_effect=AssertionError("dry run started a process"))
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(subprocess, "Popen", boom), \
                mock.patch.object(subprocess, "run", boom), \
                mock.patch.object(os, "wait4", boom, create=True):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = ab.main(["--out", os.path.join(tmp, "ab"), "--dry-run"])
            self.assertEqual(code, 0)
            self.assertFalse(os.path.exists(os.path.join(tmp, "ab")))
        text = out.getvalue()
        builds = [l for l in text.splitlines()
                  if l.endswith("cargo build --release --locked -p manta-cli")]
        self.assertEqual(len(builds), 8)
        self.assertEqual(text.count("write_cpu_budget_scene"), 1)
        self.assertIn("CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 "
                      "CARGO_PROFILE_RELEASE_PANIC=unwind", text)

    def test_missing_scene_is_refused_before_anything_runs(self):
        boom = mock.Mock(side_effect=AssertionError("started a process"))
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(subprocess, "Popen", boom), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            code = ab.main(["--out", os.path.join(tmp, "ab"), "--scene", tmp])
            self.assertFalse(os.path.exists(os.path.join(tmp, "ab")))
        self.assertEqual(code, 2)
        self.assertIn("cpu_budget.wav", err.getvalue())

    def test_choose_error_keeps_the_timings(self):
        # A base whose multi-thread output differs from its single-thread output makes choose()
        # raise; main must still save what it measured instead of losing the whole session.
        def fake_run(argv, env, stdout=None, stderr=None, cwd=None):
            if "decode" in argv:
                stdout.write(b"one" if env.get("RAYON_NUM_THREADS") == "1" else b"all")
            return {"wall_s": 1.0, "cpu_s": 1.0, "maxrss": 0, "exit": 0}

        def fake_copy(src, dst):
            with open(dst, "wb") as f:
                f.write(b"binary")

        with tempfile.TemporaryDirectory() as tmp:
            open(os.path.join(tmp, "cpu_budget.wav"), "wb").close()
            out = os.path.join(tmp, "ab")
            with mock.patch.object(ab, "run_measured", fake_run), \
                    mock.patch.object(ab, "host_facts", lambda cpu: {}), \
                    mock.patch.object(ab, "wav_duration_s", lambda path: 60.0), \
                    mock.patch.object(shutil, "copy2", fake_copy), \
                    contextlib.redirect_stdout(io.StringIO()), \
                    contextlib.redirect_stderr(io.StringIO()) as err:
                code = ab.main(["--out", out, "--scene", tmp, "--variants", "base",
                                "--threads", "1,all", "--reps", "1"])
            self.assertEqual(code, 1)
            with open(os.path.join(out, "results.partial.json")) as f:
                saved = json.load(f)
        self.assertIn("differs", saved["error"])
        self.assertEqual(sorted(saved["results"]["base"]["runs"]), ["1", "all"])
        self.assertIn("results.partial.json", err.getvalue())

    def test_variants_flag_always_adds_base(self):
        args = ab.parse_args(["--out", "x", "--variants", "fat_cu1"])
        self.assertEqual(args.variants, ["base", "fat_cu1"])
        self.assertEqual(args.modes, ["1"])

    def test_unknown_variant_flag_exits_2(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            ab.parse_args(["--out", "x", "--variants", "fat,turbo"])
        self.assertEqual(cm.exception.code, 2)

    def test_cpu_flag_is_refused_off_linux(self):
        with mock.patch.object(sys, "platform", "darwin"), \
                contextlib.redirect_stderr(io.StringIO()), \
                self.assertRaises(SystemExit) as cm:
            ab.main(["--out", "x", "--cpu", "3", "--dry-run"])
        self.assertEqual(cm.exception.code, 2)

    def test_render_markdown_has_one_row_per_variant_and_the_verdict(self):
        results = research_results()
        verdict = ab.choose(results)
        host = {"machine": "x86_64", "platform": "Linux", "cpu_model": "AMD Ryzen 9 5950X",
                "cpus": 32, "rustc": "rustc 1.98.0", "commit": "de39a71"}
        md = ab.render_markdown(host, results, verdict, audio_s=60.0)
        for n in ab.VARIANTS:
            self.assertEqual(sum(1 for l in md.splitlines() if l.startswith(f"| {n} |")), 2, n)
        self.assertIn("| fat_cu1 | fat | 1 | unwind | 17229944 |", md)
        self.assertIn("| base | 1 thread | 7 | 14.27 | 14.72 |", md)
        self.assertIn("| 654.2 |", md)
        self.assertIn("AMD Ryzen 9 5950X", md)
        self.assertIn("\nverdict: fat_cu1 (no speed winner", md)
        self.assertIn('\nineligible: abort, fat_cu1_abort (panic = "abort", decision D2)', md)


if __name__ == "__main__":
    unittest.main()
