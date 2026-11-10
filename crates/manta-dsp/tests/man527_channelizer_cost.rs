//! MAN-527: per-hop time and an output digest for `Channelizer::process`.
//! An `#[ignore]`d probe, never a CI gate (CLAUDE.md: the CPU budget is a
//! target). Uses only APIs present on main at de39a71, so the same file runs
//! on a baseline tree. Run in release:
//! `cargo test --release --locked -p manta-dsp --test man527_channelizer_cost -- --ignored --nocapture`

use manta_dsp::channelizer::Channelizer;
use num_complex::Complex32;
use std::time::Instant;

fn noise(n: usize, seed: u64) -> Vec<Complex32> {
    let mut state = seed;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    (0..n).map(|_| Complex32::new(next(), next())).collect()
}

fn fnv(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

#[test]
#[ignore = "timing and digest probe; run in release with --ignored --nocapture"]
fn channelizer_cost_and_digest() {
    for fs in [192_000.0f64, 96_000.0] {
        for chunk in [4096usize, 1000] {
            let mut ch = Channelizer::new(fs, 14e6).unwrap();
            let (ln, hop) = (ch.filter_len(), ch.hop());
            let n = ch.n_channels();
            let mut iq = noise(ln + 2000 * hop, 0x9E37_79B9_7F4A_7C15);
            for s in &mut iq[ln + 1500 * hop..] {
                *s = Complex32::new(0.0, -0.0);
            }
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            let mut hops = 0usize;
            for piece in iq.chunks(chunk) {
                for out in ch.process(piece) {
                    assert_eq!(out.x.len(), n);
                    for c in &out.x {
                        fnv(&mut h, c.re.to_bits());
                        fnv(&mut h, c.im.to_bits());
                    }
                    for p in &out.power {
                        fnv(&mut h, p.to_bits());
                    }
                    hops += 1;
                }
            }
            println!("MAN527 digest fs={fs} chunk={chunk} hops={hops} fnv={h:016x}");
        }
        let warm = (2.0 * fs) as usize / 4096 * 4096;
        let iq = noise(warm + 400 * 4096, 0x2545_F491_4F6C_DD1D);
        let mut per_hop = Vec::new();
        let mut hops = 0usize;
        for _ in 0..9 {
            let mut ch = Channelizer::new(fs, 14e6).unwrap();
            for piece in iq[..warm].chunks(4096) {
                drop(ch.process(piece));
            }
            hops = 0;
            let t = Instant::now();
            for piece in iq[warm..].chunks(4096) {
                let out = ch.process(piece);
                hops += out.len();
                drop(out);
            }
            per_hop.push(t.elapsed().as_secs_f64() * 1e6 / hops as f64);
        }
        per_hop.sort_by(f64::total_cmp);
        println!(
            "MAN527 cost fs={fs} hops={hops} min_us_per_hop={:.2} median_us_per_hop={:.2}",
            per_hop[0], per_hop[4]
        );
    }
}
