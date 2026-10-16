# manta

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — a
second, independent implementation alongside CW Skimmer Server, for the
Linux/ARM/headless platforms it has no native build for.

## Build and test

Needs Rust 1.98.0 (`rust-toolchain.toml`), `git`, and `libasound2-dev` on
Linux. Before pushing, run what `ci-full.yml`'s `test`/`test-hpsdr` jobs run:

```sh
bash scripts/check-rust-toolchain-pin.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo clippy -p manta-input -p manta-cli --all-targets --features hpsdr --locked -- -D warnings
cargo test -p manta-input -p manta-cli --features hpsdr --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --locked
```

## Status

M1 is implemented (manual W1AW live-copy run outstanding). M2 acceptance is
open (ROADMAP.md): both CPU-budget legs, the 24 h live-SDR soak and the
VR1–VR8 gate. The V5/V8w fading gap is classical-DSP work before M4 (D8,
docs/DECISIONS/2026-09-06-broad-review-decisions.md). `decode.engine` stays
`legacy` (`hsmm` is not promoted); `manta-dsp::single`/`freqest` are
deprecated in place and stay. A ticket's outcome goes in its decision record
and docs/DECISIONS/README.md row, not here. The log this section held until
2026-10-16 is docs/DECISIONS/2026-10-16-man238-standing-brief.md.

## Documents (read in this order)

- `docs/README.md` (start here, by audience); `README.md` (goals, install)
- `ARCHITECTURE.md` — 9-crate workspace, data flow, component design
- `docs/SPEC-decode-core.md` — the algorithm spec to implement from. §7 has
  the golden vectors: V1–V10, V31 and V51 (decode), V11–V30 and V38–V50
  (validator), VC1–VC3, VW1–VW5; `docs/SPEC-decode-core-v2.md` §8.2 has
  VR1–VR8.
- `ROADMAP.md` — milestones M0–M4 with acceptance criteria
- `wiki/INDEX.md` — gotchas: read it before deep exploration; run
  /wiki-update after substantive work. It points, never restates.

## Key constraints

- Deterministic decode: file input → byte-identical spot logs.
- Classical decoder first; ML fusion (dit's pattern) only at M4, gated on
  beating the classical baseline under simulated fading.
- CPU budget: full 192 kS/s passband in one Raspberry Pi 4 core. A target,
  not a CI gate: `crates/manta-engine/benches/cpu_budget.rs` only profiles.
- coppa (FFT, audio, Watterson) is pinned to `f8a4d16d` in `Cargo.toml`;
  name that commit when regenerating fading vectors. Watterson: 2σ Doppler
  width, ensemble-only normalization (fixes `9ab1547`/`34aec5f`/`fc35895`).
- SNR is 2500 Hz-referenced in the pipeline and testkit (whose own AWGN
  stands in until coppa's `awgn_ref_bw()` reconciles it with the benchmark's
  3 kHz); telnet and RBN uplink quote 500 Hz; JSON keeps 2500 Hz plus
  `snrRefHz` (broad-review D3).
- `AudioIqSource` needs exactly 48000 Hz input (M1 pins record, pin 3).
- JSON spot schema: `dispensa` ADR-0011 `spots.v1` (`docs/SPOT-SCHEMA.md`
  in-repo). No GUI; telnet :7300, JSON Lines/WebSocket :7301.

## Multi-agent hygiene

Other agents work concurrently in other clones, branches and worktrees.

- **Start fresh:** `git fetch` and rebase onto `origin/main` before reading
  code or making decisions; stale context produces wrong work.
- **Claim before work:** search open PRs/issues first; open a draft PR
  early — the draft PR *is* the claim.
- **Isolate:** always a branch (worktree preferred), never a shared
  checkout's main; per-session scratch dirs; no fixed ports.
- **Flush at the end:** push (`--force-with-lease` only) and open/update
  your PR. Unpushed work is invisible work.

## Merge rules

Main moves only through GitHub's merge queue (squash; the PR title becomes
the commit subject, `<type>(<scope>): <subject>`). The queue merges when:

- four required checks pass on a branch up to date with `main`:
  `test (ubuntu-latest)`, `test (macos-latest)`, `Supply Chain`,
  `Codex review clean (gate before expensive downstream jobs)`;
- @thagale approved, if the PR touches a `.github/CODEOWNERS` path;
- every review thread is resolved.

`auto-merge-trigger.yml` arms auto-merge on ready PRs by `thagale` or
`catalyst-cloud-connector[bot]` unless labelled `needs-human`, overriding the
global "Tony merges" default; `dependabot-auto-merge.yml` approves and arms
verified Dependabot minor/patch bumps. These two are the only admission gates:
run `gh pr merge --auto --squash` only on a PR you authored; never `--admin`.

## Code review convergence

Every review round fixes P1 findings inline. Round 1 is unrestricted; from
round 2 on, P2-and-lower findings go verbatim into a follow-up ticket, so
the PR converges: docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md.
