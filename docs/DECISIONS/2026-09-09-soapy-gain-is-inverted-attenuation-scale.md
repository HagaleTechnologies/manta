# `--soapy-gain` on this RSP1B is a gain-*reduction* (attenuation) scale, not a gain scale -- every prior field session used a heavily-attenuated setting

Triggered by Tony's suspicion that the RSP1B itself, not the antenna or
propagation, might be misconfigured: the same antenna hears FT8 very
strongly (a different receiver, an FTdx10), but every manta field session
to date has heard almost no real CW. All three prior live sessions
(`2026-09-08-first-live-rsp1b-run.md`,
`2026-09-09-overnight-40m-soapy-field-test.md`,
`2026-09-09-post-pr154-20m-daytime-validation.md`, plus tonight's earlier
captures) used `--soapy-gain 40`. This session checked, for the first
time, what that value actually does to the hardware's real gain stages
rather than just whether the stream activates.

## Method

A standalone Rust probe (`soapysdr` crate directly, bypassing manta,
mirroring the technique from `2026-09-08-first-live-rsp1b-run.md` Finding
2) opened the RSP1B, disabled AGC (`set_gain_mode(false)`, matching
`crates/manta-input/src/soapy.rs`'s own logic exactly), then called
`set_gain(Rx, 0, N)` for every `N` from 0 to 48 and read back the two
named gain elements (`IFGR`, `RFGR`) SoapySDRPlay3 actually applied.

## Result: `--soapy-gain`'s scale runs backwards from what its name suggests

| requested | IFGR (range 20-59) | RFGR (range 0-9) |
|---|---|---|
| 0 | 20 (min reduction) | 0 (min reduction) |
| 20 | 40 | 0 |
| 39 | 59 (**max reduction**) | 0 |
| **40 (used in every field session)** | **59 (max)** | 1 |
| 48 | 59 (max) | 9 (max reduction) |

Both `IFGR` and `RFGR` are literally *gain reduction* (attenuation)
quantities in SDRplay's own model -- higher means less sensitivity. The
generic SoapySDR `gain` abstraction here tracks that same sense directly:
requesting a *higher* overall value produces *more* attenuation, not more
amplification. `0` is maximum sensitivity; `48` is minimum. Every prior
field session, at `--soapy-gain 40`, ran with `IFGR` already pinned at
its absolute maximum (59, the top of its whole range) -- 39 dB more IF
attenuation than the true minimum, for no documented reason; that value
was chosen in `2026-09-08-first-live-rsp1b-run.md` Finding 2 only to
avoid an activation failure at the very top of the range (46-48), not
because it was anywhere near a sensitivity optimum. This wasn't
previously checked because Finding 2 only tested pass/fail of
`activateStream()` at each value, never what `{IFGR, RFGR}` split a
"safe" value actually produced.

This is consistent with an upstream SoapySDRPlay gain-modeling class of
issue (`pothosware/SoapySDRPlay2#60`, `pothosware/SoapySDRPlay3#35` --
the unnamed `setGain()` path distributing a single value across
non-orthogonal, inverted-sense named elements is a known source of
confusing behavior for this driver family), though the specific direction
observed here (IFGR saturating high before RFGR moves at all) is this
repo's own empirical data, not quoted from those threads.

## But minimum attenuation isn't automatically best either

A live 60s `manta doctor` sweep on 20m (14030 kHz, same antenna/time
window) shows a real interior optimum, not "lower is always better":

| gain | chars_decoded | snr_db_max | verdict |
|---|---|---|---|
| 0 | 0 | -1.58 | `NoisyNoDecode` (worse than 40) |
| 10 | 6465 | 2.43 | `WeakNoDecode` |
| 20 | 5008 | 6.52 (**best peak SNR of all tested**) | `WeakNoDecode` |
| 30 | 3677 | 4.11 | `WeakNoDecode` |
| 40 (prior sessions' value) | 1138 | 3.81 | `WeakNoDecode` |

At `gain=0` (max sensitivity, min attenuation) the front end is plausibly
overloaded/desensitized by broadband energy across the busy antenna and
192 kHz passband -- worse than 40, not better. But every value from 10-40
tested strictly *worse* than the one below it as attenuation increased,
and `gain=40`'s own peak SNR (3.81 dB) and char count (1138) are both
well below `gain=20`'s (6.52 dB, 5008 chars). The real optimum for this
antenna/band/time appears to sit around 10-20, not 40 -- and every prior
field session's data should be read with this in mind: **the "almost no
real CW heard" finding across all three prior live-hardware sessions may
be substantially explained by running the receiver 20-30 dB more
attenuated than optimal, not by propagation, antenna, or a manta decoder
bug.** No confirmed `Spot` was produced in any of these short 60s sweeps
at any gain value, so this reframes rather than closes the open question
from `2026-09-09-20m-dial-shift-edge-artifact-confirmed.md` (why 4/5
known-strong RBN frequencies showed zero manta track activity) -- that
test was run entirely at the old `gain=40` and should be re-run at a
better-tuned value before drawing further conclusions from it.

## Blocked mid-session: SDRplay API service degraded

Immediately after this sweep, `gain=35` and `gain=45` both failed
`activateStream()` with `sdrplay_api_ServiceNotResponding`, and a
subsequent longer capture attempt at the previously-good `gain=10`
started failing the same way (`sdrplay_api_Fail`) -- consistently, not
transiently, across three retries. Device *enumeration*
(`SoapySDRUtil --find`/`--probe`) continues to succeed throughout; only
stream *activation* fails. No client process is holding the device open
(`lsof -c sdrplay_apiService` empty). This points at the root-owned
`sdrplay_apiService` LaunchDaemon (`/Library/SDRplayAPI/3.15.1/bin/
sdrplay_apiService`, `com.sdrplay.service.plist`) itself being wedged,
requiring a privileged restart (`sudo launchctl kickstart -k
system/com.sdrplay.service` or equivalent) that this session cannot
perform without an interactive password. **Live-hardware verification of
a better gain value is blocked until the service is restarted.**

## Follow-up, same session: service restarted, device replugged, gain fix confirmed real but does NOT close the detection gap

`sudo launchctl kickstart` alone did not clear the wedged service (new
PID confirmed via `ps`, but `activateStream()` still failed identically)
-- a physical USB unplug/replug of the RSP1B itself was required. After
that, streaming activation worked cleanly again at every tested gain.

A 6-minute `manta run --json` capture at `--soapy-gain 15` against a
simultaneous fresh RBN capture (20m, nighttime this time -- different
DX than the earlier daytime session, but the point is the same-window
comparison) confirms the gain fix is real: `snr_2500_db` peaked at 18.65
dB (vs. `gain=40`'s 3.81 dB, and the earlier short sweep's best of 6.52
dB at `gain=20`) and total decoded-character volume was far higher.

**But it does not close the detection gap from
`2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`.** Checked against
6 specific real, multi-skimmer-confirmed RBN spots in the exact capture
window (K9YII @ 14037.0, WB7DND @ 14041.5, RT5T @ 14034.0, UA9URS @
14022.8, W6ME @ 14052.5, W5MP @ 14012.5 kHz) -- **zero `TrackMeta` events
within +/-3 kHz of any of them.** The 18.65 dB peak SNR traced back to
the ~14075/14045 kHz artifact clusters (the same suspected-RTTY/unknown-
interference clusters from the dial-shift doc), not a real CW station --
and there was no RBN-confirmed spot at all near 14045-14050 kHz in this
window either, so that in-CW-segment cluster isn't confirmed real either.
A wider frequency search (+/-3 kHz, ruling out simple LO/calibration
drift) still found nothing. Also notable: the passband-edge artifact's
*share* of total track activity got worse at the better gain (~55% of
all `TrackMeta` events in this run vs. 28-35% at `gain=40`) -- less
attenuation lets more energy through everywhere, including whatever's
driving the edge-channel artifact.

A same-session audit of every other RSP1B-specific setting manta doesn't
explicitly touch came back clean: `biasT_ctrl=false`, `rfnotch_ctrl=
false`, `dabnotch_ctrl=false` (all currently off; their target bands --
MW broadcast, DAB -- don't touch 14 MHz regardless), `iqcorr_ctrl=true`
and DC-offset correction on (both beneficial), and `bandwidth` auto-
selected to 200 kHz (correctly matched to the 192 kHz sample rate, not
suspiciously wide). None of these explain the gap.

**Conclusion: the gain-scale bug was real and worth fixing, but it is not
the (or not the whole) explanation for "almost no real CW heard."**
Something else -- most plausibly the physical antenna/feedline path
specifically feeding the RSP1B (distinct from whatever feeds the
FTdx10), or a detector/DSP issue that only manifests on real narrowband
CW under real noise and isn't caught by synthetic Watterson-channel test
vectors -- is still preventing this receive chain from hearing signals
that other stations clearly hear. Not resolvable from software
alone tonight.

## Next steps

1. ~~Restart `com.sdrplay.service`~~ Done, but insufficient alone -- also
   needed a physical USB replug of the RSP1B (both are human/privileged
   actions; note for next time a stream won't activate despite the
   service looking fresh).
2. Physically verify the RSP1B's actual antenna connection: is it truly
   tapped off the same feedline as the FTdx10 (splitter/multicoupler), is
   that connection solid, and does swapping which receiver sits on which
   tap change which one hears real CW? This is the most direct way to
   tell "antenna path to the RSP1B specifically" from "manta's detector"
   as the culprit.
3. If the antenna path checks out, the detector/DSP side needs real
   investigation -- e.g. capture a raw IQ file at a known-strong RBN
   frequency and inspect it independently of manta's own track/detector
   pipeline (a spectrum/waterfall view, or a minimal non-manta CW
   demodulator) to see if the signal is present in the IQ stream at all
   before blaming detection logic.
4. Consider whether `manta`'s `--soapy-gain` CLI docs/help text should
   warn explicitly that higher values mean *more* attenuation on this
   driver, since that's the opposite of the ordinary-English reading of
   "gain" -- a real footgun for anyone operating this from the command
   line without having read this doc.
