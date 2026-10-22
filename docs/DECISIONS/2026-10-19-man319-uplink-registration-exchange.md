# MAN-319: the live uplink needs a 6-character locator now; the registration exchange waits for MAN-90's capture

**Status:** accepted. The locator gate and the C8–C11 claims are implemented; the three wire scenarios stay blocked on MAN-90.

## Context

MAN-319's Gherkin asks the outbound uplink to register with RBN, apply the
spot settings RBN returns, hold spots when RBN says so, and refresh
RBN-served reference files. The ticket's own technical note is the
controlling constraint: "Implement only what MAN-90's verified capture
shows; do not enable live upload as part of this ticket."

That capture has not happened. Three sources, checked 2026-10-19 at commit
`b17da2a`:

- [The MAN-90 decision record](2026-10-11-man90-uplink-protocol-verification.md),
  line 3: "Status: accepted (interim). Verification open: no Aggregator→RBN
  capture exists yet." — and its C1–C7 claims table has every Status cell
  `open`.
- [The capture runbook](../RUNBOOKS/aggregator-rbn-capture.md), `## Captures`:
  "(Append entries here.) None yet. As of 2026-10-11 no Aggregator→RBN
  traffic has been recorded."
- The thoughts pool and the tenant event cache carry no capture or
  post-2026-10-11 MAN-90 update.

The verified capture therefore shows nothing, and the ticket instructs us to
implement only what it shows. Meanwhile the ticket's one concrete local ask —
"missing locator must fail config check, not silently disable the uplink" —
was unimplemented: on 2026-10-19 an enabled `[[rbn_uplink]]` with
`dry_run = false` and no `[server].operator_grid` passed `manta config check`
with exit 0 (reproduced with the real binary at `b17da2a`).

## Decisions

- **D-1. Locator gate, live-only.** Any `[[rbn_uplink]]` entry with
  `enabled = true` and `dry_run = false` fails config load unless
  `[server].operator_grid` is exactly 6 characters. One rule in `manta-cli`'s
  `load()`, beside the existing "`[[rbn_uplink]]` needs a `[server]` table"
  bail, so `config check`, `run`, `doctor` and the SIGHUP reload path all
  enforce it from one place; a reload whose new config goes live without a
  grid is an invalid reload (MAN-78 semantics: logged, ignored, the previous
  configuration keeps running). The error names the target and the key:
  `dry_run = false -- sending spots needs [server].operator_grid set to a
  6-character Maidenhead locator (e.g. "FN03ab")`. `[server]` is
  restart-only, so `reload_once` adds a second, reload-only check (PR #362
  review): a reload that would flip an enabled target live is refused when
  the daemon started without a 6-character grid, even when the same edit
  adds one, since that grid is not in effect until a restart.
  Rejected alternatives:
  gating every enabled uplink including dry-run rehearsals (Scenario 1's own
  Given ties the locator to dry-run being off, and every guide example is a
  dry-run config); accepting a locator derived from
  `operator_lat`/`operator_lon` (`maidenhead::locator` already derives a
  6-char grid for JSON spots, but registration will transmit the configured
  string, and the operator should own the exact bytes); auto-extending a
  4-character grid (an invented subsquare would transmit a made-up position —
  a 4-char value is refused with `-- "<grid>" is only 4 characters`).
- **D-2. The three wire scenarios are blocked on MAN-90.** Registration with
  returned spot settings (Scenario 1), waiting on a hold directive
  (Scenario 2) and matching RBN-served reference files (Scenario 3) each
  need wire knowledge no capture has produced; implementing any of them now
  would invent RBN's protocol, exactly what MAN-90 D-2 ("No protocol change
  on assumptions") forbids. Claims C8–C11 are added to MAN-90's table below
  as the open claims a capture must settle; the moment a capture lands, a
  follow-up phase implements them from captured bytes only, pinning fixtures
  as MAN-90 D-5 already prescribes. Nothing is put on the wire by this
  ticket: the uplink's bytes stay pinned by
  `the_uplink_session_bytes_match_the_man90_reference`.
- **D-3. Dry-run keeps its recorded reading of Scenario 4.** "Nothing is
  transmitted" means no spot lines: the dry-run uplink still connects and
  logs in, MAN-159's recorded design, pinned by
  `dry_run_logs_in_but_does_not_forward_the_spot_line` and
  `omitted_dry_run_key_logs_in_but_does_not_transmit`
  (`crates/manta-server/tests/uplink_acceptance.rs`) and quoted verbatim in
  the node guide. Redefining dry-run to "send nothing at all" would
  contradict those pins for no wire-path benefit. Bound on the future: the
  registration phase, when it exists, must not perform registration while
  dry_run is on — dry-run transmits the login rehearsal only.
- **D-4. Live upload stays owner-gated.** `dry_run` still defaults `true`,
  the WARN + `EXPERIMENTAL_NOTICE` regime is untouched, and nothing here
  enables live upload; promoting it stays an owner decision, as MAN-90 left
  it.

## What shipped instead

The locator gate (`crates/manta-cli/src/config.rs`), the C8–C11 rows in the
MAN-90 record, the capture runbook's extension (what a capturer records for
C8–C11), and the one-sentence locator requirement in every operator doc
surface for going live (README, `docs/CONFIG.md`, `manta.example.toml`, the
`manta config init` scaffold, `packaging/README.md`, the node guide's "Going
live").

## Related

- [MAN-90 uplink protocol verification](2026-10-11-man90-uplink-protocol-verification.md):
  the claims table this record extends, and D-2/D-5 this record applies.
- [Capturing Aggregator→RBN traffic](../RUNBOOKS/aggregator-rbn-capture.md).
- [Broad-review decisions](2026-09-06-broad-review-decisions.md): D1 (the
  Aggregator path is primary), D3/MAN-159 (dry-run default).
- [MAN-316 reference-list refresh](2026-10-12-man316-reference-list-refresh.md):
  the third-party `cty.dat`/`MASTER.SCP` subsystem — distinct from the
  RBN-served reference exchange C11 asks about.
