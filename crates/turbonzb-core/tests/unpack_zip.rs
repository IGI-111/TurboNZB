//! Split-archive and extension-handling coverage (§8.5, §8.8 of TEST_PLAN.md),
//! plus the documented ZIP policy.
//!
//! These complement `unpack_robustness.rs` (formats, passwords, traversal,
//! bombs) with the parts of `unpack()` that are pure filename logic, which is
//! where real-world Usenet releases actually break: numbered split archives,
//! uppercase extensions, and archives whose extension says one thing while
//! the bytes say another.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use turbonzb_core::unpack;

const GOOD_7Z: &[u8] = include_bytes!("data/good.7z");

const GOOD_CONTENT: &str = "HELLO-TURBONZB-CONTENT";

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("turbonzb-split-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn read_out(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("extracted file present")
        .trim_end()
        .to_string()
}

/// §8.5 — a split 7z (`.7z.001`, `.7z.002`, …) is reassembled in numeric
/// order and unpacked as one archive.
#[test]
fn split_7z_is_reassembled_in_order() {
    let dir = temp_dir();
    let mid = GOOD_7Z.len() / 2;
    std::fs::write(dir.join("good.7z.001"), &GOOD_7Z[..mid]).unwrap();
    std::fs::write(dir.join("good.7z.002"), &GOOD_7Z[mid..]).unwrap();

    let report = unpack::unpack(&dir.join("good.7z.001"), &dir, None).expect("split set unpacks");

    assert!(!report.was_encrypted, "a plain 7z set is not encrypted");
    assert_eq!(
        read_out(&dir.join("arch").join("sub").join("hello.txt")),
        GOOD_CONTENT
    );
    assert!(
        !dir.join(".turbonzb-7z-tmp.dat").exists(),
        "the concatenated temp file is cleaned up"
    );
}

/// §8.5 — a split set whose parts stop early still unpacks from what is on
/// disk: enumeration stops at the first missing number rather than failing.
#[test]
fn split_7z_with_single_part_unpacks() {
    let dir = temp_dir();
    std::fs::write(dir.join("solo.7z.001"), GOOD_7Z).unwrap();

    unpack::unpack(&dir.join("solo.7z.001"), &dir, None).expect("one-part split set unpacks");
    assert_eq!(
        read_out(&dir.join("arch").join("sub").join("hello.txt")),
        GOOD_CONTENT
    );
}

/// A later part handed to the unpacker on its own cannot be decoded in
/// isolation, and says so instead of producing a half-populated tree.
#[test]
fn later_split_part_is_reported_not_guessed() {
    let dir = temp_dir();
    std::fs::write(dir.join("set.7z"), GOOD_7Z).unwrap();
    // Rename to a non-first part: `.002` is not the archive start.
    std::fs::rename(dir.join("set.7z"), dir.join("set.7z.002")).unwrap();

    let err = unpack::unpack(&dir.join("set.7z.002"), &dir, None)
        .expect_err("a middle part alone cannot be unpacked");
    match err {
        unpack::UnpackError::Unsupported(msg) => {
            assert!(msg.contains(".002"), "the message names the part: {msg}");
        }
        other => panic!("expected an unsupported-format error, got {other:?}"),
    }
    // The destination must be left untouched by the rejected part.
    assert!(!dir.join("arch").exists());
}

/// §8.8 — volume-part detection is case-insensitive and only matches the
/// `.rNN` convention, never the main `.rar` or a four-digit counter.
#[test]
fn volume_part_names_are_recognized_case_insensitively() {
    assert!(unpack::is_rar_part_name("movie.r00"));
    assert!(
        unpack::is_rar_part_name("movie.R00"),
        "extensions are matched case-insensitively"
    );
    assert!(unpack::is_rar_part_name("Movie Set.R99"));

    // The main volume and non-volume suffixes are not parts.
    assert!(!unpack::is_rar_part_name("movie.rar"));
    assert!(!unpack::is_rar_part_name("movie.part01.rar"));
    assert!(!unpack::is_rar_part_name("movie.r0"));
    assert!(!unpack::is_rar_part_name("movie"));

    // Discontinuities in numbering are still parts: a set may start anywhere.
    assert!(unpack::is_rar_part_name("set.r42"));
}

/// §8 — ZIP is deliberately not implemented; a `.zip` name is a clean,
/// readable error rather than a silent no-op or a panic.
#[test]
fn zip_extension_reports_unsupported_cleanly() {
    let dir = temp_dir();
    let archive = dir.join("release.zip");
    std::fs::write(&archive, b"PK\x03\x04 stub archive").unwrap();

    let err = unpack::unpack(&archive, &dir.join("out"), None)
        .expect_err("zip is not a supported format");
    match err {
        unpack::UnpackError::Unsupported(msg) => {
            assert!(
                msg.contains("zip"),
                "the message names the extension: {msg}"
            );
        }
        other => panic!("expected an unsupported-format error, got {other:?}"),
    }
}

/// An extension is not evidence of format: detection reads the header bytes,
/// so a text file wearing a `.rar` name is not mistaken for an archive,
/// while both real RAR generations and 7z are recognized.
#[test]
fn format_detection_reads_content_not_extension() {
    let dir = temp_dir();

    let rar = dir.join("real.rar");
    std::fs::write(&rar, b"Rar!\x1a\x07\x01\x00 payload").unwrap();
    assert!(unpack::is_rar_data(&rar));
    assert!(unpack::is_archive_data(&rar));

    let rar4 = dir.join("old.rar");
    std::fs::write(&rar4, b"Rar!\x1a\x07\x00 payload").unwrap();
    assert!(unpack::is_rar_data(&rar4), "RAR4 magic is recognized too");

    let seven = dir.join("real.7z");
    std::fs::write(&seven, GOOD_7Z).unwrap();
    assert!(unpack::is_7z_data(&seven));
    assert!(unpack::is_archive_data(&seven));
    assert!(!unpack::is_rar_data(&seven), "7z is not a RAR");

    // Cloaked names: `.rar` suffix on plain text, and an empty file.
    let text = dir.join("cloak.rar");
    std::fs::write(&text, b"a plain text file with a .rar name").unwrap();
    assert!(!unpack::is_rar_data(&text));
    assert!(!unpack::is_archive_data(&text));

    let empty = dir.join("empty.rar");
    std::fs::write(&empty, "").unwrap();
    assert!(!unpack::is_rar_data(&empty));

    // Missing files are a non-match rather than a panic.
    assert!(!unpack::is_rar_data(&dir.join("absent.rar")));
}

/// A real RAR set unpacks end to end from the `.rar` main volume, which is
/// what the volume normalization is supposed to guarantee.
#[test]
fn multi_volume_rar_set_unpacks_from_main_volume() {
    const SET_RAR: &[u8] = include_bytes!("data/set.rar");
    const SET_R00: &[u8] = include_bytes!("data/set.r00");

    let dir = temp_dir();
    std::fs::write(dir.join("set.rar"), SET_RAR).unwrap();
    std::fs::write(dir.join("set.r00"), SET_R00).unwrap();

    let out = dir.join("out");
    let report = unpack::unpack(&dir.join("set.rar"), &out, None).expect("volume set unpacks");
    assert!(
        !report.extracted_files.is_empty(),
        "the report lists extracted files"
    );
    assert!(
        report.extracted_files.iter().any(|f| f.ends_with(".txt")),
        "extracted entries are text payloads: {:?}",
        report.extracted_files
    );
}
