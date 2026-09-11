# 2026-09-06 — Broad UI/UX/market review: architectural decisions

**Status:** Decided (Tony, 2026-09-06). Supersedes any conflicting framing in README.md, ROADMAP.md,
or open tickets predating this date on the specific points below — and, on the narrower points D3,
D8, D9, and D14 each call out explicitly, supersedes the specific prior accepted decision doc or
normative-guidance section named there too (`AGENTS.md`, `docs/SPEC-decode-core.md`, the 2026-09-01
capability matrix, and the 2026-09-02 threat model respectively). Each of those four sections says so
inline rather than relying on this blanket line, since a blanket "supersedes everything" clause isn't
enough to safely override a named, accepted decision record.

## Context

A broad review of manta's UI/UX, operations, decode quality, competitive positioning, and repo
polish ran 2026-09-05/06 (five parallel sessions, one per lens, against `origin/main` @ `5b9e747`).
The full review and its five per-lens reports are in `thoughts/shared/reports/` (gitignored, not
committed — this doc is the durable, public record of the decisions it produced). The review
produced 94 hit-list items and raised 16 distinct strategic/logistics questions needing Tony's
judgment before those items could be filed as tickets; several of those 16 turned out to share one
answer (e.g. the K5TR-hardware-arriving-in-~1-week status folded into D1's node-credibility framing
and D5's data plan, rather than standing alone), so this doc records them as the **15 decisions
below**, not a literal one-to-one list of 16. All 94 hit-list items are now filed as MAN-73 through
MAN-158 (some folded into existing tickets instead of duplicating them — see each ticket's own
comments for cross-references).

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
on a consenting node operator's system (MAN-90), rather than asking RBN's team cold. This clarifies
D1's "already shipped" framing rather than contradicting it: MAN-32's uplink code exists and works
against a mock listener, but `dry_run` **defaults to `false`** (`config.rs::default_dry_run`) —
enabling `[[rbn_uplink]]` at all starts transmitting to whatever host/port is configured unless an
operator explicitly sets `dry_run = true`. So "shipped" means the capability exists, not that it's
been run against the real network — nobody should point it at a real RBN target (with or without
`dry_run` set) until MAN-90 confirms what that target actually expects. Documentation (README, the
operator guide in MAN-148) should tell operators to set `dry_run = true` explicitly today, and this
default is itself worth revisiting to fail safe.

**Resolved 2026-09-06 (MAN-159):** the last sentence above is now done --
`default_dry_run()` returns `true`, so an `[[rbn_uplink]]` block that omits
`dry_run` connects and logs in but transmits nothing, and `uplink::serve`
logs the mode per target at startup. D2's recommendation that documentation
"tell operators to set `dry_run = true` explicitly" is therefore obsolete
and was never written into README; README instead documents the safe
default and how to opt out of it. The substantive part of D2 stands
unchanged: nobody should point the uplink at a real RBN target until MAN-90
confirms what that target accepts.

### D3 — Wire SNR convention: dual-referenced

Telnet and the RBN uplink report SNR in the 500 Hz reference bandwidth RBN/CW Skimmer use (so manta
doesn't read as "deaf" next to other nodes); the JSON stream keeps the native 2500 Hz channel
measurement plus an explicit `snrRefHz` field (camelCase, matching every other `SpotMessage` wire
key — `dxDxcc`, `decodeConfidence` — since the struct uses `#[serde(rename_all = "camelCase")]`), so
the credibility/benchmark work (MAN-116,
MAN-136-adjacent) wants the uncorrected detector value to calibrate against. This is a dispensa
contract change for the JSON field (MAN-102).

**Explicitly supersedes** `AGENTS.md`'s "this repo's spec froze SNR-in-2500-Hz" line and
`docs/SPEC-decode-core.md` §2.3/§7's `SNR_2500`-only framing, but only at the wire-output boundary:
the internal detector computation, the confidence formula (`q = clamp(SNR_2500/20, 0.3, 1.0)`), and
the golden-vector pass criteria all stay defined in 2500 Hz exactly as SPEC documents today — nothing
about the internal pipeline changes. This decision adds a +7 dB conversion applied only when
formatting the telnet/uplink wire line, and a new `snrRefHz` field on the JSON output. MAN-102
(the implementing ticket) and this doc's own §2.3/§7 update should both make that scope explicit so
a future reader of SPEC doesn't conclude the internal 2500 Hz convention was abandoned.

### D4 — Multi-band identity: `CALL-N-#` per band

Once multi-source/multi-band support (MAN-13) lands, a manta node identifies each band/segment on
RBN as `CALL-N-#` (e.g. `W5AU-1-#`, `W5AU-2-#`), matching RBN's own live convention — confirmed by a
live capture where 10 of 71 observed spotters used exactly this pattern. MAN-89 is the blocking
callsign-grammar fix; MAN-13/14 carry the design decision itself.

### D5 — RBN-parity benchmark data: work backwards from K5TR's existing IQ

MAN-20's data-dependency blocker (no recorded contest-weekend IQ paired with reference RBN spots) may
not need a fresh *live simultaneous telnet* capture — the specific gap D5 removes. K5TR IQ recordings
already exist. RBN publishes daily public archive CSVs carrying per-spot callsign/frequency/time/
SNR/WPM from the spotting station for that time window — if the recording's exact UTC window and
receiver chain (same antenna/SDR that fed his live SkimSrv at the time) can be confirmed, that archive
slice stands in as the reference dataset for whatever recordings exist, with no live capture session
required to get one.

**This does not yet satisfy ROADMAP.md M3's ≥ 2 h parity-benchmark requirement on its own.** The two
named recordings today (`wpx_cw_iq_96khz.wav`, ~10 min, and `B2_20251129_000000_7080kHz.wav`, ~15
min) total roughly 25 minutes, well short of 2 hours. D5 removes the "need a live capture at all"
blocker and gives a real path to a first, partial validation from what already exists — it does not
retire the ≥ 2 h requirement, which stays open until either more K5TR (or another operator's)
recordings are identified, or a fresh capture happens after all. See the comment thread on MAN-20
for the concrete next step and MAN-20's own body for the exact requirement text.

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

**Explicitly revises** `AGENTS.md`'s Status section, which currently frames V2/V5/V6/V8w as
"deferred to M4 ML fusion by design, not M2 blockers." That framing is now wrong for exactly the
fading-related subset of those vectors (V5, V6, V8w): D8 says fix them with classical DSP work first,
gated on the tickets above, before M4 rather than deferring to M4. It does not change V2's own disposition (V2's issue is the near-channel-edge WPM-estimation bug
tracked as MAN-7 and MAN-103, unrelated to fading). **AGENTS.md's Status section has been updated in
this same PR** to reflect this reclassification now rather than after MAN-107 through MAN-113 land —
leaving the entrypoint document telling agents to defer these vectors to M4 while this decision says
otherwise would have undermined the decision the moment it was recorded.

### D9 — Contester persona: in scope

N1MM's Spectrum Display UDP protocol (MAN-141) is in scope, not excluded by manta's headless-RBN-node
framing. It's CW Skimmer's second real market, the PFB channelizer already computes the raw power
spectrum this feed needs, and it doesn't require a GUI. **This is a panadapter/spectrum feed, not
band-map support** — the capability matrix already correctly treats Band Map UI (decoded-call display,
still a Non-goal — manta isn't an interactive receiver) and Spectrum-via-UDP (raw power spectrum for a
third-party panadapter) as two separate rows, and MAN-141 is only the second one. Don't scope MAN-141
as if it delivered decoded-call band-map data; N1MM's own band map comes from its DX cluster telnet
connection instead, which manta's existing telnet server already serves.

**Explicitly supersedes** the accepted `docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`'s row
disposing "Spectrum via UDP (feeds a power spectrum to third-party panadapters like N1MM+)" as a
Non-goal — that row alone, not the separate Band Map UI row. It should be updated to point at MAN-141
as a real, in-scope gap rather than a deliberate exclusion, the next time that matrix doc is touched.

### D10 — cqdx contract gap: vendor the fix

Close the `dxDxcc`-required-but-null finding (already on file in MAN-45) by vendoring a small ADIF
DXCC entity-number table alongside `cty.dat` (MAN-136), rather than waiting on a dispensa contract
relaxation. This ships from manta's side without needing cqdx/dispensa to act first.

### D11 — `run` becomes the daemon verb, promoted from `listen`

`manta run --config <file>` becomes the daemon entry point (MAN-77), replacing `listen
--server-config` as the primary way to start manta as a service. `decode` and `gen` remain simple
dev/test tools, unaffected.

### D12 — AI-process artifacts: archived publicly, not moved to `thoughts/`

`docs/superpowers/` (agent planning/task content, currently linked from ROADMAP.md as if it were
design documentation) moves to a clearly-labeled, still-public archive directory (e.g.
`docs/archive/plans/`) with a provenance note — not to `thoughts/`, which is reserved for a different
purpose and shouldn't be mixed with this (MAN-150). `.catalyst/config.json` stops being tracked. The
Claude co-author commit trailer gets fixed at the squash-merge template level so it stops
recurring -- that template was Mergify's when this decision was written, but #185 retired Mergify
here, so the knob to change is now the *repository-level* squash-commit defaults,
`squash_merge_commit_title` and `squash_merge_commit_message` (Settings -> General -> Pull
Requests, or `PATCH /repos/{owner}/{repo}`), which the merge queue's squash merges inherit. The
native `merge_queue` rule on the `main-protection` ruleset is **not** where this lives: that rule
selects the merge method (and queue sizing/timeouts), and has no parameter for the generated
commit's title or body. There is no `.mergify.yml` to open for this either. **A specific commit count is deliberately not pinned here**: roughly two-thirds of the
commits carrying any `Co-authored-by` trailer name Claude specifically (the rest are legitimate
`dependabot[bot]` and Tony Hagale attribution, unrelated to the standing no-trailer policy this
decision is about), but the exact counts drift with ordinary repo activity — including every commit
this PR itself adds while fixing review feedback, each of which adds one more Claude-trailer commit
to the very history being measured. Re-run `git log --format='%H %(trailers:key=Co-authored-by,valueonly)'`
against current `HEAD` if an exact count is needed at the time this ticket is actually worked, rather
than trusting a number frozen here. Git history
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

**Explicitly reverses, for `/metrics` only,** the accepted disposition in
`docs/DECISIONS/2026-09-02-man23-threat-model.md` (finding 11: "Accepted risk, documented
explicitly"), and the same posture repeated in `ARCHITECTURE.md` §7/§8 and
`docs/RUNBOOKS/network-exposure.md`. All three currently and correctly state that `bind_addr` is
**shared** across all three listeners today (`crates/manta-cli/src/main.rs`) — there is no
per-listener bind option yet — so firewalling was the only safe mitigation, and defaulting
`bind_addr` to loopback would have silently taken telnet/JSON offline too. MAN-132 has to actually
add the per-listener bind option before this decision can be implemented; it isn't a config-default
flip on the existing shared setting. Once MAN-132 lands, the threat-model doc's finding 11,
ARCHITECTURE.md's exposure-policy note, and the network-exposure runbook's mitigation guidance all
need updating to reflect the new default and the fact that telnet/JSON's public-by-default posture
is unchanged and still deliberate.

### D15 — README "Why" framing: soften now

Soften the "closed-source, single point of failure... maintained by a single author" framing (MAN-146)
before any public RBN-OPS post. VE3NEA is a decorated, still-active member of the RBN community who
open-sources new work (JTSkimmer). RBN-OPS is a small community that will quote the current sentence
back verbatim. Reframe as "a second, independent, open implementation" and lead with the genuinely
unfilled gap: Linux, ARM, Raspberry Pi, headless operation.

## Notes on ticket filing

The 94 hit-list items break down exactly as follows (86 + 5 + 3 = 94 — auditable, not approximate):

- **86 items got a new ticket** (MAN-73 through MAN-158), each with priorities and blocking/relates
  links set at creation. See each new ticket's own body for its source lens/item number.
- **5 items were disposed via a comment on an existing ticket instead of a new ticket** — one of
  those five (the multi-band-identity item) touched two tickets, so this produced six ticket-comments
  from five items: MAN-4 (one), MAN-13 and MAN-14 (one item, both), MAN-20 (one), MAN-52 (one), MAN-60
  (one).
- **3 items needed no ticket and no comment**: one was already fully covered by an existing ticket's
  Gherkin (RST/QRL extraction, already MAN-33's exact scope); one is a documented decision not to
  build something (no TUI — the operator-visibility questions it would answer are covered by other
  filed tickets instead); one was folded into the scope of two other new tickets rather than tracked
  standalone (the ROADMAP post-1.0 reordering and HamSCI outreach items, absorbed into MAN-145 and
  MAN-148 respectively).
- Separately, and not double-counted above: **MAN-17, MAN-25, MAN-40, MAN-45, MAN-49, and MAN-64**
  each also received a 2026-09-06 comment from this review — but as cross-references (new blocking
  sub-tickets, a decision recorded, a corrected safety-default finding) on tickets that already
  existed and aren't themselves one of the 94 hit-list items, not as a fold of a hit-list item that
  would otherwise have gotten its own ticket.
- MAN-43 was discovered to be an exact-text duplicate of MAN-40 (both open, both P0, identical body)
  and has been marked Duplicate accordingly.
