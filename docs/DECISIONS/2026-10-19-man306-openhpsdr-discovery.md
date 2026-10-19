# MAN-306: discover an openHPSDR radio when no address is configured

## Behavior

An HPSDR input with frequency and sample rate but no `host` now discovers
radios on the LAN. One available Protocol-1 radio is selected. Several radios
produce an error listing every address, MAC, board type, firmware byte and
availability. `input.mac`, `MANTA_INPUT_MAC` and `--hpsdr-mac` pin a radio
across DHCP address changes. MACs accept hexadecimal digits with colons,
dashes or no separators and print in uppercase colon form.

An explicit `host` keeps the existing direct connection and sends no discovery
requests. Setting both host and MAC is an error. Frequency and sample rate
remain required. A busy radio is listed but refused for automatic selection;
an explicit host is the deliberate override. Protocol-2 responses are listed
but refused for streaming until MAN-305 supplies that driver. A radio answering
both protocols keeps its P2 answer; explicit host can force the P1 path.

All five source commands use the same opener. Reconnect invokes discovery
again, so a pinned MAC follows an address change. Two configured inputs with
the same MAC or two unqualified discovery inputs are refused, like duplicate
host inputs; so is an unqualified discovery input beside a MAC-pinned one on
the same port, since the unqualified one takes the only radio that answers.
The spike documents an avahi/mDNS discovery path for Red Pitaya and QMTech
boards, which this ticket does not implement, so the docs tell their operators
to set an explicit host. Several receivers on one
radio remain MAN-307's work.

## Discovery

`manta-input::hpsdr::discovery` sends both the 63-byte P1 and 60-byte P2 request
shapes at 0, 250, 500 and 750 ms and collects for 1.5 seconds. It validates
reply lengths, sync and status, takes the IP from the datagram source and
deduplicates by MAC. Board names use separate protocol tables; ID 4 means
Angelia in P1 and Orion in P2. Unknown IDs are displayed as Unknown.

Destinations are limited broadcast `255.255.255.255` and loopback-directed
broadcast `127.255.255.255`, both on the configured control port, normally
1024. No dependency was added. On a host with several interfaces, limited
broadcast may miss the radio's LAN; an explicit host remains the solution.
A failed destination does not discard a successful send to another destination.
If every send fails, or socket setup/receive fails, discovery reports an error.

`manta devices` now broadcasts by design. It reports HPSDR discovery separately
from audio and Soapy, retaining successful sections if another fails. Empty
HPSDR discovery is `ok` with an empty list; an I/O failure is `error` and makes
the command exit 1. The JSON entries carry `ip`, `mac`, `board`, `firmware`,
`status` and `protocol`. Other source commands discover only after selecting
HPSDR, preserving MAN-239's file-replay socket behavior.

## Plan corrections

The plan's D8 and part of its sixth acceptance criterion required host XOR MAC,
contradicting its first scenario and config acceptance test. Neither is required
here; both remain forbidden. The planned `discover -> Vec` signature could not
report the required socket-failure state, so it returns `io::Result<Vec<_>>`.
The planned `config show` command does not exist; `config check` prints the
normalized MAC in its existing source summary. No new subcommand was added.

The five clap blocks remain separate to preserve their existing per-command
semantics, including `check`'s positional WAV input. Every HPSDR selector
conflicts with other source families, while frequency/rate/port can select
HPSDR without a host. Explicit port has no clap default; the opener applies
1024 when it is omitted.

## Verification and remaining hardware work

Unit tests cover request bytes, reply parsing, per-protocol board tables,
MAC normalization, deduplication, selection errors and bounded socket reads.
CLI acceptance uses an ephemeral-port fake responding to broadcasts. It sends
valid Metis IQ frames and requires `manta_source_health{source="hpsdr"} 1`
for both flag and config spellings, including selection among two MACs.
An injected resolver test proves that explicit host never calls discovery and
that hostless reopen resolves again. A looping five-second 48 kHz stereo
silence replay with `run --config empty.toml --source silence.wav --source-iq`
held zero open socket file descriptors (checked via `/proc/<pid>/fd`; this
container ships no `strace`, so the fd table stands in for the MAN-239
`strace -f -e trace=socket` procedure).

Live HL2 reply capture and behavior under another client remain MAN-52's
hardware leg. The new ignored real-radio discovery test records that boundary.
P2 live streaming remains MAN-305; the port-1025-first variant used by one HL2
tool is not implemented. Broadcast delivery is tested on Linux in this run;
macOS and Windows retain the same tests for CI verification.

## Follow-up scope

The dispatch restricts GitHub writes to MAN-306's branch and PR and provides
no Linear access. These follow-ups are recorded here rather than filed from
this phase:

- Operators on hosts with several subnets should discover radios on each local
  LAN. Add per-interface directed broadcasts to the existing destination list;
  retain explicit-host behavior and test interface enumeration without hardware.
- When MAN-305 lands, reuse this discovery parser and socket loop for its
  directed probe, filter replies to the selected host, and replace the P2
  streaming refusal with the new driver. Keep one set of board tables and
  request builders.
