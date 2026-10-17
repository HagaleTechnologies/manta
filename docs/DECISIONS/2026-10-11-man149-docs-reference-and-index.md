# MAN-149: config, metrics and spot-schema references, and a docs index

Operators had no single place to look up a config key's default or whether
the key did anything yet, 15 of the 40 `/metrics` families appeared in no
operator document, and the JSON spot stream's fields were described only by
a link to a contract outside readers cannot open. MAN-149 adds three
reference documents and two indexes, and tests that fail when one of them
falls behind the code.

| Document | Answers |
|---|---|
| `docs/CONFIG.md` | Every TOML key: its default, and which commands and engines read it. |
| `docs/METRICS.md` | Every `/metrics` family, plus `/healthz` and `/status`. |
| `docs/SPOT-SCHEMA.md` | Every field of the `:7301` JSON Lines / WebSocket spot message. |
| `docs/README.md` | Where operators, contributors and researchers each start; every runbook. |
| `docs/DECISIONS/README.md` | One row per decision record. |

## Decisions

1. **Spot fields are documented in-repo, not only linked.** dispensa's
   `contracts/spots/spots.v1.schema.json` (ADR-0011) stays the cross-repo
   contract, but dispensa is private, so a link alone sends an outside
   developer to a page they cannot open. `SPOT-SCHEMA.md` lists every field
   manta emits and names the one divergence from the contract (`snrRefHz`).
   The unit test `every_wire_field_is_documented` in
   `crates/manta-server/src/spot_message.rs` pins its field table to
   `SpotMessage`'s serialized keys.
2. **`CONFIG.md` is the operator reference; SPEC §9 stays normative** for
   the decode-core defaults. `CONFIG.md` links SPEC §9 and SPEC-v2 §7 for
   meanings rather than restating them. Its Default column is pinned to the
   `manta config init` scaffold, which the scaffold drift tests in
   `config_cmd.rs` already pin to the code, so the defaults are enforced
   without a third copy of them in Rust.
3. **Wiring status comes from reading the code.** A black-box probe on V1
   can show that a key has an effect; it cannot show that a key has none.
   Each non-plain Wired cell was checked against the code that reads the
   key. The audit found no key that is accepted but unused everywhere.
4. **The scaffold's wrong claims are fixed; nothing else in it changes.**
   `refine_bw_hz` was documented as "reserved, no effect yet" in the
   scaffold, `manta.example.toml` and two doc comments in `manta-decode`,
   but `manta-engine`'s `TrackManager` has applied it under every engine
   since MAN-168/MAN-194 (`oracle` excepted). The two `[decode]` group
   headers now say which engines their keys affect. Keys, values and table
   order are unchanged. `SPEC-decode-core-v2.md` §2.3 gains a dated update:
   `edge-legacy` and `hsmm` now feed the spectral noise reference.
5. **The drift tests live where the repo already keeps doc tests.**
   `crates/manta-cli/tests/docs_consistency.rs` checks that every scaffold
   key has a `CONFIG.md` row with the scaffold's default, every constant-only
   key is listed, every rendered metric family has a `METRICS.md` row of the
   right type, every runbook is linked from `docs/README.md`, every decision
   record is linked from `docs/DECISIONS/README.md`, each audience has a
   resolving `Start here:` link, and the new documents' relative links
   resolve. Each failure names the missing item and the file to edit, so a
   later PR that adds a key, metric, wire field, runbook or decision record
   adds its row in the same change.
6. **The CLI `--json` debug format stays undocumented** beyond a "not this
   schema, not stable" note. Documenting it would imply a stability promise
   nobody has made.

Out of scope: a published docs site, folding `snrRefHz` into the dispensa
contract (cross-repo), and changing any key, default, metric or wire field.
