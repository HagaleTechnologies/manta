# MAN-62: bounded `SpotBus::occurrence_counts`

MAN-62 corrected an over-broad "already covered" disposition
(`docs/DECISIONS/2026-09-02-man23-threat-model.md`, finding 15) that
assumed real-world callsign cardinality bounds `occurrence_counts`'
(`crates/manta-server/src/bus.rs`, a `HashMap<String, u32>` keyed by
callsign, never expired or capped) size. That assumption holds only
against genuine over-the-air traffic. Finding 2 in the same threat model
already accepts, as inherent to OpenHPSDR/Hermes having no cryptographic
framing, that a source-IP-spoofed peer can inject fabricated-but-decodable
CW manta's decode pipeline can't distinguish from real RF. Combined: an
attacker able to spoof the HPSDR input can transmit an unbounded sequence
of distinct synthetic callsigns, growing this map for the life of the
process with no bound tied to real callsign space at all.

## Options considered (per the ticket's own technical notes)

1. **Bounded-capacity map with LRU-on-touch eviction**, matching
   `RECENT_HISTORY_CAP`'s existing bounded-collection pattern in the same
   file.
2. **Periodic/TTL-based expiry** of stale entries.

Both need a real design decision on capacity/TTL values and on whether
evicting a genuinely-active real callsign's occurrence count during a
flood is an acceptable trade-off — flagged explicitly in the ticket as not
assumed.

## Decision: bounded capacity, LRU-on-touch (not FIFO-by-insertion)

`OccurrenceTracker` (`bus.rs`) replaces the bare `HashMap<String, u32>`
with `HashMap<String, (u32, u64)>` (count, `last_touched` tick) plus a
monotonic touch counter. Every publish for a callsign — new or existing —
bumps its `last_touched` tick. Once at `MAX_OCCURRENCE_ENTRIES` (20,000)
capacity, inserting a genuinely new callsign evicts whichever tracked
entry has the OLDEST `last_touched` tick.

LRU-on-touch specifically, not FIFO-by-insertion, is the load-bearing
choice: a real, currently-active station keeps getting touched by its own
repeated spots and so keeps its tick fresh — a flood of one-shot synthetic
callsigns (each touched exactly once, at insertion, then never again) is
exactly what accumulates the oldest ticks and gets evicted first under
sustained pressure. FIFO-by-insertion would get this backwards: a real
callsign spotted once early in a session, before a later flood begins,
would be evicted ahead of the flood's own most-recent entries purely
because it was inserted first, regardless of how active it's been since.
Tested directly (`occurrence_tracker_protects_a_repeatedly_touched_callsign_during_a_flood`,
inserted first, then a full-capacity flood run through with periodic
re-touches, still present afterward).

**Trade-off accepted, matching the ticket's own framing:** a real callsign
that goes silent for the rest of a flood-length window can still be
evicted once its own tick ages out past every flood entry's more recent
one — occurrence-count history isn't "sacred," and `set dx filter unique
> n` degrading (a suppressed spot count resetting to 1) under active
adversarial pressure is an acceptable cost against unbounded memory
growth, the same trade-off class as `RECENT_HISTORY_CAP` already accepts
for spot history.

**Eviction is an O(capacity) scan**, not an O(1) doubly-linked-list LRU —
deliberately simpler, since the scan only runs on the rare "insert a new
key while already at capacity" path, never on an ordinary touch of an
existing key. Real callsign cardinality (even an unusually busy multi-band
contest weekend, per the capacity comment in `bus.rs`) sits far under
20,000 in ordinary operation; sustained eviction pressure is the
exception, not the steady state, so the scan's cost is paid rarely and
only under the exact adversarial condition this fix exists for.

**No new dependency** — no `lru` crate pulled in for a data structure this
small and purpose-specific, consistent with `tasks::IpQuota` and
`rate_limit::IpRateLimiter`'s own hand-rolled `std::sync::Mutex`-backed
approach elsewhere in this codebase.

**Capacity chosen (20,000), not TTL-based expiry:** a bounded-capacity
approach needs no background reaper task and no wall-clock dependency —
the map bounds itself purely from insert/touch pressure. A TTL approach
would need a periodic sweep (like `rate_limit::spawn_stale_entry_reaper`,
MAN-57) plus a real-time clock read per touch; capacity-bounding gets the
same memory guarantee more simply here, since (unlike MAN-57's per-IP rate
map, where a legitimately-idle source's entry *should* eventually expire)
there's no meaningful "idle timeout" concept for a callsign's historical
occurrence count — it should persist as long as there's room, not decay
just because time passed.
