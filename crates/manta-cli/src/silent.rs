//! MAN-14: the multi-input daemon's silent-receiver check. Once per
//! `[server].silent_input_warning_secs` window it compares each input's
//! published spots and cross-input dedupe losses over the window: an input
//! that heard neither while another published at least one spot is flagged
//! -- a warning naming both, and
//! `manta_input_silent{source}` at 1 -- unless its sample delivery went
//! down during the window (its own health alarms cover that). The first
//! spot or cross-input dedupe loss after a flag clears it with an info line.
//! Unhealthy delivery clears a flag without a line. Silence is not delivery
//! loss: `/healthz` and `manta_source_health` are untouched.

use std::time::{Duration, Instant};

/// What one `SilentWatch::tick` found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SilentNote {
    /// `input` decoded nothing over the window while `other` decoded
    /// `other_spots`.
    Silent {
        input: usize,
        other: usize,
        other_spots: u64,
    },
    /// `input` decoded again after `silent_for` of silence.
    Recovered { input: usize, silent_for: Duration },
    /// `input` is no longer flagged without hearing activity of its own:
    /// delivery became unhealthy, or no other input published a spot.
    Cleared { input: usize },
}

/// The check's state across ticks. Pure: the caller supplies the clock,
/// the per-input published-spot and cross-input-dedupe totals, and each
/// input's delivery health.
pub(crate) struct SilentWatch {
    window: Duration,
    window_start: Instant,
    /// Each input's published-spot and cross-input-dedupe totals at `window_start`.
    base_spots: Vec<u64>,
    base_dupes: Vec<u64>,
    /// Inputs whose delivery was down at any tick of this window.
    excluded: Vec<bool>,
    /// A flagged input's silence start and both totals when flagged.
    flagged: Vec<Option<(Instant, u64, u64)>>,
}

impl SilentWatch {
    pub(crate) fn new(window: Duration, now: Instant, spots: &[u64], dupes: &[u64]) -> Self {
        assert_eq!(spots.len(), dupes.len());
        Self {
            window,
            window_start: now,
            base_spots: spots.to_vec(),
            base_dupes: dupes.to_vec(),
            excluded: vec![false; spots.len()],
            flagged: vec![None; spots.len()],
        }
    }

    /// Whether `input` is flagged silent now.
    #[cfg(test)]
    fn is_silent(&self, input: usize) -> bool {
        self.flagged.get(input).is_some_and(Option::is_some)
    }

    pub(crate) fn tick(
        &mut self,
        now: Instant,
        spots: &[u64],
        dupes: &[u64],
        healthy: &[bool],
    ) -> Vec<SilentNote> {
        assert_eq!(spots.len(), self.base_spots.len());
        assert_eq!(dupes.len(), spots.len());
        assert_eq!(healthy.len(), spots.len());
        let mut notes = Vec::new();
        for (input, flag) in self.flagged.iter_mut().enumerate() {
            if !healthy[input] {
                if flag.take().is_some() {
                    notes.push(SilentNote::Cleared { input });
                }
            } else if let Some((since, at_spots, at_dupes)) = *flag {
                if spots[input] > at_spots || dupes[input] > at_dupes {
                    notes.push(SilentNote::Recovered {
                        input,
                        silent_for: now.saturating_duration_since(since),
                    });
                    *flag = None;
                }
            }
        }
        for (excluded, up) in self.excluded.iter_mut().zip(healthy) {
            *excluded |= !up;
        }
        if now.saturating_duration_since(self.window_start) < self.window {
            return notes;
        }
        let spot_deltas: Vec<u64> = spots
            .iter()
            .zip(&self.base_spots)
            .map(|(now, base)| now.saturating_sub(*base))
            .collect();
        let dupe_deltas: Vec<u64> = dupes
            .iter()
            .zip(&self.base_dupes)
            .map(|(now, base)| now.saturating_sub(*base))
            .collect();
        for input in 0..spot_deltas.len() {
            if spot_deltas[input] > 0 || dupe_deltas[input] > 0 || self.excluded[input] {
                continue;
            }
            // The busiest other input, the first in config order on a tie.
            let busiest = (0..spot_deltas.len())
                .filter(|&j| j != input)
                .max_by_key(|&j| (spot_deltas[j], std::cmp::Reverse(j)));
            match busiest {
                Some(other) if spot_deltas[other] > 0 => {
                    let since = self.flagged[input].map_or(self.window_start, |(s, _, _)| s);
                    self.flagged[input] = Some((since, spots[input], dupes[input]));
                    notes.push(SilentNote::Silent {
                        input,
                        other,
                        other_spots: spot_deltas[other],
                    });
                }
                _ => {
                    if self.flagged[input].take().is_some() {
                        notes.push(SilentNote::Cleared { input });
                    }
                }
            }
        }
        self.window_start = now;
        self.base_spots = spots.to_vec();
        self.base_dupes = dupes.to_vec();
        for (excluded, up) in self.excluded.iter_mut().zip(healthy) {
            *excluded = !up;
        }
        notes
    }
}

