# MAN-572: rtrb 0.4 and the HPSDR macOS timing failures

Date: 2026-11-27. Ticket: MAN-572.

## Context

Dependabot's rtrb 0.3.5 to 0.4.1 bump (manta #589, later ignored) failed
macOS CI twice, each time on a different HPSDR timing test:
`multi_input_hpsdr` at line 367, then `hpsdr_protocol2_acceptance` at
line 519. rtrb is a ring buffer, so a behaviour change looked possible.

Manta uses rtrb for audio capture. HPSDR protocol 1 queues samples in a
`VecDeque` (`crates/manta-input/src/hpsdr.rs:767`). Protocol 2 uses a
`Vec` plus a look-ahead `VecDeque`
(`crates/manta-input/src/hpsdr/proto2/mod.rs:212`, `:234`). The same
`multi_input_hpsdr` line 367
failure fired in #588's merge group, a PR with no Rust change, so the
macOS failure pre-dates the bump: it is the starvation flake MAN-558
measured at about 16 percent of macOS shard 3 runs. A starved receiver
lets the kernel receive buffer overflow; the datagrams are gone before
manta's in-process queue, so the queue-drop counter the failure printed
stayed at zero and said nothing.

The owner re-scoped on 2026-11-18 (ticket comment 1): fix or quarantine
the flake first, then take rtrb 0.4. On 2026-11-26 (comment 2): bump
rtrb in Cargo.toml by hand inside this PR; the `@dependabot unignore
rtrb` comment is a separate hand step after merge.

## Decisions

