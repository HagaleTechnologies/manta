# MAN-490: a clean repeat after an `AR`/`SK`-damaged copy of the same call takes its frame

Date: 2026-11-14. Ticket: MAN-490.

**Status:** accepted. Amends [MAN-201's callsign roles](2026-11-02-man201-callsign-roles.md)
(its follow-up "Framing a clean call through a damaged copy of itself", and
its accepted cost "A clean call right after a damaged copy of itself does not
spot"), for one damage shape only.

## Context

MAN-201 gives every callsign occurrence one role, and only a settled
`self_id` with a spot form is a candidate. On the V8w faded pileup, track 19
decodes `CQ CQ DE N<SK>NXI N3NXI K`: the decoder garbled the `3` of the first
copy into an `SK` glyph. Two rules lost the clean copy:

- an `AR`/`SK` glyph ends the turn even inside a word, so `N3NXI` opened a new
  turn, and no frame is read across a turn end;
- even inside one turn, rule 8 leaves a call after another call unframed
  (`uncertain`), since the damaged word (`NNXI` once stripped) took the `DE`.

The ticket's numbers (27 to 25 genuine at `3a6f3c9`) predate MAN-201's last
three fixes. Re-measured on main `0fab172` (research phase, `manta decode
--json` and `replay_spots`, and independently by
`v8w_with_repetition_fusion_on_stays_at_zero_bogus`): 48 spots, 28/50 genuine
distinct, 0 bogus. VE3QHG already spots; N3NXI and W5REB do not. N3FXO is a
genuine scene call, not a bogus gain.

## Decision

- **D1. An embedded glyph in an implausible word is damage, not a turn end.**
  The validator keeps, per buffered word, the position of its first `AR`/`SK`
  glyph (characters before it) and whether a second arrived. At the word's
  boundary the glyph is *embedded* when it is the only one, a character came
  before and after it, and the word stripped of it is call-shaped
  (`exchange::is_call_shaped`: no keyword, report, serial, Q-code or
  correction) and fails `grammar::is_plausible`. An embedded glyph sets no turn end and is kept as
  `Word::damage_at`; every other `AR`/`SK` (standalone, leading, trailing,
  twice in a word, or in a plausible word) ends the turn as before. A silence
  turn end is set on the completed word independently. `<ERR>` still
  discards the buffer, glyph evidence included. Refinement clears
  `damage_at` when it replaces the word's text.
- **D2. The clean repeat right after it inherits its frame.** A crate-private
  `exchange::interpret_with_damage` takes the positions; the public
  `interpret` passes none and is unchanged. In rule 8's fallback only (after
  correction, before-`DE`, the direct frames, `TU TEST`, `UP`,
  report/serial/query and sign-off), and only when no other station's
  self-ID earlier in the turn makes the run a mention, a run takes the frame
  of the damaged word directly before it when all of these hold:
  - the run is a plausible call with no damage evidence of its own, in the
    same turn as the damaged word;
  - the damaged word is call-shaped and not `corrected`, its glyph is
    strictly inside it, and it is not a plausible call;
  - removing the run's one character at the glyph's position spells the
    damaged word exactly (ASCII case-insensitive), and that character is a
    letter or digit. No edit distance, SCP or dictionary;
  - the word before the damaged one, in its turn, is `DE`, `CQ` or a filler,
    and the existing back-scan from the damaged word reaches a `DE` or a
    `CQ`. No correction or second damaged word is skipped; `TEST`, sign-off
    and `V V V` frames are not inherited.

  The run gets rule 3's frame and basis for that scan (`AfterDe`/`AfterCq`),
  and its span starts at the frame, so the damaged word is part of its
  context. It settles and latches as any run does; the damaged word's own
  role is never reopened.
- **D3. Evidence stays the clean word's.** The candidate is the exact clean
  word with its own sample span, confidence and boundary timestamp. The
  damaged word adds no repetition and no character reading, and grammar,
  cty, blocklist, notch, repetition, variant and dedupe checks all still
  apply. One message still counts once (V39): `N<SK>NXI N3NXI N3NXI K` sent
  once stays one repetition.
- **D4. No public surface changes.** No configuration, CLI flag, JSON field,
  public Rust field or enum variant, metric or wire format.

## Evidence

Commands, at this change (coppa pinned at
`f8a4d16df7e5776a0756943c05712038774e6c70`; no fading vector regenerated):

```sh
cargo test -p manta-spot --locked                 # all pass, MAN-490 tests included
cargo test -p manta-cli --locked --test golden_v8w man490 -- --nocapture
./target/debug/manta decode --json v8w.wav        # twice, then cmp
```

V8w, default settings (fusion and prior off), against the fixture manifest:

| | spots | genuine distinct | bogus | N3NXI | VE3QHG | W5REB | N3FXO |
|---|---|---|---|---|---|---|---|
| main `0fab172` | 48 | 28/50 | 0 | absent | spots | absent | spots (genuine) |
| this change | 49 | 29/50 | 0 | spots `cq`, track 19 | spots | absent | spots (genuine) |

Added: N3NXI, confirmed at sample 3,097,600, the clean copy of the
`N<SK>NXI N3NXI` transmission. Removed: none. The 28 baseline calls are the
manifest's 50 less the 22 unspotted ones the research census listed, and
all 28 still spot. Two decodes of the same rendered V8w gave byte-identical
reports (`cmp` clean; 49 spots both times).

Unit and event vectors:

- `golden_v53_v57_roles.rs`: `man490_clean_copy_after_embedded_prosign_spots`
  (eight transmissions, real `SK` and `AR` glyphs at position 1, bundled
  tables, no allowlist; a second call, `DE W<SK>REB W5REB K`, spots `De`),
  `man490_recovery_counts_messages_not_words` (one transmission: 1 of 2
  repetitions; two transmissions 20.8 s apart: spot at the clean word's own
  boundary) and `man490_real_turn_ends_and_mentions_stay_excluded`
  (trailing, standalone, leading and doubled glyphs, `SK`/`AR`/`K`/`OP`
  words, 5 s silence, `N3N6I N3NXI`, a following `DE` or `5NN`, and another
  station's self-ID earlier in the turn).
- `exchange.rs`: `man490_damage_match_requires_exact_surrounding_letters`,
  `man490_recovery_is_bounded` and `man490_settled_recovery_is_stable` (the
  settled-prefix invariant on the damage path).
- `golden_man207_observations.rs`:
  `man490_recovery_observations_preserve_spots_and_provenance` (spots equal
  at no record, Summary, Records and Text, decisions on and off; role
  `self_id`, frame `Cq`, basis `AfterDe`, context `CQ CQ DE NNXI N3NXI`;
  never role-skipped after the window moves on) and
  `man490_refinement_clears_the_damaged_reading_s_glyph_position`.
- `golden_v8w.rs`: `man490_v8w_recovers_n3nxi_without_bogus` (N3NXI and
  VE3QHG spot, at least 29/50 genuine, 0 bogus). It shares the bogus gate's
  default decode, so the binary decodes the scene no more often than before.

No existing test changed: no old assertion pinned the behaviour this ticket
repairs (MAN-201 D12).

## Accepted costs and open claims

- One damaged word, one glyph, one character. A glyph that stands for
  several garbled characters, two damaged copies in a row, or damage without
  a glyph (`NIMN=I`, `R3NXI`) is not recognized.
- A plausible near miss (`N3N6I N3NXI`) is another station's self-ID by rule
  8, and the clean call stays a mention. On V8w, track 19's later loops are
  this shape; adjacency alone cannot tell a near miss from a different
  station. W5REB's damage (`AT HREB`, `W5RED W5REH`, `W5EIEB`) has no glyph
  and fragments across words, so W5REB stays lost.
- The research counted 14 role-settling losses in this realization; this
  change recovers one. The other 13 need near-miss or multi-word reasoning
  this record does not take on.
- A call-shaped, implausible word with one embedded `AR`/`SK` no longer
  ends its turn. A real `AR`/`SK` sent run-on into such a word (`73<SK>EE`)
  is now read as damage, and the next word joins the turn: the episode
  number does not advance there, and a mention can reach the next word. A
  stripped keyword (`D<SK>E`, `C<AR>Q`, `5N<SK>N`) is not call-shaped and
  keeps its turn end, so it frames nothing. A 5 s silence or a turn end word
  still ends the turn.
- Refinement clears the glyph position but does not restore the turn end:
  a re-decode that reads characters where the glyph was confirms the glyph
  was a garbled character.
- Synthetic precision is not real-traffic role accuracy: no live-SDR or
  RBN-replay measurement was made, and MAN-200's default-on gate is not
  touched.
