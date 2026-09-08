//! `SpotMessage` -- the JSON Lines wire shape for manta's `:7301` stream.
//! Field names/types mirror dispensa's `contracts/spots/spots.v1.schema.json`
//! (ADR-0011). `dxDxcc`/`deDxcc`/`dxContinent`/`deContinent`/`dxCqZone` are
//! ALL required and non-nullable on that contract -- see
//! docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md.
//! `dxDxcc`/`deDxcc` are resolved from the vendored `dxcc.tsv` ADIF entity-
//! number table (MAN-136); `dxContinent`/`dxCqZone`/`dxLat`/`dxLon` (and the
//! `de*` counterparts) are resolved from the same vendored `cty.dat` the
//! validator already trusts for the plausibility gate (`manta_spot::cty`).
//! When a callsign isn't cty-resolvable, each required field gets a named
//! out-of-domain `UNKNOWN_*` sentinel below rather than a fabricated-looking
//! real value or (where the contract forbids it) `null`.

use manta_spot::cty;
use manta_spot::Spot;
use serde::Serialize;

/// Emitted for `dxDxcc`/`deDxcc` when `cty.lookup` cannot resolve the
/// callsign. Deliberately NEGATIVE: ADIF entity codes run 1-522, and ADIF
/// code 0 already has a specific different meaning -- "None: the contacted
/// station is known to NOT be within a DXCC entity" -- which would be a false
/// positive claim about a call manta merely failed to resolve. dispensa's
/// spots.v1 declares this field required and non-nullable, so `null` is not
/// available. See docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md.
pub const UNKNOWN_DXCC: i64 = -1;

/// Emitted for `dxContinent`/`deContinent` when `cty.lookup` cannot resolve
/// the callsign. Outside the field's real domain -- the seven two-letter
/// continent codes -- so it reads as "unknown", never as geography.
pub const UNKNOWN_CONTINENT: &str = "";

/// Emitted for `dxCqZone` when `cty.lookup` cannot resolve the callsign.
/// Real CQ zones are 1-40, so 0 is unambiguously "unknown".
pub const UNKNOWN_CQ_ZONE: u16 = 0;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotMessage {
    pub id: String,
    pub source: &'static str,
    pub timestamp: i64,
    pub ingested_at: i64,
    pub frequency: i64,
    pub band: String,
    pub mode: &'static str,
    pub dx_call: String,
    pub dx_grid: Option<String>,
    pub dx_lat: Option<f64>,
    pub dx_lon: Option<f64>,
    pub dx_dxcc: i64,
    pub dx_continent: String,
    pub dx_cq_zone: u16,
    pub de_call: String,
    pub de_grid: Option<String>,
    pub de_lat: Option<f64>,
    pub de_lon: Option<f64>,
    pub de_dxcc: i64,
    pub de_continent: String,
    pub snr: Option<i32>,
    pub wpm: Option<i32>,
    pub decode_confidence: Option<f32>,
    pub decoder_version: Option<String>,
    pub channelizer_resolution_hz: Option<f64>,
}

