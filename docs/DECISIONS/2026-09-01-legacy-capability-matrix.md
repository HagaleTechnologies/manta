# 2026-09-01 — CW Skimmer / SkimSrv / Aggregator capability matrix

**Status:** accepted (investigation only — no implementation in this doc or its PR).

## Decision

Every capability of the three legacy programs a real RBN operator runs today
— **CW Skimmer** (decode engine/GUI), **SkimSrv** (headless multi-band decode
engine), and **Aggregator** (multi-instance combiner + RBN telnet feed) — is
catalogued below and given exactly one disposition: already covered by manta
or an existing ticket, a genuine gap filed as its own new ticket, or a
deliberate non-goal.

Sources are each program's own published documentation only (clean-room — no
decompilation, no leaked/pirated source):

- `CwSkimmer.pdf` v2.1 (Afreet Software, official manual, dxatlas.com)
- `dxatlas.com/skimserver/` (CW Skimmer Server product page)
- *Using Aggregator with the Reverse Beacon Network* v6.0, Dec 2019 (RBN's own
  CMS, `cms.reversebeacon.net`)
- RBN-OPS (groups.io) forum research, two rounds, already logged as comments
  on MAN-15 (2026-09-02) — not re-run here.

Cross-checked against `README.md` §Non-goals, `ARCHITECTURE.md` §6 (`manta-spot`
validation pipeline), `ROADMAP.md`, and MAN-10 through MAN-23.

## Correction to MAN-15's own technical notes

CW Skimmer, SkimSrv, and Aggregator are **three separate programs with three
separate jobs**, not two:

- **CW Skimmer** — the interactive decode engine + GUI. One process, one
  receiver, human-facing.
- **SkimSrv** ("CW Skimmer Server") — the same Bayesian decode engine,
  headless, covering up to 7 bands (192 kHz each) in one process. Interfaces
  the decode engine to SDRs beyond CW Skimmer's own hardware list.
- **Aggregator** — a *separate* Windows program (by Dick Williams W3OA / Pete
  Smith N4ZR, distributed by the RBN team, not Afreet) that telnets into one
  or more CW Skimmer/SkimSrv/RTTYSkimServ/RCKskimmer instances, filters and
  dedupes their spots, and forwards them to the RBN network.

## Capability matrix

### CW Skimmer (decode engine/GUI)

