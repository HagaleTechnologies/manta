# MAN-86: Aggregator `SKIMMER/SETT` handshake, greeting banner, `BYE`, login validation

**Status:** accepted, implemented.

## Context

Decision D1 of `docs/DECISIONS/2026-09-06-broad-review-decisions.md` makes Aggregator-compatible
operation manta's primary near-term RBN admission path: it's the only publicly documented way for
a source to get its spots forwarded into the RBN. Per the RBN's own Aggregator manual v6.0 §9.2, a
Skimmer/SkimSrv-compatible source that never answers `SETT` has its spots dropped after roughly 5
minutes ("connection failed"). Before this change, manta's telnet server (`crates/manta-server/src/
telnet.rs`) sent a bare `login: \r\n` greeting with no banner, parsed both `SKIMMER/SETT` and `BYE`
into `Command::Unknown` (silently accepted, no reply, no disconnect), and accepted any login value
that was valid UTF-8 — including embedded control bytes.

## Primary-source recovery

The MAN-86 research document concluded the exact `SKIMMER/SETT` wire grammar was unrecoverable from
anything available in this repository or its thoughts pool, because a prior review session's
`pdftotext` manual dumps were never persisted. That conclusion is superseded: this container had
working network egress, and the sources below were re-fetched and re-extracted directly. They are
reproduced verbatim (with retrieval date, URL, and byte size) specifically so this grammar is never
"unrecoverable" again.

### Source 1 — CW Skimmer manual

`http://www.dxatlas.com/CwSkimmer/files/CwSkimmer.pdf` (98 pp., 1,374,410 bytes), retrieved
2026-09-07, HTTP 200, extracted with `pypdf` 6.17.0. The "Telnet Commands" chapter, verbatim:

```
Login
When a Telnet client connects to CW Skimmer, the server sends it a greeting message:

Welcome to the CW Skimmer Telnet cluster port!
CW Skimmer 1.3 is operated by Alex, VE3NEA in Richmond Hill, ON (FN03GW)
Please enter your callsign: ZZ0ZZZ

This message contains the software name and version, operator's name and callsign, and
the QTH and Grid Square of the station. These data come from the Operator tab of the
Settings dialog.
```

```
The SKIMMER/SETT command returns the validation level, the CQ filter, if enabled, and
the boundaries of the decodable segment:

SKIMMER/SETT
SETT: vlNormal CQ 14000.0-14070.0

To end the Telnet session, the client must send the BYE command:

BYE
CU AGN!
```

### Source 2 — RBN Aggregator manual v6.0

`https://cms.reversebeacon.net/sites/cms.reversebeacon.net/files/2019/12/21/Using%20Aggregator%20-%20v6.0.pdf`
(29 pp., 867,578 bytes), retrieved 2026-09-07, HTTP 200, same extraction method.

- §3.1: *"Aggregator also sends a SKIMMER/SETT command to Skimmer to find information about the
  Skimmer such as the operator's callsign, the location, etc."*
- §9.2: *"When connecting directly to a Skimmer Aggregator relies on Skimmer's SETT command to
  provide information on the operator, location, bands covered, and more. If Aggregator does not
  receive a response to a SETT command to a Skimmer, it will not forward spots from that source."*
- §10.5 documents Aggregator's own local user port command set: `BYE or bye`, `Control-D`, `sh/dx`,
  `sh/dx XX`, `sh/dx XXm` — corroborating `BYE` as the ecosystem's standard disconnect verb.

### Source 3 — a real multi-segment SETT capture

`http://lists.contesting.com/pipermail/skimmertalk/2015-December/001624.html`, HB9CAT reporting a
two-SkimServ setup, quoting Aggregator's own Skimmer Traffic tab (the leading `0`/`8` is
Aggregator's own source index per manual §6.1, not part of the reply):

```
0SETT: vlNormal 7000.0-7040.0,14000.0-14070.0
8SETT: vlNormal 7040.0-7060.0,14070.0-14100.0
```

with the corresponding `SkimSrv.ini` in the same message:

```
[User]
Call=HB9H
Name=Art
QTH=Switzerland
Square=JN46la
[Telnet]
CqOnly=0
[Skimmer]
CenterFreqs192=3591000,7091000,10191000,14091000
SegmentSel192=0101
CwSegments=3500000-3570000,7000000-7040000,10100000-10140000,14000000-14070000
```

