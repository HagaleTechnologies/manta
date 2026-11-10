# MAN-532: full-duration beacon fading regression

Date: 2026-11-09. Ticket: MAN-532.

## Decision

Keep the production validator unchanged. MAN-201 D8 already restricts
the trailing-`T` Beacon heuristic to the decoded allowlisted call.
Listing K5ARH does not exempt NE5ARH or V5ARH. Add permanent full-duration
IQ coverage and a focused validator regression for those historical garbles.

MAN-120 D2 measured garbled Beacon calls at 120 seconds and +10/+13/+16/+19
dB before MAN-201. Its committed test used only 40 seconds at +10 dB.
Those historical measurements remain in its record. MAN-532 restores
120-second coverage and adds the ticket's +6 dB case.

## Recipe and assertions

`crates/manta-cli/tests/man532_beacon_fading.rs` renders four scenes.
All use 96 kS/s, 120 seconds, center 14 MHz, offset +9100 Hz, 20 WPM,
looping `K5ARH T`, noise seed `0x4D414E313230`, and 5 ms rise.
SNR is +6 or +10 dB referenced to 2500 Hz. Each SNR has a Watterson
CCIR-poor scene with fade seed `0x5631` and an AWGN control with the
same noise seed. Watterson is pinned to coppa `f8a4d16d`.

Each scene is decoded with `--engine legacy`, once unlisted and once with
only K5ARH allowlisted. EOF completes track closure. Every spot in the
complete report must carry K5ARH, regardless of type. Unlisted scenes
must emit no Beacon. Listed faded scenes must emit at least one genuine
Beacon; listed AWGN controls must emit exactly one. Positive and negative
controls are separate tests. No faded exactly-once requirement is imposed,
because a type change can legitimately re-spot a genuine call.

Fixtures use a MAN-532-prefixed process-local temporary directory and
separate SNR/channel subdirectories, shared through OnceLock. Both WAV
and manifest must exist after rendering. No persistent cache or generated
WAV is committed, so a shortened MAN-120 fixture cannot satisfy this test.

## Regression sensitivity

`cargo test -p manta-spot --locked --test golden_v58_beacon_evidence historical_garbles -- --nocapture`
passed with MAN-201 intact. Removing only the decoded-candidate allowlist
restriction in `find_candidates` made the same command exit 101.
It emitted NE5ARH as a tentative Beacon at 22 WPM, confidence 0.475,
and failed the no-spot assertion. The mutation was restored immediately.
`cargo test -p manta-spot --locked --test golden_v58_beacon_evidence -- --nocapture`
then passed both tests. The permanent regression checks NE5ARH and V5ARH
independently with metadata, a plausible final speed and TrackClosed,
and requires one BeaconNoEvidence decision for each occurrence.
Its local country fixture includes Namibia so V5ARH is allocated.

## Measured results

On fetched main `58406a20d7cb57fa2a21ae10a1dbe2a1012b050d`, all eight
120-second decodes passed. Every emitted call was K5ARH.

- +6 dB poor, unlisted: no spots.
- +6 dB poor, K5ARH listed: 4 Beacon and 2 Unknown.
- +10 dB poor, unlisted: no spots.
- +10 dB poor, K5ARH listed: 5 Beacon and 3 Unknown.
- AWGN at both SNRs, unlisted: no spots.
- AWGN at both SNRs, K5ARH listed: 1 Beacon at 2.898667 s.

`cargo test -p manta-cli --locked --test man532_beacon_fading --test man120_beacon_fading --test golden_vw -- --nocapture`
exited 0: MAN-532 4 passed in 64.07 s, MAN-120 4 passed,
VW1–VW5 5 passed. These are current passing measurements, not a
reproduction of the historical failure. The existing fading-is-real
check and the clean timing controls remain active.

`cargo test -p manta-spot --locked` exited 0, including V11/V18/V30/V42/V58
and pending-beacon lifecycle coverage. `cargo build --all-targets` exited 0.

Preparation rebased the pinned checkout onto the fetched main SHA above.
Upstream commits are separate from the six MAN-532 regression/documentation
files. Production validation policy is unchanged relative to that main.

## Scope and limits

This closes the recorded trailing-`T` mechanism on the specified
deterministic scenes. It does not measure real RF, +13/+16/+19 dB today,
arbitrary allowlists containing corrupted calls, or framed-beacon false
positives. A non-allowlisted `V V V <call>` still uses deferred judgement.
That separate, unmeasured risk remains open.

Do not add a confidence floor or repetition/rival gate on this evidence.
V42 deliberately preserves a genuine single W6DPG ID after repeated
corrupted W6DPH. This ticket changes no decoder output policy, golden
baseline, dependencies or allowlist contract.
