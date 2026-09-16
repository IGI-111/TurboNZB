//! Throughput regression benchmarks (§13.1 of TEST_PLAN.md).
//!
//! These are coarse by design: they catch a two- or three-fold regression in
//! the hot paths (yEnc decode, PAR2 verify) rather than measuring to the
//! microsecond. Bounds are loose enough to hold on a loaded CI machine while
//! still failing on a real regression. Each test prints its own numbers.
//!
//! Run just these with:
//!
//! ```text
//! cargo test --release -p turbonzb-core --test bench_throughput -- --nocapture
//! ```

use std::hint::black_box;
use std::time::Instant;

use crc32fast::Hasher;
use turbonzb_core::par2;
use turbonzb_core::yenc::{self, Decoder};

const SEG_SIZE: usize = 700_000;
const ARTICLES: usize = 40;

/// Encode one payload the way a poster would: `=ybegin`, escaped body with a
/// `=ypart` window, and a closing `=yend` with the CRC.
fn yenc_article(payload: &[u8], name: &str, begin: u64, end: u64, total: u64) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::with_capacity(payload.len() + 16);
    let mut crc = Hasher::new();
    for &b in payload {
        crc.update(&[b]);
        let enc = b.wrapping_add(42);
        if matches!(enc, b'=' | b'\r' | b'\n' | 0) {
            body.push(b'=');
            body.push(enc.wrapping_add(64));
        } else {
            body.push(enc);
        }
    }
    let mut out = format!("=ybegin line=128 size={total} name={name}\r\n").into_bytes();
    // A part that covers the whole file carries no `=ypart` header.
    if begin != 1 || end != total {
        out.extend_from_slice(format!("=ypart begin={begin} end={end}\r\n").as_bytes());
    }
    out.extend_from_slice(&body);
    out.extend_from_slice(b"\r\n");
    let crc_field = if begin == 1 && end == total {
        "crc32"
    } else {
        "pcrc32"
    };
    out.extend_from_slice(
        format!(
            "=yend size={} {crc_field}={:08x}\r\n",
            payload.len(),
            crc.finalize()
        )
        .as_bytes(),
    );
    out
}

fn payload(seed: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64) ^ seed).wrapping_mul(31).wrapping_add(7) as u8)
        .collect()
}

fn mb(bytes: u64, elapsed: Instant) -> f64 {
    let secs = elapsed.elapsed().as_secs_f64().max(f64::EPSILON);
    bytes as f64 / secs / (1024.0 * 1024.0)
}

/// §13.1 — whole-article yEnc decode throughput. This is the path every
/// segment takes, so a regression here caps download speed directly.
#[test]
fn yenc_whole_article_decode_throughput() {
    let articles: Vec<Vec<u8>> = (0..ARTICLES as u64)
        .map(|i| {
            let p = payload(i, SEG_SIZE);
            yenc_article(
                &p,
                &format!("bench-{i}.bin"),
                1,
                SEG_SIZE as u64,
                SEG_SIZE as u64,
            )
        })
        .collect();

    let start = Instant::now();
    let mut decoded_bytes = 0u64;
    let mut checked = 0usize;
    for article in &articles {
        let part = yenc::decode_article(article).expect("fixture article decodes");
        assert!(part.crc_ok, "fixture article CRC must match");
        decoded_bytes += part.data.len() as u64;
        checked += 1;
    }
    let mbps = mb(decoded_bytes, start);

    assert_eq!(checked, ARTICLES);
    assert_eq!(decoded_bytes, (SEG_SIZE * ARTICLES) as u64);
    println!("yEnc decode: {mbps:.0} MiB/s over {decoded_bytes} bytes");
    assert!(mbps > 30.0, "decode fell to {mbps:.0} MiB/s");
}

/// §13.1 — the streaming decoder used by the NNTP reader, same corpus line by
/// line. It is a different code path from `decode_article` and is what runs on
/// a live socket.
#[test]
fn yenc_streaming_decode_throughput() {
    let article = yenc_article(
        &payload(1, SEG_SIZE),
        "stream.bin",
        1,
        SEG_SIZE as u64,
        SEG_SIZE as u64,
    );
    // The NNTP layer hands the decoder one CR-LF-stripped line at a time.
    let lines: Vec<Vec<u8>> = article
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_suffix(&b"\r"[..]).unwrap_or(l).to_vec())
        .collect();

    let start = Instant::now();
    let mut total = 0u64;
    for _ in 0..ARTICLES {
        let mut dec = Decoder::new();
        for line in &lines {
            dec.push_line(black_box(line))
                .expect("stream accepts fixture");
        }
        let part = dec.finish().expect("fixture finishes cleanly");
        assert!(part.crc_ok);
        total += part.data.len() as u64;
    }
    let mbps = mb(total, start);

    assert_eq!(total, (SEG_SIZE * ARTICLES) as u64);
    println!("yEnc streaming decode: {mbps:.0} MiB/s over {total} bytes");
    assert!(mbps > 15.0, "streaming decode fell to {mbps:.0} MiB/s");
}

/// §13.1 — PAR2 verify throughput over an intact set: hashing and CRC matching
/// dominate, and this runs after every download.
#[test]
fn par2_verify_throughput_intact_set() {
    let dir = std::env::temp_dir().join(format!("turbonzb-bench-par2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let files: Vec<(Vec<u8>, String)> = (0..6)
        .map(|i| (payload(i, 2_000_000), format!("data-{i}.bin")))
        .collect();
    let refs: Vec<(&[u8], &str)> = files
        .iter()
        .map(|(bytes, name)| (bytes.as_slice(), name.as_str()))
        .collect();
    let packet = par2::build_par2_set(&refs);

    for (bytes, name) in &files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    std::fs::write(dir.join("bench.par2"), &packet).unwrap();

    let parsed = par2::parse_par2_files(&[dir.join("bench.par2")]).expect("built set parses");

    let start = Instant::now();
    let report = par2::verify(&parsed, &dir);
    let elapsed = start.elapsed();

    let bytes: u64 = files.iter().map(|(b, _)| b.len() as u64).sum();
    let mbps = {
        let secs = elapsed.as_secs_f64().max(f64::EPSILON);
        bytes as f64 / secs / (1024.0 * 1024.0)
    };
    println!(
        "PAR2 verify: {mbps:.0} MiB/s over {bytes} bytes ({} healthy, {} damaged, {} missing)",
        report.healthy, report.damaged, report.missing
    );

    assert_eq!(report.missing, 0, "an intact set has no missing files");
    assert_eq!(report.damaged, 0, "an intact set has no damaged files");
    assert_eq!(report.healthy as usize, files.len());
    assert!(mbps > 60.0, "verify fell to {mbps:.0} MiB/s");

    let _ = std::fs::remove_dir_all(&dir);
}
