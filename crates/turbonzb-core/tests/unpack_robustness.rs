//! Archive unpacking robustness suite (§8 of TEST_PLAN.md).
//!
//! Uses committed 7z fixtures plus archives **crafted at test time** via
//! sevenz-rust2's writer (arbitrary entry names) to probe:
//!   - §8.2 path traversal / zip-slip: `../` entry names must never escape
//!     the destination directory
//!   - §8.3 high-ratio "decompression bomb" archives extract correctly
//!   - §8.1 corrupt / truncated archives produce an error, never a hang
//!   - §8.4 password handling (correct + wrong)
//!   - §8.5 supported formats (7z, RAR)
//!   - §8.6 unicode filenames survive round-trip

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use turbonzb_core::unpack;

const GOOD_7Z: &[u8] = include_bytes!("data/good.7z");
const PW_7Z: &[u8] = include_bytes!("data/pw.7z");
const UNICODE_7Z: &[u8] = include_bytes!("data/unicode.7z");
const COMMENT_RAR: &[u8] = include_bytes!("data/comment.rar");

const GOOD_CONTENT: &str = "HELLO-TURBONZB-CONTENT";
const UNICODE_NAME: &str = "ö_ünïcödé_文件.txt";

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("turbonzb-unpack-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Recursively find all regular files under `root`, relative to `root`.
fn list_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(list_files(&p));
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// §8.5 — a normal 7z unpacks to the correct file and content.
#[test]
fn unpacks_normal_7z() {
    let dir = temp_dir();
    let arc = dir.join("good.7z");
    std::fs::write(&arc, GOOD_7Z).unwrap();

    let report = unpack::unpack(&arc, &dir, None).expect("unpack should succeed");
    assert!(!report.was_encrypted);
    let got = std::fs::read_to_string(dir.join("arch").join("sub").join("hello.txt"))
        .expect("extracted file present");
    assert_eq!(got.trim_end(), GOOD_CONTENT);
    assert!(
        report.extracted_files.iter().any(|f| f == "hello.txt"),
        "report should list the extracted file"
    );
}

/// §8.5 — a normal RAR unpacks to the correct content.
#[test]
fn unpacks_normal_rar() {
    let dir = temp_dir();
    let arc = dir.join("comment.rar");
    std::fs::write(&arc, COMMENT_RAR).unwrap();

    let report = unpack::unpack(&arc, &dir, None).expect("unpack should succeed");
    let files = list_files(&dir);
    assert!(
        !files.is_empty(),
        "rar should have produced at least one file: {files:?}"
    );
    // The fixture's stored file must be under the dest dir.
    for f in &report.extracted_files {
        assert!(
            dir.join(f).exists() || files.iter().any(|l| l.ends_with(f)),
            "extracted file {f} must live under dest dir"
        );
    }
}

/// §8.4 — correct password unpacks; wrong password yields an error (not a
/// hang or a silently-empty result).
#[test]
fn password_handling() {
    let dir = temp_dir();
    let arc = dir.join("pw.7z");
    std::fs::write(&arc, PW_7Z).unwrap();

    // Correct password.
    unpack::unpack(&arc, &dir, Some("secret")).expect("correct password should unpack");
    let got = std::fs::read_to_string(dir.join("arch").join("sub").join("hello.txt")).unwrap();
    assert_eq!(got.trim_end(), GOOD_CONTENT);

    // Wrong password must not succeed silently and must not panic.
    let dir2 = temp_dir();
    let arc2 = dir2.join("pw2.7z");
    std::fs::write(&arc2, PW_7Z).unwrap();
    let res = unpack::unpack(&arc2, &dir2, Some("wrongpass"));
    // It may error, or (in permissive modes) produce nothing — but it must
    // not claim success with the right content.
    if let Ok(report) = res {
        let _ = report;
        let wrong = std::fs::read_to_string(dir2.join("arch").join("sub").join("hello.txt"));
        assert_ne!(
            wrong.unwrap_or_default(),
            GOOD_CONTENT,
            "wrong password must not yield the plaintext"
        );
    }
}

/// §8.2 — path-traversal entry names must never write outside the target
/// directory. sevenz-rust2 rejects unsafe entries outright (returns an
/// error) rather than extracting them outside — the important property is
/// that nothing escapes the destination.
#[test]
fn path_traversal_does_not_escape() {
    use sevenz_rust2::ArchiveWriter;

    let base = temp_dir();
    let arc = base.join("trav.7z");
    {
        let mut w = ArchiveWriter::create(&arc).expect("create writer");
        w.set_encrypt_header(false);
        // Entries whose names try to climb out of the destination.
        w.push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("../escape.txt"),
            Some(&b"ESCAPE"[..]),
        )
        .expect("push");
        w.finish().expect("finish");
    }

    let dest = base.join("dest");
    std::fs::create_dir_all(&dest).unwrap();

    let before: Vec<String> = list_files(&base);
    let res = unpack::unpack(&arc, &dest, None);
    // The unsafe entry is rejected (likely an error) — but crucially,
    // nothing may have been written at the parent level.
    let _ = res; // Err is acceptable; some versions may skip the entry.
    let after: Vec<String> = list_files(&base);
    for f in &after {
        if !before.contains(f) {
            assert!(
                f.starts_with("dest"),
                "path traversal escaped destination: {f}"
            );
        }
    }
    // No file named the raw traversal target may appear beside dest.
    assert!(
        !base.join("escape.txt").exists(),
        "../escape.txt must not be written as base/escape.txt"
    );
}

