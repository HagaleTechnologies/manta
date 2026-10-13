# manta documentation

Where to start depends on what you came to do. Each section below names one
entry point, then the documents to read next.

## Operators — running a node

Start here: [README "Run it as a node"](../README.md#run-it-as-a-node)

Then:

- [CONFIG.md](CONFIG.md): every config key, its default, and which commands
  and engines it affects.
- [METRICS.md](METRICS.md): every `/metrics` family, plus `/healthz` and
  `/status`.
- [packaging/README.md](../packaging/README.md): running unattended under
  systemd, launchd or Docker Compose.
- [RUNBOOKS/monitoring.md](RUNBOOKS/monitoring.md): a ready Grafana dashboard
  and Prometheus alerts.
- The [runbooks](#runbooks) below.
- `manta <command> --help` for every flag.

## Contributors — changing the code

Start here: [ARCHITECTURE.md](../ARCHITECTURE.md)

Then:

- [CLAUDE.md](../CLAUDE.md): project status, multi-agent rules and the
  review-convergence policy.
- [DECISIONS/README.md](DECISIONS/README.md): every dated decision record,
  newest first.
- [wiki/INDEX.md](../wiki/INDEX.md): accumulated gotchas. The wiki points at
  the code and these docs; it loses any conflict with them.
- [CHANGELOG.md](../CHANGELOG.md): changes, and the rule for when decoder
  output may change.
- [SPOT-SCHEMA.md](SPOT-SCHEMA.md): the JSON spot stream, if you are
  building a consumer.

## Researchers — the algorithms and how they're measured

Start here: [SPEC-decode-core.md](SPEC-decode-core.md)

Then:

- [SPEC-decode-core-v2.md](SPEC-decode-core-v2.md): the experimental
  `edge-legacy` and `hsmm` decode engines.
- [ROADMAP.md](../ROADMAP.md): milestones M0 to M4 and their acceptance
  gates.
- `manta bench sensitivity`: the recall/CER-vs-SNR table on synthetic
  signals, regenerable from any build
  ([decision record](DECISIONS/2026-10-10-man116-sensitivity-benchmark.md)).
- [DECISIONS/README.md](DECISIONS/README.md): the measurement and gate
  records, such as the decode-core-v2 stage-2 gate.
- [superpowers/](superpowers/): working design notes and plans. They are not
  normative; the SPEC files and decision records are.

## Reference

| Document | What it answers |
|---|---|
| [CONFIG.md](CONFIG.md) | What a config key does, its default, and whether it is wired to behaviour yet. |
| [METRICS.md](METRICS.md) | What a `/metrics` family measures, its labels, and when it has samples. |
| [SPOT-SCHEMA.md](SPOT-SCHEMA.md) | Every field of the `:7301` JSON Lines / WebSocket spot message. |
| [SPEC-decode-core.md §9](SPEC-decode-core.md#9-configuration-keys) | The normative decode-core defaults and the keys that are still compile-time constants. |

## Runbooks

| Runbook | For | Use it when |
|---|---|---|
| [network-exposure.md](RUNBOOKS/network-exposure.md) | Operators | Deciding which listeners to expose, and to whom. |
| [node-health.md](RUNBOOKS/node-health.md) | Operators | Telling whether a running node is healthy from `/healthz` and `/metrics`. |
| [uplink-health.md](RUNBOOKS/uplink-health.md) | Operators | Checking an RBN uplink with `manta status`. |
| [monitoring.md](RUNBOOKS/monitoring.md) | Operators | Setting up the shipped Grafana dashboard and Prometheus alerts (MAN-129). |
| [setup-checks.md](RUNBOOKS/setup-checks.md) | Operators | Checking a whole setup with `manta doctor` before and after installing it as a service (MAN-126). |
| [rbn-comparison.md](RUNBOOKS/rbn-comparison.md) | Operators | Grading a live node's spots against the RBN feed with `[rbn_compare]`, and replaying a node's spot log with `manta grade` (MAN-301). |
| [secondary-skimmer-field-node.md](RUNBOOKS/secondary-skimmer-field-node.md) | Operators | Running a 30-day secondary-skimmer field node behind an RBN Aggregator (MAN-96). Its kit is in [RUNBOOKS/field-node/](RUNBOOKS/field-node/). |
| [m1-w1aw-live-copy.md](RUNBOOKS/m1-w1aw-live-copy.md) | Contributors | Running the M1 manual acceptance: a live W1AW copy. |
| [m2-pi4-cpu-budget.md](RUNBOOKS/m2-pi4-cpu-budget.md) | Contributors | Running the M2 Raspberry Pi 4 CPU-budget gate. |
| [aggregator-rbn-capture.md](RUNBOOKS/aggregator-rbn-capture.md) | Contributors | Checking the experimental RBN uplink against a consenting Aggregator operator's recorded RBN traffic (MAN-90). |
| [release.md](RUNBOOKS/release.md) | Maintainers | Cutting a release. |
| [ci-trust-boundary.md](RUNBOOKS/ci-trust-boundary.md) | Maintainers | Changing CI or the repository's one-time trust settings. |
| [fleet-checkout-rename.md](RUNBOOKS/fleet-checkout-rename.md) | Maintainers | Moving an old `skimmer` checkout to `manta` (MAN-27). |