/// The warning for an input flagged silent.
pub(crate) fn silent_warning(
    name: &str,
    window: Duration,
    other: &str,
    other_spots: u64,
) -> String {
    format!(
        "warning: input {name}: no spots decoded for {} s while input {other} decoded \
         {other_spots} in that window (counted in manta_input_silent{{source=\"{name}\"}}; \
         warning repeats at most once per silent_input_warning_secs)",
        window.as_secs()
    )
}

/// The info line for a flagged input decoding again.
pub(crate) fn recovered_line(name: &str, silent_for: Duration) -> String {
    format!(
        "input {name}: decoding again after {} s of silence",
        silent_for.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(600);

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn nothing_is_flagged_before_the_first_window_ends() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0], &[0, 0]);
        for secs in [1, 30, 599] {
            assert_eq!(
                w.tick(at(t0, secs), &[5, 0], &[0, 0], &[true, true]),
                vec![]
            );
        }
        assert!(!w.is_silent(1));
    }

    #[test]
    fn a_silent_input_beside_a_hearing_one_is_flagged_once_per_window() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0], &[0, 0]);
        let silent = SilentNote::Silent {
            input: 1,
            other: 0,
            other_spots: 7,
        };
        assert_eq!(
            w.tick(at(t0, 600), &[7, 0], &[0, 0], &[true, true]),
            vec![silent]
        );
        assert!(w.is_silent(1) && !w.is_silent(0));
        // Still silent: no repeat inside the next window, one at its end.
        assert_eq!(
            w.tick(at(t0, 900), &[10, 0], &[0, 0], &[true, true]),
            vec![]
        );
        assert_eq!(
            w.tick(at(t0, 1200), &[14, 0], &[0, 0], &[true, true]),
            vec![SilentNote::Silent {
                input: 1,
                other: 0,
                other_spots: 7,
            }]
        );
        // Its first spot clears it at once, timed from the first window.
        assert_eq!(
            w.tick(at(t0, 1800), &[20, 1], &[0, 0], &[true, true]),
            vec![SilentNote::Recovered {
                input: 1,
                silent_for: Duration::from_secs(1800),
            }]
        );
        assert!(!w.is_silent(1));
    }

    #[test]
    fn every_input_quiet_flags_nobody() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[3, 0], &[0, 0]);
        assert_eq!(w.tick(at(t0, 600), &[3, 0], &[0, 0], &[true, true]), vec![]);
        assert!(!w.is_silent(0) && !w.is_silent(1));
        // A flag the band then goes quiet on is cleared without a spot.
        assert_eq!(
            w.tick(at(t0, 1200), &[4, 0], &[0, 0], &[true, true]).len(),
            1
        );
        assert_eq!(
            w.tick(at(t0, 1800), &[4, 0], &[0, 0], &[true, true]),
            vec![SilentNote::Cleared { input: 1 }]
        );
        assert!(!w.is_silent(1));
    }

    #[test]
    fn an_input_whose_delivery_went_down_is_excluded_for_that_window() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0], &[0, 0]);
        assert_eq!(
            w.tick(at(t0, 300), &[2, 0], &[0, 0], &[true, false]),
            vec![]
        );
        assert_eq!(w.tick(at(t0, 600), &[5, 0], &[0, 0], &[true, true]), vec![]);
        assert!(!w.is_silent(1));
        // Healthy for the whole next window: flagged.
        assert_eq!(
            w.tick(at(t0, 1200), &[9, 0], &[0, 0], &[true, true]).len(),
            1
        );
        assert!(w.is_silent(1));
    }

    #[test]
    fn a_cross_input_dedupe_loser_is_hearing_and_recovers_promptly() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0], &[0, 0]);
        assert_eq!(w.tick(at(t0, 600), &[1, 0], &[0, 1], &[true, true]), vec![]);
        assert!(!w.is_silent(1));

        // A later window with no activity can still flag B.
        assert_eq!(
            w.tick(at(t0, 1200), &[2, 0], &[0, 1], &[true, true]),
            vec![SilentNote::Silent {
                input: 1,
                other: 0,
                other_spots: 1,
            }]
        );
        assert!(w.is_silent(1));
        // The next dedupe loss clears a prior flag before the window ends.
        assert_eq!(
            w.tick(at(t0, 1201), &[2, 0], &[0, 2], &[true, true]),
            vec![SilentNote::Recovered {
                input: 1,
                silent_for: Duration::from_secs(601),
            }]
        );
        assert!(!w.is_silent(1));
    }

    #[test]
    fn an_unhealthy_flagged_input_clears_once_then_needs_a_full_healthy_window() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0], &[0, 0]);
        assert_eq!(
            w.tick(at(t0, 600), &[1, 0], &[0, 0], &[true, true]),
            vec![SilentNote::Silent {
                input: 1,
                other: 0,
                other_spots: 1,
            }]
        );
        assert!(w.is_silent(1));
        assert_eq!(
            w.tick(at(t0, 601), &[1, 0], &[0, 0], &[true, false]),
            vec![SilentNote::Cleared { input: 1 }]
        );
        assert!(!w.is_silent(1));
        assert_eq!(
            w.tick(at(t0, 900), &[1, 0], &[0, 0], &[true, false]),
            vec![]
        );
        assert!(!w.is_silent(1));
        assert_eq!(
            w.tick(at(t0, 1200), &[2, 0], &[0, 0], &[true, true]),
            vec![]
        );
        assert_eq!(
            w.tick(at(t0, 1800), &[3, 0], &[0, 0], &[true, true]),
            vec![SilentNote::Silent {
                input: 1,
                other: 0,
                other_spots: 1,
            }]
        );
        assert!(w.is_silent(1));
    }

    #[test]
    fn the_busiest_other_input_is_named() {
        let t0 = Instant::now();
        let mut w = SilentWatch::new(WINDOW, t0, &[0, 0, 0], &[0, 0, 0]);
        assert_eq!(
            w.tick(at(t0, 600), &[2, 0, 9], &[0, 0, 0], &[true, true, true]),
            vec![SilentNote::Silent {
                input: 1,
                other: 2,
                other_spots: 9,
            }]
        );
    }

    #[test]
    fn the_lines_name_the_receiver_and_the_comparison() {
        assert_eq!(
            silent_warning("deaf", WINDOW, "live", 7),
            "warning: input deaf: no spots decoded for 600 s while input live decoded 7 in \
             that window (counted in manta_input_silent{source=\"deaf\"}; warning repeats at \
             most once per silent_input_warning_secs)"
        );
        assert_eq!(
            recovered_line("deaf", Duration::from_secs(1800)),
            "input deaf: decoding again after 1800 s of silence"
        );
    }
}
