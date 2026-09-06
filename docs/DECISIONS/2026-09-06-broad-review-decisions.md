# 2026-09-06 — Broad UI/UX/market review: architectural decisions

**Status:** Decided (Tony, 2026-09-06). Supersedes any conflicting framing in README.md, ROADMAP.md,
or open tickets predating this date on the specific points below.

## Context

A broad review of manta's UI/UX, operations, decode quality, competitive positioning, and repo
polish ran 2026-09-05/06 (five parallel sessions, one per lens, against `origin/main` @ `5b9e747`).
The full review and its five per-lens reports are in `thoughts/shared/reports/` (gitignored, not
committed — this doc is the durable, public record of the decisions it produced). The review
produced 94 hit-list items; this doc records the 8 strategic trade-offs and 8 fact/logistics
questions that needed Tony's judgment before those items could be filed as tickets. All 94 items are
now filed as MAN-73 through MAN-158 (some folded into existing tickets instead of duplicating them —
see each ticket's own comments for cross-references).

## Decisions

### D1 — RBN admission path: Aggregator-compatible first

Pursue becoming a byte-for-byte Skimmer Server drop-in behind a stock Aggregator install
(`SKIMMER/SETT` handshake, banner, `BYE`, AK1A column layout, `CALL-N-#` SSIDs — MAN-86..89) as the
primary near-term path into the RBN. It's the only publicly documented admission mechanism, and it
lets manta run as a *secondary* skimmer on any operator's already-admitted node without requiring a
conversation with RBN's operators first. The direct outbound uplink (MAN-32, already shipped) is
being verified separately against real Aggregator↔RBN traffic (MAN-90) rather than assumed correct.
Both paths can coexist; direct-uplink admission is negotiated with RBN operators from a position of
having live, working nodes (MAN-96, MAN-98), not before.

### D2 — RBN protocol verification: friendly-node capture

Verify what the RBN server actually accepts from an uplink by observing real Aggregator↔RBN traffic
on a consenting node operator's system (MAN-90), rather than asking RBN's team cold or shipping the
uplink unverified.

### D3 — Wire SNR convention: dual-referenced

Telnet and the RBN uplink report SNR in the 500 Hz reference bandwidth RBN/CW Skimmer use (so manta
doesn't read as "deaf" next to other nodes); the JSON stream keeps the native 2500 Hz channel
measurement plus an explicit `snr_ref_hz` field, since the credibility/benchmark work (MAN-116,
MAN-136-adjacent) wants the uncorrected detector value to calibrate against. This is a dispensa
contract change for the JSON field (MAN-102).

### D4 — Multi-band identity: `CALL-N-#` per band

Once multi-source/multi-band support (MAN-13) lands, a manta node identifies each band/segment on
RBN as `CALL-N-#` (e.g. `W5AU-1-#`, `W5AU-2-#`), matching RBN's own live convention — confirmed by a
live capture where 10 of 71 observed spotters used exactly this pattern. MAN-89 is the blocking
callsign-grammar fix; MAN-13/14 carry the design decision itself.

### D5 — RBN-parity benchmark data: work backwards from K5TR's existing IQ

MAN-20's data-dependency blocker (no recorded contest-weekend IQ paired with reference RBN spots) may
not need a fresh live capture. K5TR IQ recordings already exist. RBN publishes daily public archive
CSVs carrying per-spot callsign/frequency/time/SNR/WPM from the spotting station for that time window
— if the recording's exact UTC window and receiver chain (same antenna/SDR that fed his live SkimSrv
at the time) can be confirmed, that archive slice stands in as the reference dataset, with no live
simultaneous telnet capture required. See the comment thread on MAN-20 for the concrete next step.

### D6 — Pi4 CPU-budget story: paused

No Pi4/CPU-budget work and no Pi4 performance claim in README/ROADMAP until the decode-quality fixes
found in this review (busted spots under fading, SNR/WPM calibration, fading-normalization — MAN-100
through MAN-113) land. MAN-49 (the existing gate ticket) is lowered from P2 to P4 accordingly; MAN-117
(remove per-hop decoder-pool allocations) is filed but explicitly held, not next in line. This is a
sequencing decision, not a decision to drop Pi4 as a target.

### D7 — v0.1.0: tag now, labeled pre-stability alpha

Cut a real `v0.1.0` release now (MAN-84) rather than waiting on the Pi4 gate or any other milestone,
so the README's install story stops being false. Label it explicitly pre-stability alpha, expect
breakage — this project has not yet cleared its own M2/M3 acceptance gates and shouldn't imply
otherwise.

### D8 — Classical DSP fixes before M4 ML fusion

The decode-quality fixes found in this review (dit-scale matched filtering, per-element fading
normalization, detector sensitivity, gap-likelihood scoring, multi-hypothesis speed tracking —
MAN-107 through MAN-113, MAN-110-112) land before any M4 ML-fusion work starts. The measured failures
are classical-DSP defects with known classical fixes (mark stretch from threshold placement, rail lag
against fading, hard gap thresholds); an ML stage trained on the current front end would inherit all
three.

### D9 — Contester persona: in scope

N1MM+/DXLog local band-map support (specifically N1MM's Spectrum Display UDP protocol, MAN-141) is in
scope, not excluded by manta's headless-RBN-node framing. It's CW Skimmer's second real market, the
PFB channelizer already computes the spectrum data this needs, and it doesn't require a GUI.

### D10 — cqdx contract gap: vendor the fix

Close the `dxDxcc`-required-but-null finding (already on file in MAN-45) by vendoring a small ADIF
DXCC entity-number table alongside `cty.dat` (MAN-136), rather than waiting on a dispensa contract
relaxation. This ships from manta's side without needing cqdx/dispensa to act first.

### D11 — `listen` promoted to the daemon verb

`manta run --config <file>` becomes the daemon entry point (MAN-77), replacing `listen
--server-config` as the primary way to start manta as a service. `decode` and `gen` remain simple
dev/test tools, unaffected.

### D12 — AI-process artifacts: archived publicly, not moved to `thoughts/`

`docs/superpowers/` (agent planning/task content, currently linked from ROADMAP.md as if it were
design documentation) moves to a clearly-labeled, still-public archive directory (e.g.
`docs/archive/plans/`) with a provenance note — not to `thoughts/`, which is reserved for a different
purpose and shouldn't be mixed with this (MAN-150). `.catalyst/config.json` stops being tracked. The
Claude co-author commit trailer gets fixed at the squash-merge/Mergify template level so it stops
recurring (32 of 79 commits currently carry it despite the standing no-trailer policy). Git history
already contains all of this regardless of where it moves going forward — a history rewrite to make
it retroactively invisible was considered and rejected as disproportionate (breaks PR/issue
references for no real benefit, since the content itself isn't sensitive, just mis-presented).

### D13 — crates.io: not publishing

`manta`, `manta-cli`, and `manta-server` are all already actively published on crates.io by an
unrelated project (an HPE Cray HPC tool). Rather than renaming to avoid the collision, set
`publish = false` explicitly on every crate (MAN-154) — manta isn't publishing to crates.io as a
near-term goal, and the git-dependency pin on `coppa` blocks a clean publish anyway.

### D14 — Default bind: telnet/JSON public, metrics loopback

Telnet and JSON/WebSocket listeners stay public-by-default (`0.0.0.0`), matching how RBN nodes
actually run — a skimmer's cluster port is meant to be reachable. The one real mistake is
`/metrics` sharing that default; it moves to loopback-only by default (MAN-132), with the choice
made explicit at `config init` time.

### D15 — README "Why" framing: soften now

Soften the "closed-source, single point of failure... maintained by a single author" framing (MAN-146)
before any public RBN-OPS post. VE3NEA is a decorated, still-active member of the RBN community who
open-sources new work (JTSkimmer). RBN-OPS is a small community that will quote the current sentence
back verbatim. Reframe as "a second, independent, open implementation" and lead with the genuinely
unfilled gap: Linux, ARM, Raspberry Pi, headless operation.

## Notes on ticket filing

- 86 new tickets were filed (MAN-73 through MAN-158) with priorities and blocking/relates links set
  at creation. See each new ticket's own body for its source lens/item number.
- Roughly 8 review findings were folded into existing open tickets as comments rather than filed as
  duplicates — MAN-4, MAN-13, MAN-14, MAN-17, MAN-20, MAN-25, MAN-40, MAN-45, MAN-49, MAN-52, MAN-60,
  and MAN-64 each carry a 2026-09-06 comment cross-referencing the new work.
- MAN-43 was discovered to be an exact-text duplicate of MAN-40 (both open, both P0, identical body)
  and has been marked Duplicate accordingly.
