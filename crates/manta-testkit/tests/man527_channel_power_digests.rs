//! MAN-527: one line per vector with an FNV-1a 64 digest of every channel
//! power and of every complex channel output, from `Channelizer::process`
//! in 4096-sample chunks, over V1-V10 and V8w. Run on two builds and diff,
//! like `crates/manta-cli/tests/decode_fingerprints.rs`. Uses only APIs on
//! main at de39a71. Run in release:
//! `cargo test --release --locked -p manta-testkit --test man527_channel_power_digests -- --ignored --nocapture | grep '^MAN527 vector'`

use manta_dsp::channelizer::Channelizer;
use manta_testkit::vectors;

fn fnv(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

#[test]
#[ignore = "renders 11 vectors; run in release with --ignored --nocapture"]
fn channel_power_digests() {
    let specs = [
        ("v1", vectors::v1()),
        ("v2", vectors::v2()),
        ("v3", vectors::v3()),
        ("v4", vectors::v4()),
        ("v5", vectors::v5()),
        ("v6", vectors::v6()),
        ("v7", vectors::v7()),
        ("v8", vectors::v8()),
        ("v8w", vectors::v8w()),
        ("v9", vectors::v9()),
        ("v10", vectors::v10()),
    ];
    for (name, spec) in specs {
        let r = vectors::render(&spec).unwrap();
        let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
        let (mut power_fnv, mut x_fnv) = (0xcbf2_9ce4_8422_2325u64, 0xcbf2_9ce4_8422_2325u64);
        let mut hops = 0usize;
        for piece in r.samples.chunks(4096) {
            for out in ch.process(piece) {
                for p in &out.power {
                    fnv(&mut power_fnv, p.to_bits());
                }
                for c in &out.x {
                    fnv(&mut x_fnv, c.re.to_bits());
                    fnv(&mut x_fnv, c.im.to_bits());
                }
                hops += 1;
            }
        }
        println!(
            "MAN527 vector {name} fs={} samples={} hops={hops} power_fnv={power_fnv:016x} x_fnv={x_fnv:016x}",
            spec.fs,
            r.samples.len()
        );
    }
}
