# MAN-449: the keepalive-cadence test counts what the source issued and re-observes only a lost datagram

Date: 2026-11-01. Ticket: MAN-449, filed from `test (windows-latest)`
failures of `hpsdr::proto2::tests::keepalive_resends_on_cadence` on main.

## Context

The test opens a protocol-2 source against `FakeP2Radio`, reads for one
second over loopback UDP, and until now counted the keepalives the fake recorded. It
failed six times on `windows-latest` on main from 2026-10-23 to 2026-10-26
(jobs 120213903839, 120719033058, 120887576850, 121409698527, 121556343915,
121595704815), about 2.7% of the 226 Windows jobs that ran it since
2026-10-21. Each failure had a normal cadence (`[_, 4, 4, 9]`) and exactly
one datagram missing from the fake's log: a high-priority sequence number
four times, one rx-specific keepalive once, the general keepalive once.

The ticket's account needs three corrections. There were six failures, not
two. The `:1183` panic was the general assert (`general [0, 4, 4, 9]`), not
`sent[3] >= 8`. Rust 1.98's Windows `thread::sleep` uses a high-resolution
waitable timer, so coarse timers do not explain the failures. Where the
datagram is lost, in OS loopback or in the recorder's timed `recv_from`
loop, is not established; no Windows host was available.

## Decision

- **Count from the source.** Cadence is judged on the deltas of the
  source's own `seq` counters over the window. `send` bumps a counter once
  per datagram, so a lost datagram cannot change them.
- **Bound the count from above.** A stream is resent only after its
  interval, so k sends span at least k - 1 intervals: at most
  `elapsed_ms / every_ms + 1` per stream. That cannot fail a correct
  source, and it catches a source that burns extra sequence numbers.
- **Floors stay `8 / 4 / 4 / 1`.** Lowering high-priority to 7 would let a
  125 ms cadence pass.
- **Re-observe only a lost datagram, at most 3 times.** `judge` calls an
  observation `Clean`, `Lossy` (the source behaved, a datagram went
  missing) or `Broken` (wrong whatever the network did). `Broken` fails at
  once. `Lossy` observes again on a fresh fake and source. Three lossy
  observations fail: a skip or an unsent keepalive recurs, a random loss
  does not. At 2.7% loss per run, three in a row is about 2e-5.
- **Wait for the recorders.** The fixed 20 ms sleep became a 5 ms poll, up
  to 500 ms, until each stream's recorded count reaches its issued count.

Twelve mutants of the source, each run 3 times, all fail 3 of 3: keepalives
off (M1), each stream's keepalive removed (M2, M5, M6), an extra seq per
high-priority send (M3), high-priority every 200, 150 or 125 ms (M4, M8,
M9), run = 0 (M7), one seq skipped (M10), one seq issued but not sent for
high-priority (M11) or general (M12). M10 to M12 fail as "every
observation missed a datagram". A scratch hook dropping one recorded
datagram on each of the four streams passes every time.

## Not done

For a follow-up ticket: the fixture's recorder loop (a timed `recv_from`
every 10 ms) is unchanged; `overload_raises_adc0_attenuation_on_the_wire`
keeps the same exact high-priority sequence assert through the same
fixture; `hpsdr::discovery::tests::auto_retries_discovery_four_times_within_the_window`
keeps `probe_fake`'s exact count (4 Windows failures, `left: 3, right: 4`).

## Residual risk

- The floors still assume the fake delivers IQ about every 2 ms. A runner
  that starves the IQ thread for hundreds of milliseconds can fail them;
  the panic prints `elapsed` and the window counts to show it.
- A source bug that skips a sequence number in fewer than 1 in 3 runs could
  pass. A skip that recurs on every fresh source fails every time.
