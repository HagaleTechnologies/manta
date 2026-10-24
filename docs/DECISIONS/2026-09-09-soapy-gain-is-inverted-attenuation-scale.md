# `--soapy-gain` on this RSP1B is a gain-*reduction* (attenuation) scale, not a gain scale -- every prior fixed-gain field session used a heavily-attenuated setting

MAN-226 correction, 2026-10-23: narrowed the gain and remote-RBN
inferences below; gain-element mappings and recorded measurements are
unchanged.

Triggered by Tony's suspicion that the RSP1B itself, not the antenna or
propagation, might be misconfigured: the same antenna hears FT8 very
strongly (a different receiver, an FTdx10), but every manta field session
to date has heard almost no real CW. All three prior live sessions
(`2026-09-08-first-live-rsp1b-run.md` -- which also had AGC runs, whose
gain state this record does not establish --
`2026-09-09-overnight-40m-soapy-field-test.md`,
`2026-09-09-post-pr154-20m-daytime-validation.md`, plus tonight's earlier
captures) used `--soapy-gain 40` in their fixed-gain captures. This session checked, for the first
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
window) recorded the following character counts, peak SNR and verdicts.
These measurements alone do not establish real-CW reception quality.

| gain | chars_decoded | snr_db_max | verdict |
|---|---|---|---|
| 0 | 0 | -1.58 | `NoisyNoDecode` |
| 10 | 6465 | 2.43 | `WeakNoDecode` |
| 20 | 5008 | 6.52 (**best peak SNR of all tested**) | `WeakNoDecode` |
| 30 | 3677 | 4.11 | `WeakNoDecode` |
| 40 (prior sessions' value) | 1138 | 3.81 | `WeakNoDecode` |

At `gain=0` (max sensitivity, min attenuation), the sweep recorded zero
decoded characters and a -1.58 dB peak. It does not establish overload or
desensitization as the cause. `chars_decoded` fell at every step from 10
to 40 as attenuation increased. Peak SNR did not: in the gain 10-20
range, only gain 20 (6.52 dB) exceeded gain 40 (3.81 dB); gain 10
recorded 2.43 dB. Gain 30 (4.11 dB) also exceeded gain 40.

No setting produced a confirmed `Spot` in these short 60s sweeps, so
these values do not establish an optimum for receiving real CW or explain
the earlier sessions' poor reception. The
[dial-shift record](2026-09-09-20m-dial-shift-edge-artifact-confirmed.md)
reported zero track activity at four of five remotely reported active
frequencies at gain 40. Compare gains using a locally present real signal
or raw-IQ dynamic-range measurements before selecting a preferred value.

## Blocked mid-session: SDRplay API service degraded

Immediately after this sweep, `gain=35` and `gain=45` both failed
`activateStream()` with `sdrplay_api_ServiceNotResponding`, and a
subsequent longer capture attempt at the previously-good `gain=10`
started failing the same way (`sdrplay_api_Fail`) -- consistently, not
transiently, across three retries. Device *enumeration*
(`SoapySDRUtil --find`/`--probe`) continues to succeed throughout; only
stream *activation* fails. `lsof -c sdrplay_apiService` was empty, but that selects files of the
service process itself, so it does not show whether another client held the
device; a proper holder check was not done. This points at the root-owned
`sdrplay_apiService` LaunchDaemon (`/Library/SDRplayAPI/3.15.1/bin/
sdrplay_apiService`, `com.sdrplay.service.plist`) itself being wedged,
requiring a privileged restart (`sudo launchctl kickstart -k
system/com.sdrplay.service` or equivalent) that this session cannot
perform without an interactive password. Further live-hardware gain
comparisons are blocked until streaming recovers.

## Follow-up, same session: streaming recovered; gain-15 capture still showed no activity at the RBN targets

`sudo launchctl kickstart` alone did not clear the wedged service (new
PID confirmed via `ps`, but `activateStream()` still failed identically)
-- a physical USB unplug/replug of the RSP1B itself was required. After
that, streaming activation worked cleanly again at every tested gain.

A 6-minute `manta run --json` capture at `--soapy-gain 15` against a
simultaneous fresh RBN capture (20m, nighttime this time -- different
DX than the earlier daytime session, but the point is the same-window
comparison) recorded a peak `snr_2500_db` of 18.65 dB, versus 3.81 dB at
gain 40 and the earlier short sweep's maximum of 6.52 dB at gain 20.
Total decoded-character volume was also far higher. These are different
captures with different durations and RF conditions, not a controlled
gain comparison or evidence of improved real-CW reception.

Checked against six stations reported active by multiple remote RBN
skimmers in the exact capture window (K9YII @ 14037.0, WB7DND @ 14041.5,
RT5T @ 14034.0, UA9URS @ 14022.8, W6ME @ 14052.5, W5MP @ 14012.5 kHz),
there were zero `TrackMeta` events within +/-3 kHz of any of them. Remote
reception does not establish their strength or receivability at this
RSP1B's QTH. The 18.65 dB peak traced back to the ~14075/14045 kHz
artifact/interference clusters, not a confirmed real CW station. There
was no RBN-confirmed spot near 14045-14050 kHz in this window either;
that absence does not identify the cluster's origin. The wider +/-3 kHz
search still found no activity at the six targets.

The passband-edge clusters accounted for ~55% of `TrackMeta` events in
this run; the 28-35% at gain 40 came from `TrackPromoted` events. These
unlike event denominators cannot establish whether the share increased
or decreased, or how gain affected it.

A same-session audit of every other RSP1B-specific setting manta doesn't
explicitly touch came back clean: `biasT_ctrl=false`, `rfnotch_ctrl=
false`, `dabnotch_ctrl=false` (all currently off; their target bands --
MW broadcast, DAB -- don't touch 14 MHz regardless), `iqcorr_ctrl=true`
and DC-offset correction on (both beneficial), and `bandwidth` auto-
selected to 200 kHz (correctly matched to the 192 kHz sample rate, not
suspiciously wide). None of these explain the gap.

The gain-reduction mapping is established; a reception improvement or a
cause for the missing local track activity is not. Establish local
reception with raw IQ and a local reference check before ranking RF-path
and detector/DSP explanations. The later
[MAN-171 record](2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md)
explains the remote-SNR caveat and used a WWV check as additional local
evidence. It does not retroactively measure these September 9 captures.
As the [antenna-path record](2026-09-10-antenna-path-fix-resolves-detection-gap.md)
notes, an isolated 10 MHz bin does not identify WWV without
modulation/sideband or equivalent evidence.

## Next steps

1. ~~Restart `com.sdrplay.service`~~ Done, but insufficient alone -- also
   needed a physical USB replug of the RSP1B (both are human/privileged
   actions; note for next time a stream won't activate despite the
   service looking fresh).
2. Establish whether the remotely reported CW targets are locally present
   using raw IQ and a local reference check before assigning a cause to
   the receiver or detector. Inspect the IQ independently of manta's
   track/detector pipeline with a spectrum/waterfall view or a minimal
   CW demodulator.
3. Use that local evidence to guide checks of the RSP1B's antenna/feedline,
   gain settings and detector/DSP behavior. A feedline/tap comparison
   with the FTdx10 can test the RF-path hypothesis; the remote RBN
   reports alone do not rank it above other explanations.
4. Consider whether `manta`'s `--soapy-gain` CLI docs/help text should
   warn explicitly that higher values mean *more* attenuation on this
   driver, since that's the opposite of the ordinary-English reading of
   "gain" -- a real footgun for anyone operating this from the command
   line without having read this doc.