impl SpotMessage {
    /// Builds the wire message for one validated spot. `unix_ts_secs` is
    /// the spot's wall-clock time (see `rbn::format_line`'s doc comment on
    /// why that conversion happens here, not on `Spot` itself).
    /// `session_nonce` is the producing bus's full-nanosecond-precision
    /// session identity (`SpotBus::session_nonce`) -- `track_id`/
    /// `sample_ts` alone are only unique within one decode session, so two
    /// manta stations (or the same station restarted, even twice within
    /// one wall-clock second) could otherwise emit colliding `id`s that a
    /// shared cqdx ingest keyed on `id` would overwrite or drop. `id` also
    /// includes the callsign itself: MAN-28's Watch List allowlist can
    /// legitimately emit more than one spot at the SAME `track_id`/
    /// `sample_ts` within one session (several allowlisted words found
    /// before a track's first `TrackMeta`, all stamped by the metadata-
    /// arrival retry with that track's saved timestamp) -- without the
    /// callsign, those would collide too.
    pub fn from_spot(
        spot: &Spot,
        station_call: &str,
        cty: &cty::Table,
        decoder_version: &str,
        unix_ts_secs: i64,
        session_nonce: u128,
    ) -> Self {
        // Falls back to the UNKNOWN_* sentinels above when the callsign
        // isn't cty-allocated. Reachable in practice, not defensive: MAN-28's
        // Watch List lets an operator allowlist a call that bypasses
        // `cty.is_allocated()` entirely (validator.rs:669-680), so
        // `Validator` can emit a spot for a callsign `cty.lookup` genuinely
        // can't resolve.
        //
        // MAN-136 / broad-review D10: dxDxcc, deDxcc, dxContinent, deContinent
        // and dxCqZone are ALL required and non-nullable on dispensa's
        // spots.v1 contract -- emitting JSON `null` for any of them fails
        // cqdx's ingest rather than satisfying it. So each has a named,
        // out-of-domain sentinel instead. `dxLat`/`dxLon` ARE nullable on the
        // contract and DO serialize as `null` here, which is the one
        // contract-defined "geography unknown" signal a consumer can key on
        // today. Occurrences are counted as
        // `manta_spots_unresolved_geography_total` (main.rs, at publish).
        let dx = cty.lookup(&spot.callsign);
        let de = cty.lookup(station_call);
        // `band` must be derived from the SAME rounded value reported as
        // `frequency` -- computing it from the unrounded `spot.freq_hz`
        // separately (round-5 review finding) could disagree with
        // `frequency` near a band edge, e.g. 13_999_999.6 Hz rounds up
        // into 20m's `frequency` while the unrounded value alone still
        // reads as 40m's `band`.
        let frequency_hz = spot.freq_hz.round();

        Self {
            id: format!(
                "{station_call}:{session_nonce}:{}:{}:{}",
                spot.track_id, spot.sample_ts, spot.callsign
            ),
            source: "skimmer",
            timestamp: unix_ts_secs,
            // cqdx overwrites this on receipt; see the field's schema doc.
            ingested_at: unix_ts_secs,
            frequency: frequency_hz as i64,
            band: crate::band::band_for_freq_hz(frequency_hz).to_string(),
            mode: "CW",
            dx_call: spot.callsign.clone(),
            dx_grid: None,
            dx_lat: dx.map(|e| e.lat),
            dx_lon: dx.map(|e| e.lon),
            dx_dxcc: dx
                .and_then(|e| e.dxcc)
                .map(i64::from)
                .unwrap_or(UNKNOWN_DXCC),
            dx_continent: dx
                .map(|e| e.continent.clone())
                .unwrap_or_else(|| UNKNOWN_CONTINENT.to_string()),
            dx_cq_zone: dx.map(|e| e.cq_zone).unwrap_or(UNKNOWN_CQ_ZONE),
            de_call: station_call.to_string(),
            de_grid: None,
            de_lat: de.map(|e| e.lat),
            de_lon: de.map(|e| e.lon),
            de_dxcc: de
                .and_then(|e| e.dxcc)
                .map(i64::from)
                .unwrap_or(UNKNOWN_DXCC),
            de_continent: de
                .map(|e| e.continent.clone())
                .unwrap_or_else(|| UNKNOWN_CONTINENT.to_string()),
            snr: Some(spot.snr_db.round() as i32),
            wpm: Some(spot.wpm.round() as i32),
            decode_confidence: Some(spot.confidence),
            decoder_version: Some(decoder_version.to_string()),
            channelizer_resolution_hz: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

    const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
Japan:            25: 45: AS:  36.0: 138.0:  9.0:  JA:
    JA,JD,JE,JF,JG,JH,JI,JJ,JK,JL,JM,JN,JO,JP,JQ,JR,JS;
";

    fn sample_spot() -> Spot {
        Spot {
            callsign: "JA1ABC".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 23.0,
            wpm: 28.0,
            spot_type: SpotType::Cq,
            confidence: 0.9,
            track_id: 7,
            sample_ts: 12_345,
        }
    }

    #[test]
    fn frequency_and_band_never_disagree_near_a_band_edge() {
        // Regression (round-5 review): `frequency` used to round
        // `spot.freq_hz` while `band` classified the UNROUNDED value
        // separately. 13_999_999.6 Hz rounds up to 14_000_000 (inside
        // 20m's lower edge), but the unrounded value alone falls in the
        // unassigned gap just below 20m -- so `band` used to read
        // "unknown" while `frequency` read exactly 14_000_000, a visibly
        // self-contradictory pair. Both fields must now agree, derived
        // from the same rounded value.
        let cty = cty::Table::parse(CTY_FIXTURE);
        let mut spot = sample_spot();
        spot.freq_hz = 13_999_999.6;
        let msg = SpotMessage::from_spot(&spot, "W3XYZ", &cty, "manta-0.1.0", 0, 0);

        assert_eq!(msg.frequency, 14_000_000);
        assert_eq!(msg.band, "20m");
    }

    #[test]
    fn populates_required_fields_from_the_spot() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(
            &sample_spot(),
            "W3XYZ",
            &cty,
            "manta-0.1.0",
            1_700_000_000,
            1_699_999_000,
        );

        assert_eq!(msg.source, "skimmer");
        assert_eq!(msg.mode, "CW");
        assert_eq!(msg.timestamp, 1_700_000_000);
        assert_eq!(msg.frequency, 14_027_100);
        assert_eq!(msg.band, "20m");
        assert_eq!(msg.dx_call, "JA1ABC");
        assert_eq!(msg.de_call, "W3XYZ");
        assert_eq!(msg.snr, Some(23));
        assert_eq!(msg.wpm, Some(28));
    }

    #[test]
    fn resolves_dx_and_de_continent_and_cq_zone_from_cty_table() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(
            &sample_spot(),
            "W3XYZ",
            &cty,
            "manta-0.1.0",
            0,
            1_699_999_000,
        );

        assert_eq!(msg.dx_continent, "AS");
        assert_eq!(msg.dx_cq_zone, 25);
        assert_eq!(msg.de_continent, "NA");
    }

