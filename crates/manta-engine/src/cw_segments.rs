//! Per-source CW coverage. Frequencies are calibrated RF Hz; time is Unix UTC.
use std::sync::Arc;

/// A normalized union of inclusive frequency intervals.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ranges(Vec<(f64, f64)>);
impl Ranges {
    /// Validate and combine overlapping or touching ranges once at startup.
    pub fn new(mut ranges: Vec<(f64, f64)>) -> Result<Self, &'static str> {
        if ranges
            .iter()
            .any(|&(lo, hi)| !lo.is_finite() || !hi.is_finite() || lo <= 0.0 || lo >= hi)
        {
            return Err("must contain two finite positive Hz values with lo < hi");
        }
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut out: Vec<(f64, f64)> = Vec::new();
        for (lo, hi) in ranges {
            if let Some(last) = out.last_mut().filter(|last| lo <= last.1) {
                last.1 = last.1.max(hi);
            } else {
                out.push((lo, hi));
            }
        }
        Ok(Self(out))
    }
    /// Inclusive membership; an empty union admits nothing.
    pub fn contains(&self, hz: f64) -> bool {
        self.0.iter().any(|&(lo, hi)| hz >= lo && hz <= hi)
    }
}

/// Immutable daily and dated coverage policy. `None` base means unrestricted.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Policy {
    base: Option<Arc<Ranges>>,
    daily: Vec<(u16, Arc<Ranges>)>,
    overrides: Vec<(i64, i64, Arc<Ranges>)>,
}
impl Policy {
    /// Daily entries are UTC minutes; override intervals include start, exclude end.
    pub fn new(
        base: Option<Ranges>,
        mut daily: Vec<(u16, Ranges)>,
        mut overrides: Vec<(i64, i64, Ranges)>,
    ) -> Result<Self, String> {
        daily.sort_by_key(|e| e.0);
        if daily.iter().any(|e| e.0 >= 1440) {
            return Err("cw_schedule.at_utc must be HH:MM UTC".into());
        }
        if let Some(w) = daily.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(format!(
                "cw_schedule contains duplicate at_utc {:02}:{:02}",
                w[0].0 / 60,
                w[0].0 % 60
            ));
        }
        overrides.sort_by_key(|e| e.0);
        if overrides
            .iter()
            .any(|e| e.0 < 0 || e.0 >= e.1 || e.1 > 4_102_444_800)
        {
            return Err("cw_overrides requires UTC start_utc < end_utc".into());
        }
        if overrides.windows(2).any(|w| w[0].1 > w[1].0) {
            return Err("cw_overrides intervals must not overlap".into());
        }
        Ok(Self {
            base: base.map(Arc::new),
            daily: daily.into_iter().map(|(t, r)| (t, Arc::new(r))).collect(),
            overrides: overrides
                .into_iter()
                .map(|(a, b, r)| (a, b, Arc::new(r)))
                .collect(),
        })
    }
    /// Select coverage without consulting a clock or allocating ranges.
    pub fn active_at(&self, utc: i64) -> Option<Arc<Ranges>> {
        if let Some(e) = self.overrides.iter().find(|e| utc >= e.0 && utc < e.1) {
            return Some(e.2.clone());
        }
        let minute = (utc.rem_euclid(86400) / 60) as u16;
        self.daily
            .iter()
            .rev()
            .find(|e| e.0 <= minute)
            .or_else(|| self.daily.last())
            .map(|e| e.1.clone())
            .or_else(|| self.base.clone())
    }
    /// Earliest possible coverage change after `utc`, for batch splitting.
    pub fn next_transition_after(&self, utc: i64) -> i64 {
        let second = utc.rem_euclid(86400);
        let daily = self
            .daily
            .iter()
            .filter_map(|(m, _)| {
                let delta = (i64::from(*m) * 60 - second).rem_euclid(86400);
                utc.checked_add(if delta == 0 { 86400 } else { delta })
            })
            .min()
            .unwrap_or(i64::MAX);
        self.overrides
            .iter()
            .flat_map(|e| [e.0, e.1])
            .filter(|&t| t > utc)
            .min()
            .unwrap_or(i64::MAX)
            .min(daily)
    }
    /// Whether this policy needs a UTC epoch for file replay.
    pub fn scheduled(&self) -> bool {
        !self.daily.is_empty() || !self.overrides.is_empty()
    }
}

/// Clock used by a source. Replay time always advances with source samples.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Clock {
    /// Fixed epoch plus sample timestamp, also used for static sample decoding.
    #[default]
    Samples,
    /// UTC sampled once before a processing batch.
    Live,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(lo: f64, hi: f64) -> Ranges {
        Ranges::new(vec![(lo, hi)]).unwrap()
    }
    #[test]
    fn normalization_and_empty() {
        let v = Ranges::new(vec![(20., 30.), (10., 20.), (40., 50.)]).unwrap();
        assert_eq!(v.0, vec![(10., 30.), (40., 50.)]);
        assert!(v.contains(10.) && v.contains(50.) && !v.contains(35.));
        assert!(!Ranges::default().contains(20.));
        assert!(Policy::default().active_at(0).is_none());
        assert!(Policy::new(Some(Ranges::default()), vec![], vec![])
            .unwrap()
            .active_at(0)
            .is_some());
        for pair in [(0., 1.), (2., 1.), (1., f64::NAN), (1., f64::INFINITY)] {
            assert!(Ranges::new(vec![pair]).is_err());
        }
    }
    #[test]
    fn transition_arithmetic_is_bounded() {
        let p = Policy::new(None, vec![(0, r(1., 2.))], vec![]).unwrap();
        assert_eq!(p.next_transition_after(0), 86400);
        assert_eq!(p.next_transition_after(86399), 86400);
        assert_eq!(p.next_transition_after(i64::MAX), i64::MAX);
        assert!(p.next_transition_after(i64::MIN) > i64::MIN);
        assert!(p.active_at(-1).unwrap().contains(1.));
    }

    #[test]
    fn daily_wrap_exact_switch_and_override_resumption() {
        let p = Policy::new(
            None,
            vec![(1080, r(10., 20.)), (360, r(30., 40.))],
            vec![(22000, 70000, r(50., 60.))],
        )
        .unwrap();
        assert!(p.active_at(0).unwrap().contains(15.));
        assert!(p.active_at(21600).unwrap().contains(35.));
        assert!(p.active_at(22000).unwrap().contains(55.));
        assert!(p.active_at(70000).unwrap().contains(15.));
        assert!(p.active_at(86400).unwrap().contains(15.));
        assert!(p.active_at(4_102_444_800).is_some());
        assert!(Policy::new(None, vec![(0, r(1., 2.)), (0, r(2., 3.))], vec![]).is_err());
        assert!(Policy::new(None, vec![], vec![(1, 10, r(1., 2.)), (2, 3, r(1., 2.))]).is_err());
        assert!(Policy::new(
            None,
            vec![(0, r(1., 2.))],
            vec![(1, 2, r(1., 2.)), (2, 3, r(1., 2.))]
        )
        .is_ok());
    }
}
