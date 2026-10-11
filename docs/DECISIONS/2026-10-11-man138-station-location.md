# MAN-138: a JSON spot's `deGrid`/`deLat`/`deLon` are the configured station position

Every JSON spot on `:7301` names the receiving station's position. Before
this change `deLat`/`deLon` came from a `cty.dat` lookup of the station's
own callsign, which returns its DXCC country's centroid, and `deGrid` was
always `null`. Every US station reported `37.6, -91.87`, so a map stacked
every US spotter on one point. The operator's configured location is now
the only source of all three fields.

Reproduced on `9644a7f` before the change: a config with
`operator_grid = "FN31pr"` (W1AW) and one with `operator_grid = "EM12kp"`
(W5AU), loaded through `config::load` and served by `start_spot_server`,
both emitted `"deGrid":null,"deLat":37.6,"deLon":-91.87`.

## Decisions

1. **Source.** `deGrid`/`deLat`/`deLon` come only from `[server]`
   `operator_grid`/`operator_lat`/`operator_lon`, never from `cty.dat`.
   For the de side only, this supersedes the statements in
   `2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md` and
   `2026-09-07-man-89-station-callsign-ssid-grammar.md` (decision 5) that
   `deLat`/`deLon` come from the `cty.dat` lookup. That lookup still runs
   for `deDxcc`/`deContinent`.
2. **Nothing configured means `null`.** With no location key set, all
   three fields are `null`, the contract-legal "unknown" value. There is
   no centroid fallback: a fallback would keep stacking every unconfigured
   station of one country on the point above. `config check` prints a note
   and daemon start logs a warning naming the keys to set.
3. **Resolution.** `ServerConfig::station_location` resolves the keys
   once:
   - `operator_grid` only: the grid, and the centre of its square as
     `deLat`/`deLon`.
   - `operator_lat`/`operator_lon` only: the point, and the 6-character
     square that contains it as `deGrid`.
   - All three: the point must lie inside the grid's square, inclusive of
     its edges with a `1e-9` degree tolerance. The configured grid is kept,
     so a 4-character grid stays 4 characters.
   - Half a lat/lon pair is an error.
4. **Spelling.** `deGrid` is the canonical form: field and square
   uppercase, subsquare lowercase (`FN31pr`, `FN31`), whatever case the
   operator typed. Two nodes in one square send the same string.
5. **Validation.** Range and finiteness at deserialize time: latitude
   -90 to 90, longitude -180 to 180, and `nan`/`inf` rejected. The pair
   rule and the inside-the-grid rule run in `config::load` before any
   source or listener opens, so `run`, `soak`, `doctor` and `config check`
   all reject them. `start_spot_server` resolves again, for a caller that
   builds a `ServerConfig` directly. Both keys are numbers, so a
   `MANTA_SERVER_OPERATOR_LAT`/`_LON` value is TOML-probed and they are
   not string-typed environment keys.
6. **One locator parser.** `manta_server::maidenhead` converts between
   locators and coordinates. `config::check_grid` validates through the
   same parser, so the validator and the converter cannot disagree.
7. **Out of scope.** `dxLat`/`dxLon` stay `cty.dat` centroids and `dxGrid`
   stays `null` (the second half of broad-review item L2#15 / C-03 needs
   its own ticket and a consumer decision). `deDxcc`/`deContinent`, the
   telnet greeting (it still prints `operator_grid` verbatim, and a
   lat/lon-only config does not add a derived grid to it), `SKIMMER/SETT`,
   the RBN uplink and decoder output are unchanged. No CLI flags: no
   `[server]` key has one.
8. **dispensa.** Types and nullability are unchanged, so the schema needs
   no change. A field description ready to lift into ADR-0011:
   "`deGrid`/`deLat`/`deLon`: the reporting station's operator-configured
   position; `null` when the operator configured none; `deLat`/`deLon` are
   a grid-square centre when only `deGrid` was configured."

## Evidence and tests

- `a_configured_station_grid_reaches_json_spots_as_the_receiver_position`
  (`crates/manta-cli/src/main.rs`) is the ticket's scenario end to end: a
  real config file through `config::load` and `start_spot_server`, a TCP
  client on the JSON port, and two US stations whose positions differ and
  are neither `37.6, -91.87`.
- `a_station_without_a_configured_location_sends_null_position_not_a_country_centroid`
  and `tcp_client_receives_spot_as_json_lines_message` pin the `null` case.
- `de_position_is_the_configured_station_location_not_a_country_centroid`
  and `dx_geography_is_unchanged_by_a_configured_station_location`
  (`spot_message.rs`) pin the message-level split between the two sides.
- The `station_location_*` and `operator_lat_and_lon_*` tests in
  `crates/manta-server/src/config.rs`, and
  `an_inconsistent_station_location_is_rejected_before_io` in
  `crates/manta-cli/src/config.rs`, pin the rules in decisions 3 and 5.
- `maidenhead::tests` pin the arithmetic, including the pole and
  antimeridian clamp: `locator(90, 180)` is `RR99xx`, not `RR00aa`.
