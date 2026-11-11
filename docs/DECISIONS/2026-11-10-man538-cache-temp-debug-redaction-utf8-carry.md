# MAN-538: list-cache temp files are exclusive, source Debug hides the Kiwi password, and a character split across reads is accepted

The 2026-11-07 security review found three low-exploitability hygiene gaps.
All three reproduced on `f431f9a`, and all three are fixed here:

1. `list_cache::write_atomic` opened a predictable temp name with
   `File::create`, which follows a symlink.
2. `SourceFromFile` derived `Debug`, so `{:?}` of a parsed Kiwi input printed
   its `password`. `Loaded.raw`, the whole document after the `MANTA_*`
   overlay, printed every secret in the file, including the QRZ password
   that `CallsignLookupConfig`'s own `Debug` already redacts (MAN-318).
3. `bounded_io` validated UTF-8 one `fill_buf` chunk at a time, so a valid
   multi-byte character split across two reads dropped the connection with
   `line contains invalid UTF-8`.

No command, flag, output line, config key, file format, API field or log
message changes.

## Reproductions on `f431f9a`

- **Gap 1.** With symlinks planted at `.<sha>.dat.tmp.<pid>.0` (pointing at
  `victim.txt`, which held `PRECIOUS`) and `.state.json.tmp.<pid>.1`
  (dangling), `Cache::store` returned `Ok(())`. `victim.txt` then held
  `version a;\n`, the dangling target had been created, and the final
  version path was itself a symlink.
- **Gap 2.** `{:?}` of `loaded.input.source`, `InputFile`, `NamedInput`
  (multi-input) and `Loaded` (single, multi, and with the password from
  `MANTA_INPUT_PASSWORD`) all contained the password. `{:?}` of `Loaded`
  contained the QRZ password through `raw`, while `CallsignLookupConfig`
  alone did not. The pretty form read
  `Some(Kiwi { host: "h", port: 8073, freq_hz: 7030000.0, password: "hunter2-secret" })`.
- **Gap 3.** A `€` whose first two bytes end the buffer was rejected with
  `Err("line contains invalid UTF-8")` by both the plain and the telnet
  reader at buffer capacities 4, 16, 8192 and 16384. A `duplex` pipe with
  `caf E2 82` and `AC \n` in two writes failed the same way.

## D1: `create_new`, a random suffix and 16 tries

`write_atomic` now opens `.<name>.tmp.<pid>.<16 hex digits>` with
`OpenOptions::create_new` (`O_EXCL`), the pattern `config_edit.rs` already
uses for the config editor's save. `create_new` refuses any existing entry,
including a live or dangling symlink, with `AlreadyExists`. On that error the
write tries a new name, up to 16 times (`TEMP_ATTEMPTS`), and then fails
with `AlreadyExists`. An entry it did not create is never removed. The
suffix comes from `config_edit::random_suffix`, std's randomly keyed
SipHash, now `pub(crate)`.

The random suffix is required, not decoration. With `create_new` and the old
`pid.counter` name, a killed daemon's leftover `.state.json.tmp.1.0` (PID 1
in a container, the counter restarting at 0) would fail every later write,
because nothing removes leftovers (`prune` skips them). This is the same
reason `config_edit` moved to random suffixes (MAN-335).

A failed cache write was already a logged warning with the download still in
use (`list_refresh.rs`), so failing closed is graceful.

Leftover temp files from older builds keep their old names. The new names
cannot collide with them, because they always carry 16 hex digits.

## D2: no forced 0600 mode on cache files

The ticket names 0600 as part of the `config_edit` pattern. The cache keeps
the umask default instead. It holds public lists (`cty.dat`, `MASTER.SCP`,
the pattern list), and `rename` keeps the temp file's mode, so 0600 would
become the permanent mode of every cache file. That breaks a documented
use: `packaging/README.md` ("Where downloads are kept") has an operator
run `decode`/`doctor --cty-sha256` with a config whose `list_cache_dir`
holds the daemon's version, and on macOS the daemon runs as its own service
account. The Gherkin scenario asks for no mode. A later decision that wants
0600 adds one `mode(0o600)` call (`std::os::unix::fs::OpenOptionsExt`) in
`write_atomic_with` and a `docs/CONFIG.md` note.

This record does not add integrity protection for `state.json` beyond the
SHA-256 file names. Anyone who can write `list_cache_dir` can still
substitute a list. This ticket only removes the arbitrary-file overwrite.

## D3: hand-written `Debug` on `SourceFromFile` and `Loaded`

- `SourceFromFile` has a hand-written `Debug` that prints exactly what the
  derive printed, except that the Kiwi `password` reads `<redacted>`.
  `InputFile` and `NamedInput` keep their derives and inherit it.
- `Loaded` has a hand-written `Debug` that prints every field except `raw`,
  which reads `<redacted>`. `present` already lists the top-level table
  names. `Loaded` must keep a `Debug`, because tests call `unwrap_err()` on
  `Result<Loaded>`.
- Both impls destructure without `..`, so a new field fails to compile until
  someone lists it, and so decides whether it is a secret.
- `InputToml` lost its `Debug` derive. It is private, holds the Kiwi
  `password`, and nothing formatted it.
