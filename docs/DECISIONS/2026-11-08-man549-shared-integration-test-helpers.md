# MAN-549: integration tests share one manta launcher, port reservation and word_boundary_fraction

## Context

Before this change the `manta-cli` integration tests carried 33 definitions
of `fn manta()` (32 test roots plus `common/ka9q.rs`), 39 copies of the loop
that strips `MANTA_*` variables from a child's environment, and 11
port-reservation helpers, 10 of them the same hold-all-N-listeners algorithm
(MAN-371). `word_boundary_fraction` (MAN-173) was copied byte for byte in
`golden_vr.rs` and `golden_vw.rs`. A fix to any of these (MAN-410, MAN-508)
had to be applied N times, and a new test copied whichever variant it found.

## Decision

- The launcher, its scrub and the port reservation live in one `std`-only
  leaf, `crates/manta-cli/tests/common/launch.rs`: `manta()`,
  `without_manta_env(cmd)` and `free_ports::<N>()`. `common/mod.rs` declares
  `mod launch;` and re-exports all three under `#[allow(unused_imports)]`,
  since many roots include `common` for its other helpers only.
- A root without `mod common;` includes the leaf alone with
  `#[path = "common/launch.rs"] mod launch;`. Adding `mod common;` to those
  roots instead would link `manta_input`, `regex` and `tungstenite` into
  each of them: measured at +583 MB over the 71 test executables, against
  +2.0 MB for the leaf (MAN-518 packs these executables as a CI artifact).
  `multi_input.rs` already said `mod common;`, but only under
  `#[cfg(unix)]`, while its launcher and port helper run on every OS. Its
  `mod common;` is now unconditional: a second `#[path]` include of the leaf
  beside it fails clippy's `duplicate_mod` on Unix.
- `manta()` always scrubs (MAN-261), including in `docs_consistency`,
  `man203_conditioning` and `man234_bad_samples`, whose launchers did not.
- `bench_sensitivity`, `calibrate`, `man203_conditioning`,
  `man234_bad_samples` and `output_style` keep a thin local `fn manta` that
  adds one step (args in and `Output` out, stdin null, `RUST_LOG` removed)
  around the shared one. Their call sites do not change.
- `doctor_checks.rs` keeps its band allocator (MAN-333, MAN-383, MAN-410): it
  claims UDP per port over a private band, which is a different contract.
  `multi_input_hpsdr.rs`'s `ports()` was the N = 3 case of the shared body
  and is now `free_ports()`.
- The nine inline scrubs outside any `fn manta` call `manta()`, or
  `without_manta_env(...)` in `docker_image.rs`'s `script::clean`, which
  scrubs a script, not `manta`. `decode_wav_formats.rs`'s Linux-only `sh`
  wrapper calls `launch::without_manta_env` by path rather than importing
  it, so no OS sees an unused import.
- `word_boundary_fraction`, with its private `AlignOp` and
  `char_alignment_ops`, moves to `crates/manta-testkit/src/cer.rs` beside
  `cer`, and its 14 unit tests move into that module's tests. Their ids
  change from `golden_vr::word_boundary_fraction_*` to
  `cer::tests::word_boundary_fraction_*`. The scorer keeps its own
  case-preserving `normalize`; `cer`'s upper-cases.
- `crates/manta-cli/tests/shared_test_helpers.rs` scans the test sources and
  fails if a scrub loop, a `fn free_ports`, an unwrapped `fn manta` or a
  `word_boundary_fraction` copy comes back.

## Behaviour notes

In a clean environment no test changes behaviour. With a `MANTA_*` variable
set in the runner's environment, the `docs_consistency`,
`man203_conditioning` and `man234_bad_samples` tests that use `manta()` no
longer inherit it. The four inline scrubs that used `std::env::vars()` now
use `vars_os()`, which only removes a panic on a non-Unicode variable.

## Not done

This change ships in two slices, to stay within the 40-file pull-request
limit. Eighteen roots without `mod common;` still define the byte-identical
scrubbing `fn manta`: `build_identity`, `cli`, `cli_json_consumers`,
`completions_and_man_pages`, `config_command`, `decode_observations`,
`decode_sidecar`, `ftx_acceptance`, `grade_cmd`, `hpsdr_discovery`,
`iq_correction`, `live_audio_errors`, `live_state_acceptance`,
`operator_guides`, `oracle_funnel`, `packaging_examples`,
`replay_acceptance` and `source_check`. Each needs only the `#[path]`
include and `use launch::manta;`. `shared_test_helpers.rs` names them in
`PENDING_LAUNCHER_COPIES`, which may only shrink: a listed file that no
longer defines `fn manta` fails the guard. The follow-up converts them and
empties the list.

Spawns that never scrubbed keep their inline `Command::new(env!("CARGO_BIN_EXE_manta"))`:
routing them through `manta()` would change what they send the child.
`free_port` (singular), `free_udp_port` and `list_refresh.rs`'s
`refused_source` (MAN-508) are untouched. No `manta-engine` test helper
moves, because no two bodies of `decode(`, `scene(` or `spec(` under
`crates/manta-engine/tests` are identical. No production code, `Cargo.toml`
or CI workflow changes.