| Capability | Disposition |
|---|---|
| Multi-channel CW decode (simultaneous decoders across the whole passband) | **Covered, via a different algorithm, at a lower configured capacity** — corrected after Codex review on PR #57: `manta-decode` is NOT a Bayesian-statistics decoder like CW Skimmer's; verified against ARCHITECTURE.md §5, it's a classical dual-rail EMA keying detector + a width-4 beam search over the Morse tree. Different algorithm, same capability (simultaneous multi-channel decode across the passband). The shipped default (`PipelineConfig::track_cap`, `crates/manta-engine/src/track.rs`) caps concurrent tracks at 500, not CW Skimmer's claimed "up to 700 on a 3-GHz P4" ceiling. `track_cap` is a config value, not an architectural limit — raising it is gated by the same Pi4 CPU-budget invariant MAN-18 already tracks, so no separate ticket is filed; caught by Codex review on PR #57 |
| CQ/DE/beacon message-context parsing (running-vs-S&P proxy) | **Covered** — verified against `crates/manta-spot/src/context.rs`: `SpotType::{Cq,De,Beacon,Unknown}` is implemented and matches the exact pattern families ARCHITECTURE.md §6 describes; `Cq`/`De` classification is manta's equivalent of CW Skimmer's CQ/DE-prefix running-vs-S&P convention |
| RST(599 label)/QRL? message-content extraction | **Gap** — see MAN-33 below. ARCHITECTURE.md §6's prose reads as if this were covered too, but `manta-spot/src/context.rs` has no RST or QRL? extraction at all — caught by Codex review on PR #57 (correctly), verified against source before filing |
| Callsign plausibility (pattern/grammar, ITU-block rejection) | **Covered, with a known bug** — `manta-spot` step 2, cty.dat prefix lookup. Codex review on PR #57 found and I verified a real defect (not a matrix-disposition error): `cty.rs::clean_alias` strips the leading `=` off exact-call overrides (e.g. `=4U1UN`) without preserving that they're exact-only, so `is_allocated`'s any-length prefix match lets a bogus extension (`4U1UNA`) pass. Filed as MAN-35 (bug, not a capability gap — the mechanism exists, it has a hole) |
| Master.dta/SCP cross-check | **Covered** — `manta-spot` step 3, `master.scp` (confidence-raising only, an improvement on Aggregator's own binary SCP filter, which the RBN team itself recommends against — see below) |
| Verified-calls (≥2 occurrences) filtering | **Covered** — `manta-spot` step 4, repetition requirement |
| Same-track/single-source dedup (10 min, freq bucket) | **Covered** — `manta-spot` step 5. The 10-minute window and freq-bucket key independently match the operator-stated community rule found in forum research ("same DE+DX+frequency within ~10 min = dupe") — good corroboration, no action needed. |
| Watch List (explicit allowlist that bypasses repetition/validation, used for low-repetition NCDXF-style beacons) | **Gap** — see MAN-28 below. Widened after Codex review on PR #57: `Validator::try_spot` runs grammar/cty rejection *before* the repetition gate, so a real Watch List equivalent must bypass all three, not just repetition — MAN-28 now covers the general allowlist mechanism plus the beacon case that motivated it |
| Waterfall display, Band Map UI, Callsign List window | **Non-goal** — README: "Not an interactive receiver or panadapter" |
| DSP audio monitoring (noise blanker/AGC/anti-click/CW filter for a human listening on headphones) | **Non-goal** — same; manta's noise/AGC handling is internal to the decode pipeline, not an operator audio-monitoring feature |
| I/Q Recorder/player (live in-app WAV capture with RIFF metadata tags) | **Non-goal** — operators can capture raw IQ with standard OS/SDR tooling; not core to spot generation. (Distinct from manta's own WAV-based *test* corpus, MAN-20, which is a different concern.) |
| Spectrum via UDP (feeds a power spectrum to third-party panadapters like N1MM+) | **Non-goal** — panadapter-adjacent, covered by the same non-interactive-receiver non-goal |
| CAT/rig control (OmniRig) for band-scope alignment, wideband sources | **Non-goal** — per CW Skimmer's own manual, CAT is required only in 3-kHz and SoftRock-IF (narrowband) modes and is explicitly *not used* with wideband SDRs (SDR-IQ, QS1R, Mercury, Perseus) — manta's OpenHPSDR/Hermes target hardware (MAN-10/11) is in this same wideband class. Band-scope alignment itself is also panadapter-adjacent. |
| RF center-frequency reference for the rig-audio input mode | **Gap** — see MAN-34 below. Caught by Codex review on PR #57: the CAT non-goal above only holds for wideband sources — manta's already-shipped `listen`/`listen --device` audio-passband mode has no CAT need either, but also has no *other* way to know the tuned RF frequency. Verified: `AudioIqSource::center_freq_hz()` always returns `0.0` (`crates/manta-input/src/audio.rs:77`), a deliberate M1-scope decision, not a non-goal |
| Remote SKIMMER/QSY, SKIMMER/AUDIOIF, SKIMMER/LO_FREQ (narrowband retune-by-telnet commands) | **Non-goal** — these retune a single narrowband receiver; manta's channelizer decodes the whole configured passband at once and has nothing to retune |
| Remote SKIMMER/START, SKIMMER/STOP (process start/stop via telnet) | **Non-goal** — process lifecycle is an ops/systemd concern (MAN-21), not a wire-protocol feature |
| Multiple instances via per-instance `.ini` files | **Covered (superseded)** — MAN-13's single-daemon multi-source model replaces this |
| Auto-start (command-line switch / VBScript) | **Covered** — falls under MAN-21's non-developer install/operate scope |
| Frequency calibration (manual correction-factor procedure) | **Gap** — see MAN-29 below |

### SkimSrv (headless multi-band decode engine)

| Capability | Disposition |
|---|---|
| Headless, multi-band (up to 7 × 192 kHz) decode in one process | **Covered** — MAN-13, single daemon combining multiple SDRs/channels |
| Built-in telnet server (spot output, default port 7310) | **Covered** — MAN-12 |
| Per-band `.ini` config (`CenterFreqs*`, `SegmentSel*`) | **Covered** — implementation detail of MAN-11/13's per-source configuration |
| `CwSegments` — explicit non-contiguous CW sub-range restriction (skip RTTY/PSK/etc. gaps to save CPU) | **Gap** — see MAN-30 below |
| Shares CW Skimmer's Watch List file | **Gap** — same disposition as CW Skimmer's Watch List, MAN-28 |

### Aggregator (multi-instance combiner + RBN feed)

| Capability | Disposition |
|---|---|
| Combines multiple *native* SDR/audio sources into one spot stream | **Covered (different approach)** — MAN-13's in-process single-daemon model natively combines multiple raw IQ/audio sources, which is what most operators use Secondary Skimmers for |
| Combines spots from *external* CW Skimmer/SkimSrv/RTTYSkimServ/RCKskimmer processes over telnet (Secondary/Combined Skimmers) | **Non-goal** — corrected from an earlier "Covered" label (caught by Codex review on PR #57, verified: `manta-input` has no telnet-client input, only `audio.rs`/`kiwi.rs`/`soapy.rs` raw sources). This is already README's existing non-goal ("Not a cluster network. `manta` is a spot source, not an aggregator.") — an operator keeping an external legacy Skimmer instance can't feed it into manta, by design |
| Cross-instance spot dedup | **Covered** — MAN-16 |
| Bad Calls list (operator-maintained callsign blocklist) | **Gap** — see MAN-31 below |
| Notched Frequencies (operator-maintained frequency-range exclusion list, for known false-spot sources) | **Gap** — same ticket as MAN-31, see below |
| Super Check Partial (MASTER.SCP) binary include-filter | **Covered, and already improved on** — manta's SCP cross-check (confidence-raising only) avoids the exact false-negative failure mode the RBN team itself warns against ("screens out ... new calls ... casual contesters") for Aggregator's binary version |
| VHF-specific grid-format and low-SNR false-positive filters | **Non-goal** — corrected from an earlier "Covered" label (caught by Codex review on PR #57): manta targets HF CW only, so no VHF/grid-format spot class exists to filter — this is a non-goal, not a covered capability |
| RBN telnet feed — inbound serving (clients connect to manta) | **Covered** — MAN-12's telnet/JSON output; a stock DX cluster client connects to and reads from manta, per ROADMAP.md M3's acceptance criterion |
| RBN telnet feed — outbound submission (manta pushes into RBN's own network) | **Gap** — see MAN-32 below. Corrected from an earlier "Covered" label (caught by Codex review on PR #57): this is Aggregator's actual core job (§2.0 of its manual: "Forwards selected spots to the RBN server via an Internet connection") and is the opposite direction from MAN-12's inbound server — nothing in the current backlog lets manta submit itself as an RBN-contributing node |
| Dry-run / "don't actually send to RBN" test mode | **Covered (folded into MAN-32)** — corrected from an earlier "Covered by MAN-17" label (caught by Codex review on PR #57): MAN-17 is offline recall/false-spot benchmarking against recorded IQ, not a runtime switch on a live outbound connection. Once MAN-32 (outbound RBN submission) adds that connection, it needs its own suppress-transmission mode — folded into MAN-32's scope rather than left to a validation ticket that doesn't touch the outbound path at all |
| Transverter base-frequency offset | **Non-goal** — a narrow hardware-specific accessory; no MAN-10/11 target hardware is described as transverter-fed |
| Local User Port (second telnet stream, independently configurable to show all decoded spots vs. only RBN-forwarded spots) | **Covered, requirement made explicit in MAN-12** — corrected after Codex review on PR #57: "a design nuance to consider" left no real acceptance criterion. MAN-12's technical notes now explicitly call for two independently filterable output streams (all decoded spots vs. only-forwarded-to-RBN) once MAN-32 exists, rather than leaving it as an unrequired implementation choice |
| `.ini` file rotation (scheduled day/night, weekday/contest swaps, sunrise/sunset-relative) | **Gap** — same ticket as MAN-30 (SkimSrv's `CwSegments`), see below — this is the scheduling half of the same underlying capability |
| Patt3Ch.lst sync (auto-check/download updated pattern files from the RBN server every ~20 min) | **Non-goal** — manta's validation patterns (cty.dat, master.scp) are bundled/refreshable config, not a community-shared file requiring a bespoke sync client; refreshing them is an ops/packaging concern (MAN-21), not a new capability |
| FT4/FT8 UDP monitoring (up to 33 WSJT-X/JTDX instances) | **Non-goal** — README: "Not a general digital-mode skimmer. FT8 and RTTY are out of scope for 1.0" |
| Associate Programs (sequenced launch of up to 8 companion Windows programs at startup) | **Non-goal** — a workaround for the legacy stack's multi-process Windows sprawl (Skimmer + Aggregator + virtual-audio-cable software + WSJT-X, etc.); manta's single-binary architecture (ARCHITECTURE.md) has nothing to orchestrate |
| Beacon detection heuristic (flags NCDXF-style beacons in the traffic tab) | **Covered** — corrected after Codex review on PR #57: this duplicated row 49 above. `context::parse` already recognizes `V V V <call>` and returns `SpotType::Beacon` — detection is implemented today. Only the low-repetition validation *exemption* for beacon-type spots is the actual gap, already correctly assigned to MAN-28 at row 55 |
| Cluster-side "unique spot" filter interop (`set dx filter unique > 1`) | **Covered, reattributed to MAN-12** — corrected after Codex review on PR #57: MAN-17 (ROADMAP.md M3) is the offline recorded-IQ recall/false-spot benchmark and never touches live client filter commands; ARCHITECTURE.md §7 assigns "enough command grammar (`sh/dx`, filters) for common clients not to choke" to `manta-server`, i.e. MAN-12's territory |
| Computer sizing guidance (8-core AMD FX-8350: 7 bands @ 192 kHz, 8–11% CPU) | **Informational only** — useful reference data point for MAN-18's Pi4 CPU-budget work, not a capability to catalog |

## New gap tickets filed from this matrix

| Ticket | Capability | Why it's a genuine gap |
|---|---|---|
| MAN-28 | Watch List equivalent (general validation-bypass allowlist, beacon exemption is the motivating case) | Widened after Codex review on PR #57: `Validator::try_spot` runs grammar/cty rejection before the repetition gate, so a real Watch List must bypass all three. manta's context parser already type-tags BEACON messages (ARCHITECTURE.md §6 step 1) but the repetition gate (step 4) has no exemption for them — NCDXF-style beacons ID once per power-step and would be silently dropped, mirroring exactly the problem CW Skimmer's Watch List was built to solve (Aggregator manual Appendix A2) |
| MAN-29 | Per-source frequency-calibration correction factor | Forum research already flagged "systematic 200+ kHz frequency-calibration errors" as a known recurring bad-spot class; CW Skimmer/SkimSrv's `FreqCalibration=` key and the Aggregator manual's dedicated calibration appendix show this is a real, actively-managed operator workflow today, with no manta equivalent |
| MAN-30 | Configurable CW sub-segment decode restriction, optionally schedulable (time-of-day, weekday/contest, sunrise/sunset-relative) | SkimSrv's `CwSegments` (skip non-CW ranges) and Aggregator's `.ini` rotation (day/night, contest, sunrise/sunset-relative swaps) are the same underlying capability split across two legacy programs; lower priority — may be closed as unnecessary if MAN-18's Pi4 CPU-budget gate passes without it. MAN-30's own scenario is trigger-agnostic (any scheduled swap, not just time-of-day) — this label was narrower than the ticket, caught by Codex review on PR #57 |
| MAN-31 | Operator-configurable spot suppression: bad-call blocklist + notched frequency ranges | Manual operator override lists, orthogonal to manta-spot's automatic validation pipeline (cty.dat/SCP/plausibility); addresses a real, named failure mode (birdies/spurs at fixed frequencies, known-bad callsigns) that automatic validation doesn't catch |
| MAN-32 | Outbound RBN submission (become an RBN-contributing node) | Aggregator's core job is pushing spots INTO the RBN network; MAN-12's telnet/JSON server is the opposite (inbound) direction. Found by Codex review on PR #57, not the original research pass — the initial matrix mis-disposed this as covered |
| MAN-33 | RST/QRL? spot-content extraction | `manta-spot/src/context.rs` classifies CQ/DE/Beacon but has no RST or QRL? extraction, contra the original matrix's over-broad "covered" claim. Found by Codex review on PR #57, verified against source before filing |
| MAN-34 | RF center-frequency reference for the rig-audio input mode | `AudioIqSource::center_freq_hz()` always returns `0.0` — the already-shipped `listen`/`listen --device` mode reports baseband offsets, not absolute frequency. Found by Codex review on PR #57 — the original CAT non-goal was correct for wideband sources but too broad to also cover this already-shipped narrowband mode |
| MAN-35 | (Bug, not a capability gap) `is_allocated` lets a callsign extending an exact-call alias pass validation | `cty.rs::clean_alias` strips the exact-call marker (leading `=`) without preserving that the entry is exact-only, so `is_allocated`'s any-length prefix match lets e.g. `4U1UNA` pass because `4U1UN` (from `=4U1UN`) matches as a length-5 slice. Found by Codex review on PR #57, verified against `crates/manta-spot/src/cty.rs` |

MAN-28 through MAN-31 came from the original research pass; MAN-32 through
MAN-35 came from Codex's review of this PR across two rounds
(`docs/DECISIONS` diff), which caught capabilities the original matrix had
mis-disposed as "covered" without checking the actual source, plus one
real validation bug (MAN-35, not a disposition error) surfaced along the
way. Each was re-verified against `manta-spot`/`manta-input` source before
filing or updating.

## Non-goals not yet in README.md

Two capability classes are Non-goal dispositions above but don't fit any of
README's four existing bullets. Recommend folding these in if this doc is
accepted:

- Not a multi-process Windows orchestrator — manta is a single Rust binary;
  there is no companion-program sprawl to sequence-launch.
- No CW Skimmer-style dual MME/WDM soundcard configuration surface, and no
  CAT/rig control to align a narrowband receiver with the channelizer, for
  manta's wideband sources. (Narrower than an earlier draft of this bullet,
  which read as excluding manta's own already-shipped rig-audio input
  mode entirely — caught by Codex review on PR #57; see MAN-34 above for
  the gap that mode still has.)

## Non-outcomes

- No implementation work was done from this ticket.
- manta's existing `manta-spot` validation design (confidence-raising SCP,
  10-minute same-track dedup) independently converges with, and in the SCP
  case improves on, the legacy stack's approach — no design change indicated.
