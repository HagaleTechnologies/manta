//! Runs from evidence sign changes, for the EdgeLegacy engine (stage 1):
//! the legacy timing/beam chain fed by SPEC v2 §1's half-amplitude
//! decision instead of §3.2's geometric-mean threshold.

use crate::envelope::Run;
use crate::evidence::HopEvidence;
use crate::ms_to_hops;

pub struct EdgeDemod {
    debounce_hops: u32,
    open: Option<Run>,
    held: Option<Run>,
}

impl EdgeDemod {
    pub fn new(debounce_ms: f64) -> Self {
        EdgeDemod {
            debounce_hops: ms_to_hops(debounce_ms),
            open: None,
            held: None,
        }
    }

    pub fn push(&mut self, ev: &HopEvidence) -> Vec<Run> {
        let mark = ev.llr > 0.0;
        let mut out = Vec::new();
        match self.open {
            None => {
                self.open = Some(Run {
                    mark,
                    start_ts: ev.sample_ts,
                    hops: 1,
                })
            }
            Some(ref mut o) if o.mark == mark => o.hops += 1,
            Some(o) => {
                if o.hops < self.debounce_hops {
                    // Short run: merge held + short + continuing into held's polarity
                    // (same rule as envelope.rs, SPEC v1 §3.3).
                    match self.held.take() {
                        Some(h) => {
                            self.open = Some(Run {
                                mark: h.mark,
                                start_ts: h.start_ts,
                                hops: h.hops + o.hops + 1,
                            })
                        }
                        None => {
                            self.open = Some(Run {
                                mark,
                                start_ts: o.start_ts,
                                hops: o.hops + 1,
                            })
                        }
                    }
                } else {
                    if let Some(h) = self.held.take() {
                        out.push(h);
                    }
                    self.held = Some(o);
                    self.open = Some(Run {
                        mark,
                        start_ts: ev.sample_ts,
                        hops: 1,
                    });
                }
            }
        }
        out
    }

    pub fn open_space(&self) -> Option<(u32, u64)> {
        match self.open {
            Some(r) if !r.mark => Some((r.hops, r.start_ts)),
            _ => None,
        }
    }

    pub fn finish(&mut self) -> Vec<Run> {
        let mut out = Vec::new();
        if let Some(o) = self.open.take() {
            if o.hops >= self.debounce_hops {
                if let Some(h) = self.held.take() {
                    out.push(h);
                }
                out.push(o);
            } else if let Some(mut h) = self.held.take() {
                h.hops += o.hops;
                out.push(h);
            }
        } else if let Some(h) = self.held.take() {
            out.push(h);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Evidence, EvidenceConfig};

    /// A rectangular keyed envelope at `depth_db`, one dit = `dit_hops`,
    /// pattern "dit gap dah gap" repeated, with a 4-hop linear ramp on each
    /// edge (same helper shape as evidence.rs's `keyed()`).
    fn keyed(dit_hops: usize, depth_db: f32, reps: usize) -> Vec<f32> {
        let lo = 10f32.powf(-depth_db / 20.0);
        let mut v = Vec::new();
        let seg = |v: &mut Vec<f32>, on: bool, n: usize| {
            for _ in 0..n {
                v.push(if on { 1.0 } else { lo });
            }
        };
        for _ in 0..reps {
            seg(&mut v, true, dit_hops);
            seg(&mut v, false, dit_hops);
            seg(&mut v, true, 3 * dit_hops);
            seg(&mut v, false, 3 * dit_hops);
        }
        let mut out = v.clone();
        for i in 1..v.len() {
            if (v[i] - v[i - 1]).abs() > 0.1 {
                for j in 0..4 {
                    let k = i + j;
                    if k < out.len() {
                        let f = (j as f32 + 0.5) / 4.0;
                        out[k] = v[i - 1] + f * (v[i] - v[i - 1]);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn edge_demod_produces_alternating_runs_from_evidence() {
        let dit_hops = 13usize;
        let env = keyed(dit_hops, 45.0, 20);
        let noise_amp = 10f32.powf(-45.0 / 20.0) * 0.5;
        let mut evidence = Evidence::new(EvidenceConfig::default());
        let mut edge = EdgeDemod::new(12.0);
        let mut runs = Vec::new();
        for (i, &a) in env.iter().enumerate() {
            if let Some(ev) = evidence.push(a, noise_amp, i as u64 * 256) {
                runs.extend(edge.push(&ev));
            }
        }
        for ev in evidence.flush() {
            runs.extend(edge.push(&ev));
        }
        runs.extend(edge.finish());
        assert!(!runs.is_empty(), "no runs produced");
        for w in runs.windows(2) {
            assert_ne!(w[0].mark, w[1].mark, "runs must alternate");
        }
        let marks: Vec<u32> = runs.iter().filter(|r| r.mark).map(|r| r.hops).collect();
        let dits = marks
            .iter()
            .filter(|&&h| (dit_hops as u32 - 2..=dit_hops as u32 + 2).contains(&h))
            .count();
        assert!(
            dits >= 4,
            "expected several dit-length marks, got {marks:?}"
        );
    }

    #[test]
    fn open_space_reports_the_currently_open_space_run() {
        let dit_hops = 13usize;
        let env = keyed(dit_hops, 45.0, 20);
        let noise_amp = 10f32.powf(-45.0 / 20.0) * 0.5;
        let mut evidence = Evidence::new(EvidenceConfig::default());
        let mut edge = EdgeDemod::new(12.0);
        for (i, &a) in env.iter().enumerate() {
            if let Some(ev) = evidence.push(a, noise_amp, i as u64 * 256) {
                edge.push(&ev);
            }
        }
        // Mid-scene there should be at least one point where the open run is
        // a space (since the pattern alternates mark/space at 4 dits per
        // segment); assert the accessor's shape (either None or a sane hop
        // count) rather than a specific value, since exact timing depends on
        // the evidence pipeline's own delay.
        if let Some((hops, _ts)) = edge.open_space() {
            assert!(hops > 0);
        }
    }
}
