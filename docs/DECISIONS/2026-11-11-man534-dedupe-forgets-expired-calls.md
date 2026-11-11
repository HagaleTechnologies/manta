# MAN-534: dedupe forgets a call once no candidate can still fall inside its window

## Decision

D1. `Dedupe::sweep(floor_ts)` retains entries whose stored sample timestamp is
at least `floor_ts.saturating_sub(suppression_window_samples)`. The boundary
is inclusive. The validator calls it on its existing 10-second sample-time
sweep. No wall clock is involved. Assessment, timestamp lookup and the
600-second suppression window remain unchanged.

D2. The floor starts at `now_ts - gate_window_samples`, protecting refinements
that use retained gate occurrences. It is lowered by every open track's
last sample timestamp, every retained word's end timestamp, and the oldest
pending beacon's capture timestamp. Settled and fused candidates use word
end times. Deferred beacons use their capture times. Taking the minimum
over all words avoids depending on their push order.

D3. The floor cannot lag `now_ts` by more than 3600 seconds. A track can stay
open for the daemon's lifetime, so an uncapped floor would preserve the
original leak. The cap bounds retained history even when a track keeps an
unsettled word or pending beacon indefinitely. It is cached in samples at
construction and adds no configuration, metric or output field.

D4. Two residual cases can see changed decisions: a candidate more than an
hour behind the validator clock, and a newly arriving track whose first
word is more than the gate's 90-second window behind that clock. Multi-input
skew has no hard bound, as recorded in MAN-13 D5. Both cases remain
deterministic. Ordinary within-window suppression and stale-beacon guards
are preserved within the retained candidate horizon.

D5. Amends MAN-200 D9 in
[the repetition-evidence fusion record](2026-11-03-man200-repetition-evidence-fusion.md).
Its disagreeing-call veto now means "spotted within dedupe's memory",
rather than "ever spotted". The veto uses the presence of a dedupe entry;
an evicted pair returns `None`. Fusion remains opt-in and defaults off.
The earlier record is left intact.

## Evidence

The planning phase ran these probes at `ccaf07258308531326d8bb37f565e3a9101e69fe`.
These numbers describe those probes, not a live-SDR soak.

| Probe | Main | Bare now cutoff | Pending-beacon-only floor | Chosen floor |
|---|---|---|---|---|
| 2,000 calls over 7 days, never-closing track | 2,000 entries | Not run | Not run | 5 entries, same 2,000 spots |
| K5ARH repeats at 597/598 seconds after another track advances to 605 seconds | Withheld | Emitted | Not run | Withheld |
| Deferred beacon after a newer spot ages another 700 seconds | No spot, stale count 1 | Emitted, stale count 0 | Emitted, stale count 0 | No spot, stale count 1 |
| Open track holds an unsettled beacon for 7 days while another spots 2,000 calls | 2,000 entries | Not run | 2 entries | 14 entries |

The pending-beacon-only floor misses beacons still held as unsettled words
before track close. This is why the chosen floor includes every open
track's retained words.

Regression coverage is in `crates/manta-spot/src/dedupe.rs` and
`crates/manta-spot/src/validator.rs`. It covers 50,000 distinct pairs,
the inclusive cutoff, a deterministic two-hour swept/unswept comparison,
periodic eviction without track closes, late candidates, stale deferred
beacons and the one-hour cap. Existing tests are unchanged.
