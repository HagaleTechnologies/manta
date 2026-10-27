//! Common HF digital-mode dial frequencies. This is a CW spawn policy,
//! not a transmit band plan. Each USB dial starts a half-open 3000 Hz window.

// Pinned WSJT-X and JS8Call defaults; provenance and policy in
// docs/DECISIONS/2026-10-19-man311-digital-window-exclusions.md.
// Rows: 160, 80, 60, 40, 30, 20, 17, 15, 12, 10 m. Columns: FT8, FT4, WSPR, JS8.
const DIALS_HZ: [[Option<u32>; 4]; 10] = [
    [Some(1_840_000), None, Some(1_836_600), Some(1_842_000)],
    [
        Some(3_573_000),
        Some(3_575_000),
        Some(3_568_600),
        Some(3_578_000),
    ],
    [Some(5_357_000), Some(5_357_000), None, None],
    [
        Some(7_074_000),
        Some(7_047_500),
        Some(7_038_600),
        Some(7_078_000),
    ],
    [
        Some(10_136_000),
        Some(10_140_000),
        Some(10_138_700),
        Some(10_130_000),
    ],
    [
        Some(14_074_000),
        Some(14_080_000),
        Some(14_095_600),
        Some(14_078_000),
    ],
    [
        Some(18_100_000),
        Some(18_104_000),
        Some(18_104_600),
        Some(18_104_000),
    ],
    [
        Some(21_074_000),
        Some(21_140_000),
        Some(21_094_600),
        Some(21_078_000),
    ],
    [
        Some(24_915_000),
        Some(24_919_000),
        Some(24_924_600),
        Some(24_922_000),
    ],
    [
        Some(28_074_000),
        Some(28_180_000),
        Some(28_124_600),
        Some(28_078_000),
    ],
];

/// Membership of the union of half-open [dial, dial + 3000) windows.
/// Called only while constructing a receiver's channel mask.
pub(crate) fn contains(freq_hz: f64) -> bool {
    DIALS_HZ.iter().flatten().flatten().any(|&dial| {
        let start = f64::from(dial);
        (start..start + 3000.0).contains(&freq_hz)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digital_window_dials_match_the_pinned_defaults() {
        // Rows are 160, 80, 60, 40, 30, 20, 17, 15, 12 and 10 metres;
        // columns are FT8, FT4, WSPR and JS8, all in integer Hz.
        assert_eq!(
            DIALS_HZ,
            [
                [Some(1_840_000), None, Some(1_836_600), Some(1_842_000)],
                [
                    Some(3_573_000),
                    Some(3_575_000),
                    Some(3_568_600),
                    Some(3_578_000)
                ],
                [Some(5_357_000), Some(5_357_000), None, None],
                [
                    Some(7_074_000),
                    Some(7_047_500),
                    Some(7_038_600),
                    Some(7_078_000)
                ],
                [
                    Some(10_136_000),
                    Some(10_140_000),
                    Some(10_138_700),
                    Some(10_130_000)
                ],
                [
                    Some(14_074_000),
                    Some(14_080_000),
                    Some(14_095_600),
                    Some(14_078_000)
                ],
                [
                    Some(18_100_000),
                    Some(18_104_000),
                    Some(18_104_600),
                    Some(18_104_000)
                ],
                [
                    Some(21_074_000),
                    Some(21_140_000),
                    Some(21_094_600),
                    Some(21_078_000)
                ],
                [
                    Some(24_915_000),
                    Some(24_919_000),
                    Some(24_924_600),
                    Some(24_922_000)
                ],
                [
                    Some(28_074_000),
                    Some(28_180_000),
                    Some(28_124_600),
                    Some(28_078_000)
                ],
            ]
        );
        for dial in DIALS_HZ.iter().flatten().flatten() {
            assert!(contains(f64::from(*dial)), "start {dial}");
            assert!(contains(f64::from(*dial) + 2999.999), "end {dial}");
        }
    }

    #[test]
    fn digital_window_boundaries_and_overlaps() {
        // Isolated 20 m FT8 window.
        assert!(!contains(14_073_999.999));
        assert!(contains(14_074_000.0));
        assert!(contains(14_074_500.0));
        assert!(contains(14_076_999.999));
        assert!(!contains(14_077_000.0));
        // JS8 and FT4 overlap. JS8's endpoint is still inside FT4.
        assert!(contains(14_078_000.0));
        assert!(contains(14_081_000.0));
        assert!(contains(14_082_999.999));
        assert!(!contains(14_083_000.0));
        for freq in [0.0, 1_800_000.0, 14_050_000.0, 50_313_000.0, f64::NAN] {
            assert!(!contains(freq), "outside table: {freq}");
        }
    }
}
