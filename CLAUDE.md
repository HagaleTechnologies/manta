# manta

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — a
second, independent implementation alongside CW Skimmer Server, for the
Linux/ARM/headless platforms it has no native build for.

## Build and test

Needs Rust 1.98.0 (`rust-toolchain.toml`), `git`, and on Linux `libasound2-dev`
and `pkg-config`. Before pushing, run what `ci-full.yml`'s `test`/`test-hpsdr`
jobs run (on Windows CI runs only `test_package_release` of the script suites;
`ci_gates` and `verify_dependabot_diff_safe` run on Linux only):

```sh
bash scripts/check-rust-toolchain-pin.sh && cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --locked
bash scripts/tests/release-version.test.sh && bash scripts/tests/release-ancestry.test.sh
python3 -m unittest -v scripts.tests.test_{ci_trust_boundary,release_checksums,soapy_release,\
packaging,package_release,score_against_rbn_replay,ci_gates,verify_dependabot_diff_safe,\
decisions_index}
cargo clippy -p manta-input -p manta-cli --all-targets --features hpsdr --locked -- -D warnings
cargo test -p manta-input -p manta-cli --features hpsdr --locked
```

## Status

M1 is implemented (manual W1AW live-copy run outstanding). M2 acceptance is
open (ROADMAP.md): both CPU-budget legs, the 24 h live-SDR soak and the
VR1–VR8 gate. The V5/V8w fading gap is classical-DSP work before M4 (D8,
docs/DECISIONS/2026-09-06-broad-review-decisions.md). `decode.engine` stays
`legacy` (`hsmm` is not promoted); `manta-dsp::single`/`freqest` are
deprecated in place and stay. A ticket's outcome goes in its decision record,
not here; its docs/DECISIONS/README.md row is generated after it lands
(MAN-440). The log this section held until
2026-10-16 is docs/DECISIONS/2026-10-16-man238-standing-brief.md.

MAN-272 (replaces MAN-33): each spot carries its track's latest RST (`5NN`
read as `599`) and a sticky `QRL?` flag, scanned per completed word by
`manta_spot::message`; neither gates validation. JSON spots gain `rst`/
`qrlQuery` (ahead of dispensa) and the `SPOT:` line an optional ` rst=599`/
` QRL?`; telnet and the RBN uplink are unchanged. A track merge does not
carry the loser's annotations (D5). SPEC §7.1 V32/V33. See
docs/DECISIONS/2026-10-16-man272-rst-and-qrl-on-spots.md.

MAN-273: the live-audio Hilbert FIR is 511 taps (sparse over its 256
nonzero taps; >= 70 dB image rejection over [600, 23400] Hz at 48 kHz),
and `AudioIqSource` declares `IqSource::analytic_guard_hz()` = 600 Hz,
which spawn-vetoes the DC and Nyquist rims (22 of 512 channels at
48 kHz, spawns only, like MAN-196). Tones below ~515 Hz (or as close to
Nyquist) start no track through rig audio; 600 Hz still does. Testkit
fixtures keep the 129-tap `HILBERT_TAPS_M1_LEGACY`, so no golden vector
moves. An analytic source
also vetoes spawns on the mirror `(n - k) % n` of an owned or stronger
channel (raw power, 3 dB margin): the FIR strips negative-side noise as
deeply as the image, so the image cleared its floor. See
docs/DECISIONS/2026-10-16-man273-hilbert-image-guard.md.

MAN-319: a live `[[rbn_uplink]]` (`enabled` + `dry_run = false`) refuses
to load without a 6-character `[server].operator_grid`, from the one
cross-table rule in `manta-cli`'s `load()` (check, run, doctor and the
SIGHUP reload all share it; a no-grid live flip is an invalid reload per
MAN-78, and `[server]` being restart-only, a reload goes live only if the
daemon started with the grid). The ticket's registration/settings/wait/
reference-file scenarios put nothing on the wire: they are blocked on
MAN-90's capture and recorded as open claims C8–C11 in the MAN-90 table.
Dry-run keeps its recorded "logs in, sends no spots" reading; the future
registration phase must not register while dry-run is on. See
docs/DECISIONS/2026-10-19-man319-uplink-registration-exchange.md.

MAN-14: with 2+ `[[input]]` entries, /metrics carries per-input
`manta_input_spots_total`, `manta_input_distinct_calls_total`,
`manta_input_cross_input_dupes_total` (label = the withheld input) and
`manta_input_silent`; the status line appends `hearing=name:spots/calls/dupes`
and /status an `inputs` array. `[server].silent_input_warning_secs` (default
600, 0 off) warns when one input decodes nothing while another decodes;
/healthz and the spot wire are unchanged. See
docs/DECISIONS/2026-10-19-man14-per-receiver-hearing-visibility.md.

## Documents (read in this order)

- `docs/README.md` (start here, by audience); `README.md` (goals, install)
- `ARCHITECTURE.md` — 9-crate workspace, data flow, component design
- `docs/SPEC-decode-core.md` — the algorithm spec to implement from. §7 has
  the golden vectors: V1–V10, V31, V34 and V51 (decode), V11–V30 and V38–V50
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
