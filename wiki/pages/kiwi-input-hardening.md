# Why doesn't KiwiSDR's input hardening look like HPSDR's?

Both `crates/manta-input/src/hpsdr.rs` (MAN-22) and
`crates/manta-input/src/kiwi.rs` (MAN-60) had to survive a hostile/
malformed peer without taking the whole daemon down, but the fixes are
shaped differently for a reason: **a WebSocket protocol violation is
connection-terminal in a way a bad UDP datagram is not.** RFC 6455 requires
*failing the connection* on a protocol violation, and `tungstenite`
implements that by tearing down its own state machine — so HPSDR's fix
("discard the bad packet, keep reading the same socket") only transfers to
frames `tungstenite` itself accepts but `kiwi.rs` finds unusable
(`MAX_CONSECUTIVE_UNUSABLE_FRAMES`). For a frame `tungstenite` rejects, or a
clean `Close`, there's no usable socket left — the only way to "keep
operating normally" is a bounded, backoff-capped **reconnect**
(`KiwiIqSource::reconnect`).

A reconnect creates its own correctness trap: `Spot.sample_ts` (samples
since session start) is the *only* time base a spot carries, converted to
wall clock at the server boundary as `epoch + sample_ts / sample_rate`
(`SpotBus::unix_ts_for`). A reconnect that emits zero samples for the
outage duration silently **back-dates every subsequent spot** by that
duration — trading a crash for a quieter correctness bug. `kiwi.rs`
zero-fills the outage (capped at `MAX_ZERO_FILL_SECONDS`) specifically to
avoid this; if you're adding recovery logic to another sample-clock-based
`IqSource`, check whether it needs the same compensation.

The full STRIDE pass, every finding's disposition, and the reconnect
budget's exact numbers are in
[`docs/DECISIONS/2026-09-04-man60-kiwi-threat-model.md`](../../docs/DECISIONS/2026-09-04-man60-kiwi-threat-model.md).
