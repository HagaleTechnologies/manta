# Post-PR#154 20m daytime session: fix holds, first plausible real off-air catch

Follow-up to `docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md`
(root-caused the 29/29 overnight false positives to the `SpotType::Beacon`
repetition-gate exemption) and to PR #154 (`455e1af`, merged 2026-09-09),
which defers non-allowlisted Beacon candidates until the track's true final
close and requires a confirmed real `SpeedUpdate` before resolving one.
Same RSP1B (hwVer 6, serial 2402041760), `--soapy-gain 40`. Ran on 20m
daytime (`--soapy-freq 14030000 --soapy-rate 192000`, ≈13935-14125 kHz),
~17:15-17:35 CDT (late afternoon, still within 20m daylight propagation
per the wiki's propagation note) -- the band Finding 4 of the overnight
doc flagged as the promising untried lead.

Sequence: a 60s `manta doctor --json` sanity check first
(`spots_confirmed: 0`, `verdict: WeakNoDecode`, 1138 chars decoded, 29
tracks promoted, `snr_db_max` 3.8 dB -- real signal-level activity, no
false Beacon spot in this short window), then a 15-minute `manta listen
--json` capture summarized with `scripts/summarize-listen-jsonl.sh`.

## Result: 2 confirmed spots, not 29 -- and the shape changed

```
chars_decoded:   18941   distinct_chars: 51
tracks_promoted: 410     tracks_closed:  373
snr_2500_db: min=-8.2794 median=-8.249806 max=15.97354

G0K      freq=14122.813kHz  snr=-8.214dB  confidence=0.148  wpm=44.09  type=Beacon
XE1EE    freq=14025.957kHz  snr=-6.040dB  confidence=0.259  wpm=21.32  type=Cq
```

**`G0K` (Beacon) matches the documented residual gap, not a new bug.**
Low confidence (0.148, inside the old 0.12-0.17 artifact band) and a
3-character callsign too short to be a valid current UK amateur full
callsign (`G0` + 2-3 suffix letters is the real format, e.g. `G0ABC`) --
consistent with a noise-decoded garble. WPM 44.09 sits just under
`MAX_PLAUSIBLE_WPM = 45.0` (`crates/manta-spot/src/validator.rs:33`), so
it doesn't trip PR #154's implausibility gate. This is exactly the "small
residual (structurally plausible, not-implausibly-fast garble)" the PR's
own doc comment and the wiki call out as a known, accepted gap (tracked
in issue #163, deprioritized) -- not evidence the fix failed.

**`XE1EE` (Cq) is the first plausible genuine off-air decode across all
three live sessions to date.** Not Beacon-typed -- went through the
ordinary repetition gate, not the exemption path that produced every prior
false positive. WPM 21.32 is squarely realistic hand-sent CW speed (the
overnight doc's artifact averaged ~51.5 WPM, implausibly fast). Confidence
0.259 is above the 0.12-0.17 artifact band. `XE1EE` is a well-formed
Mexican amateur callsign (XE + region digit + 2 letters). Not independently
verified against a callsign database or DX cluster in this session --
flagging as *plausible*, not confirmed-correct copy.

## Verdict

29/29 false positives -> 1 residual (explained, expected) + 1 plausible
real catch, in a shorter window (15 min vs. ~5.5h) and on a band that was
untested live before now. PR #154 appears to be doing what it was built
to do. Not yet a statistically solid answer to "does live 20m daytime
produce real validated CW copy reliably" -- one session, one plausible
catch. A longer 20m daytime session (ideally with independent callsign/DX
cluster corroboration) would strengthen this; the interior-passband-
cluster question from the overnight doc (6936.0/6944.0/6968.0/7064.0 kHz)
remains untouched by this session (different band, no dial-shift retest
done here).
