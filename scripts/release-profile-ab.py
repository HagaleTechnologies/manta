#!/usr/bin/env python3
"""MAN-528: A/B the release profile on the shipped `manta decode` binary.

Builds `manta` once per release-profile variant (`lto`, `codegen-units`, `panic`, set through
`CARGO_PROFILE_RELEASE_*` so no source edit is needed), records each binary's size and build cost,
then times `manta decode <scene> --json` on the MAN-49 CPU-budget scene that
`crates/manta-engine/tests/cpu_budget.rs`'s `write_cpu_budget_scene` renders. The verdict comes
from `choose()`, a decision rule fixed in advance (decision D4 of
docs/DECISIONS/2026-11-10-man528-release-profile.md):

1. Drop any variant whose decode output digest differs from base on any run.
2. Drop any variant with `panic = "abort"` (decision D2). It is still reported.
3. margin = max(3%, (base median - base min) / base median). A variant wins on speed when its
   median CPU is below base median x (1 - margin) and its best-of-N CPU is below
   base min x (1 - margin). Among winners, pick the lowest median; ties go to the smaller binary,
   then to variant order.
4. Otherwise, among the remaining variants (base included) whose clean-build CPU seconds stay
   within +50% of base, pick the smallest binary; ties go to variant order.

The rule times the single-thread mode (`RAYON_NUM_THREADS=1`, plus `taskset -c N` on Linux with
`--cpu N`), which matches the Pi 4 budget of one core. The multi-thread mode's timings are
informational and it is never pinned to one CPU, but its output digests still count under step 1.

Unix only (it uses os.wait4 for child CPU time). Run from the repo root, e.g. on a Pi 4:

    python3 scripts/release-profile-ab.py --out /tmp/ab --threads 1,all --cpu 3 --reps 7

and on macOS (no taskset there, so no --cpu):

    python3 scripts/release-profile-ab.py --out /tmp/ab --threads 1,all --reps 7

Tests: python3 -m unittest scripts.tests.test_release_profile_ab -v
"""
import argparse
import gzip
import hashlib
import json
import os
import platform
import shutil
import statistics
import struct
import subprocess
import sys
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

# name -> (lto, codegen-units, panic). Base names Cargo's release defaults explicitly, so it still
# means "no [profile.release]" after Cargo.toml pins one.
VARIANTS = {
    "base": ("false", "16", "unwind"),
    "thin": ("thin", "16", "unwind"),
    "fat": ("fat", "16", "unwind"),
    "cu1": ("false", "1", "unwind"),
    "thin_cu1": ("thin", "1", "unwind"),
    "fat_cu1": ("fat", "1", "unwind"),
    "abort": ("false", "16", "abort"),
    "fat_cu1_abort": ("fat", "1", "abort"),
}
ENV_KEYS = (
    "CARGO_PROFILE_RELEASE_LTO",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    "CARGO_PROFILE_RELEASE_PANIC",
)
HOP_RATE_HZ = 375.0  # manta_decode::FO_HZ (crates/manta-decode/src/lib.rs)
SPEED_MARGIN = 0.03
BUILD_CPU_CAP = 0.50
INELIGIBLE = {"panic": "abort"}
MODES = ("1", "all")
SCENE_NAME = "cpu_budget"


def variant_env(name, env=None):
    """A copy of `env` (default: os.environ) with the variant's three profile keys set."""
    if name not in VARIANTS:
        raise ValueError(f"unknown variant {name!r}; known: {', '.join(VARIANTS)}")
    out = dict(os.environ if env is None else env)
    for key, value in zip(ENV_KEYS, VARIANTS[name]):
        out[key] = value
    return out


def build_command():
    """The shipped build, as every release route runs it."""
    return ["cargo", "build", "--release", "--locked", "-p", "manta-cli"]


