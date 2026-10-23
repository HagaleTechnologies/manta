# Post-PR#154 20m daytime session: new code runs live, first plausible real off-air catch

Follow-up to `docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md`
(root-caused the 29/29 overnight false positives to the `SpotType::Beacon`
repetition-gate exemption) and to PR #154 (`455e1af`, merged 2026-09-09),
which defers non-allowlisted Beacon candidates until the track's true final
close and requires a confirmed real `SpeedUpdate` before resolving one.
Same RSP1B (hwVer 6, serial 2402041760), `--soapy-gain 40`. Ran on 20m
daytime (`--soapy-freq 14030000 --soapy-rate 192000`, ≈13935-14125 kHz),
~17:15-17:35 CDT (late afternoon, still within 20m daylight propagation
per the wiki's propagation note) -- the band Finding 4 of the overnight
doc had already probed pre-fix with three 30-minute windows (10 spots,
most sharing the low-confidence signature) and flagged as the open lead.

Sequence: a 60s `manta doctor --json` sanity check first
(`spots_confirmed: 0`, `verdict: WeakNoDecode`, 1138 chars decoded, 29
tracks promoted, `snr_db_max` 3.8 dB -- real signal-level activity, no
false Beacon spot in this short window), then a 15-minute `manta listen
--json` capture summarized with `scripts/summarize-listen-jsonl.sh`.

## Result: 2 confirmed spots in 15 minutes -- one via the ordinary repetition gate

```
chars_decoded:   18941   distinct_chars: 51
tracks_promoted: 410     tracks_closed:  373
snr_2500_db: min=-8.2794 median=-8.249806 max=15.97354

G0K      freq=14122.813kHz  snr=-8.214dB  confidence=0.148  wpm=44.09  type=Beacon
XE1EE    freq=14025.957kHz  snr=-6.040dB  confidence=0.259  wpm=21.32  type=Cq
```

**`G0K` (Beacon) is consistent with the documented residual gap, but
unresolved.** Low confidence (0.148, inside the old 0.12-0.17 artifact
band), and WPM 44.09 sits just under `MAX_PLAUSIBLE_WPM = 45.0`
(`crates/manta-spot/src/validator.rs:38`), so it doesn't trip PR #154's
implausibility gate. That fits the "small residual (structurally
plausible, not-implausibly-fast garble)" the PR's own doc comment and the
wiki call out as a known, accepted gap (tracked in issue #163,
deprioritized) -- not evidence the fix failed. Its length does not rule
it out as a real call: the UK Special Contest Call Sign scheme
(rsgb.org/scc) issues exactly this shape -- `G` or `M`, one digit, one
letter (e.g. `G8Z`, `M7R`) -- and manta's own grammar check
(`grammar::is_plausible`) and cty.dat allocation check
(`cty::is_allocated`, England) both admit `G0K`. No assignment or
activity check was possible in this session, so the decode is
unresolved: consistent with the known garble gap, not ruled out as a
real short-format UK call.

