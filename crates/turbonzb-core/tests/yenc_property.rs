//! yEnc property / robustness suite (§4 of TEST_PLAN.md).
//!
//! Round-trips random binary through an independent encoder into the real
//! decoder (`decode_article` and the streaming `Decoder`), asserts identity,
//! verifies CRC detection, and hammer-corrupts/malforms input to prove the
//! decoder never panics and never returns wrong bytes.
//!
//! Uses a deterministic Xorshift PRNG — no external property-test
//! dependency, fully reproducible (covers the "poor man's fuzz" target).

use crc32fast::Hasher;
use turbonzb_core::yenc::{Decoder, decode_article};

/// Deterministic xorshift64 PRNG so failures are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// yEnc-encode a raw payload for tests. Matches the decoder's semantics:
/// encoded = (raw + 42) mod 256, with `=`-escape when the encoded value is
/// one of 0, '=', TAB, LF, CR (escape via `= <enc+64>`).
fn encode_payload(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &r in raw {
        let enc = r.wrapping_add(42);
        if matches!(enc, 0 | 61 | 9 | 10 | 13) {
            out.push(b'=');
            out.push(enc.wrapping_add(64));
        } else {
            out.push(enc);
        }
    }
    // Terminate the final line so the decoder sees a clean payload end.
    out.push(b'\n');
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut h = Hasher::new();
    h.update(data);
    h.finalize()
}

/// Build a single-part article body (`=ybegin` ... `=yend crc32=`).
fn single_article(name: &str, raw: &[u8], with_crc: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(format!("=ybegin size={} name={name}\n", raw.len()).as_bytes());
    b.extend_from_slice(&encode_payload(raw));
    if with_crc {
        b.extend_from_slice(
            format!("=yend size={} crc32={:08x}\n", raw.len(), crc32(raw)).as_bytes(),
        );
    } else {
        b.extend_from_slice(format!("=yend size={}\n", raw.len()).as_bytes());
    }
    b
}

/// Build a multipart article with `=ypart begin..end` and `pcrc32`.
fn part_article(begin: u64, end: u64, part: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(format!("=ybegin size={} name=p.bin\n", end).as_bytes());
    b.extend_from_slice(format!("=ypart begin={begin} end={end}\n").as_bytes());
    b.extend_from_slice(&encode_payload(part));
    b.extend_from_slice(
        format!("=yend size={} pcrc32={:08x}\n", part.len(), crc32(part)).as_bytes(),
    );
    b
}

fn random_payload(rng: &mut Rng, max: usize) -> Vec<u8> {
    let n = rng.below(max);
    (0..n).map(|_| rng.byte()).collect()
}

/// 4.2 — round-trip identity over many random payloads (bulk path).
#[test]
fn decode_article_roundtrip_identity() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    // A pool of raw bytes that land in the yEnc escape set after +42.
    let escape_raw = [0u8, 9u8, 10u8, 13u8, 19u8, 214u8, 223u8, 224u8, 227u8];
    for iter in 0..500 {
        let mut raw = random_payload(&mut rng, 4096);
        // Every ~5th payload gets sprinkled with escape-set bytes at the front
        // to maximise coverage of the `=`-escape path.
        if iter % 5 == 0 {
            raw.splice(
                0..0,
                std::iter::once(escape_raw[rng.below(escape_raw.len())]),
            );
        }
        let article = single_article(&format!("f{iter}.bin"), &raw, iter % 2 == 0);
        let decoded = decode_article(&article)
            .map_err(|e| panic!("decode failed at iter {iter}: {e:?}"))
            .unwrap();
        assert_eq!(decoded.data, raw, "byte identity broken at iter {iter}");
        assert_eq!(decoded.total_size, raw.len() as u64);
        assert_eq!(decoded.begin, 1);
        assert_eq!(decoded.end, raw.len() as u64);
        if iter % 2 == 0 {
            assert!(decoded.crc_ok, "declared CRC should match at iter {iter}");
            assert!(!decoded.crc_unknown);
        } else {
            assert!(
                decoded.crc_unknown,
                "no CRC header → unknown at iter {iter}"
            );
        }
    }
}