def scene_command():
    """Renders the MAN-49 scene through the harness's own writer test."""
    return [
        "cargo", "test", "--release", "--locked", "-p", "manta-engine", "--test", "cpu_budget",
        "write_cpu_budget_scene", "--", "--ignored", "--exact", "--nocapture",
    ]


def decode_command(binary, wav, mode, cpu=None):
    argv = [binary, "decode", wav, "--json"]
    if mode == "1" and cpu is not None:
        argv = ["taskset", "-c", str(cpu)] + argv
    return argv


def decode_env(mode, env=None):
    out = dict(os.environ if env is None else env)
    if mode == "1":
        out["RAYON_NUM_THREADS"] = "1"
    else:
        out.pop("RAYON_NUM_THREADS", None)
    return out


def run_measured(argv, env, stdout=None, stderr=None, cwd=None):
    """Runs argv to completion; returns wall seconds, user+sys CPU seconds of the child and the
    descendants it waited for, max RSS in KiB (Linux) or bytes (macOS), and the exit code."""
    start = time.monotonic()
    proc = subprocess.Popen(argv, env=env, stdout=stdout, stderr=stderr, cwd=cwd)
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.monotonic() - start
    code = os.waitstatus_to_exitcode(status)
    proc.returncode = code
    return {
        "wall_s": wall,
        "cpu_s": usage.ru_utime + usage.ru_stime,
        "maxrss": usage.ru_maxrss,
        "exit": code,
    }


def wav_info(path):
    """Reads the RIFF `fmt `/`data` chunks; works for 32-bit float WAVs, which Python's `wave`
    module refuses."""
    with open(path, "rb") as f:
        head = f.read(12)
        if len(head) != 12 or head[:4] != b"RIFF" or head[8:12] != b"WAVE":
            raise ValueError(f"{path}: not a RIFF/WAVE file")
        fmt = None
        data_size = None
        while True:
            chunk = f.read(8)
            if len(chunk) < 8:
                break
            cid, size = chunk[:4], struct.unpack("<I", chunk[4:])[0]
            if cid == b"fmt ":
                body = f.read(size)
                tag, channels, rate, _byte_rate, block_align, bits = struct.unpack(
                    "<HHIIHH", body[:16]
                )
                fmt = {"format_tag": tag, "channels": channels, "rate": rate,
                       "block_align": block_align, "bits": bits}
            elif cid == b"data":
                data_size = size
                f.seek(size, os.SEEK_CUR)
            else:
                f.seek(size, os.SEEK_CUR)
            if size % 2:
                f.seek(1, os.SEEK_CUR)
    if fmt is None or data_size is None:
        raise ValueError(f"{path}: missing fmt or data chunk")
    if fmt["block_align"] == 0 or fmt["rate"] == 0:
        raise ValueError(f"{path}: zero block_align or sample rate")
    fmt["frames"] = data_size // fmt["block_align"]
    return fmt


def wav_duration_s(path):
    info = wav_info(path)
    return info["frames"] / info["rate"]


def summarize(values):
    """min, median, max and spread ((max - min) / min) of a non-empty list."""
    if not values:
        raise ValueError("no values to summarize")
    lo, hi = min(values), max(values)
    return {
        "n": len(values),
        "min": lo,
        "median": statistics.median(values),
        "max": hi,
        "spread": (hi - lo) / lo if lo > 0 else 0.0,
    }


def per_hop_us(cpu_s, audio_s):
    """CPU microseconds per decoder hop: cpu_s / (audio_s x 375 Hz)."""
    return cpu_s / (audio_s * HOP_RATE_HZ) * 1e6


def _ineligible_reason(record):
    for key, value in INELIGIBLE.items():
        if record.get(key) == value:
            return f'{key} = "{value}", decision D2'
    return None


