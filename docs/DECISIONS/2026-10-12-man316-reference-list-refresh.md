# MAN-316: scheduled refresh of cty.dat and MASTER.SCP

manta builds `cty.dat` (country prefixes: a call whose prefix it lacks is
not spotted) and `MASTER.SCP` (known calls: a listed call gains confidence)
into the binary. MAN-79 let an operator replace either with a local file,
but only by downloading it by hand and restarting. The bundled `cty.dat` is
from 2026-07-25; the copy upstream on 2026-10-12 (Last-Modified 2026-09-15)
lists 7,497 prefixes against the bundled 7,143, so a call from any of the
354 prefixes in between is rejected until the next release.

With `[spot].refresh_lists = true`, `manta run` from a live receiver now
checks `cty.dat` every 7 days and `MASTER.SCP` every day, swaps a download
that passes the acceptance checks into every consumer without a restart,
keeps the list in use on any failure, and stores accepted versions under
`[spot].list_cache_dir`. File replay never downloads. `--cty-sha256` and
`--scp-sha256` (or `[spot].cty_sha256`/`scp_sha256`) pin an exact version by
its SHA-256 for a repeatable replay.

## Decisions

1. **D1. Opt-in, default off.** manta made no outbound connection the
   operator did not configure. Turning one on by default, to two
   third-party volunteer hosts, is a product and privacy change, and
   packaged installs had no writable directory before this change. Flipping
   the default later is a one-line change plus its documentation rows.
2. **D2. Only `run` with a live source refreshes.** The switch is
   `LiveSourceSpec::is_reconnectable()`, the one MAN-73 already uses to keep
   file replay byte-identical. `run` refreshes with or without `[server]`;
   without it the metrics go nowhere and the log lines remain. `soak`,
   `doctor`, `check`, `calibrate`, `decode` and `oracle` never refresh:
   `soak`'s spot log must not depend on download timing.
