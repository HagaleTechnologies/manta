# Synthetic repros (MAN-206)

Each `*.json` file here is a minimized, synthetic-only reproduction of a
failure: a `manta-testkit` `SceneSpec`, the probe that detects the failure,
the failure signature it returns, and per-target digests of the rendered
samples. `synthetic_repros.rs` replays every file in required CI and lints
the directory.

## Rules

- Synthetic only. A scene is a description the testkit renders
  (`origin: "synthetic"` is the only origin). Never commit recorded IQ or
  audio, or anything derived from it: the lint rejects any file other than
  `*.json` and this README, any file over 8 KiB, and any `name` or
  `found_in` naming a `.wav`/`.cf32`/`.ci16` file, `audio-corpus`, or a
  recording or hash from `crates/manta-testkit/audio-corpus/manifest.json`.
- No RBN or dictionary identities as ground truth. Callsigns come from the
  scene (V8w's are ChaCha8-generated).
- `expect: "fails"` means the failure is still present: replay passes while
  the probe returns `signature` and fails once someone fixes the failure.
  Flipping `expect` to `"passes"` (and refreshing `digests` if the fix moved
  rendered bytes) is part of the change that fixes it.
- `generator_revision` must equal `manta_testkit::scenespec::GENERATOR_REVISION`.
  Digests are keyed `"{ARCH}-{OS}"`; replay checks the running target's
  entry when one is recorded (faded scenes go through coppa's SIMD FFT).

## Making one

1. Describe the failing scene as a `SceneSpec` (for a golden vector,
   `SceneSpec::from_vector`).
2. Write a probe returning `None` (passes) or a sorted failure signature;
   `support/ghost.rs` is the example. Map decode errors to their own entry.
3. Run `manta_testkit::minimize::minimize` and write the result with
   `manta_testkit::repro::from_minimized`. The existing repro came from:

   ```sh
   MANTA_REPRO_OUT="$PWD/crates/manta-engine/tests/repros/v8w-near-dc-mirror-ghost.json" \
     cargo test -p manta-engine --locked --test minimizer \
     minimizer_shrinks_a_120s_multi_signal_scene -- --nocapture
   ```

4. Add the probe's name to `synthetic_repros.rs` if it is new.
