# 2026-10-07 — MAN-96: secondary-skimmer field node (kit)

**Status:** accepted, implemented (kit); field run outstanding. The kit is on branch `MAN-96`:
source-outage counters on `/metrics`, `scripts/field-node.py`, `scripts/shadow-compare.py`, the
runbook `docs/RUNBOOKS/secondary-skimmer-field-node.md` and its deployment files under
`docs/RUNBOOKS/field-node/`. MAN-96 itself stays open until the 30-day hardware run's evidence is
recorded (see "Open steps" below).

## Context

MAN-96 asks for a real manta node to run for 30 days as a secondary skimmer behind an admitted
RBN Aggregator, decoding and spotting with no manual intervention beyond planned restarts, and,
after at least a week, for its spots to be compared against the primary CW Skimmer/SkimSrv and
the comparison recorded. That run needs physical hardware, an antenna, a cooperating Aggregator
operator and 30 calendar days. No implement container can do it, so this ticket's code change is
the kit the run needs and the repo lacked, plus a tested procedure.

What the research and planning found:

- **The named blocker is gone.** The Aggregator handshake works end to end (MAN-86/87/88/89):
  banner, `CALL-N-#` login, `SETT: vlNormal <segments>`, `BYE`. Planning reproduced it
  independently against a live daemon: a telnet client logged in, sent `SKIMMER/SETT`, received a
  live skimmer-format spot line (28 dB at 500 Hz on telnet for the 21 dB at 2500 Hz on JSON, per
  broad-review D3), the `SETT` reply and `CU AGN!` on `BYE`.
- **No multi-day run has ever happened** (research §3). The longest real-hardware run is about
  90 min (about 38 min streaming). ROADMAP M2's 24 h soak and M3's 7-day soak are both open. The
  one documented reliability problem is an SDRplay API service wedge that reopening the device
  cannot clear (`2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md`): it needed
  a service restart, sometimes a USB replug, and sometimes cleared by itself in 1–2 min. MAN-73's
  reconnect loop retries forever but cannot restart a driver service, and watchdog policy is
  MAN-73's documented non-scope.
- **Reconnects were invisible between scrapes.** The only trace of a source drop was an
  `eprintln!` line; `manta_source_health` is a gauge, so a drop shorter than the scrape interval
  left no mark. A 30-day uptime claim must count those.
- **No deployment artifacts exist on `main`** (research §6). There is no systemd unit, plist or
  compose file; MAN-75's packaging PR #124 closed unmerged on 2026-10-06. The config surface
  (MAN-261, MAN-76's `config check`) is complete and `config check` validates every table before
  the `soapy` feature gate.
- **No shadow-mode comparison tool exists** (research §7). `scripts/score-against-rbn.py`'s
  matcher can be reused unchanged on live spots (planning ran it on a live SpotMessage line
  against a two-row RBN CSV: 1 true positive, 0 false positives), but its loader does not filter
  `tx_mode`, which a full RBN daily dump needs.
- **Aggregator forwarding is all or nothing** (RBN *Using Aggregator v6.0*, extracted during
  planning). §9.1: up to eight secondary skimmers, numbered 1–8, functional only while the
  primary is connected. §5.6: the only "don't send spots" switch stops all forwarding. §6.1: the
  Skimmer Traffic tab prefixes each spot with its source index and marks `+`/`-` for forwarded or
  not. The manual does not say which spotter callsign a secondary's forwarded spots carry on RBN.

## Decisions

Verbatim from the MAN-96 plan. R5 and R14 are that plan's planning-run checks (the live
handshake and the Aggregator manual extract, both summarized under Context); "research §N" is
the MAN-96 research document's section N.

