//! RAR volume handling for deobfuscated posts: volume-aware naming and
//! multi-volume extraction.
//!
//! Fixtures:
//! - `set.rar` + `set.r00`: a genuine RAR 5.0 two-volume set with a
//!   single stored file split across both volumes (validated with `unrar t`).
//! - `comment.rar`: a single-part archive.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use turbonzb_core::par2;
use turbonzb_core::postprocess::{PostProcessConfig, PostProcessStatus, post_process};
use turbonzb_core::unpack;

const FIRST_VOL: &[u8] = include_bytes!("data/set.rar"); // volume 1 (first)
const SUBSEQUENT_VOL: &[u8] = include_bytes!("data/set.r00"); // volume 2 (not first)
const SINGLE: &[u8] = include_bytes!("data/comment.rar");

const FULL_PAYLOAD: &str = "Hello world! This is a really long payload that is split across two \
    volumes to test multi-volume extraction end to end.";

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("turbonzb-rar-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn is_rar_part_name_rejects_main_volume_and_others() {
    assert!(unpack::is_rar_part_name("release.r00"));
    assert!(unpack::is_rar_part_name("release.r99"));
    assert!(unpack::is_rar_part_name("file.R01"));
    assert!(!unpack::is_rar_part_name("release.rar"));
    assert!(!unpack::is_rar_part_name("release.7z"));
    assert!(!unpack::is_rar_part_name("release.par2"));
    assert!(!unpack::is_rar_part_name("release.000.rar"));
    assert!(!unpack::is_rar_part_name("release"));
}

#[test]
fn normalize_renames_volume_set_to_rar_convention() {
    // Deobfuscated generic names (what the engine produces when no real
    // name is available): release.000.rar (first volume) + release.001.rar
    // (subsequent volume). After normalization the set must follow the
    // standard `stem.rar` / `stem.r00` convention.
    let dir = temp_dir();
    std::fs::write(dir.join("release.000.rar"), FIRST_VOL).unwrap();
    std::fs::write(dir.join("release.001.rar"), SUBSEQUENT_VOL).unwrap();

    unpack::normalize_rar_volumes(&dir, "Mr.Robot.S04").unwrap();

    assert!(
        dir.join("Mr.Robot.S04.rar").exists(),
        "first volume must be renamed to Mr.Robot.S04.rar"
    );
    assert!(
        dir.join("Mr.Robot.S04.r00").exists(),
        "second volume must be renamed to Mr.Robot.S04.r00"
    );
    assert!(
        !dir.join("release.000.rar").exists(),
        "original generic name must be gone"
    );
}

#[test]
fn normalize_leaves_independent_archives_alone() {
    let dir = temp_dir();
    std::fs::write(dir.join("lone.000.rar"), SINGLE).unwrap();

    unpack::normalize_rar_volumes(&dir, "Mr.Robot").unwrap();

    assert!(
        dir.join("lone.000.rar").exists(),
        "independent rar untouched"
    );
}

#[test]
fn unpack_follows_volume_set_and_reconstructs_file() {
    // A complete set, named per the conventional volume scheme. The unrar
    // library follows the sibling volume on its own; the split file must
    // be reconstructed in full.
    let dir = temp_dir();
    std::fs::write(dir.join("set.rar"), FIRST_VOL).unwrap();
    std::fs::write(dir.join("set.r00"), SUBSEQUENT_VOL).unwrap();

    let out = dir.join("out");
    let report = unpack::unpack(&dir.join("set.rar"), &out, None).unwrap();
    assert_eq!(report.extracted_files, vec!["hello.txt"]);

    let got = std::fs::read(out.join("hello.txt")).unwrap();
    assert_eq!(
        got,
        FULL_PAYLOAD.as_bytes(),
        "split file fully reconstructed"
    );
}

#[test]
fn unpack_incomplete_volume_set_is_graceful() {
    // The second volume is missing — unpacking must not fail the whole
    // job; it returns what it can.
    let dir = temp_dir();
    std::fs::write(dir.join("set.rar"), FIRST_VOL).unwrap();

    let out = dir.join("out");
    let report = unpack::unpack(&dir.join("set.rar"), &out, None).unwrap();
    assert!(report.extracted_files.is_empty());
}

