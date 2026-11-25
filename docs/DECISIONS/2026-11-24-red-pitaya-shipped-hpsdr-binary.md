# 2026-11-24 — Red Pitaya's packaged HPSDR receiver server sends scrambled IQ; rebuild it from upstream source

**Status:** accepted (field finding; no manta code change).

## What happened

manta's protocol 1 input was run against real hardware for the first time:
a Red Pitaya SDRlab 122-16 on Red Pitaya OS 3.00-57 (Ubuntu 24.04, glibc
2.39), running the packaged `sdr_receiver_hpsdr_122_88` app, version
1.0-20260302. The radio answered discovery as protocol 1 (board 1,
firmware 25), and a 178.8 s capture at 96 kS/s arrived with contiguous
packet sequence numbers and no gaps. The samples were nonsense:

- Decoded as the spec says (24-bit big-endian), the stream sat at a constant
  -8.6 dBFS with peaks at full scale, held only 12,206 distinct values in
  2 M samples, and showed no transmit blanking from the station's FT8 rig.
- No byte order fixed it. In the raw 6-byte I/Q groups, the sign-only byte
  (`00` or `ff`) was not in a consistent position, and about one sample in
  eight was garbage at fixed positions in the USB frame.
- A standalone Python protocol 1 client got the same bytes as manta, so the
  fault is upstream of manta's parser.

## Cause

The packaged server binary,
`/opt/redpitaya/www/apps/sdr_receiver_hpsdr_122_88/sdr-receiver-hpsdr`
(16452 bytes, dated 2026-03-02), is the fault. A/B on the same board, with the
app's own bitstream freshly loaded by its `fpga.sh`:

| Server | Byte 0 of each 24-bit sample is a sign byte | Level |
|---|---|---|
| Packaged binary | 28% of samples | nonsense |
| Upstream `projects/sdr_receiver_hpsdr_122_88/server/sdr-receiver-hpsdr.c` (pavel-demin/red-pitaya-notes master), unmodified, built on the board with `gcc -D_GNU_SOURCE -O3 … -lm -lpthread` | 100% | -65 dBFS |
| Same source with the FIFO `memcpy` replaced by explicit 32-bit volatile reads | 100% | -63 dBFS |

The third row rules out the board's glibc `memcpy` as the cause: the
unmodified upstream source works with it. Why the packaged build differs was
not established.

## Result with the rebuilt server

A 3-minute `manta listen` on 40 m (centre 7.030 MHz, 96 kS/s) copied real
CW traffic in plain text: QSO exchanges such as `5NNPLUS VRYNICE SIGS`
and `=ALL GOODHERE`, repeated callsigns and a `CQDX`. It produced
no spots in that window. The receiver had no frequency calibration applied,
and the spotting gap was not investigated here.

## What to do on a Red Pitaya

- Rebuild the server from upstream source on the board, and install it over
  the packaged binary, keeping the original. `/opt/redpitaya` is a read-only
  FAT mount, so remount it read-write for the copy and read-only again
  afterwards.
- Start the receiver through the web UI or `GET /bazaar?start=<app>` as
  usual. Use the receive-only `sdr_receiver_hpsdr*` app, not a transceiver
  build, when the antenna is shared with a transmitter.
- The app's `start.sh` runs two servers, on `eth0` and on a macvlan `mvl0`
  with its own MAC. manta talks to the `eth0` one through `--hpsdr-host`.
- Before trusting a new Red Pitaya image, look at the raw bytes: in a working
  stream, the first byte of every 24-bit sample is almost always `00` or
  `ff` at normal HF levels.

## Follow-ups

- MAN-591: a `listen --record-iq` capture stopped by SIGINT or SIGTERM leaves
  an empty `.meta.jsonl.partial` and no metadata, found during this session.
- Not yet reported to Red Pitaya.