- **D1 — the ticket's question: no reviewed rtrb 0.4 change explains
  the macOS timing failures.** Version 0.4.0 restores synchronization
  between dropping one end of the ring and calling `is_abandoned()` on
  the other. It also includes the panic-safety fix for
  `ReadChunk::commit` and `commit_all`, which is already present in the
  previous lockfile version, 0.3.6. Version 0.4.1 fixes a double-free
  when a `ReadChunkIntoIter` is forgotten.
  [0.4.0 release notes](https://github.com/mgeier/rtrb/releases/tag/0.4.0)
  [0.3.6 source](https://docs.rs/rtrb/0.3.6/src/rtrb/chunks.rs.html#964-1000)
  [0.4.1 release notes](https://github.com/mgeier/rtrb/releases/tag/0.4.1)

  Manta's audio capture ring uses `RingBuffer::new`, `slots`, `push`,
  and `read_chunk`. The reader accesses the chunk through `as_slices`
  and releases the copied prefix with `chunk.commit(got)`
  (`crates/manta-input/src/audio_capture.rs:76`, `:101`, `:105`, `:134`,
  `:138`, `:141`, `:156`). `TaggedFrame` is `Copy`, so dropping a frame
  cannot panic. The reader counts frames within the chunk, so `got`
  stays within its length. The audio ring has no call to `is_abandoned`
  or `commit_all` and does not use `ReadChunkIntoIter`. HPSDR uses the
  `VecDeque` and `Vec` queues above. The failures
  are the pre-existing starvation flake: #588's merge group failed the
  same line with no Rust change, and MAN-558 measured the flake at
  about 16 percent of macOS shard 3 runs and 169,511 dropped datagrams
  per source in 70 s with kernel `RcvbufErrors` matching. This session
  reproduced the mechanism on Linux: three busy loops and the test
  pinned to core 0 fail the outage test in 75.24 s with 168,657 and
  168,673 kernel-dropped packets per source (exhibit in D3).
- **D2 — protocol 1 now requests a 4 MiB `SO_RCVBUF`.**
  `crates/manta-input/src/hpsdr.rs` defines
  `const RCVBUF_BYTES: usize = 4 << 20` and requests it on the protocol
  1 IQ socket right after the bind in `open()`, through
  `socket2::SockRef::from(&socket).set_recv_buffer_size(...)` — the
  same std-socket pattern ka9q uses (`ka9q/mod.rs:1128`) and the same
  4 MiB protocol 2 (D5, `hpsdr/proto2/mod.rs:35`, `:281`) already
  requests. A loaded runner deschedules the reader, the kernel receive
  buffer overflows, and the samples never reach the in-process queue,
  so the queue-drop counter stays at zero while the input vanishes; a
  smaller grant is not an error (MAN-558 finding 4). The capacity probe
  socket carries one control exchange and keeps no request. Linux
  cannot prove the grant: this container masks
  `/proc/sys/net/core` and runs as uid 10001, and MAN-558's host capped
  the grant at the default — the macOS CI runs are the only honest
  proof of effect, and the ledger below tracks them.
- **D3 — wait failures now name every input-loss series.** The
  `multi_input_hpsdr` failure helper renamed from `queue_drop_lines` to
  `input_loss_lines` extracts all three series
  (`manta_input_queue_dropped_samples_total`,
  `manta_input_dropped_packets_total`,
  `manta_input_gaps_detected_total`), and the four `why()` closures
  print them as `input loss`. Before (planning session's starved run,
  rtrb 0.3.6, same harness):

  ```
  no progress for 70s (last: 0, elapsed 70.028234005s): spots [];
  queue manta_input_queue_dropped_samples_total{source="40m"} 0;
  manta_input_queue_dropped_samples_total{source="80m"} 0;
  manta_input_queue_dropped_samples_total{source="independent"} 0; <daemon stderr>
  ```

  After (this session, same harness, three busy loops plus the test on
  core 0, 75.24 s, exit 101):

  ```
  no progress for 70s (last: 0, elapsed 70.001794138s): spots [];
  input loss manta_input_dropped_packets_total{source="40m"} 168657;
  manta_input_dropped_packets_total{source="80m"} 168673;
  manta_input_dropped_packets_total{source="independent"} 0;
  manta_input_gaps_detected_total{source="40m"} 10500;
  manta_input_gaps_detected_total{source="80m"} 10501;
  manta_input_gaps_detected_total{source="independent"} 0;
  manta_input_queue_dropped_samples_total{source="40m"} 0;
  manta_input_queue_dropped_samples_total{source="80m"} 0;
  manta_input_queue_dropped_samples_total{source="independent"} 0; <daemon stderr>
  ```

  The unit test `input_loss_lines_extracts_the_three_loss_series`
  pins the helper: fed all three series it expects them extracted while
  `manta_input_malformed_packets_total` and free text are not, and
  against the old queue-only filter it fails wanting the two new
  series.
- **D4 — the bump is a manual manifest edit, not a Dependabot
  retake.** `Cargo.toml` reads `rtrb = "0.4.1"` with the comment naming
  MAN-530's tagged capture ring and the two-version lock. No source
  file changed for the bump: `cargo tree -i rtrb@0.4.1` shows only
  manta-input, `cargo tree -i rtrb@0.3.6` shows only the pinned
  coppa-audio at `f8a4d16d`, and `Cargo.lock` holds both versions —
  valid Cargo state, so coppa keeps its own 0.3 copy. Linux runs on the
  bumped tree: `cargo test -p manta-input --locked` green, the
  `multi_input_hpsdr` and `hpsdr_protocol2_acceptance` binaries green
  in the workspace gate.
- **D5 — the Dependabot ignore is the maintainer's hand step.** The
  ignore lives server-side from the maintainer's comment on #589, not
  in `.github/dependabot.yml`. Once five green macOS runs on complete
  heads are recorded in the ledger below, the owner posts
  `@dependabot unignore rtrb` on #589 by hand. This PR does not perform
  it.

## macOS run ledger

One row per counted CI run: head SHA, macOS shard conclusions, both
tests' outcomes. Counted runs must be on heads containing every change
above; the merge queue's merge-group run is a bonus, uncounted. Each
counted run also re-runs every `multi_input_hpsdr` test ten extra times
through the MAN-523 repeat step because the test file changed, and runs
`hpsdr_protocol2_acceptance` once because that file deliberately does
not change.

| run | head | macOS shards | multi_input_hpsdr | hpsdr_protocol2_acceptance |
|-----|------|--------------|-------------------|----------------------------|
| —   | —    | not run      | not run           | not run                    |

unverified: the implement phase's hard window ended before any counted
run. The PR left this branch as a draft, so `ci-full.yml` was never
dispatched: no macOS shard, and neither test ran on macOS here. The
ledger row above is the shortfall the plan's Phase 6 contingency
directs: the code is good on Linux, the owner holds the unignore until
five green runs land, and the next session drives them by marking the
PR ready and appending rows as runs conclude. The precondition for D5's
hand step is unmet; the owner decides.

## Not done

The `@dependabot unignore rtrb` comment (D5, maintainer's hand step).
Any change to `hpsdr_protocol2_acceptance.rs` — its diagnostics, pacing
and receive buffer already exist (MAN-505, MAN-497, MAN-459). Any CI
workflow change: the five runs ride existing triggers and the existing
MAN-523 repeat mechanism. No 70 s idle-bound raise, no blind retry, no
MAN-533 revert — lost samples do not return, and a retry would hide
real breakage the progress wait exists to catch (MAN-558
recommendation 3). No decode A/B grid, per ticket comment 2. No move of
coppa-audio off its own rtrb 0.3. No `docs/DECISIONS/README.md`
regeneration; the index is lag-tolerant and the Decisions Index
workflow adds rows after merge. Windows legs, the real-radio run
(MAN-52) and the W1AW live-copy run stay out.

## Residual risk

The macOS grant of the 4 MiB request is unproven from Linux; if macOS
caps it low, the flake rate drops but may not reach zero, and the
ledger is what would show it. A runner starved harder than three busy
loops on one core can still fail the timing tests; the D3 output would
then say which loss series it was, which is the point. Five green runs
bound the rate, they do not prove it zero (MAN-497's residual risk
noted the same limit). rtrb 0.4.1's audio-capture behaviour is proven
only by the same Linux suites every release runs.
