//! PAR2 golden / interop suite (§7.5 of TEST_PLAN.md).
//!
//! The PAR2 packets that actually get downloaded come from QuickPar and
//! par2cmdline, not from our own builder. To keep the parser honest about the
//! on-the-wire format, this suite hand-assembles packets exactly as the PAR
//! 2.0 specification describes them (`PAR2\0PKT` magic, little-endian numeric
//! fields, `length` covering the whole packet including the header, `hash`
//! over everything but the first three header fields) and checks the parser
//! reads them correctly. It then checks our builder emits the same layout, so
//! sets we write are readable elsewhere.
//!
//! The layout constants are transcribed independently of `src/par2.rs` on
//! purpose: when the two disagree, this is where it shows up.

use md5::{Digest, Md5};
use turbonzb_core::par2::{VerifyStatus, parse_par2_bytes, repair, verify};

const MAGIC: &[u8; 8] = b"PAR2\0PKT";
const T_MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
const T_FILE_DESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
const T_IFSC: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
const T_RECV: &[u8; 16] = b"PAR 2.0\0RecvSlic";
const T_CREATOR: &[u8; 16] = b"PAR 2.0\0Creator\0";

const SLICE: usize = 16 * 1024;

/// Append one spec-shaped packet: 8 magic + 8 length + 16 hash + 16 set id +
/// 16 type + body padded with NULs to a 4-byte boundary.
fn push_packet(out: &mut Vec<u8>, set_id: &[u8; 16], ptype: &[u8; 16], body: &[u8]) {
    let total = 64 + body.len();
    let padded = (total + 3) & !3;
    let padding = padded - total;

    // The hash covers everything except magic, length, and itself.
    let mut h = Md5::new();
    h.update(set_id);
    h.update(ptype);
    h.update(body);
    for _ in 0..padding {
        h.update([0u8]);
    }
    let hash: [u8; 16] = h.finalize().into();

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(padded as u64).to_le_bytes());
    out.extend_from_slice(&hash);
    out.extend_from_slice(set_id);
    out.extend_from_slice(ptype);
    out.extend_from_slice(body);
    out.extend(std::iter::repeat_n(0u8, padding));
}

/// Spec-defined File ID: MD5(hash16k ‖ length ‖ name).
fn spec_file_id(hash16k: &[u8; 16], len: usize, name: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(hash16k);
    h.update((len as u64).to_le_bytes());
    h.update(name);
    h.finalize().into()
}

fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().into()
}

/// A payload spanning three slices (two full, one short) plus the hashes the
/// recovery set records for it.
fn golden_fixture() -> (Vec<u8>, [u8; 16], [u8; 16], [u8; 16]) {
    let content: Vec<u8> = (0u8..=255).cycle().take(2 * SLICE + 7).collect();
    let hash16k = md5(&content[..SLICE]);
    let hashfull = md5(&content);
    let file_id = spec_file_id(&hash16k, content.len(), b"golden.bin");
    (content, hash16k, hashfull, file_id)
}

