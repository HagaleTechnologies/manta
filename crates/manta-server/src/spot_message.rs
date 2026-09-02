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
    pub fn from_spot(
        spot: &Spot,
        station_call: &str,
        cty: &cty::Table,
        decoder_version: &str,
        unix_ts_secs: i64,
    ) -> Self {
        // Falls back to empty/zero only if `dx_call` isn't cty-allocated --
        // should be unreachable in practice, since `Validator` only emits
        // spots for callsigns that already passed `cty.is_allocated()`.
        let dx = cty.lookup(&spot.callsign);
        let de = cty.lookup(station_call);

        Self {
            id: format!("{}:{}", spot.track_id, spot.sample_ts),
            source: "skimmer",
            timestamp: unix_ts_secs,
            // cqdx overwrites this on receipt; see the field's schema doc.
            ingested_at: unix_ts_secs,
            frequency: spot.freq_hz.round() as i64,
            band: crate::band::band_for_freq_hz(spot.freq_hz).to_string(),
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
    fn populates_required_fields_from_the_spot() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg =
            SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 1_700_000_000);

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
        let msg = SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 0);

        assert_eq!(msg.dx_continent, "AS");
        assert_eq!(msg.dx_cq_zone, 25);
        assert_eq!(msg.de_continent, "NA");
    }

    #[test]
    fn dxcc_entity_numbers_are_null_not_fabricated() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 0);

        assert_eq!(msg.dx_dxcc, None);
        assert_eq!(msg.de_dxcc, None);
    }

    #[test]
    fn optional_decoder_metadata_fields_are_populated() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 0);

        assert_eq!(msg.decode_confidence, Some(0.9));
        assert_eq!(msg.decoder_version.as_deref(), Some("manta-0.1.0"));
    }

    #[test]
    fn serializes_with_schema_camel_case_field_names() {
        let cty = cty::Table::parse(CTY_FIXTURE);
        let msg = SpotMessage::from_spot(&sample_spot(), "W3XYZ", &cty, "manta-0.1.0", 0);
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