#[test]
fn unpack_single_archive_extracts_once() {
    let dir = temp_dir();
    std::fs::write(dir.join("s.rar"), SINGLE).unwrap();

    let out = dir.join("out");
    let report = unpack::unpack(&dir.join("s.rar"), &out, None).unwrap();
    let entries = report
        .extracted_files
        .iter()
        .filter(|f| f.as_str() == ".gitignore")
        .count();
    assert_eq!(entries, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_process_fixes_obfuscated_rar_set_with_par2() {
    // The most complete obfuscated scenario: a deobfuscated RAR volume
    // set PLUS a PAR2 that knows the real volume names. Verification must
    // match by content (files are renamed), then restore the real names,
    // then unpack the whole set and move the result out.
    let dir = temp_dir();
    let completed = temp_dir();
    std::fs::write(dir.join("release.000.rar"), FIRST_VOL).unwrap();
    std::fs::write(dir.join("release.001.rar"), SUBSEQUENT_VOL).unwrap();
    let par2 = par2::build_par2_set(&[(FIRST_VOL, "set.rar"), (SUBSEQUENT_VOL, "set.r00")]);
    std::fs::write(dir.join("release.002.par2"), &par2).unwrap();

    let cfg = PostProcessConfig {
        download_dir: dir.clone(),
        completed_dir: completed.clone(),
        category: None,
        cleanup_archives: false, // keep the archives so we can assert renames
        archive_password: None,
        skip_verify: false,
    };
    let report = post_process(cfg).await.unwrap();
    assert_eq!(report.status, PostProcessStatus::Complete);
    assert_eq!(report.verify.as_ref().map(|v| v.healthy), Some(2));
    assert_eq!(report.verify.as_ref().map(|v| v.missing), Some(0));

    // Real volume names restored before unpacking.
    assert!(
        dir.join("set.rar").exists(),
        "main volume renamed to set.rar"
    );
    assert!(dir.join("set.r00").exists(), "volume 2 renamed to set.r00");
    assert!(
        !dir.join("release.000.rar").exists(),
        "generic volume name must be gone"
    );

    let got = std::fs::read(completed.join("hello.txt"))
        .expect("unpacked file must be moved to completed dir");
    assert_eq!(got, FULL_PAYLOAD.as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_process_part_rar_multivolume_set() {
    // Regression: obfuscated post whose volumes use the `.partNN.rar`
    // convention (as real "TyHD"-style releases do). Post-processing must
    // unpack ONLY the first part — unrar follows the rest automatically —
    // instead of unpacking each part separately (which yields a
    // "File CRC error" for every continuation volume).
    let dir = temp_dir();
    let completed = temp_dir();
    std::fs::write(dir.join("Show.S01E08.part01.rar"), FIRST_VOL).unwrap();
    std::fs::write(dir.join("Show.S01E08.part02.rar"), SUBSEQUENT_VOL).unwrap();

    let cfg = PostProcessConfig {
        download_dir: dir.clone(),
        completed_dir: completed.clone(),
        category: None,
        cleanup_archives: true,
        archive_password: None,
        skip_verify: true,
    };
    let report = post_process(cfg).await.unwrap();
    assert_eq!(report.status, PostProcessStatus::UnpackedWithoutVerify);

    let got = std::fs::read(completed.join("hello.txt"))
        .expect("unpacked file must be moved to completed dir");
    assert_eq!(got, FULL_PAYLOAD.as_bytes(), "volumes followed and joined");

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "parts must be cleaned up: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_process_fixes_obfuscated_rar_set() {
    // A deobfuscated RAR set (no PAR2): generic names from the engine.
    // Post-processing must normalize the volumes, unpack them, and move the
    // extracted file out — all without marking the job damaged.
    let dir = temp_dir();
    let completed = temp_dir();
    std::fs::write(dir.join("release.000.rar"), FIRST_VOL).unwrap();
    std::fs::write(dir.join("release.001.rar"), SUBSEQUENT_VOL).unwrap();

    let cfg = PostProcessConfig {
        download_dir: dir.clone(),
        completed_dir: completed.clone(),
        category: None,
        cleanup_archives: true,
        archive_password: None,
        skip_verify: true,
    };
    let report = post_process(cfg).await.unwrap();
    assert_eq!(
        report.status,
        turbonzb_core::postprocess::PostProcessStatus::UnpackedWithoutVerify
    );

    let got = std::fs::read(completed.join("hello.txt"))
        .expect("unpacked file must be moved to completed dir");
    assert_eq!(got, FULL_PAYLOAD.as_bytes());

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "rar volumes must be cleaned up: {leftovers:?}"
    );
}
