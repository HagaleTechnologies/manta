# MAN-311: exclude common digital-mode windows from CW track starts

Date: 2026-10-19

## Outcome

The default detector starts no candidate on a channel whose source-reported
RF center falls in the common HF FT8, FT4, WSPR or JS8 windows. Each window
is `[USB dial, USB dial + 3000 Hz)`. The exact table and operator examples
are in [CONFIG.md](../CONFIG.md#digital-mode-windows).

`[detector] digital_window_filter = false` includes these windows for CW.
`MANTA_DETECTOR_DIGITAL_WINDOW_FILTER=false` overrides the file. The value
must be a boolean, and changes require a restart. Omitted keys enable the
policy, including in existing configs. One setting applies to all service
inputs and file decoding. Other signal filters still apply.

## Decisions

- Store the fixed dial frequencies as integer Hz in the engine's private
  `digital_windows` module. The table follows the pinned
  [WSJT-X defaults](https://github.com/WSJTX/wsjtx/blob/d679c9345e5c0ef5943a988d39ca9dbff4721de9/models/FrequencyList.cpp)
  and [JS8Call defaults](https://github.com/js8call/js8call/blob/3a3f5291c597ea3ed593ab16a99e7d4ad6720e5d/FrequencyList.cpp).
  Include common HF defaults from 160 m to 10 m, including 60 m. Regional
  and DXpedition alternatives and VHF/UHF remain outside this policy.
- Build an optional boolean mask in `TrackManager::new`, using each
  receiver's center, rate and wrapped channel offsets. Disabled filtering
  allocates no mask. Both spawn arbitration slots consult the mask; no
  per-hop table scan is needed. Construction here also covers reconstructed
  managers and avoids a separate offline or multi-input setup path.
- Treat overlapping windows as their union. Membership includes the dial
  and excludes the upper endpoint unless another window covers it. The
  normal grid is 93.75 Hz. This is a spawn restriction, so leakage near an
  edge can reach an allowed channel, and existing tracks may drift across
  boundaries. Downstream frequency correction does not move the mask.
- Preserve the scaffold's established commented-default convention with
  `#digital_window_filter = true`. Its comment explains the opt-out and
  restart requirement. This differs from the plan's active default line;
  `config_cmd.rs::scaffold_lists_exactly_the_keys_the_loader_accepts` parses
  commented settings and enforces that convention.
- Leave decoder engines, spot formats, metrics and SETT unchanged. MAN-30's
  configurable ranges and MAN-310's segment expansion remain separate.

## Verification

Before adding the policy, the new unit test
`digital_window_continuous_14074500_starts_no_candidate` failed at hop 3763:
`total_spawns` was 1 rather than 0. It uses the floor warmup and a steady
20 dB peak on channel 48 of a 512-channel, 48 kS/s receiver centered at
14.070 MHz. With the policy it stays zero throughout. Disabling only this
setting creates one candidate. The census also verifies both arbitration
slots, a stronger excluded adjacent channel, negative offsets, receiver
centers, an allowed channel and independence from RTTY/non-CW filtering.

The deterministic integration suite renders repeated clean CW at
14.0745 MHz with W1AW at 14.06334 MHz. Defaults suppress the in-window
station; opting out recovers its K5ARH text and preserves W1AW. It exercises
`decode_samples`, the streaming listener and two simultaneous memory-backed
service inputs with different centers. Repeated offline and single-input
streaming runs compare whole reports or event/spot output.

Config tests cover omitted/true/false values, wrong types, environment
precedence, restart classification and scaffold/SPEC defaults. The existing
RTTY and non-CW suites explicitly opt out because their synthetic fixtures
cross these windows; their signal-classifier assertions and golden files
are preserved.

Focused verification commands:

```sh
cargo test -p manta-engine --lib --locked digital_window
cargo test -p manta-engine --test digital_windows --locked
cargo test -p manta-cli --locked digital_window
cargo test -p manta-cli --locked scaffold
cargo test -p manta-cli --test docs_consistency --locked
```

The implementation phase records the all-target build and the repository's
full pre-push gate separately. No live radio is needed to verify this static
policy. Candidate counts prove avoided allocations and decoder leasing;
no whole-passband CPU saving has been measured.
