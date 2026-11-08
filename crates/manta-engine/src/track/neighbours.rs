//! MAN-524: one moment's track centres, sorted by circular channel position,
//! so ownership, spawn-admission and merge checks read only the tracks a few
//! channels away instead of every track. Each query returns a superset of
//! the tracks that can change its check's answer; the check then runs its
//! original float comparison on each candidate. See
//! docs/DECISIONS/2026-11-08-man524-linear-track-neighbour-queries.md.

/// MAN-524: added to every window radius. 0.0 gave 1 mismatch in 3,888,308
/// randomized checks against the full scans (float rounding at the window
/// edge after `rem_euclid`); 1e-6 gave none.
pub(super) const NEAR_SLACK_CH: f64 = 1e-6;

/// MAN-524: the track centres of one pass, for checks that only care about
/// tracks a few channels away.
pub(super) struct CenterIndex {
    n: usize,
    /// Ascending id: the old `centers` slice, also the full-scan order.
    by_id: Vec<(u32, f64)>,
    /// (circular position in [0, n), id, raw centre), sorted by (position,
    /// id). Empty while any centre is non-finite.
    by_pos: Vec<(f64, u32, f64)>,
    /// False once any centre is NaN or infinite.
    all_finite: bool,
}

#[cfg(test)]
thread_local! {
    /// Entries `any_near` handed to its closure (window-walk test only).
    static WALKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_walk() {
    WALKED.with(|w| w.set(w.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn note_walk() {}

/// Circular position of `c` in `[0, n)`. `rem_euclid` can return `n` for a
/// tiny negative `c`; that is position 0.
fn position(c: f64, n: usize) -> f64 {
    let span = n as f64;
    let p = c.rem_euclid(span);
    if p >= span {
        0.0
    } else {
        p
    }
}

impl CenterIndex {
    /// `centers` in ascending id (BTreeMap order).
    pub(super) fn new(centers: impl IntoIterator<Item = (u32, f64)>, n: usize) -> Self {
        let by_id: Vec<(u32, f64)> = centers.into_iter().collect();
        debug_assert!(by_id.windows(2).all(|w| w[0].0 < w[1].0));
        let all_finite = by_id.iter().all(|&(_, c)| c.is_finite());
        let mut by_pos: Vec<(f64, u32, f64)> = if all_finite {
            by_id
                .iter()
                .map(|&(id, c)| (position(c, n), id, c))
                .collect()
        } else {
            Vec::new()
        };
        by_pos.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        Self {
            n,
            by_id,
            by_pos,
            all_finite,
        }
    }

    /// Spawn pass: add a track at its final centre. Ids can wrap (MAN-13
    /// D3), so both orders take a sorted insert, and an id still present
    /// (a wrap onto a live id, which `spawn` overwrites) is replaced.
    pub(super) fn insert(&mut self, id: u32, center: f64) {
        let i = self.by_id.partition_point(|&(other, _)| other < id);
        if self.by_id.get(i).is_some_and(|&(other, _)| other == id) {
            self.by_id.remove(i);
            self.by_pos.retain(|&(_, other, _)| other != id);
        }
        self.by_id.insert(i, (id, center));
        if !center.is_finite() {
            self.all_finite = false;
            self.by_pos.clear();
        }
        if self.all_finite {
            let p = position(center, self.n);
            let j = self
                .by_pos
                .partition_point(|&(q, other, _)| q < p || (q == p && other < id));
            self.by_pos.insert(j, (p, id, center));
        }
    }

    /// Calls `f(id, centre)` on every entry whose circular distance from
    /// `x` may be <= `r`, and stops at the first `true`. Falls back to every
    /// entry in id order when any centre, `x` or `r` is non-finite, or the
    /// window covers the whole circle.
    pub(super) fn any_near(&self, x: f64, r: f64, mut f: impl FnMut(u32, f64) -> bool) -> bool {
        let span = self.n as f64;
        let r = r + NEAR_SLACK_CH;
        if !self.all_finite || !x.is_finite() || !r.is_finite() || 2.0 * r >= span {
            return self.by_id.iter().any(|&(id, c)| {
                note_walk();
                f(id, c)
            });
        }
        let x = position(x, self.n);
        let (lo, hi) = (x - r, x + r);
        let mut walk = |a: f64, b: f64| {
            let s = self.by_pos.partition_point(|&(p, _, _)| p < a);
            let e = self.by_pos.partition_point(|&(p, _, _)| p <= b);
            self.by_pos[s..e.max(s)].iter().any(|&(_, id, c)| {
                note_walk();
                f(id, c)
            })
        };
        if lo < 0.0 {
            walk(0.0, hi) || walk(lo + span, span)
        } else if hi >= span {
            walk(lo, span) || walk(0.0, hi - span)
        } else {
            walk(lo, hi)
        }
    }

    /// The id-ordered entries (merge sweep, tests).
    pub(super) fn by_id(&self) -> &[(u32, f64)] {
        &self.by_id
    }
}

#[cfg(test)]
mod tests {
    use super::super::{channel_distance, short_of, ISOLATION_CH, STRADDLE_NEIGHBOR_CH};
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn below(&mut self, m: u64) -> u64 {
            self.next() % m
        }
    }

    // Reference scans, verbatim from 915db0e.
    fn ref_exclusive(me: u32, my_center: f64, k: usize, centers: &[(u32, f64)], n: usize) -> bool {
        let own = channel_distance(k as f64, my_center, n);
        centers.iter().all(|&(id, center)| {
            id == me || {
                let other = channel_distance(k as f64, center, n);
                own < other || (own == other && me < id)
            }
        })
    }
    fn ref_neighbor(me: u32, my_center: f64, centers: &[(u32, f64)], n: usize) -> bool {
        centers
            .iter()
            .any(|&(id, c)| id != me && channel_distance(c, my_center, n) < 2.5)
    }
    fn ref_straddle(me: u32, my_center: f64, centers: &[(u32, f64)], n: usize) -> Option<u32> {
        let mut nearest = STRADDLE_NEIGHBOR_CH;
        let mut best = None;
        for &(id, center) in centers {
            let distance = channel_distance(center, my_center, n);
            if id == me || distance >= nearest {
                continue;
            }
            nearest = distance;
            best = Some(id);
        }
        best
    }
    fn ref_isolated(k: usize, centers: &[(u32, f64)], n: usize) -> bool {
        centers
            .iter()
            .all(|&(_, c)| channel_distance(c, k as f64, n) >= ISOLATION_CH)
    }
    /// `spawn_admits`' owner scan, reduced to "which tracks own `k`".
    fn ref_owners(k: usize, centers: &[(u32, f64)], n: usize) -> Vec<u32> {
        let owned = |c: f64| {
            let c = c.round() as i64;
            let n = n as i64;
            [
                (c - 1).rem_euclid(n),
                c.rem_euclid(n),
                (c + 1).rem_euclid(n),
            ]
        };
        centers
            .iter()
            .filter(|&&(_, c)| owned(c).contains(&(k as i64)))
            .map(|&(id, _)| id)
            .collect()
    }
    fn ref_merge_pairs(centers: &[(u32, f64)], n: usize) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for i in 0..centers.len() {
            for j in (i + 1)..centers.len() {
                if channel_distance(centers[i].1, centers[j].1, n) < 1.0 {
                    out.push((centers[i].0, centers[j].0));
                }
            }
        }
        out
    }

    // Index-backed forms, written with the exactness rules (production
    // code in `track.rs` uses the same expressions).
    fn idx_exclusive(me: u32, my_center: f64, k: usize, near: &CenterIndex, n: usize) -> bool {
        let own = channel_distance(k as f64, my_center, n);
        !near.any_near(k as f64, own, |id, center| {
            id != me && {
                let other = channel_distance(k as f64, center, n);
                !(own < other || (own == other && me < id))
            }
        })
    }
    fn idx_neighbor(me: u32, my_center: f64, near: &CenterIndex, n: usize) -> bool {
        near.any_near(my_center, 2.5, |id, c| {
            id != me && channel_distance(c, my_center, n) < 2.5
        })
    }
    fn idx_straddle(me: u32, my_center: f64, near: &CenterIndex, n: usize) -> Option<u32> {
        let mut nearest = STRADDLE_NEIGHBOR_CH;
        let mut best: Option<u32> = None;
        near.any_near(my_center, STRADDLE_NEIGHBOR_CH, |id, center| {
            let distance = channel_distance(center, my_center, n);
            let skip = distance > nearest || (distance == nearest && best.is_none_or(|b| id > b));
            if id != me && !skip {
                nearest = distance;
                best = Some(id);
            }
            false
        });
        best
    }
    fn idx_isolated(k: usize, near: &CenterIndex, n: usize) -> bool {
        !near.any_near(k as f64, ISOLATION_CH, |_, c| {
            short_of(channel_distance(c, k as f64, n), ISOLATION_CH)
        })
    }
    fn idx_owners(k: usize, near: &CenterIndex, n: usize) -> Vec<u32> {
        let mut out = Vec::new();
        near.any_near(k as f64, 2.0, |id, c| {
            let r = c.round() as i64;
            let m = n as i64;
            if [
                (r - 1).rem_euclid(m),
                r.rem_euclid(m),
                (r + 1).rem_euclid(m),
            ]
            .contains(&(k as i64))
            {
                out.push(id);
            }
            false
        });
        out.sort_unstable();
        out
    }
    fn idx_merge_pairs(near: &CenterIndex, n: usize) -> Vec<(u32, u32)> {
        let mut pairs = Vec::new();
        for &(a, ca) in near.by_id() {
            near.any_near(ca, 1.0, |b, cb| {
                if b > a && channel_distance(ca, cb, n) < 1.0 {
                    pairs.push((a, b));
                }
                false
            });
        }
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }

    fn draw_centers(rng: &mut Rng, n: usize) -> (Vec<(u32, f64)>, bool) {
        let span = n as f64;
        let mode = rng.below(6);
        let count = 1 + rng.below(80) as usize;
        let cluster = rng.unit() * span;
        let mut non_finite = false;
        let mut centers = Vec::with_capacity(count);
        let mut id = 1u32;
        for _ in 0..count {
            id += 1 + rng.below(3) as u32;
            let c = match mode {
                0 => rng.unit() * span,
                1 => (rng.unit() * span * 4.0).floor() / 4.0,
                2 => (rng.unit() * 6.0 - 3.0).rem_euclid(span),
                3 => {
                    let c = (rng.unit() * span * 4.0).floor() / 4.0;
                    if rng.below(2) == 0 {
                        c + span
                    } else {
                        c - span
                    }
                }
                4 => cluster + (rng.unit() * 16.0).floor() / 4.0 - 2.0,
                _ => {
                    if rng.below(40) == 0 {
                        non_finite = true;
                        [f64::NAN, f64::INFINITY, f64::NEG_INFINITY][rng.below(3) as usize]
                    } else {
                        (rng.unit() * 40.0 * 4.0).floor() / 4.0
                    }
                }
            };
            centers.push((id, c));
        }
        (centers, non_finite)
    }

    #[test]
    fn matches_the_full_scans() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut checks, mut mismatches, mut ties, mut fallbacks) = (0u64, 0u64, 0u64, 0u64);
        for _ in 0..1_500 {
            let n = [64, 512, 1024, 2048][rng.below(4) as usize];
            let (centers, non_finite) = draw_centers(&mut rng, n);
            let near = CenterIndex::new(centers.iter().copied(), n);
            for &(me, mc) in &centers {
                let home = (mc.round() as i64).rem_euclid(n as i64) as usize;
                let lo = (mc.floor() as i64).rem_euclid(n as i64) as usize;
                let hi = (mc.ceil() as i64).rem_euclid(n as i64) as usize;
                for k in [(home + n - 1) % n, home, (home + 1) % n, lo, hi] {
                    let own = channel_distance(k as f64, mc, n);
                    if centers
                        .iter()
                        .any(|&(id, c)| id != me && channel_distance(k as f64, c, n) == own)
                    {
                        ties += 1;
                    }
                    checks += 1;
                    if ref_exclusive(me, mc, k, &centers, n) != idx_exclusive(me, mc, k, &near, n) {
                        mismatches += 1;
                    }
                    checks += 1;
                    mismatches +=
                        u64::from(ref_isolated(k, &centers, n) != idx_isolated(k, &near, n));
                    // `Track::owned` casts to i64; an infinite centre would
                    // overflow there as well, so only finite scenes.
                    if !non_finite {
                        checks += 1;
                        mismatches +=
                            u64::from(ref_owners(k, &centers, n) != idx_owners(k, &near, n));
                    }
                }
                checks += 2;
                mismatches +=
                    u64::from(ref_neighbor(me, mc, &centers, n) != idx_neighbor(me, mc, &near, n));
                mismatches +=
                    u64::from(ref_straddle(me, mc, &centers, n) != idx_straddle(me, mc, &near, n));
                if non_finite || !mc.is_finite() {
                    fallbacks += 1;
                }
            }
            checks += 1;
            mismatches += u64::from(ref_merge_pairs(&centers, n) != idx_merge_pairs(&near, n));
        }
        eprintln!("MAN-524 differential: checks={checks} mismatches={mismatches} ties={ties} fallbacks={fallbacks}");
        assert_eq!(
            mismatches, 0,
            "checks={checks} ties={ties} fallbacks={fallbacks}"
        );
        assert!(checks >= 500_000, "checks={checks}");
        assert!(ties >= 50_000, "ties={ties}");
        assert!(fallbacks >= 1_000, "fallbacks={fallbacks}");
    }

    fn walked(f: impl FnOnce()) -> u64 {
        WALKED.with(|w| w.set(0));
        f();
        WALKED.with(|w| w.get())
    }

    #[test]
    fn window_walk_is_bounded() {
        let n = 2048;
        for shift in [0.0, -3.0] {
            let centers: Vec<(u32, f64)> = (0..300u32)
                .map(|i| (i + 1, (f64::from(i) * 6.8 + shift).rem_euclid(n as f64)))
                .collect();
            let near = CenterIndex::new(centers.iter().copied(), n);
            let ownership = walked(|| {
                for &(me, mc) in &centers {
                    let home = (mc.round() as i64).rem_euclid(n as i64) as usize;
                    for k in [(home + n - 1) % n, home, (home + 1) % n] {
                        idx_exclusive(me, mc, k, &near, n);
                    }
                    idx_neighbor(me, mc, &near, n);
                    idx_straddle(me, mc, &near, n);
                }
            });
            assert!(ownership <= 2_000, "ownership walk {ownership}");
            let merge = walked(|| {
                idx_merge_pairs(&near, n);
            });
            assert!(merge <= 600, "merge walk {merge}");
        }
    }

    #[test]
    fn insert_keeps_both_orders() {
        let n = 1024;
        let mut near = CenterIndex::new([(2, 100.0), (5, 300.0), (9, 500.0)], n);
        near.insert(1, 400.0);
        near.insert(7, 300.0);
        let ids: Vec<u32> = near.by_id().iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, [1, 2, 5, 7, 9]);
        assert!(near
            .by_pos
            .windows(2)
            .all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1)));
        let mut found = Vec::new();
        near.any_near(300.5, 1.0, |id, _| {
            found.push(id);
            false
        });
        assert_eq!(found, [5, 7]);
        assert!(near.any_near(399.0, 1.0, |id, _| id == 1));
        // A wrap onto a live id replaces its entry.
        near.insert(5, 700.0);
        assert_eq!(near.by_id().len(), 5);
        assert_eq!(near.by_pos.len(), 5);
        assert!(!near.any_near(300.0, 1.0, |id, _| id == 5));
        assert!(near.any_near(700.0, 1.0, |id, _| id == 5));
    }

    #[test]
    fn non_finite_falls_back_to_id_order() {
        let n = 1024;
        let order = |near: &CenterIndex, x: f64, r: f64| {
            let mut seen = Vec::new();
            near.any_near(x, r, |id, _| {
                seen.push(id);
                false
            });
            seen
        };
        let nan = CenterIndex::new([(1, 900.0), (2, f64::NAN), (3, 10.0)], n);
        assert_eq!(order(&nan, 10.0, 1.0), [1, 2, 3]);
        let finite = CenterIndex::new([(1, 900.0), (2, 500.0), (3, 10.0)], n);
        assert_eq!(order(&finite, f64::NAN, 1.0), [1, 2, 3]);
        assert_eq!(order(&finite, 10.0, f64::INFINITY), [1, 2, 3]);
        assert_eq!(order(&finite, 10.0, 1.0), [3]);
        let mut grown = CenterIndex::new([(1, 900.0)], n);
        grown.insert(2, f64::NAN);
        grown.insert(3, 10.0);
        assert_eq!(order(&grown, 10.0, 1.0), [1, 2, 3]);
    }

    #[test]
    fn position_of_a_tiny_negative_centre_is_zero() {
        let n = 1024;
        assert_eq!(position(-1e-17, n), 0.0);
        let near = CenterIndex::new([(1, -1e-17)], n);
        assert!(near.any_near(1.0, 1.0, |id, _| id == 1));
        assert!(near.any_near(1023.5, 1.0, |id, _| id == 1));
    }
}
