//! `SpotMessage` -- the JSON Lines wire shape for manta's `:7301` stream.
//! Field names/types mirror dispensa's `contracts/spots/spots.v1.schema.json`
//! (ADR-0011): required fields manta cannot resolve from data it actually
//! has (`dxDxcc`/`deDxcc` -- an ADIF DXCC entity-number table isn't
//! vendored here) are serialized as JSON `null` rather than a fabricated
//! placeholder value. `dxContinent`/`dxCqZone`/`dxLat`/`dxLon` (and the
//! `de*` counterparts) ARE resolved, from the same vendored `cty.dat` the
//! validator already trusts for the plausibility gate (`manta_spot::cty`).

use manta_spot::cty;
use manta_spot::Spot;
use serde::Serialize;

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
    pub dx_dxcc: Option<i64>,
    pub dx_continent: String,
    pub dx_cq_zone: u16,
    pub de_call: String,
    pub de_grid: Option<String>,
    pub de_lat: Option<f64>,
    pub de_lon: Option<f64>,
    pub de_dxcc: Option<i64>,
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
        // Falls back to empty-string/zero when `dx_call` isn't
        // cty-allocated. Reachable in practice, not just a defensive
        // fallback: MAN-28's Watch List lets an operator allowlist a call
        // that bypasses `cty.is_allocated()` entirely (e.g. a deliberately
        // unallocated or malformed test callsign), so `Validator` can emit
        // a spot for a callsign `cty.lookup` genuinely can't resolve.
        // dxContinent/dxCqZone (and their de* counterparts) are REQUIRED,
        // non-nullable fields on dispensa's spots.v1 wire contract, unlike
        // dxDxcc/deDxcc (declared nullable there) -- there is currently no
        // contract-defined "unknown" representation for these two fields,
        // so this fallback stays a real (if honestly imperfect) value
        // rather than null, which the contract would reject outright. See
        // the follow-up ticket linked from this PR's round-6 review thread
        // for the cross-repo contract question this raises.
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
            dx_dxcc: None,
            dx_continent: dx.map(|e| e.continent.clone()).unwrap_or_default(),
            dx_cq_zone: dx.map(|e| e.cq_zone).unwrap_or(0),
            de_call: station_call.to_string(),
            de_grid: None,
            de_lat: de.map(|e| e.lat),
            de_lon: de.map(|e| e.lon),
            de_dxcc: None,
            de_continent: de.map(|e| e.continent.clone()).unwrap_or_default(),
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

    #[test]
    fn dxcc_entity_numbers_are_null_not_fabricated() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(
            &sample_spot(),
            "W3XYZ",
            &cty,
            "manta-0.1.0",
            0,
            1_699_999_000,
        );

        assert_eq!(msg.dx_dxcc, None);
        assert_eq!(msg.de_dxcc, None);
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
        assert_eq!(json["dxDxcc"], serde_json::Value::Null);
    }
}