def choose(results, mode="1"):
    """Decision D4. `results` maps variant name -> record with `panic`, `size_bytes`,
    `build_cpu_s` and `runs[mode]` (a list of {"cpu_s", "sha256"}). Output digests are compared
    against base across every mode in `runs`; CPU times are read from `mode` only. Returns
    (winner, reason, ineligible), where ineligible is a list of (name, why)."""
    if "base" not in results:
        raise ValueError("results have no base variant")
    order = [n for n in VARIANTS if n in results] + sorted(n for n in results if n not in VARIANTS)

    def runs(name):
        r = results[name]["runs"].get(mode) or []
        if not r:
            raise ValueError(f"{name} has no runs in mode {mode!r}")
        return r

    def all_runs(name):
        """Every run of `name` in every collected mode. Rule 1 checks output digests on any run;
        only the timing in rules 3 and 4 is limited to `mode`."""
        runs(name)  # the verdict mode must still be present
        return [run for mode_runs in results[name]["runs"].values() for run in mode_runs]

    base_digests = {run["sha256"] for run in all_runs("base")}
    if len(base_digests) != 1:
        raise ValueError("base decode output differs between its own runs or thread modes")
    ref = next(iter(base_digests))

    ineligible = []
    eligible = []
    for name in order:
        rec = results[name]
        if any(run["sha256"] != ref for run in all_runs(name)):
            ineligible.append((name, "decode output differs from base"))
            continue
        why = _ineligible_reason(rec)
        if why:
            ineligible.append((name, why))
            continue
        eligible.append(name)

    stats = {n: summarize([run["cpu_s"] for run in runs(n)]) for n in eligible}
    base = stats["base"]
    if base["min"] <= 0:
        raise ValueError("base decode used no measurable CPU time; check the scene")
    margin = max(SPEED_MARGIN, (base["median"] - base["min"]) / base["median"])
    rank = {n: i for i, n in enumerate(order)}
    fast = [
        n for n in eligible
        if n != "base"
        and stats[n]["median"] < base["median"] * (1 - margin)
        and stats[n]["min"] < base["min"] * (1 - margin)
    ]
    if fast:
        winner = min(fast, key=lambda n: (stats[n]["median"], results[n]["size_bytes"], rank[n]))
        gain = 1 - stats[winner]["median"] / base["median"]
        reason = (
            f"speed winner: median CPU {gain:.1%} below base, beyond the {margin:.1%} margin on "
            f"both median and best-of-N"
        )
        return winner, reason, ineligible

    cap = results["base"]["build_cpu_s"] * (1 + BUILD_CPU_CAP)
    affordable = [n for n in eligible if results[n]["build_cpu_s"] <= cap]
    winner = min(affordable, key=lambda n: (results[n]["size_bytes"], rank[n]))
    reason = (
        f"no speed winner: no variant beat base by more than {margin:.1%} on both median and "
        f"best-of-N; smallest binary within +{BUILD_CPU_CAP:.0%} build CPU"
    )
    return winner, reason, ineligible


def format_ineligible(ineligible):
    groups = {}
    for name, why in ineligible:
        groups.setdefault(why, []).append(name)
    if not groups:
        return "ineligible: none"
    return "ineligible: " + "; ".join(f"{', '.join(names)} ({why})" for why, names in groups.items())


