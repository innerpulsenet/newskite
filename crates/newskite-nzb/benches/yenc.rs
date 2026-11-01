//! Decoder throughput on a synthetic article: `cargo bench -p newskite-nzb --bench yenc`.
//!
//! Prints decoded MB/s (decimal megabytes of *decoded* output per second) for a 750 KB
//! part encoded with 128-character lines, the shape of a typical binary segment. No
//! harness: a plain `main` that times a fixed number of iterations.

use std::hint::black_box;
use std::time::Instant;

use newskite_nzb::yenc::{decode, decode_data, encode_article};

fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.extend_from_slice(&seed.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn bench(label: &str, iters: usize, decoded_len: usize, mut f: impl FnMut()) {
    // Warm-up, then timed runs; report the best of three batches so a scheduler hiccup
    // does not pass for a slower decoder.
    for _ in 0..iters / 4 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    let mb = (decoded_len as f64 * iters as f64) / 1e6;
    println!("{label:<40} {:>8.0} MB/s ({iters} x {decoded_len} B)", mb / best);
}

fn main() {
    let part_len = 750_000usize;
    let data = pseudo_random(part_len, 0x5EED);
    let article = encode_article("bench.bin", (part_len * 100) as u64, 7, 100, 6 * part_len as u64 + 1, &data, 128);
    let part = decode(&article).expect("valid article");
    assert_eq!(part.data, data);
    assert_eq!(part.crc_ok, Some(true));

    // Raw data lines only (no headers, no CRC): the inner loop by itself.
    let body_start = article.windows(7).position(|w| w == b"=ypart ").unwrap();
    let body_start = body_start + article[body_start..].iter().position(|&b| b == b'\n').unwrap() + 1;
    let body_end = article.windows(6).rposition(|w| w == b"=yend ").unwrap();
    let raw = &article[body_start..body_end];
    assert_eq!(decode_data(raw), data);

    let iters = 400;
    bench("decode (headers + data + crc32)", iters, part_len, || {
        black_box(decode(black_box(&article)).unwrap());
    });
    bench("decode_data (data lines only)", iters, part_len, || {
        black_box(decode_data(black_box(raw)));
    });
    bench("crc32fast::hash", iters, part_len, || {
        black_box(crc32fast::hash(black_box(&data)));
    });
}