3. **D3. Per-list startup precedence.**
   1. a pin (`--cty-sha256` > `MANTA_SPOT_CTY_SHA256` > `cty_sha256`);
   2. a path (MAN-79's precedence);
   3. the cache, read only when refresh is on, `list_cache_dir` is set, the
      cached version's bytes match its name and pass the acceptance checks,
      and its `downloaded_at` is later than the bundled copy's retrieval
      date (`vintage::CTY_DAT_RETRIEVED`, and the new
      `vintage::MASTER_SCP_RETRIEVED`);
   4. the bundled copy.

   A list chosen by 1 or 2 is never refreshed: its version is the
   operator's choice. `run` says so with `note: cty_path is set, so list
   refresh leaves cty.dat alone` (and likewise for each key and list).
4. **D4. File replay never reads the cache's `state.json`.** It reads the
   cache directory only to resolve an explicit pin, so "replay uses lists
   pinned by hash, never whatever was fetched last" holds by construction.
   With `refresh_lists = true`, file replay prints `note: list refresh is
   off for file replay, so the run is repeatable; using cty … and scp …;
   pin a downloaded copy with --cty-sha256 or --scp-sha256` and makes no
   connection.
5. **D5. Acceptance checks on every download and every startup cache
   load.** Floors are relative to the bundled copy, so refreshing the
   bundled data never needs them retuned.
   - `cty.dat`: UTF-8; the trimmed text ends with `;`; at least 90% of the
     bundled table's prefixes; all eight canary calls allocated (K1ABC,
     VE3ABC, G4ABC, UA3ABC, JA1ABC, VK2ABC, PY1ABC, ZS6ABC).
   - `MASTER.SCP`: UTF-8; at least half the bundled set's calls; every
     member line is `[A-Z0-9/]+` after uppercasing.

   Explicit `--cty`/`--scp` files keep MAN-79's rule (non-empty only), and
   pins check only the hash: the operator chose those exact bytes.
6. **D6. One multi-reader cell, `manta_spot::reference::Live<T>`.** A
   `RwLock<Arc<T>>` plus an `AtomicU64` generation;
   `ReferenceLists { cty, scp }` is shared as an `Arc` by all three
   consumers: the validator (generation compared once per decoder event in
   `emit()`, beside MAN-78's operator-list apply point, then
   `Validator::replace_reference_tables`), the daemon's per-spot geography
   check, and each JSON client's render. `ListenObservers::reference_lists
   = None`, which every replay, bench and `soak` passes, skips the check
   entirely.
7. **D7. A blocking HTTP/1.1 client on a `manta-lists` std thread.** No
   tokio runtime, so it works without `[server]`, and no new dependency.
   Connect timeout 10 s per resolved address; 60 s deadline per fetch;
   headers at most 16 KiB; body at most 8 MiB and equal to
   `Content-Length` when one is sent. It sends `User-Agent: manta/<version>`,
   `Accept-Encoding: identity`, `Connection: close`, and `If-Modified-Since`
   when the cached state's URL is the configured one. Redirects, chunked
   transfer and any content encoding are counted failures whose reason
   names the fix. The thread sleeps in slices of at most 1 s, checking the
   shared stop flag, and is not joined at shutdown: a fetch in flight is
   bounded by its deadline and every disk write is an atomic rename.
8. **D8. A persisted schedule.** A list is due when it has no state, when
   `now >= checked_at + interval` (7 days for `cty.dat`, 1 day for
   `MASTER.SCP`), or when `checked_at` is in the future (the clock moved
   back). After a failure the next try is 1 hour later. Failures are not
   persisted, so a restart retries at once, at most once per restart. The
   intervals and the retry are constants, as MAN-79's 180-day staleness
   threshold is.
9. **D9. `sha2` moves from `[dev-dependencies]` to `[dependencies]` in
   `manta-cli` only.** It was already in `Cargo.lock`, so no crate is
   added. `manta-spot` stays free of hashing and I/O. A hash is taken over
   the exact downloaded bytes, before BOM stripping, so it names a file a
   person can check with `sha256sum`.
10. **D10. Code placement**, one file per mechanism as `clock_check.rs`,
    `reload.rs` and `reconnect.rs` are: `manta-spot/src/reference.rs` (the
    cell and the acceptance checks), `manta-engine/src/listen.rs` (the
    apply point), `manta-server/src/{metrics,json_stream}.rs` (the metric
    families and the per-client read), and in `manta-cli`
    `list_http.rs` (the GET), `list_cache.rs` (the store),
    `list_refresh.rs` (the schedule, thread, startup resolution and log
    lines) plus the wiring in `config.rs`, `config_cmd.rs` and `main.rs`.

### Operator surface

- Six `[spot]` keys: `refresh_lists` (default `false`), `list_cache_dir`
  (no default; relative to the config file's directory, or to the working
  directory from `MANTA_SPOT_LIST_CACHE_DIR`), `cty_url` (default
  `http://www.country-files.com/cty/cty.dat`), `scp_url` (default
  `http://www.supercheckpartial.com/MASTER.SCP`), `cty_sha256` and
  `scp_sha256` (64 hex digits, no default). Every key is restart-only;
  SIGHUP behaviour is unchanged and the existing restart-only warning
  reports a changed key. See [CONFIG.md](../CONFIG.md#spot).
- `https://` URLs are refused at load: `[spot]: cty_url must be an http://
  URL (manta has no TLS support), got "…"`. A malformed pin is refused the
  same way.
- The cache holds immutable, content-addressed version files
  (`cty/<sha256>.dat`, `scp/<sha256>.scp`) and one `state.json` per list
  naming the current version, its URL, `downloaded_at`, `checked_at` and
  `Last-Modified`. Every write goes to a unique temporary name and is
  renamed into place, so a crash, or two daemons sharing the directory,
  never leaves a half-written file. The 30 newest versions per list are
  kept; other files are never touched.
- Three metric families, documented in [METRICS.md](../METRICS.md):
  `manta_list_info{list,sha256,origin}`, `manta_list_refresh_total{list,outcome}`
  and `manta_list_last_success_timestamp_seconds{list}`.
- Packaging: the systemd unit gains `CacheDirectory=manta` and
  `Environment=MANTA_SPOT_LIST_CACHE_DIR=/var/cache/manta`; the Compose
  file mounts a `manta-lists` named volume at `/var/cache/manta` and sets
  the same variable; the Dockerfile creates `/var/cache/manta` owned by
  `manta` so the volume inherits that owner. Refresh stays off until the
  operator enables it. See `packaging/README.md`, "Keeping cty.dat and
  MASTER.SCP current".

## Threat note

Downloads are plain HTTP. An on-path attacker can substitute a list that
passes the acceptance checks. The impact is bounded to what the lists
decide: whether a prefix counts as allocated (so a call is spotted or not)
and the confidence boost from SCP membership. A list cannot inject spots
that no signal produced, and an SCP entry alone never admits a call. This
matches the MAN-23 threat model's accepted risk for unauthenticated
transport. The mitigations are a hash pin (`cty_sha256`/`scp_sha256`),
which fixes the bytes in use and disables refresh for that list, and a
local mirror reached over a trusted network through `cty_url`/`scp_url`.
HTTPS needs manta's first TLS dependency and is a follow-up.

## Evidence

Measured in the implementation container on 2026-10-12 against `f6a41cc`:

- `curl --http1.1` against both default URLs returned `HTTP/1.1 200 OK`
  with `Content-Length` and `Last-Modified`, no redirect and no chunked
  encoding. Repeating each with `If-Modified-Since: <that Last-Modified>`
  returned `304` with an empty body.
  - `cty.dat`: 106,119 bytes, Last-Modified `Tue, 15 Sep 2026 20:52:28
    GMT`, SHA-256
    `e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1`,
    7,497 prefixes.
  - `MASTER.SCP`: 359,684 bytes, `# Release 2026.10.09`, SHA-256
    `00692208fa86b1498dcd6fc0f88e14d2a1a0462ba5d481855da5de69be4e3cf0`
    (equal to the server's `ETag`), 50,020 calls, none outside
    `[A-Z0-9/]+`.
  - Bundled: `data/cty.dat` SHA-256
    `17757121f8c63b7b68a8773379a3a3bf232e9b732730b626c982aa9ed919d763`,
    7,143 prefixes; `data/master.scp` SHA-256
    `75aaca91f62bd010b32d3e98193e9abe2330a640d9edf9a0a58b4d628ef1aa6c`,
    50,000 calls.
- An Apache 404 page parses to 0 `cty.dat` prefixes (MAN-79's empty check
  catches it) but to 4 SCP "calls": `scp::Set::parse` alone accepts junk,
  so the SCP check tests line shape, not just emptiness.
- A `cty.dat` cut at 25/50/75/90/99% of its bytes parses to
  22.8/47.5/74.9/92.1/99.6% of the bundled prefixes, with 1/3/4/5/7 of the
  8 canaries allocated. At 99% only `ZS6ABC` is missing. A prefix-share
  floor alone would accept the 92% file; the canaries reject it, and an
  HTTP `Content-Length` mismatch catches a cut in transport.
- All eight canaries are allocated in both the bundled and the upstream
  `cty.dat`.
- A refreshed `cty.dat` may add an entity the vendored `dxcc.tsv` lacks.
  Its spots then carry the unknown-DXCC sentinel and are counted by
  `manta_spots_unresolved_geography_total`, MAN-79 decision 6's existing
  drift behaviour.

## Supersedes

- The closing sentence of
  [MAN-79](2026-10-10-man79-operator-cty-scp-override.md) ("No downloader,
  scheduled refresh, new dependency or bundled-data refresh is
  included"), for the downloader and the scheduled refresh. MAN-79's
  explicit-file rules, its 180-day staleness warning and its bundled data
  are unchanged.
- [MAN-78](2026-10-10-man78-live-reload.md)'s follow-up 5 ("Country/SCP
  table hot reload across validation and geography consumers"), for the
  scheduled case: a refresh swaps both tables into all three consumers.
  SIGHUP still does not reload `cty_path` or `scp_path`.

## Not done here

- The calls-heard list (cqdx) and the RBN beacon list named in the
  ticket: no manta code reads either list, so refreshing them would change
  no behaviour. The refresher's list registry is the extension point.
- HTTPS, redirect following and chunked responses.
- Configurable intervals.
- Refresh from `soak`, `doctor`, `check`, `calibrate`, `decode` or
  `oracle`, and SIGHUP-triggered refresh.
- A `manta doctor` reachability check for the list hosts.
- A launchd cache directory: macOS service users set `list_cache_dir`.
- Refreshing `dxcc.tsv`.
- Any change to the JSON spot schema or the `decode --json` report: list
  hashes go to stderr and `/metrics` only.

## Follow-ups

1. Calls-heard list from cqdx. It needs a cqdx export endpoint, a dispensa
   contract and a manta consumer design that addresses the
   bogus-spot-amplifier risk.
2. RBN beacon list (`bcn.csv`) as a cross-check for `SpotType::Beacon`.
3. HTTPS downloads (manta's first TLS dependency).
4. A `manta doctor` check that the list URLs are reachable.
5. A launchd cache directory (`packaging/launchd`).
6. Refreshing `dxcc.tsv` alongside `cty.dat`, so a newly added entity gets
   its ADIF number.
7. Following redirects and accepting chunked responses from mirrors, if
   operators ask for them.
