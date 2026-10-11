//! Maidenhead locators <-> latitude/longitude (MAN-138): the arithmetic behind
//! `[server].operator_grid` / `operator_lat` / `operator_lon` as the station
//! position every JSON spot carries (`deGrid`/`deLat`/`deLon`). A locator
//! names an AREA -- 4 characters a 1 x 2 degree square, 6 characters a
//! 2.5' x 5' subsquare -- so a locator-only station reports its centre.
//! Decimal degrees, north and east positive: `manta_spot::cty::Entry`'s
//! convention.

/// Slack on `Square::contains`'s edges, so a point typed exactly on a
/// boundary is not rejected by the last bit of a float sum.
const EDGE_TOLERANCE: f64 = 1e-9;

/// The area one 4- or 6-character locator names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Square {
    pub south: f64,
    pub west: f64,
    pub height: f64,
    pub width: f64,
}

/// Parses a 4- or 6-character locator, case-insensitively; `None` for
/// anything else. `config::check_grid` validates through this, so the
/// validator and the converter cannot disagree about what a grid is.
pub fn square(locator: &str) -> Option<Square> {
    let c: Vec<char> = locator.chars().map(|c| c.to_ascii_uppercase()).collect();
    if !matches!(c.len(), 4 | 6) {
        return None;
    }
    let field_lon = index(c[0], 'A', 'R')?;
    let field_lat = index(c[1], 'A', 'R')?;
    let square_lon = index(c[2], '0', '9')?;
    let square_lat = index(c[3], '0', '9')?;
    // This operation order is the one Design §2's reference values were
    // computed with; keep it, or the grid-centre floats drift in the last bit.
    let mut sq = Square {
        south: -90.0 + field_lat * 10.0 + square_lat,
        west: -180.0 + field_lon * 20.0 + square_lon * 2.0,
        height: 1.0,
        width: 2.0,
    };
    if c.len() == 6 {
        let sub_lon = index(c[4], 'A', 'X')?;
        let sub_lat = index(c[5], 'A', 'X')?;
        sq.width = 2.0 / 24.0;
        sq.height = 1.0 / 24.0;
        sq.west += sub_lon * sq.width;
        sq.south += sub_lat * sq.height;
    }
    Some(sq)
}

/// `c`'s offset from `first`, when it lies in `first..=last`.
fn index(c: char, first: char, last: char) -> Option<f64> {
    (first..=last)
        .contains(&c)
        .then(|| f64::from(c as u32 - first as u32))
}

impl Square {
    pub fn center(&self) -> (f64, f64) {
        (self.south + self.height / 2.0, self.west + self.width / 2.0)
    }

    /// Inclusive of the edges, with `EDGE_TOLERANCE` for float rounding: a
    /// point typed exactly on a boundary belongs to both squares it touches.
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.south - EDGE_TOLERANCE
            && lat <= self.south + self.height + EDGE_TOLERANCE
            && lon >= self.west - EDGE_TOLERANCE
            && lon <= self.west + self.width + EDGE_TOLERANCE
    }
}

/// `FN31pr`: field and square uppercase, subsquare lowercase.
pub fn canonical(locator: &str) -> Option<String> {
    square(locator)?;
    Some(
        locator
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i < 4 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
    )
}

/// The 6-character locator of the subsquare containing (lat, lon). Callers
/// pass in-range values (config validates them). The north pole and the
/// antimeridian's east edge clamp into the last subsquare.
pub fn locator(lat: f64, lon: f64) -> String {
    // Clamp the coordinates, not each index: an index-only clamp maps
    // (90, 180) to `RR00aa`, because `x % 20` wraps back to 0 at x = 360.
    let x = (lon + 180.0).clamp(0.0, 360.0 - EDGE_TOLERANCE);
    let y = (lat + 90.0).clamp(0.0, 180.0 - EDGE_TOLERANCE);
    // The `min`s only guard the casts; the clamps above already keep every
    // index in range.
    let digit = |v: f64, max: u8| (v.floor() as u8).min(max);
    let chars = [
        b'A' + digit(x / 20.0, 17),
        b'A' + digit(y / 10.0, 17),
        b'0' + digit((x % 20.0) / 2.0, 9),
        b'0' + digit(y % 10.0, 9),
        b'a' + digit((x % 2.0) * 12.0, 23),
        b'a' + digit((y % 1.0) * 24.0, 23),
    ];
    chars.iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "{actual} is not within 1e-9 of {expected}"
        );
    }

    #[test]
    fn a_six_character_locator_names_its_subsquare() {
        // FN31pr: south 41.708333.., west -72.75, 1/24 deg x 2/24 deg.
        let sq = square("FN31pr").unwrap();
        assert_close(sq.south, 41.708_333_333_333_336);
        assert_close(sq.west, -72.75);
        assert_close(sq.height, 1.0 / 24.0);
        assert_close(sq.width, 2.0 / 24.0);
        let (lat, lon) = sq.center();
        assert_close(lat, 41.729_166_666_666_67);
        assert_close(lon, -72.708_333_333_333_33);
    }

    #[test]
    fn a_four_character_locator_names_its_square() {
        assert_eq!(square("FN31").unwrap().center(), (41.5, -73.0));
    }

    #[test]
    fn locators_are_case_insensitive_and_canonicalised() {
        assert_eq!(square("fn31PR"), square("FN31pr"));
        assert_eq!(canonical("fn31PR").as_deref(), Some("FN31pr"));
        assert_eq!(canonical("fn31").as_deref(), Some("FN31"));
    }

    #[test]
    fn the_corner_squares_reach_the_poles_and_the_antimeridian() {
        let sw = square("AA00aa").unwrap();
        assert_eq!((sw.south, sw.west), (-90.0, -180.0));
        let ne = square("RR99xx").unwrap();
        assert_close(ne.south + ne.height, 90.0);
        assert_close(ne.west + ne.width, 180.0);
    }

    #[test]
    fn malformed_locators_are_rejected() {
        // config.rs's operator_grid_rejects_non_maidenhead_values set, plus
        // a non-digit square and an out-of-range subsquare.
        for bad in [
            "ZZ99xx", "JN4", "JN46lax", "1N46la", "JN46l", "", "FN3a", "FN31yy",
        ] {
            assert!(square(bad).is_none(), "{bad:?}");
            assert!(canonical(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_point_maps_to_the_subsquare_that_contains_it() {
        // W1AW's published position (ARRL HQ, Newington CT).
        assert_eq!(locator(41.714775, -72.72726), "FN31pr");
        assert_eq!(locator(0.0, 0.0), "JJ00aa");
        // Range edges clamp into the last subsquare rather than producing 'S'.
        assert_eq!(locator(90.0, 180.0), "RR99xx");
        assert_eq!(locator(-90.0, -180.0), "AA00aa");
    }

    #[test]
    fn every_subsquare_centre_maps_back_to_its_own_locator() {
        for g in [
            "FN31pr", "EM12kp", "FN03gw", "JN46la", "JJ00aa", "AA00aa", "RR99xx", "QF22lb",
        ] {
            let (lat, lon) = square(g).unwrap().center();
            assert_eq!(locator(lat, lon), g);
        }
    }

    #[test]
    fn contains_is_inclusive_of_the_square_edges() {
        let sq = square("FN31pr").unwrap();
        assert!(sq.contains(41.714775, -72.72726));
        assert!(sq.contains(41.75, -72.75)); // north-west corner
        assert!(!sq.contains(40.0, -74.0));
        assert!(!sq.contains(41.714775, -72.6));
    }
}