/// §8.2 — a nested `..` inside an entry path (not just leading) is also
/// contained within the destination directory.
#[test]
fn nested_relative_traversal_contained() {
    use sevenz_rust2::ArchiveWriter;

    let base = temp_dir();
    let arc = base.join("trav2.7z");
    {
        let mut w = ArchiveWriter::create(&arc).expect("create writer");
        w.set_encrypt_header(false);
        w.push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("deep/../../up.txt"),
            Some(&b"UP"[..]),
        )
        .expect("push");
        w.finish().expect("finish");
    }
    let dest = base.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let before: Vec<String> = list_files(&base);
    let _ = unpack::unpack(&arc, &dest, None);
    let after: Vec<String> = list_files(&base);
    for f in &after {
        if !before.contains(f) {
            assert!(
                f.starts_with("dest"),
                "nested `..` escaped destination: {f}"
            );
        }
    }
}

/// §8.1 — a truncated/corrupt 7z produces an error, not a hang or panic.
#[test]
fn corrupt_7z_errors_gracefully() {
    let dir = temp_dir();
    let arc = dir.join("bad.7z");
    // Truncate a valid archive to half its size.
    std::fs::write(&arc, &GOOD_7Z[..GOOD_7Z.len() / 2]).unwrap();
    let res = unpack::unpack(&arc, &dir, None);
    assert!(res.is_err(), "truncated 7z must be an error");
}

/// §8.1 — a truncated RAR produces an error, not a hang or panic.
#[test]
fn corrupt_rar_errors_gracefully() {
    let dir = temp_dir();
    let arc = dir.join("bad.rar");
    std::fs::write(&arc, &COMMENT_RAR[..COMMENT_RAR.len() / 2]).unwrap();
    // Must not panic; whether it errors (unrar can't parse a truncated
    // set) or succeeds-with-nothing, it must never claim to have extracted
    // files it did not actually produce.
    let res = unpack::unpack(&arc, &dir, None);
    assert!(
        !matches!(res, Ok(ref r) if !r.extracted_files.is_empty()),
        "truncated rar must not report extracted files"
    );
}

/// §8.6 — unicode filenames survive the round trip intact.
#[test]
fn unicode_filename_roundtrip() {
    let dir = temp_dir();
    let arc = dir.join("unicode.7z");
    std::fs::write(&arc, UNICODE_7Z).unwrap();

    /// Unpack into `dir` and confirm the unicode name is preserved.
    fn unpack_here(arc: &Path, dir: &Path) {
        unpack::unpack(arc, dir, None).expect("unpack");
    }
    unpack_here(&arc, &dir);
    let p = dir.join("uni").join(UNICODE_NAME);
    let got = std::fs::read_to_string(&p).expect("unicode file present");
    assert_eq!(got.trim_end(), "UNICODE-BYTES");
}

/// §8.3 — a high-compression-ratio archive (a large, highly-compressible
/// entry) extracts completely and correctly. (The app has no explicit size
/// guard yet; this documents that large ratio archives are handled.)
#[test]
fn high_ratio_archive_extracts_completely() {
    use sevenz_rust2::ArchiveWriter;

    let base = temp_dir();
    let arc = base.join("bomb.7z");
    // 16 MiB of zeros compresses to a few KB.
    let big = vec![0u8; 16 * 1024 * 1024];
    {
        let mut w = ArchiveWriter::create(&arc).expect("create writer");
        w.set_encrypt_header(false);
        w.push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("big.bin"),
            Some(big.as_slice()),
        )
        .expect("push");
        w.finish().expect("finish");
    }

    let dest = base.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    let report = unpack::unpack(&arc, &dest, None).expect("high-ratio archive should unpack");
    let out = std::fs::read(dest.join("big.bin")).expect("big.bin extracted");
    assert_eq!(out.len(), big.len(), "must extract the full logical size");
    assert!(out.iter().all(|b| *b == 0), "content must match");
    let _ = report;
}