def render_markdown(host, results, verdict, audio_s=None):
    winner, reason, ineligible = verdict
    lines = [
        "host: {machine}, {platform}, {cpu_model} x{cpus}, {rustc}, commit {commit}".format(
            **{k: host.get(k, "?") for k in
               ("machine", "platform", "cpu_model", "cpus", "rustc", "commit")}
        ),
    ]
    if host.get("load_start") or host.get("load_end"):
        lines.append(f"load average: start {host.get('load_start')}, end {host.get('load_end')}")
    if host.get("cpu_pin") is not None:
        lines.append(f"single-thread runs pinned with taskset -c {host['cpu_pin']}")
    for key in ("throttled_start", "throttled_end", "arm_clock_start", "arm_clock_end"):
        if host.get(key):
            lines.append(f"{key}: {host[key]}")
    if audio_s:
        lines.append(f"scene: {audio_s:.1f} s of audio = {audio_s * HOP_RATE_HZ:,.0f} hops")
    lines += [
        "",
        "| variant | lto | cgu | panic | size B | gzip -9 B | build wall s | build CPU s |",
        "|---|---|---|---|---|---|---|---|",
    ]
    names = [n for n in VARIANTS if n in results]
    for n in names:
        r = results[n]
        lines.append(
            f"| {n} | {r['lto']} | {r['codegen_units']} | {r['panic']} | {r['size_bytes']} | "
            f"{r['gzip_bytes']} | {r['build_wall_s']:.1f} | {r['build_cpu_s']:.1f} |"
        )
    lines += [
        "",
        "| variant | mode | runs | CPU min s | CPU median s | spread | median us/hop | output sha256 |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for mode in MODES:
        for n in names:
            runs = results[n]["runs"].get(mode)
            if not runs:
                continue
            s = summarize([run["cpu_s"] for run in runs])
            hop = f"{per_hop_us(s['median'], audio_s):.1f}" if audio_s else "?"
            digests = sorted({run["sha256"][:16] for run in runs})
            label = "1 thread" if mode == "1" else "all threads"
            lines.append(
                f"| {n} | {label} | {s['n']} | {s['min']:.2f} | {s['median']:.2f} | "
                f"{s['spread']:.1%} | {hop} | {' '.join(digests)} |"
            )
    lines += ["", format_ineligible(ineligible), f"verdict: {winner} ({reason})"]
    return "\n".join(lines) + "\n"


def _capture(argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True, check=False).stdout.strip()
    except OSError:
        return ""


ARM_PARTS = {"0xd03": "Cortex-A53", "0xd08": "Cortex-A72", "0xd0b": "Cortex-A76"}


def cpu_model_from_cpuinfo(text):
    """x86 names the CPU in `model name`; a Pi 4's aarch64 kernel has no such key, so fall back
    to `Model` (the board) plus the ARM `CPU part` (0xd08 is a Cortex-A72)."""
    first = {}
    for line in text.splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            first.setdefault(key.strip().lower(), value.strip())
    if first.get("model name"):
        return first["model name"]
    part = first.get("cpu part", "").lower()
    core = ARM_PARTS.get(part, f"ARM part {part}" if part else "")
    board = first.get("model") or first.get("hardware") or ""
    return ", ".join(x for x in (board, core) if x)


def host_facts(cpu_pin):
    model = ""
    if os.path.exists("/proc/cpuinfo"):
        with open("/proc/cpuinfo") as f:
            model = cpu_model_from_cpuinfo(f.read())
    if not model and sys.platform == "darwin":
        model = _capture(["sysctl", "-n", "machdep.cpu.brand_string"])
    facts = {
        "machine": platform.machine(),
        "platform": platform.platform(),
        "cpu_model": model or "unknown CPU",
        "cpus": os.cpu_count(),
        "rustc": _capture(["rustc", "-V"]),
        "commit": _capture(["git", "-C", ROOT, "rev-parse", "HEAD"]),
        "load_start": "/".join(f"{x:.2f}" for x in os.getloadavg()),
        "cpu_pin": cpu_pin,
    }
    if shutil.which("vcgencmd"):
        facts["throttled_start"] = _capture(["vcgencmd", "get_throttled"])
        facts["arm_clock_start"] = _capture(["vcgencmd", "measure_clock", "arm"])
    return facts


def _parse_list(text, known, what):
    items = [x.strip() for x in text.split(",") if x.strip()]
    bad = [x for x in items if x not in known]
    if bad:
        raise argparse.ArgumentTypeError(f"unknown {what}: {', '.join(bad)}")
    return items


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--out", required=True, help="scratch directory for targets, binaries, results")
    p.add_argument("--variants", default=",".join(VARIANTS),
                   help="comma list (default: all 8); base is always added")
    p.add_argument("--reps", type=int, default=7, help="timed rounds per variant and mode")
    p.add_argument("--threads", default="1",
                   help="comma list of modes: 1 (verdict) and/or all (informational)")
    p.add_argument("--cpu", type=int, default=None,
                   help="Linux only: pin single-thread decodes with taskset -c N")
    p.add_argument("--scene", default=None, help="reuse a directory holding cpu_budget.wav")
    p.add_argument("--dry-run", action="store_true", help="print the commands, run nothing")
    args = p.parse_args(argv)
    try:
        chosen = set(_parse_list(args.variants, VARIANTS, "variant")) | {"base"}
        args.variants = [n for n in VARIANTS if n in chosen]
        args.modes = [m for m in MODES if m in _parse_list(args.threads, MODES, "thread mode")]
    except argparse.ArgumentTypeError as e:
        p.error(str(e))
    if not args.modes:
        p.error("--threads names no mode")
    if args.reps < 1:
        p.error("--reps must be at least 1")
    if args.cpu is not None and not sys.platform.startswith("linux"):
        p.error("--cpu needs taskset, which only Linux has; drop it on this platform")
    return args


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    out = os.path.abspath(args.out)
    scene_dir = os.path.abspath(args.scene) if args.scene else os.path.join(out, "scene")
    wav = os.path.join(scene_dir, f"{SCENE_NAME}.wav")
    verdict_mode = "1" if "1" in args.modes else "all"
    if args.scene and not os.path.isfile(wav):
        print(f"--scene {args.scene}: no {SCENE_NAME}.wav there", file=sys.stderr)
        return 2

    if args.dry_run:
        if not args.scene:
            print(f"MANTA_CPU_BUDGET_SCENE_DIR={scene_dir} "
                  f"CARGO_TARGET_DIR={os.path.join(out, 'targets', 'scene')} "
                  + " ".join(scene_command()))
        for n in args.variants:
            lto, cgu, panic = VARIANTS[n]
            print(f"CARGO_PROFILE_RELEASE_LTO={lto} CARGO_PROFILE_RELEASE_CODEGEN_UNITS={cgu} "
                  f"CARGO_PROFILE_RELEASE_PANIC={panic} "
                  f"CARGO_TARGET_DIR={os.path.join(out, 'targets', n)} "
                  + " ".join(build_command()))
        for mode in args.modes:
            prefix = "RAYON_NUM_THREADS=1 " if mode == "1" else ""
            for n in args.variants:
                binary = os.path.join(out, "bin", f"manta-{n}")
                print(prefix + " ".join(decode_command(binary, wav, mode, args.cpu))
                      + f"  # x{args.reps} + 1 warm-up")
        if verdict_mode != "1":
            print("note: no single-thread mode, so the verdict would be informational only")
        return 0

    for sub in ("logs", "bin", "runs", "targets"):
        os.makedirs(os.path.join(out, sub), exist_ok=True)
    host = host_facts(args.cpu)

    if not args.scene:
        env = variant_env("base")
        env["CARGO_TARGET_DIR"] = os.path.join(out, "targets", "scene")
        env["MANTA_CPU_BUDGET_SCENE_DIR"] = scene_dir
        print("rendering scene:", " ".join(scene_command()), flush=True)
        with open(os.path.join(out, "logs", "scene.log"), "wb") as log:
            r = run_measured(scene_command(), env, stdout=log, stderr=subprocess.STDOUT, cwd=ROOT)
        if r["exit"] != 0:
            print(f"scene render failed (exit {r['exit']}); see {out}/logs/scene.log",
                  file=sys.stderr)
            return 1
    audio_s = wav_duration_s(wav)

    results = {}
    for n in args.variants:
        lto, cgu, panic = VARIANTS[n]
        target = os.path.join(out, "targets", n)
        # Clean builds only: a target dir left by an earlier run would make this an incremental
        # (near no-op) build and corrupt the build-cost figures the D4 cap reads.
        shutil.rmtree(target, ignore_errors=True)
        env = variant_env(n)
        env["CARGO_TARGET_DIR"] = target
        print(f"building {n} (lto={lto} codegen-units={cgu} panic={panic})", flush=True)
        with open(os.path.join(out, "logs", f"build-{n}.log"), "wb") as log:
            b = run_measured(build_command(), env, stdout=log, stderr=subprocess.STDOUT, cwd=ROOT)
        if b["exit"] != 0:
            print(f"build {n} failed (exit {b['exit']}); see {out}/logs/build-{n}.log",
                  file=sys.stderr)
            return 1
        binary = os.path.join(out, "bin", f"manta-{n}")
        shutil.copy2(os.path.join(target, "release", "manta"), binary)
        with open(binary, "rb") as f:
            blob = f.read()
        results[n] = {
            "lto": lto, "codegen_units": cgu, "panic": panic,
            "size_bytes": len(blob), "gzip_bytes": len(gzip.compress(blob, 9)),
            "build_wall_s": b["wall_s"], "build_cpu_s": b["cpu_s"], "build_maxrss": b["maxrss"],
            "runs": {m: [] for m in args.modes},
        }

    def decode(n, mode, timed):
        binary = os.path.join(out, "bin", f"manta-{n}")
        path = os.path.join(out, "runs", f"{n}-{mode}.json")
        with open(path, "wb") as sink, open(os.path.join(out, "logs", f"decode-{n}.log"), "ab") as err:
            r = run_measured(decode_command(binary, wav, mode, args.cpu), decode_env(mode),
                             stdout=sink, stderr=err, cwd=ROOT)
        if r["exit"] != 0:
            raise RuntimeError(f"{n} decode ({mode}) exited {r['exit']}; see logs/decode-{n}.log")
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for block in iter(lambda: f.read(1 << 20), b""):
                h.update(block)
        if timed:
            results[n]["runs"][mode].append(
                {"cpu_s": r["cpu_s"], "wall_s": r["wall_s"], "sha256": h.hexdigest()}
            )

    def save_partial(error):
        partial = os.path.join(out, "results.partial.json")
        with open(partial, "w") as f:
            json.dump({"host": host, "audio_s": audio_s, "results": results, "error": str(error)},
                      f, indent=2)
        print(f"{error}; timings so far are in {partial}", file=sys.stderr)
        return 1

    try:
        for mode in args.modes:
            for n in args.variants:
                print(f"warm-up {n} ({mode})", flush=True)
                decode(n, mode, timed=False)
        for rnd in range(args.reps):
            k = rnd % len(args.variants)
            order = args.variants[k:] + args.variants[:k]
            for mode in args.modes:
                for n in order:
                    decode(n, mode, timed=True)
            print(f"round {rnd + 1}/{args.reps} done", flush=True)
    except RuntimeError as e:
        return save_partial(e)

    host["load_end"] = "/".join(f"{x:.2f}" for x in os.getloadavg())
    if shutil.which("vcgencmd"):
        host["throttled_end"] = _capture(["vcgencmd", "get_throttled"])
        host["arm_clock_end"] = _capture(["vcgencmd", "measure_clock", "arm"])
    try:
        verdict = choose(results, verdict_mode)
    except ValueError as e:  # e.g. base output differs between runs or thread modes
        return save_partial(e)
    md = render_markdown(host, results, verdict, audio_s)
    if verdict_mode != "1":
        md += "note: no single-thread mode was run, so this verdict is informational only\n"
    with open(os.path.join(out, "results.json"), "w") as f:
        json.dump({"host": host, "audio_s": audio_s, "results": results,
                   "verdict": {"winner": verdict[0], "reason": verdict[1],
                               "ineligible": verdict[2], "mode": verdict_mode}}, f, indent=2)
    with open(os.path.join(out, "results.md"), "w") as f:
        f.write(md)
    print(md, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