/// Assemble a spec-shaped set (Creator, FileDesc, IFSC, Main, two recovery
/// slices) the way another implementation would.
fn golden_stream(
    content: &[u8],
    hash16k: &[u8; 16],
    hashfull: &[u8; 16],
    file_id: &[u8; 16],
) -> Vec<u8> {
    // The main packet body first: its MD5 is the set id every packet carries.
    let mut main_body = Vec::new();
    main_body.extend_from_slice(&(SLICE as u64).to_le_bytes()); // blocksize
    main_body.extend_from_slice(&1u32.to_le_bytes()); // recoverable file count
    main_body.extend_from_slice(file_id);
    let set_id = md5(&main_body);

    let mut out = Vec::new();
    push_packet(&mut out, &set_id, T_CREATOR, b"golden-harness");

    let mut desc_body = Vec::new();
    desc_body.extend_from_slice(file_id);
    desc_body.extend_from_slice(hashfull);
    desc_body.extend_from_slice(hash16k);
    desc_body.extend_from_slice(&(content.len() as u64).to_le_bytes());
    desc_body.extend_from_slice(b"golden.bin");
    while desc_body.len() % 4 != 0 {
        desc_body.push(0);
    }
    push_packet(&mut out, &set_id, T_FILE_DESC, &desc_body);

    // IFSC: File ID, then {MD5, CRC32} per slice, short slice zero-padded.
    let mut ifsc_body = Vec::new();
    ifsc_body.extend_from_slice(file_id);
    for slice in content.chunks(SLICE) {
        let mut padded = slice.to_vec();
        padded.resize(SLICE, 0);
        let mut c = crc32fast::Hasher::new();
        c.update(&padded);
        ifsc_body.extend_from_slice(&md5(&padded));
        ifsc_body.extend_from_slice(&c.finalize().to_le_bytes());
    }
    push_packet(&mut out, &set_id, T_IFSC, &ifsc_body);

    push_packet(&mut out, &set_id, T_MAIN, &main_body);
    push_packet(&mut out, &set_id, T_RECV, &[0u8, 0, 0, 0]); // exponent 0
    push_packet(&mut out, &set_id, T_RECV, &[1u8, 0, 0, 0]); // exponent 1
    out
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("turbonzb-par2-golden-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// §7.5 — packets written by another implementation (here: assembled straight
/// from the spec text) parse into the expected set.
#[test]
fn parses_spec_shaped_packet_stream() {
    let (content, hash16k, hashfull, file_id) = golden_fixture();
    let stream = golden_stream(&content, &hash16k, &hashfull, &file_id);

    let set = parse_par2_bytes(&stream).expect("spec-shaped stream must parse");

    assert_eq!(set.slice_size, SLICE as u64);
    assert_eq!(set.recovery_file_ids, vec![file_id]);
    assert_eq!(set.recovery_count, 2, "both recovery slice packets counted");

    let desc = set
        .file_descriptions
        .get(&file_id)
        .expect("file description keyed by the spec-defined file id");
    assert_eq!(desc.filename, "golden.bin");
    assert_eq!(desc.length, content.len() as u64);
    assert_eq!(desc.md5_full, hashfull);
    assert_eq!(desc.md5_16k, hash16k);
    assert_eq!(desc.file_id, file_id);

    let checksums = set
        .slice_checksums
        .get(&file_id)
        .expect("IFSC present for the file id");
    assert_eq!(checksums.len(), 3, "two full slices plus one short slice");
    assert_eq!(checksums[0].md5, hash16k, "slice 0 MD5 is the 16k MD5");

    // Every packet shares the main packet body's MD5 as its set id.
    assert_eq!(set.set_id, md5_of_main_body(&[file_id]));
}

fn md5_of_main_body(ids: &[[u8; 16]]) -> [u8; 16] {
    let mut body = Vec::new();
    body.extend_from_slice(&(SLICE as u64).to_le_bytes());
    body.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        body.extend_from_slice(id);
    }
    md5(&body)
}

/// Verify on disk: content-based matching must accept the file even when its
/// on-disk name differs from the name recorded in the PAR2 set.
#[test]
fn verify_matches_renamed_file_from_spec_stream() {
    let (content, hash16k, hashfull, file_id) = golden_fixture();
    let stream = golden_stream(&content, &hash16k, &hashfull, &file_id);
    let set = parse_par2_bytes(&stream).unwrap();

    let dir = temp_dir("rename");
    std::fs::write(dir.join("hashed-name.bin"), &content).unwrap();

    let report = verify(&set, &dir);
    assert_eq!(report.healthy, 1);
    assert_eq!(report.damaged, 0);
    assert_eq!(report.missing, 0);
    assert_eq!(
        report.files,
        vec![("golden.bin".to_string(), VerifyStatus::Ok)]
    );
    // The match records where the bytes actually were.
    assert_eq!(report.matches.len(), 1);
    assert_eq!(report.matches[0].1, "golden.bin");
    assert!(report.matches[0].0.ends_with("hashed-name.bin"));
}

/// Identical content in two files: each recovery-set entry must consume a
/// distinct candidate, so neither file is reported missing.
#[test]
fn duplicate_content_matches_distinct_entries() {
    let content: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
    let set = turbonzb_core::par2::build_par2_set(&[
        (content.as_slice(), "a.bin"),
        (content.as_slice(), "b.bin"),
    ]);
    let set = parse_par2_bytes(&set).unwrap();
    assert_eq!(set.recovery_file_ids.len(), 2);

    let dir = temp_dir("dupes");
    std::fs::write(dir.join("a.bin"), &content).unwrap();
    std::fs::write(dir.join("b.bin"), &content).unwrap();

    let report = verify(&set, &dir);
    assert_eq!(report.healthy, 2, "both entries must find their file");
    assert_eq!(report.missing, 0);
}

/// Builder output must satisfy a strict reader: magic at every packet start,
/// length covering header + padded body, hash covering everything after the
/// length field, one shared set id, and packets tiling the buffer exactly.
#[test]
fn builder_emits_spec_shaped_packets() {
    let content: Vec<u8> = (0u8..=200).cycle().take(1000).collect();
    let stream = turbonzb_core::par2::build_par2_set(&[(content.as_slice(), "built.bin")]);

    let mut pos = 0usize;
    let mut seen_types = Vec::new();
    let mut set_ids = Vec::new();
    while pos + 64 <= stream.len() {
        assert_eq!(&stream[pos..pos + 8], MAGIC, "packet magic at {pos}");
        let len = u64::from_le_bytes(stream[pos + 8..pos + 16].try_into().unwrap()) as usize;
        assert!(len >= 64, "length covers at least the header");
        assert!(
            pos + len <= stream.len(),
            "packet must not overrun the buffer"
        );
        assert_eq!(len % 4, 0, "packets are padded to a 4-byte boundary");

        let hash: [u8; 16] = stream[pos + 16..pos + 32].try_into().unwrap();
        let set_id: [u8; 16] = stream[pos + 32..pos + 48].try_into().unwrap();
        let ptype: [u8; 16] = stream[pos + 48..pos + 64].try_into().unwrap();
        let body = &stream[pos + 64..pos + len];

        let mut h = Md5::new();
        h.update(set_id);
        h.update(ptype);
        h.update(body);
        assert_eq!(hash, <[u8; 16]>::from(h.finalize()), "hash field at {pos}");

        seen_types.push(ptype);
        set_ids.push(set_id);
        pos += len;
    }
    assert_eq!(pos, stream.len(), "packets must tile the buffer exactly");

    for expected in [T_MAIN, T_FILE_DESC, T_IFSC, T_RECV, T_CREATOR] {
        assert!(
            seen_types.contains(expected),
            "missing packet type {expected:?}"
        );
    }
    assert!(
        set_ids.windows(2).all(|w| w[0] == w[1]),
        "all packets must share one set id"
    );
}

/// The builder's set id must be MD5(main body) and its File IDs must follow
/// the spec definition, so a foreign writer and our parser agree on grouping.
#[test]
fn builder_set_id_and_file_ids_follow_spec() {
    let content: Vec<u8> = (0u8..=99).cycle().take(300).collect();
    let stream = turbonzb_core::par2::build_par2_set(&[(content.as_slice(), "set.bin")]);
    let set = parse_par2_bytes(&stream).unwrap();

    let hash16k = md5(&content[..content.len().min(SLICE)]);
    let file_id = spec_file_id(&hash16k, content.len(), b"set.bin");

    assert_eq!(set.recovery_file_ids, vec![file_id], "File ID per spec");
    assert_eq!(
        set.set_id,
        md5_of_main_body(&[file_id]),
        "set id == MD5(main body)"
    );

    let desc = set.file_descriptions.get(&file_id).expect("description");
    assert_eq!(desc.filename, "set.bin");
    assert_eq!(desc.length, content.len() as u64);
    assert_eq!(desc.md5_16k, hash16k);
    assert_eq!(desc.md5_full, md5(&content));
}

/// Leading junk, a truncated tail, and an absurd length field must all still
/// yield whatever is parseable rather than an error or a panic.
#[test]
fn tolerates_junk_prefix_and_truncated_tail() {
    let content: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
    let stream = turbonzb_core::par2::build_par2_set(&[(content.as_slice(), "tol.bin")]);

    let mut with_prefix = b"\0\0 junk \0".to_vec();
    with_prefix.extend_from_slice(&stream);
    let set = parse_par2_bytes(&with_prefix).unwrap();
    assert_eq!(set.slice_size, SLICE as u64);
    assert_eq!(set.recovery_file_ids.len(), 1);

    // Everything after the first packet cut off.
    let first_len = u64::from_le_bytes(stream[8..16].try_into().unwrap()) as usize;
    let set = parse_par2_bytes(&stream[..first_len]).unwrap();
    assert_eq!(set.recovery_file_ids.len(), 1, "main packet still parsed");

    // A length field pointing past the buffer is skipped, not trusted.
    let mut broken = stream.clone();
    broken[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    let set = parse_par2_bytes(&broken).unwrap();
    assert!(!set.file_descriptions.is_empty());

    let set = parse_par2_bytes(&[]).unwrap();
    assert!(set.recovery_file_ids.is_empty());
}

/// Repair against a spec-shaped stream whose recovery slices carry real data.
#[test]
fn repair_of_damaged_slice_is_byte_identical() {
    let content: Vec<u8> = (0u8..=255).cycle().take(2 * SLICE + 7).collect();
    let set_bytes = turbonzb_core::par2::build_par2_set(&[(content.as_slice(), "golden.bin")]);
    let set = parse_par2_bytes(&set_bytes).unwrap();

    let dir = temp_dir("repair");
    // Damage the second slice only: the leading 16 kB still matches the
    // File Description, which is what makes the file repairable rather than
    // an unrecognized loss.
    let mut damaged = content.clone();
    for b in damaged[SLICE..2 * SLICE].iter_mut() {
        *b ^= 0xA5;
    }
    std::fs::write(dir.join("golden.bin"), &damaged).unwrap();

    let before = verify(&set, &dir);
    assert_eq!(before.damaged, 1, "second-slice damage is still detectable");
    assert_eq!(before.missing, 0);
    assert!(before.repairable);

    let report = repair(&set, &dir).unwrap();
    assert!(
        report.total_slices_repaired >= 1,
        "one bad slice must be rebuilt"
    );
    assert_eq!(std::fs::read(dir.join("golden.bin")).unwrap(), content);
    assert_eq!(verify(&set, &dir).healthy, 1);
}
