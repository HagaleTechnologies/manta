# MAN-307: several bands from one HPSDR radio

Status: implemented. No physical-radio validation was performed in this run.

This supersedes the HPSDR deferral in
[MAN-13](2026-10-15-man13-multi-input-daemon.md). Its existing shared validator,
clock, track IDs, calibration and output paths remain in use.

## Capacity and allocation

`[[input]]` entries with the same case-insensitive configured host and effective
port share one radio. File order assigns DDC indices. Native rates must match;
processing rate, dial override and calibration remain per input. An optional
`receivers` value, integer 1 through 255, belongs on the first member only.
It declares hardware capacity, while the existing eight-active-DDC and link
budget limits still apply to the requested allocation. Changes require restart.

The research phase assumed Protocol 1 could not report capacity. Generic
Protocol 1 has no portable count, but HL2 extends its discovery reply. The
[vendor protocol](https://github.com/softerhardware/Hermes-Lite2/wiki/Protocol)
and [vendor Python implementation](https://github.com/softerhardware/Hermes-Lite2/blob/master/software/hermeslite/hermeslite.py)
specify radio ID 6 and the count at offset `0x13`. The Python implementation
uses an exactly 60-byte reply and a 60-byte request beginning `EF FE 02`.

Shared radios receive a directed query at the configured endpoint. Only a
positive recognized HL2 count from that peer is usable; idle and sending
statuses both convey capacity. Unknown, malformed, oversized and wrong-peer
replies do not. A total one-second receive deadline bounds malformed floods
as well as silence. Address and socket failures remain errors. No count uses
the declaration, or fails if absent. A declaration never overrides a smaller
reported count. Single-input and selected diagnostic paths need no probe.
`config check` does no network I/O and labels its capacity check as deferred
until `run`. Exact examples and diagnostics are in the
[operator guide](../GUIDES/multiple-receivers.md#several-bands-on-one-hpsdr-radio).

## One owner per radio

`hpsdr_group` resolves endpoints concurrently and rejects alias collisions
before control I/O. Probe and first open use the same resolved address; a
reopen resolves the configured host again, as a standalone HPSDR input does,
without probing capacity again. A member that loses a race with a sibling's
failure waits for the replacement instead of ending its input. One opener
owns a generation, and concurrent members await it with 100 ms stop checks.
Initial failures are attributed to the first configured member; engine startup
failure cancels the other pending workers and releases completed sources.

A read failure retires the complete radio generation. Retirement invalidates
queues and stops pumping/keepalives before any replacement starts. Old read
results and errors cannot affect the replacement. A receiver destructor only
detaches its slot; it never sends a stale stop command. Group recovery uses
the existing capped backoff policy, with productive samples resetting it.
All members go down together; a newly opened UDP socket remains unhealthy
until valid samples arrive. Another radio remains independent.

Every replacement rebuilds each input's decimator and dial wrapper. Metadata
must match the initial allocation. The driver captures acquisition time
immediately before its start command. Each member tracks its last delivered
sample boundary against that generation's acquisition time, including consumed
queue gaps. The next generation's start determines its outage gap, so worker
scheduling time is included while acquisition already represented by buffered
samples is excluded. This follows the timing lesson in MAN-259's earlier
validation without depending on that ticket's unlanded APIs.

## Bounded queues and observations

Each receiver retains at most `2 * native_rate` complex samples, with a fixed
reserved capacity. At 192 kS/s this is 384,000 samples, about 3.07 MB of sample
payload per receiver, excluding decoder and allocator overhead. Overflow
removes the oldest samples, preserves the newest, and affects only that
receiver. Detached slots receive no further samples; wire stride and receiver
indices stay fixed until the next open.

Queue gaps are latched by the read consuming the next retained samples.
Eviction after that read cannot change its pending gap. Decimation starts a
fresh filter at a gap, converts the count with a carried remainder, and never
returns a buffer containing samples from both sides. EOF can carry a pending
gap. The standalone reconnect wrapper also forwards inner gaps.

`InputHealthCounters` composes shared transport atomics with a per-DDC queue
counter. `InputHealthTotals` retires each member's view once per generation and
preserves cumulative counts. The new metric counts complex hardware-rate
samples before decimation; it is not a network-loss estimate. Packet counters
remain shared radio-wide views and must not be summed across members. The
[metric reference](../METRICS.md) specifies zero/absent behavior and reset
semantics. Rust users with exhaustive `InputHealth` literals must add
`queue_dropped_samples` or use the existing `Default` construction.

## Automated evidence

- `config::tests::hpsdr_allocation_is_ordered_and_offline` and
  `hpsdr_declarations_and_active_limits_are_checked` cover allocation order,
  case/default-port grouping, independent ports and declaration/rate limits.
  `shared_hpsdr_check_is_offline_and_reports_declaration` runs the actual CLI
  with occupied listener ports and a UDP socket that receives no query.
- HPSDR driver tests cover the capacity parser/query, malformed-flood deadline,
  exact bounded-queue retention, independent loss counters, read-associated
  gaps and detach behavior. Existing multi-DDC wire tests remain in place.
- `hpsdr_group` tests inject the opener and clock to cover concurrent initial
  opens and reopens, generation retirement, backoff, stop, counter totals,
  health and acquisition-boundary timing. Decimator and reconnect tests cover
  gap forwarding/reset and byte-identical output on a continuous stream.
- `multi_input_hpsdr` runs real processes against loopback Protocol 1 radios.
  Four separately tuned 192 kHz streams produce W1AW, JA1ABC, DL1ABC and
  VK2ABC on stdout and the shared JSON listener within 100 Hz of their
  expected RF frequencies. It checks the receiver-count/tuning bytes,
  hardware and declared capacity refusals, fallback, aliases, selected input,
  per-radio outage recovery and an independent radio producing a spot.
- `metrics_acceptance::operator_get_request_sees_input_packet_loss_counters`
  scrapes a real HTTP listener and checks a nonzero queue-loss sample.
  Documentation consistency tests execute the four-band example offline.

The loopback evidence establishes protocol/software behavior. It does not
establish physical RF reception, four-band Pi 4 CPU headroom or a 24-hour
live-radio soak. An optional field exercise is to run the guide's example
on HL2, request six bands, interrupt and restore its network, and record
spots, CPU and queue losses. That exercise has not been run.
