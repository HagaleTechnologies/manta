# MAN-497: four loopback UDP flakes

Date: 2026-11-05. Ticket: MAN-497.

## Context

The foreign-datagram test derived a port at base + 40, but its fake
protocol-2 radio rejected only bases above 65524. A base in 65496..=65524
therefore panicked in debug builds. The planning investigation reproduced
the overflow at base 65500 and found four matching CI failures.

The same investigation covered three sibling failures named in the ticket
discussion. N1MM's test read the sent counter before the sender updated it.
The protocol-2 acceptance fake streamed faster than a CPU-starved receiver
could drain its socket. Protocol 1 sent only one stop datagram, and five
Windows failures reported that the fake never stopped streaming.

## Decision

- Reserve the foreign socket's full span with one shared test constant.
  Base 65495 remains valid; higher bases are retried before offset arithmetic.
- Wait for N1MM's sent count after receiving each datagram.
  The sender records the count after `send_to` returns.
- Stream protocol-2 acceptance fixtures at real time. OS receive-buffer
  caps can be far below the requested 4 MiB (about 425 KB on the research
  Linux host). Faster streaming lost the first-pass W1AW decode under load.
  Keep the timeouts and spot-clock assertions.
- Send protocol-1 stop twice, sharing protocol 2's repeat count (MAN-305 D8).
  Retirement remains idempotent, so stale handles cannot stop a replacement.
  Record run bits and senders in the protocol-1 fake's failure diagnostics.

The plan's probes force the boundary port, delay the N1MM counter update,
run live acceptance on two CPUs with one busy loop, and discard the first
protocol-1 stop. These checks target the causes rather than relying on one
ordinary green run. Decoder output is unchanged.

## Not done

The acceptance fake still reserves only its own protocol span, base + 11.
No port-0 foreign bind, serialized live decodes, wider timeout, receive-buffer
sysctl, or change to the fake's reconnect behavior is needed. Other UDP
flakes named in the investigation remain outside this ticket.

## Residual risk

The Windows loss point is still unknown; this Linux run cannot establish
whether loss occurs in OS loopback or the fake's timed receive loop.
A runner starved harder than two CPUs plus one busy loop can still fail
the live tests. Repeating stop tolerates one lost datagram, not arbitrary
loss. A single CI run cannot prove every intermittent failure is gone.
