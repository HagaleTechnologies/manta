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

## Next steps (not done here)

1. Restart `com.sdrplay.service` (needs sudo -- human action).
2. Re-run the dial-shift/RBN-cross-correlation test from
   `2026-09-09-20m-dial-shift-edge-artifact-confirmed.md` at a
   properly-tuned gain (start around 15-20, sweep narrower from there) to
   see whether the passband-edge artifact and the "4/5 known-strong
   frequencies produce zero tracks" gap persist at a sane gain, or
   substantially resolve.
3. Consider whether `manta`'s `--soapy-gain` CLI docs/help text should
   warn explicitly that higher values mean *more* attenuation on this
   driver, since that's the opposite of the ordinary-English reading of
   "gain" -- a real footgun for anyone operating this from the command
   line without having read this doc.