**`XE1EE` (Cq) is the first plausible genuine off-air decode across all
three live sessions to date.** Not Beacon-typed -- went through the
ordinary repetition gate, not the exemption path that produced every prior
false positive. WPM 21.32 is squarely realistic hand-sent CW speed (the
overnight doc's artifact averaged ~51.5 WPM, implausibly fast). Its
confidence (0.259) is not separate evidence: the two published
confidences are not on a comparable scale. A non-allowlisted Beacon
skips the repetition gate and is scored at the track's close with
however many repetitions it had -- repetition factor 0.5 for the single
decode the residual-gap shape describes -- while this ordinary Cq
candidate had to pass the gate (a flat 2 repetitions at the time of this
session; MAN-95's later pattern tiers put `XE1@@` in the Common tier,
also 2), so its factor is at least 0.75, and it gains the 15%
`master.scp` boost `G0K` lacks (`crates/manta-spot/src/confidence.rs:23-44`,
`crates/manta-spot/src/validator.rs:1419-1432,1763-1774`). `XE1EE`'s
repetition count was exactly 2, so its factor was exactly 0.75: in
`455e1af`'s `Validator::evaluate_candidate` an ordinary candidate emits
the first time `RepetitionGate::record` (at most one more per call)
returns 2, with no variant arbitration yet (MAN-100 landed later) and
dedupe passing a first spot, and `listen --json` and the summarizer
print every emitted spot -- this record reports 2 spots, one of them
`XE1EE`. `G0K`'s count is not recorded. Backing the factors out gives
character evidence of ≈0.300 for `XE1EE`, and at most ≈0.296 for `G0K`
(the single decode the residual-gap shape describes; ≈0.197 at two).
At `G0K`'s single-decode reading the two are nearly identical, so the
confidence value does not separate this catch from the artifact band.
`XE1EE` is a
well-formed Mexican amateur callsign (XE + region digit + 2 letters). Not
independently verified against a callsign database or DX cluster in this
session -- flagging as *plausible*, not confirmed-correct copy.

## Verdict

Per-hour rates, from each record's own self-reported counts (no raw
capture of either session is committed, so nothing is recomputed):

| Session | Window | Spots | Spots/h | Artifact-signature spots | Signature/h |
|---|---|---|---|---|---|
| Pre-fix 20m (overnight doc, Finding 4) | 1.5h (3 x 30 min) | 10 | 6.67 | 7 (SpotType not recorded) | 4.67 |
| Post-PR#154 20m (this session) | 0.25h (15 min) | 2 | 8.0 | 1 (`G0K`, Beacon; plus 1 Cq, `XE1EE`, 4.0/h) | 4.0 |
| Pre-fix 40m (overnight doc) -- the cross-band figure the earlier draft compared against | ~5.5h | 29 | 5.27 | 29 | 5.27 |

The signature is the 40m artifact's: confidence pinned low (0.12-0.17)
with `snr_db` at or near the -8.28 dB clamp floor. The pre-fix 20m count
is 7 by elimination: Finding 4 says most of the 10 share it and names 3
that sit measurably above the floor (all 10 had low confidence). It does
not record their SpotType; every 40m spot with this signature was a
Beacon, and `G0K` (Beacon, 0.148, -8.214 dB) fits it. On the same band
the signature rate went from ≈4.7/h to 4.0/h, at samples of 7 and 1 --
unchanged within noise. 20m was not untested before this session: the
Finding 4 windows ran pre-fix on the same hardware and gain. The earlier
"29/29 false positives -> 1 residual + 1 catch" reading compared a
~5.5h 40m count with a 15-minute 20m count; per hour, this session's
total (8.0/h) is above the 40m figure (5.27/h), not below it. So this
session does not demonstrate a reduction attributable to PR #154. What
it shows is the post-PR#154 code completing a live 15-minute 20m run
with the expected residual shape: 1 Beacon consistent with the
documented gap (`G0K`, unresolved) and 1 plausible Cq catch (`XE1EE`).
Not yet a statistically solid answer to "does live 20m daytime produce
real validated CW copy reliably" -- one session, one plausible catch. A
longer 20m daytime session (ideally with independent callsign/DX cluster
corroboration) would strengthen this; the interior-passband-cluster
question from the overnight doc
(6936.0/6944.0/6968.0/7064.0 kHz) remains untouched by this session
(different band, no dial-shift retest done here).

## Revision note

Revised 2026-10-23 for MAN-177 (deferred PR #164 review findings): the
verdict is reframed on normalized same-band rates instead of a cross-band
raw count, `G0K` is reclassified as unresolved pending an assignment/
activity check, the unnormalized confidence comparison for `XE1EE` is
removed, and the title no longer says "fix holds". The wiki recap of this
record (`wiki/pages/live-hardware-field-testing.md`) is narrowed to match,
including its RF-gap wording for PR #154's deferred close. No reported
measurement changed.