### Source 4 — RTTY Skimmer Server manual

`http://www.dxatlas.com/rttyskimserv/files/rttyskimserv.pdf` (441,313 bytes), retrieved 2026-09-07,
HTTP 200. Corroborates the semantics from the same vendor and pins the validation-level enum:

```
0 = Minimal
1 = Normal
2 = Aggressive
3 = Paranoid
...
Currently, the RBN recommends a value of "1".
```

## Decisions

1. **The SETT reply carries no callsign or location.** The ticket's own Gherkin reads as if `SETT`
   itself answers with "the operator's callsign, location, and passband settings", but Source 1
   shows the real `SETT` reply is only `SETT: vlNormal[ CQ] <lo>-<hi>[,<lo>-<hi>...]` — validation
   level, optional CQ-filter flag, decodable segments. Cross-referencing Source 2's §3.1/§9.2 (which
   attribute operator/location to "the SETT command") against Source 1, the only reading consistent
   with both is that Aggregator parses operator/callsign/QTH/grid out of the **greeting banner**,
   treating login as one handshake transaction. This is an inference — flagged, not hidden — but it
   costs nothing: this ticket implements both halves (the banner is the ticket's own scenario 2).
2. **Greeting banner is a fixed three-line shape**, matching Source 1 verbatim: `Welcome to the
   manta Telnet cluster port!`, then an operator line (`manta <version> is operated by [<name>, ]
   <call>[ in <qth>][ (<grid>)]`, each optional field cleanly dropped when absent), then
   `Please enter your callsign: `.
3. **Validation level is a fixed `vlNormal`, no config key.** manta's validator (`manta-spot`) has
   no notion of CW Skimmer's four-level scale; a configurable key would invite operators to set a
   level manta doesn't actually honor. `vlNormal` is both the Aggregator-expected token and the
   RBN-recommended value (Source 4).
4. **The `CQ` token is omitted.** manta has no CQ-only mode — it spots CQ, DE, and beacon message
   types uniformly. `SettSettings::cq_only` exists as a field so a future CQ-only mode is a one-line
   change, not a format rewrite.
5. **Segments are derived from the live passband intersected with the amateur allocation table**
   (`band::allocations()`), one segment per overlapped band, ascending — the honest analogue of
   Source 3's "SETT reports what is *currently decodable*, not what is configured" behavior. A
   passband overlapping no allocation at all (a receiver test tone, a nonsense `--dial-freq-hz`)
   falls back to the raw passband rather than an empty list, since an empty list reads to
   Aggregator as "decoding nothing" — worse than an honest (if unlabeled) range.
   **Amended (PR #128 review):** the passband is the source's own *RF passband*
   (`IqSource::rf_passband_hz`, `(lo, hi)` offsets in Hz from the centre frequency), not a width
   derived from its processing sample rate, and not assumed symmetric. Three findings drove this:
   - `KiwiIqSource` upsamples a ~12 kS/s receiver stream to 96 kS/s while asking the receiver for
     `low_cut=-5000 high_cut=5000` — advertising the sample rate there claimed centre ±48 kHz of
     coverage Aggregator would then expect spots from, against a real 10 kHz.
   - `AudioIqSource` Hilbert-transforms a rig's AF output, so its decodable energy is at *positive*
     offsets from `--dial-freq-hz` over the rig's ~3 kHz passband (ARCHITECTURE §3) — half of a
     symmetric ±24 kHz claim is on the wrong side of the dial entirely. Its bounds are the named
     constants `manta_input::AUDIO_PASSBAND_LO_HZ`/`..._HI_HZ` (300/3000 Hz, the nominal widest
     rig AF passband); a rig running a narrow CW filter passes less, and making the claim exact
     needs a config key for the rig's own filter, which is out of MAN-86's scope.
   - The advertised bounds are scaled by the same `input.freq_correction_ppm` calibration factor
     the validator applies to every emitted spot frequency. At the supported ±1000 ppm limit that
     is ~14 kHz on 20 m, enough for manta to advertise bounds that exclude frequencies in its own
     spot stream.

   `sample_rate()` remains the value `SpotBus` uses for sample-index-to-wall-clock conversion;
   only coverage claims use the passband.
6. **Login validation is a permissive shape check, not authentication.** `ARCHITECTURE.md` §7
   already commits normatively to no client authentication on this listener. `manta_spot::grammar::
   is_plausible` was considered and rejected for this purpose: MAN-45 research finding 2 records it
   rejecting real callsigns (`JW/LB2PG`, `GB3LER/B`), and it would also reject SSID login forms like
   `W3XYZ-2` a real cluster client may use. `telnet::sanitize_login` instead accepts 3–16 characters
   of alphanumerics/`/`/`-`, containing at least one letter and one digit, after trimming
   surrounding whitespace and NUL bytes.
7. **Trailing CR/NUL is stripped, not rejected — and it also TERMINATES the line.** RFC 854
   encodes a Telnet NVT "Enter" keypress as CR NUL; real clients (macOS `telnet`, per the ticket)
   send it. Rejecting on its mere presence
   would disconnect legitimate operators. `sanitize_login` strips it (and any other leading/trailing
   whitespace) before validating the remainder — an embedded (non-trailing) control character is
   still a rejection. One rejected attempt closes the connection immediately (no retry loop): this
   listener is unauthenticated by design, and a retry loop is free budget for a scanner.
   **Amended (PR #128 review round 2):** stripping alone was not enough. An NVT client sends
   `CR NUL` and nothing else — no `LF` — and the shared read path (`bounded_io::read_line_bounded`)
   only completed a line on `LF`, so such a login never reached `sanitize_login` at all (the client
   was dropped 30 s later by `IDLE_READ_TIMEOUT`) and a post-login `SKIMMER/SETT` or `BYE` sat in
   the buffer indefinitely — precisely the "Aggregator never gets its SETT answer" failure this
   ticket exists to remove, for every client that terminates that way. The Telnet listener now
   reads through `bounded_io::read_telnet_line_bounded`, which completes a line on `LF` **or** on
   `CR NUL` (including when the two bytes arrive in separate TCP segments); the metrics HTTP path
   (`CRLF`-terminated per RFC 9112) and the outbound RBN uplink keep the strict `LF`-only
   behavior. `command::parse` treats NUL as a token separator alongside whitespace for the same
   reason — otherwise `BYE\r\0` parses as two tokens and is answered as `Unknown`.
8. **The post-login prompt (`de <call>-# >`) is unchanged.** CW Skimmer's own post-login prompt has
   a different shape (`<call> de SKIMMER <date> <time>Z CwSkimmer > `, Source 1) — out of this
   ticket's three named scenarios, and filed as a follow-up (see MAN-86's plan document's
   Follow-ups section) rather than fixed inline, per
   `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Follow-ups (not fixed inline)

Per `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`, P2 findings raised after review
round 1 are captured here rather than chased inline:

- **Emitted spots are not constrained to the advertised SETT segments** (PR #128 review). The SETT
  reply clips the live passband to the amateur allocation table (decision 5 above), but nothing on
  the emission side applies a matching predicate, so a spot decoded at e.g. 13.995 MHz from a
  96 kS/s passband centred on 14.040 MHz is still emitted outside the coverage manta advertised.
  Neither of the reviewer's two options belongs in this ticket: advertising the *unclipped*
  passband would reverse decision 5 (and invite Aggregator to expect spots from non-amateur
  spectrum), while enforcing the allocation bounds at emission is a cross-crate change to the
  deterministic decode path (`manta-engine`/`manta-spot`), not to the telnet handshake — and as
  written it would suppress *every* spot from a file source run without `--dial-freq-hz`, whose
  centre frequency is 0 Hz and whose spot frequencies are therefore baseband, outside every
  allocation. That is the configuration the V1–V10 golden vectors run in, so this needs its own
  ticket, its own golden-vector review, and a decision on what a source with no RF reference
  should claim at all.

## Residual risk

No live Aggregator instance was exercised (no Windows/Wine host reachable from this container) —
the wire format is reconstructed from four primary sources, a much stronger footing than the prior
research's "unrecoverable" conclusion, but "Aggregator's parser accepts this exact byte sequence" is
only provable against a real Aggregator or a friendly node operator's capture. This is the same
verification gap MAN-90 exists to close for the outbound uplink side.
