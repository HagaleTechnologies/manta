# MAN-158: ROADMAP holds gates and pointers; SECURITY.md states the shipped scope

ROADMAP.md's M2 section now keeps the acceptance criteria, their status and
pointers to the evidence. SECURITY.md now describes the shipped inputs and
network exposure. No milestone gate or runtime behaviour changes.

## Where the M2 history lives

The removed narration repeated these records. Keep measurements and
implementation history there, with later records taking precedence over
the original pins.

| Fragment removed from ROADMAP.md | Record |
|---|---|
| Mac CPU measurements, the warmup correction, and the Pi4 estimate | [MAN-18 CPU-budget status](2026-09-02-man18-pi4-cpu-budget-gate.md) |
| VR1–VR8 origin: the B2/K5TR keying-threshold defect V1–V10 missed, and their promotion to standing M2 gates (MAN-166) | [Decode-core real-HF design](../superpowers/specs/2026-09-09-decode-core-real-hf-design.md) §1 and §8 |
| The 2026-09-09 hsmm VR result, 3/9 passing | [Decode-core-v2 stage-2 gate](2026-09-09-decode-core-v2-stage2-gate.md) |
| V8w spot recall restored to 28/50, with 0 bogus calls | [MAN-213 fading fixes](2026-10-05-man213-fade-tracking-keying-rails.md) |
| PFB channelizer completion | [PFB pins](2026-07-18-m2-pfb-channelizer-pins.md) |
| Detector, track manager and decoder pool completion | [Detector/track-pool pins](2026-07-19-m2-detector-track-pool-pins.md) |
| Pileup validation and CPU benchmark implementation | [Pileup/CPU-budget pins](2026-07-24-m2-pileup-cpu-budget-pins.md) |
| SoapySDR input completion | [SoapySDR pins](2026-07-25-m2-soapysdr-input-pins.md) |
| KiwiSDR input completion | [KiwiSDR pins](2026-07-25-m2-kiwisdr-input-pins.md) |

The original V8w measurement was **1/34 (2.9 %)** of signals at ≥ +6 dB
SNR meeting CER < 10 %, against a ≥ 90 % threshold. It was tracked as
GitHub issue #28 and is also recorded in the pileup/CPU-budget pins above.
This is a historical per-signal CER result, distinct from MAN-213's later
28/50 spot-validated recall. Restoring recall did not close the CER gate.

## Standing decisions and open gates

[Broad-review decision D6](2026-09-06-broad-review-decisions.md) pauses
Pi4 CPU-budget work and performance claims until the decode-quality fixes
land. ROADMAP retains the CPU-budget targets without presenting historical
measurements as a pass. Both the Mac and Pi4 legs remain unresolved.

The same record's D8 requires classical-DSP fading fixes before M4's ML
fusion. ROADMAP now states that ordering, replacing its conflicting
assignment of the V8w gap to M4. V8w remains non-blocking for M2, and its
per-signal CER gate remains open.

M2 is still incomplete. Its open gates are VR1–VR8, both CPU-budget legs
and the 24 h live-SDR soak. Completing the five M2 sub-projects does not
discharge those gates. The soak procedure remains in the
[field-node runbook](../RUNBOOKS/secondary-skimmer-field-node.md).

## Security scope

SECURITY.md now names WAV files and paced replay, rig audio, and the
shipped KiwiSDR, HPSDR and ka9q-radio network inputs. It identifies
SoapySDR as implemented behind the opt-in `soapy` build feature.

The scope also states that telnet and JSON Lines/WebSocket bind publicly
by default, metrics binds to loopback by default, the MAN-324 live-state
stream is opt-in and loopback by default, and the outbound RBN uplink
defaults to dry-run. Untrusted-input examples now include hostile network
peers on the SDR inputs, the `[rbn_compare]` feed and the MAN-316
cty.dat/MASTER.SCP refresh, which downloads over plain HTTP. It points to
both recorded threat models, each with the surfaces it covers: the
[MAN-23 threat model](2026-09-02-man23-threat-model.md) (HPSDR Protocol 1 input;
telnet, JSON/WebSocket and metrics servers; RBN uplink) and the
[MAN-275 KiwiSDR threat model](2026-10-17-man275-kiwi-threat-model.md),
which replaced MAN-60. It also points to the
[network-exposure guide](../RUNBOOKS/network-exposure.md). It does not
claim that the two records cover every input or surface. The
vulnerability-reporting instructions and response expectation stay
unchanged.