- The password always reads `<redacted>`, even when empty, as the Gherkin
  scenario asks. That differs on purpose from MAN-318's `"none"`/`"set"` on
  `CallsignLookupConfig`, which stays as it is.
- No `Secret` newtype on `password`. It would change every constructor and
  comparison of `SourceFromFile::Kiwi` for no gain over the hand-written
  `Debug` the codebase already uses.

Residual: a direct `{:?}` of the bare `toml::Table` (`loaded.raw`,
`ReloadContext.startup_raw`) still prints values. Nothing does that today,
and wrapping it would ripple into `reload.rs`, `run_cmd.rs` and
`config_cmd.rs`. The ticket's scenario is `{:?}` of the parsed source config,
which this record covers.

## D4: `Utf8Tail`, an explicit caller-owned carry

UTF-8 is now validated over the line, not per chunk. A chunk that ends
inside a character keeps its at most three bytes in a `Utf8Tail`.
`read_line_bounded` and `read_line_bounded_with_timeout` take it as a new
parameter beside `buf`. `read_line_bounded_telnet` keeps it in `IacFilter`
(`IacFilter::utf8`, cleared by `reset_line`), where the telnet reader
already keeps its IAC parse state, so `telnet.rs` does not change.

- **Why a parameter, not a hidden local.** A carry held inside the read
  future is lost when `tokio::select!` drops that future mid-line. Owned by
  the caller beside `buf`, it survives a cancelled read exactly as `buf`
  does, and the next call on the same `buf` resumes from it.
- **Why a lost carry can only fail closed.** If a carry were ever lost, the
  next chunk would start with a continuation byte, which is never valid
  UTF-8 on its own. The worst case is the old rejection, never wrong text.
- **When the carry is kept.** Only when `Utf8Error::error_len()` is `None`,
  which means the bytes ended inside a character more bytes could complete.
  std rejects impossible prefixes (`E0 80`, `ED A0`, `F4 90`) with
  `Some(1)`, so the carry never holds more than three bytes.
- **Threat-model finding 19 still holds**
  (`docs/DECISIONS/2026-09-02-man23-threat-model.md`). Invalid bytes, a
  character cut by the newline, and EOF inside a character all still return
  `InvalidData` with `line contains invalid UTF-8`, and the carry is empty
  afterwards.
- **Length cap.** The plain reader starts its byte count at
  `buf.len() + tail.len()`. The telnet reader counted carried bytes in
  `filter.raw_line_bytes` when they arrived. A 1024-byte line with a
  straddling character is accepted, and 1025 bytes are rejected.
- **CR NUL.** The telnet reader's `CR NUL` line end looks at the byte before
  the NUL, which is now the pending carry when there is one, so `CR`,
  a pending lead byte, then `NUL` does not end a line.

All eight plain-reader call sites (`uplink.rs`, `metrics_http.rs`,
`relay/skimmer.rs`, `rbn_compare/feed.rs`, `manta-cli`'s `daemon_client.rs`)
declare the tail beside the buffer it belongs to. `manta-server` is
`publish = false`, so the signature change affects only this workspace.

This supersedes the "Known limitation (PR #80 review, round 3, P3)" comment
in `bounded_io.rs` (MAN-58) and the per-chunk validation note in the MAN-87
record (`docs/DECISIONS/2026-09-07-man87-telnet-iac-policy.md`). Those
records are not edited.

The ticket says "16 KiB". Production readers are `BufReader::new`, 8 KiB in
tokio 1.54.1, and any chunk boundary triggers the bug, including a TCP
segment split. The tests cover capacities 4, 16, 8192 and 16384, and a split
across two TCP writes to the telnet server.

## Tests

- `crates/manta-cli/src/list_cache.rs`:
  `store_ignores_symlinks_at_the_old_predictable_names`,
  `a_guessed_temp_name_is_skipped_never_followed`,
  `a_write_fails_closed_when_every_temp_name_is_taken` (portable; its
  symlink half is Unix-only).
- `crates/manta-cli/src/config.rs`:
  `debug_of_a_kiwi_source_redacts_the_password`,
  `debug_of_loaded_hides_every_secret_in_raw`,
  `debug_of_other_sources_matches_the_derive`.
- `crates/manta-server/src/bounded_io.rs`:
  `a_character_split_across_chunks_is_accepted`,
  `a_split_character_survives_a_cancelled_read`,
  `a_split_character_survives_a_cancelled_telnet_read`,
  `invalid_utf8_is_still_rejected_at_any_boundary`,
  `the_line_cap_counts_carried_bytes`,
  `cr_then_a_pending_lead_byte_then_nul_is_not_a_line_end`.
- `crates/manta-server/tests/telnet_acceptance.rs`:
  `a_command_with_a_character_split_across_tcp_writes_is_read_whole`.

## References

- `docs/DECISIONS/2026-10-12-man316-reference-list-refresh.md` (the list
  cache)
- `docs/DECISIONS/2026-10-27-man318-callsign-lookup.md` (the redacting
  `Debug` precedent)
- `docs/DECISIONS/2026-09-07-man87-telnet-iac-policy.md` (`IacFilter`)
- `docs/DECISIONS/2026-09-02-man23-threat-model.md` (finding 19)