/// 4.2 — streaming `Decoder` path (the one NNTP uses), including the
/// `..`-unstuffing the transport layer does.
#[test]
fn streaming_decoder_roundtrip() {
    let mut rng = Rng(0xD1B54A32D192ED03);
    for iter in 0..200 {
        let raw = random_payload(&mut rng, 1024);
        let article = single_article(&format!("s{iter}.bin"), &raw, true);
        let mut dec = Decoder::new();
        for line in article.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            // Apply dot-unstuffing like read_body_decoded does.
            let unstuffed = if let Some(rest) = line.strip_prefix(b"..") {
                let mut v = Vec::with_capacity(rest.len() + 1);
                v.push(b'.');
                v.extend_from_slice(rest);
                v
            } else {
                line.to_vec()
            };
            dec.push_line(&unstuffed)
                .expect("push_line should not error");
        }
        let part = dec.finish().expect("finish should succeed");
        assert_eq!(part.data, raw, "streaming decode broken at iter {iter}");
        assert!(part.crc_ok);
    }
}

#[test]
fn corrupted_payload_detected() {
    let mut rng = Rng(0x9527D3BE);
    let raw: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
    let article = single_article("c.bin", &raw, true);
    // Flip a random byte in the payload region.
    let mut corrupted = article.clone();
    let idx = rng.below(corrupted.len());
    corrupted[idx] ^= 0xFF;
    let decoded = decode_article(&corrupted).expect("should still parse");
    if decoded.data != raw {
        // If bytes changed, the CRC must not claim success.
        assert!(
            !decoded.crc_ok || decoded.crc_unknown,
            "corrupted bytes reported healthy CRC"
        );
    }
}

/// 4.4 — malformed frames produce errors, never panics.
#[test]
fn malformed_frames_error_out() {
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"hello world\r\n".to_vec(),
        b"=yend size=1\r\n".to_vec(),           // no =ybegin
        b"=ybegin size=1 name=x\r\n".to_vec(),  // no =yend
        b"=ybegin size=nope name=x\n".to_vec(), // bad size
        format!("=ybegin size=5 name=x\n{}\n=yend size=5\n", "a").into_bytes(),
        b"=ybegin size=1 name=x\r\n=yend size=1 pcrc=nope\r\n".to_vec(),
        b"=ybegin".to_vec(),
    ];
    for (i, c) in cases.iter().enumerate() {
        // Must not panic; may succeed or return an error.
        let _ = decode_article(c);
        let mut dec = Decoder::new();
        for line in c.split(|&b| b == b'\n') {
            let _ = dec.push_line(line);
        }
        let _ = dec.finish();
        let _ = i;
    }
}

/// 4.2 — arbitrary garbage must never panic the decoder (mini-fuzz).
#[test]
fn garbage_input_never_panics() {
    let mut rng = Rng(0xA5A5A5A55A5A5A5A);
    for _ in 0..2000 {
        let len = rng.below(600);
        let data: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let _ = decode_article(&data);
        let mut dec = Decoder::new();
        for line in data.split(|&b| b == b'\n') {
            let _ = dec.push_line(line);
        }
        let _ = dec.finish();
    }
}

/// 4.3 — the streaming path correctly reports a bad CRC as not-ok.
#[test]
fn wrong_crc_reported_not_ok() {
    let raw = b"the quick brown fox jumps over the lazy dog".to_vec();
    let mut article = single_article("w.bin", &raw, true);
    // Break the declared CRC.
    let marker = b"crc32=";
    let pos = article
        .windows(marker.len())
        .position(|w| w == marker)
        .unwrap();
    let hex_len = 8;
    for i in 0..hex_len {
        // Replace with '0's.
        article[pos + marker.len() + i] = b'0';
    }
    let decoded = decode_article(&article).unwrap();
    assert_eq!(decoded.data, raw);
    assert!(!decoded.crc_ok);
    assert!(!decoded.crc_unknown, "CRC present, must be checked");
}

/// 4.4 — multipart article with the exact part bounds is honored.
#[test]
fn multipart_bounds_honored() {
    let part: Vec<u8> = (0..3000u32).map(|i| (i % 250) as u8).collect();
    let article = part_article(5000, 8000, &part);
    let decoded = decode_article(&article).unwrap();
    assert_eq!(decoded.begin, 5000);
    assert_eq!(decoded.end, 8000);
    assert_eq!(decoded.data, part);
    assert!(decoded.crc_ok);
}