| ID | Decision | Rationale |
|---|---|---|
| D-A | **Availability ≥ 99.0%** of the 30-day span, excluding planned-restart windows. Time counts as up only when `/healthz` returned 200; unreachable, unhealthy and unobserved time all count against it. | 1% of 30 d = 7.2 h. That absorbs dozens of watchdog-recovered SDR wedges (≈10–12 min each, D-G) while failing a node that is down for most of a day. |
| D-B | **Aggregator connected ≥ 95%** of reachable samples (`manta_telnet_clients_connected ≥ 1`), valid because the firewall (D-M) admits only the Aggregator host to telnet. | Below 99% because Aggregator-side restarts (Windows updates, SkimSrv ini rotation per manual §8.4) are outside manta's control. 95% still fails a node the Aggregator keeps dropping. |
| D-C | **≥ 1 spot on every full UTC day** of the span (from `manta_spots_total` deltas, restart-aware). | This is the measurable form of "decoding and spotting". A 192 kHz CW segment has activity every UTC day. |
| D-D | **Manual interventions = 0.** Any human action on the node other than a planned restart is a manual intervention: USB replug, a hand-run restart, a config edit, an unplanned reboot. It is logged with `note --kind manual` and fails scenario 1. Fix the cause and restart the 30-day clock with a new `start` note. | This is the ticket's literal bar ("without manual intervention beyond planned restarts"). |
| D-E | **Planned restart** = a `note --kind planned` written before the restart. Its window runs from the note until the first healthy sample after it, capped at 1800 s, and is excluded from the availability denominator. Every planned restart is listed. | A note makes intent auditable. The cap stops a "planned" label from hiding a long outage. |
| D-F | Ledger **interval 60 s**. A gap between consecutive samples > 3 × interval is **unobserved**: it counts as down unless a planned window covers it. | Conservative: the ledger not running is no evidence the node was up. 3× tolerates timer jitter. |
| D-G | **Watchdog**: run `--recover-cmd` once the node has been continuously bad (metrics unreachable, or any `manta_source_health == 0`) for **600 s**. Then cool down **1800 s**, with at most **6 recoveries per UTC day**. Each recovery is logged and listed in the report. Automated recoveries are not manual interventions, but their downtime counts against D-A. | The field record shows self-recovery "within a minute or two", so 10 min leaves 5× margin. The cooldown and daily cap stop a dead device from being restart-looped. Watchdog policy belongs outside manta's process (research §3; MAN-73's documented non-scope). |
| D-H | **Comparison data source**: the primary's spots come from RBN's public daily archive filtered by the primary's spotter callsign. The node's spots come from its own `record-spots` archive. Defaults: `--freq-tol-hz 500` (= `score-against-rbn.py`), `--time-tol-s 600` (= manta's dedupe `SUPPRESSION_SECONDS`), CW rows only (`tx_mode`), a required passband, and every `deCall` seen in the node's spots (with and without SSID) excluded from corroborating spotters. | D5 makes the archive the reference set. The node's own archive does not depend on how Aggregator labels secondaries (unverifiable here, R14). 600 s matches the longest gap between two spots of a continuing station from manta, so phase-offset re-spots are not miscounted as disagreements. Self-exclusion stops the node's forwarded spots from corroborating themselves. |
| D-I | **Go/no-go before joining the Aggregator** (Stage 1, 24 h private soak). GO requires all four: availability ≥ 99% over 24 h, 0 manual interventions, ≥ 20 node spots, and **≤ 10% of node spots uncorroborated** (no RBN spotter reported that call within tolerance). | Aggregator forwarding is all-or-nothing (R14). The 2026-09-09 overnight test produced 29/29 false spots, and that must not reach RBN under a real callsign. 10% is twice M3's ≤ 5% false bar, allowing for stations only the node heard. A floor of 20 spots makes the rate meaningful and catches a deaf rig. |
| D-J | The 30-day recipe targets **Linux + systemd**. | systemd gives the restart semantics this needs (`Restart=always`, `StartLimitIntervalSec=0`, `TimeoutStopSec`). It is the project's target platform. The macOS rig is where the SDRplay wedge was observed, and Windows reliability is open (#187). The Mac rig remains fine for Stage 0 bench checks. |
| D-K | **Same antenna as the primary** (splitter, or a second SDR on the same feedline). Any input type is allowed. The example uses `type = "soapy"`/SDRplay because it is the only family with field evidence. | Differences in the comparison should come from the decoder, not the antenna. The runbook shows the `[input]` swaps for kiwi/hpsdr. |
| D-L | The field config has **no `[[rbn_uplink]]`**, and a test asserts it. | D1/D2: the direct uplink stays unverified until MAN-90. The Aggregator does the forwarding. |
| D-M | **Firewall**: telnet 7300 accepts only the Aggregator host's IP. JSON 7301 and metrics 7302 are loopback-only. `bind_addr = "0.0.0.0"`. | MAN-132 has not landed (network-exposure runbook). This also makes D-B's "telnet client = Aggregator" inference sound. |
| D-N | **Raw evidence** (ledger, spot archive) stays on the node and in the operator's archive. The field report records file names, sizes and SHA-256 sums, plus the `report` and `shadow-compare` outputs verbatim. | About 40 MB each over 30 days, too large for git. The hashes make the summary auditable. |

## What the kit contains

- **`manta_source_outages_total{source}`** (count of healthy → unhealthy edges) and
  **`manta_source_down_seconds_total{source}`** (seconds in outages that have ended), counted in
  `Metrics::set_source_health`, which every source-health transition already flows through. An
  outage in progress shows only as `manta_source_health == 0` until it ends. A source that starts
  unhealthy (HPSDR until confirmed live) is not an outage.
- **`scripts/field-node.py`** (stdlib, Python ≥ 3.9): `sample` (one ledger record per minute and
  the D-G watchdog), `note` (D-D/D-E), `record-spots` (the node's own spot archive from the JSON
  Lines port) and `report` (the scenario-1 verdict, D-A…D-F; exit 0 PASS, 1 FAIL or in progress).
  For D-C, a `manta_spots_total` rise between two samples on different UTC days (after a
  restart, between the restart and the later sample) is credited to neither day. A full day passes
  D-C when a rise falls inside it, or when the spot archive (`--spots-dir`, which the runbook
  always passes) recorded a spot timestamped that day. That also covers a spot emitted just after
  the midnight following its timestamp. Without the archive, a day whose only spots fall in such
  a crossing interval fails D-C, and the report warns.
- **`scripts/shadow-compare.py`**: the scenario-2 comparison per D-H, and the uncorroborated
  share the D-I go/no-go reads. It has no pass/fail of its own: the ticket asks for the
  comparison to be recorded.
- **`docs/RUNBOOKS/secondary-skimmer-field-node.md`** and **`docs/RUNBOOKS/field-node/`**: the
  example config (no `[[rbn_uplink]]`, D-L), `manta-field.service` (`Restart=always`,
  `StartLimitIntervalSec=0`, `TimeoutStopSec=60` to outlast MAN-85's 50 s drain plus the 2 s
  runtime shutdown), the ledger and spot-recorder units, and `manta-field-recover`.
- Rust tests that keep the kit consistent with the code on every CI leg: every metric
  `field-node.py` requires is rendered, the example config validates (feature-aware), the units
  call only subcommands and flags the script has, and the runbook cites only rendered metrics and
  resolvable links.

## Scope boundaries

- **MAN-75 (general packaging).** The field units are field-specific (SDR-service ordering, the
  ledger and recorder companions) and live under `docs/RUNBOOKS/field-node/`. If MAN-75's
  general `packaging/` units land, these stay as they are and the runbook links MAN-75's unit as
  the general-purpose alternative.
- **The shadow-mode diff ticket** (broad review A-12). `shadow-compare.py` is a one-shot window
  comparison, which is what MAN-96 needs to record. The scheduled nightly parity report,
  publication under `docs/BENCH/` and any dashboard stay with that ticket, which can build on this
  script.
- **MAN-90 (direct RBN uplink).** The field config has no `[[rbn_uplink]]`; the Aggregator
  forwards. The direct uplink stays gated on MAN-90 (broad-review D1/D2).
- **MAN-132 (per-listener bind).** Until it lands, the firewall (D-M) is what keeps JSON and
  metrics private while telnet stays reachable.
- **Not in scope:** root-causing the SDRplay service wedge (the watchdog contains it), Pi 4
  claims (broad-review D6, paused), decoder-quality work (MAN-107…113), SoapySDR input counters,
  spot type on the JSON wire (a dispensa contract change), wiring `scripts/tests/` into CI, and
  macOS/Windows recipes for the 30-day run (D-J).

## Open steps (the field run, on hardware)

Each step is in the runbook.

1. Stage 0: `manta doctor` reports `DECODING` on the field rig.
2. Stage 1: a 24 h private soak passes `field-node.py report --min-days 1` and D-I's go/no-go.
3. Stage 2: the Aggregator shows the node as a secondary skimmer with `+` forwarded spots, and
   the `start` note is written. The 30-day clock starts there.
4. Day 7 (scenario 2): the `shadow-compare.py` output for the first 7 days is committed in
   `docs/DECISIONS/<date>-man96-30-day-field-run.md` by a docs PR.
5. Day 30 (scenario 1): `field-node.py report --min-days 30` exits 0, and its output, the 30-day
   comparison and the raw-evidence hashes (D-N) are committed in the same field report by a docs
   PR. On FAIL, the report records why and the run restarts from Stage 2 with a new `start` note.
6. ROADMAP gate lines and `CLAUDE.md` Status are updated to match the recorded evidence, and not
   beyond it.

## References

- Runbook: `docs/RUNBOOKS/secondary-skimmer-field-node.md`; metrics: `docs/RUNBOOKS/node-health.md`
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` (D1, D2, D3, D5, D6, D14)
- `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md` (Aggregator manual URL)
- `docs/DECISIONS/2026-09-07-man85-signal-handling.md` (drain deadline)
- `docs/DECISIONS/2026-10-05-man73-source-reconnect.md`
- `docs/DECISIONS/2026-10-05-man128-node-health-metrics.md`
- `docs/DECISIONS/2026-10-06-man261-config-surface.md`, `2026-10-07-man76-config-check-init.md`
- `docs/DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md`
- `docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`
