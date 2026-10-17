# MAN-91: the RBN uplink forwards CQ and beacon spots by default

RBN accepts a non-beacon station only when it is calling CQ or TEST. Before
MAN-91, `uplink::forward_loop` checked `dry_run` and then sent every spot it
received, DE and untyped spots included. Each `[[rbn_uplink]]` target now
has a `spot_types` setting. By default it sends only CQ and beacon spots;
`"all"` restores the old behaviour. Origin: the 2026-09-05 broad review,
lens 3 (decode quality), hit list #11.

## Decisions

1. **Key shape.** `spot_types` is a lowercase string enum on each
   `[[rbn_uplink]]` block: `"cq_beacon"` (the default when the key is
   omitted) or `"all"`. It is per target, like `dry_run`, and modelled on
   `[server].line_format`'s two-value enum. The ticket asks only for a
   default and an `"all"` override, so there is no per-type list. An unknown
   value fails `run` and `config check` before any I/O, naming the key and
   both valid values.
2. **What `cq_beacon` sends.** `SpotType::Cq` and `SpotType::Beacon`; it
   holds back `De` and `Unknown`. RBN's "CQ or TEST" already maps onto
   `Cq`: `manta-spot`'s context parser classes `CQ TEST <call>` and
   `CQ … DE <call>` as `Cq`. An untyped spot carries no CQ evidence, so it
   is held back too. A bare `TEST <call>` is not parsed yet (lens-3 hit
   list #9); that is separate work.
3. **Counting.** A held-back spot increments the target's existing
   `suppressed` counter, which already counts dry-run. No new metric
   series, `/status` field or `manta status` column. ARCHITECTURE §8's
   "every dropped/evicted/suppressed item is counted" holds; `dry_run`
   tells the two causes apart. The `manta_uplink_*suppressed_total` HELP
   text now names both causes, and `SUPPR` is non-zero on a live uplink.
4. **Placement.** The check is its own `if` in `forward_loop`, ahead of the
   `dry_run` check. `forward_loop` takes the filter as its own
   `spot_types: UplinkSpotTypes` parameter (`Copy`), passed from
   `connect_and_forward`, and never reads it through `config`. MAN-78
   (open PR #232) replaces `forward_loop`'s `dry_run` source for SIGHUP
   reload, so the two changes touch adjacent lines but not the same
   expression. Under MAN-78, a changed `spot_types` is restart-only.
5. **Local outputs and SETT are unchanged.** Telnet, JSON and WebSocket
   each hold their own `SpotBus` subscription, so a spot the uplink holds
   back still reaches them. `SettSettings::cq_only` describes the local
   telnet stream, which still carries every type, so it stays false.
   `NCQ` tagging of DE spots on local output is out of scope.
6. **`dry_run` still defaults to true (MAN-159).** "Default configuration"
   in the ticket means `spot_types` omitted. A target still sends nothing
   until `dry_run = false`.

## Where it shows

- `manta config check` prints `spot_types=` on each `rbn_uplink:` line.
- `manta config init` and `manta.example.toml` list
  `#spot_types = "cq_beacon"` with its explanation.
- Each target's startup log line carries a `spot_types` field.

## Not done here

- Verifying against a real RBN ingest (MAN-90).
- Parsing bare `TEST <call>` and contest CQ forms (lens-3 #9).
- A separate `filtered` counter.