    /// MAN-136 scenario 1: dispensa's spots.v1 declares dxDxcc REQUIRED and
    /// non-nullable; manta used to emit `null` for EVERY spot, including ones
    /// whose callsign cty.dat resolves fine -- cqdx's ingest would reject the
    /// batch on this field alone. CTY_FIXTURE's entities carry the real primary
    /// prefixes `JA` and `K`, so the real vendored dxcc.tsv resolves them.
    #[test]
    fn a_resolvable_callsign_carries_its_real_adif_dxcc_entity_number() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 0, 0);

        assert_eq!(msg.dx_dxcc, 339, "JA1ABC -> Japan");
        assert_eq!(msg.de_dxcc, 291, "W3XYZ -> United States");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["dxDxcc"], 339);
        assert_eq!(json["deDxcc"], 291);
        assert!(!json["dxDxcc"].is_null(), "the whole point of MAN-136");
    }

    /// MAN-136 scenario 2 (also MAN-45 finding 1): MAN-28's Watch List allowlist
    /// (validator.rs:669-680) lets an operator emit a spot for a call cty.lookup
    /// genuinely cannot resolve. All three required geography fields must then
    /// carry a named, out-of-domain sentinel -- never a fabricated-looking value,
    /// and never null.
    ///
    /// Uses the REAL vendored cty.dat, not CTY_FIXTURE: a call that is
    /// unallocated in a 2-entity fixture is usually allocated in the real file
    /// (ZZ9ZZZ -> Brazil/108 via ZZ; NOCALL -> US/291 via N). ITU allocates no Q
    /// prefixes, so QQ1AAA is genuinely unresolvable.
    #[test]
    fn an_unresolvable_callsign_emits_named_out_of_domain_sentinels() {
        let cty = cty::Table::parse(manta_spot::CTY_DAT);
        let mut spot = sample_spot();
        spot.callsign = "QQ1AAA".to_string();
        assert!(cty.lookup(&spot.callsign).is_none(), "test premise");

        let msg = SpotMessage::from_spot(&spot, "W3XYZ", &cty, "manta-0.1.0", 0, 0);

        assert_eq!(msg.dx_dxcc, UNKNOWN_DXCC);
        assert_eq!(msg.dx_continent, UNKNOWN_CONTINENT);
        assert_eq!(msg.dx_cq_zone, UNKNOWN_CQ_ZONE);
        // Each sentinel must be outside its field's real domain. UNKNOWN_DXCC's
        // own doc comment covers why it's negative and not 0 (ADIF 0 means
        // "confirmed not in any entity") -- both are constant properties, not
        // something to assert at runtime here.
        assert!(!(1..=40).contains(&msg.dx_cq_zone));
        assert!(msg.dx_continent.len() != 2);
        // The contract-legal unknown signal consumers can key on today.
        assert!(msg.dx_lat.is_none());
        assert!(msg.dx_lon.is_none());
        // The station's own call still resolves -- de geography is unaffected.
        assert_eq!(msg.de_dxcc, 291);
        assert_eq!(msg.de_continent, "NA");
    }

    /// The de side gets the same treatment: `station_callsign` is operator config
    /// and is not required to be cty-resolvable.
    #[test]
    fn an_unresolvable_station_callsign_also_gets_the_sentinels() {
        let cty = cty::Table::parse(manta_spot::CTY_DAT);
        let msg = SpotMessage::from_spot(&sample_spot(), "QQ1AAA", &cty, "manta-0.1.0", 0, 0);

        assert_eq!(msg.de_dxcc, UNKNOWN_DXCC);
        assert_eq!(msg.de_continent, UNKNOWN_CONTINENT);
        assert!(msg.de_lat.is_none());
        // dx geography is unaffected.
        assert_eq!(msg.dx_dxcc, 339);
    }

    #[test]
    fn optional_decoder_metadata_fields_are_populated() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(
            &sample_spot(),
            "W3XYZ",
            &cty,
            "manta-0.1.0",
            0,
            1_699_999_000,
        );

        assert_eq!(msg.decode_confidence, Some(0.9));
        assert_eq!(msg.decoder_version.as_deref(), Some("manta-0.1.0"));
    }

    #[test]
    fn id_differs_across_callsigns_sharing_the_same_track_and_sample() {
        // Regression (round-6 review): MAN-28's Watch List allowlist can
        // legitimately emit multiple distinct spots for the SAME
        // track_id/sample_ts within one session -- when several
        // allowlisted words accumulate before that track's first
        // TrackMeta, the metadata-arrival retry evaluates and emits all of
        // them stamped with the same saved sample_ts. Without the
        // callsign in `id`, a cqdx ingest keyed on `id` would discard all
        // but one of them.
        let cty = cty::Table::parse(CTY_FIXTURE);
        let mut spot_a = sample_spot();
        spot_a.callsign = "JA1ABC".to_string();
        let mut spot_b = sample_spot();
        spot_b.callsign = "K5ARH".to_string();
        assert_eq!(spot_a.track_id, spot_b.track_id);
        assert_eq!(spot_a.sample_ts, spot_b.sample_ts);

        let msg_a = SpotMessage::from_spot(&spot_a, "W3XYZ", &cty, "manta-0.1.0", 0, 1_000);
        let msg_b = SpotMessage::from_spot(&spot_b, "W3XYZ", &cty, "manta-0.1.0", 0, 1_000);

        assert_ne!(
            msg_a.id, msg_b.id,
            "distinct callsigns at the same track/sample must not collide"
        );
    }

    #[test]
    fn id_differs_across_stations_and_sessions_for_the_same_track_and_sample() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let spot = sample_spot();

        let station_a = SpotMessage::from_spot(&spot, "W3XYZ", &cty, "manta-0.1.0", 0, 1_000);
        let station_b = SpotMessage::from_spot(&spot, "N0CALL", &cty, "manta-0.1.0", 0, 1_000);
        let restarted = SpotMessage::from_spot(&spot, "W3XYZ", &cty, "manta-0.1.0", 0, 2_000);

        assert_ne!(
            station_a.id, station_b.id,
            "different stations must not collide"
        );
        assert_ne!(
            station_a.id, restarted.id,
            "a restart must not collide with the prior session"
        );
    }

    #[test]
    fn serializes_with_schema_camel_case_field_names() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(
            &sample_spot(),
            "W3XYZ",
            &cty,
            "manta-0.1.0",
            0,
            1_699_999_000,
        );
        let json = serde_json::to_value(&msg).unwrap();

        for key in [
            "id",
            "source",
            "timestamp",
            "ingestedAt",
            "frequency",
            "band",
            "mode",
            "dxCall",
            "dxDxcc",
            "dxContinent",
            "dxCqZone",
            "deCall",
            "deDxcc",
            "deContinent",
            "snr",
            "wpm",
            "decodeConfidence",
            "decoderVersion",
            "channelizerResolutionHz",
        ] {
            assert!(json.get(key).is_some(), "missing key: {key}");
        }
        assert_eq!(json["dxDxcc"], 339);
        assert!(!json["dxDxcc"].is_null());
    }
}
