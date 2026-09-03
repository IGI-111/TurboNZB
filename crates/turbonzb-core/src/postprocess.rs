//! Post-processing pipeline: after download completes, verify PAR2 health,
//! unpack archives, move files to category folders, and clean up.
//!
//! M4 scope: verify-only (no repair), unpack, category folders, cleanup.
//! If PAR2 verification reports damaged files, the download is marked as
//! "damaged — manual repair needed" and unpacking is skipped.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::error::{CoreError, Result};
use crate::par2::{self, VerifyReport};
use crate::unpack::{self, UnpackError, UnpackReport};

/// Configuration for post-processing.
#[derive(Debug, Clone)]
pub struct PostProcessConfig {
    /// Directory where downloaded files live.
    pub download_dir: PathBuf,
    /// Directory to move unpacked files to (e.g. ~/Downloads/turbonzb/tv).
    pub completed_dir: PathBuf,
    /// Category subfolder (e.g. "tv", "movies"). If empty, no subfolder.
    pub category: Option<String>,
    /// Whether to delete archive files and .parts dirs after successful unpack.
    pub cleanup_archives: bool,
    /// Password for encrypted archives (if any).
    pub archive_password: Option<String>,
    /// Whether to skip PAR2 verification (e.g. if no PAR2 files exist).
    pub skip_verify: bool,
}

/// Result of the full post-processing pipeline.
#[derive(Debug, Clone)]
pub struct PostProcessReport {
    /// PAR2 verification result (if run).
    pub verify: Option<VerifyReport>,
    /// Unpack result (if unpacking was attempted).
    pub unpack: Option<UnpackReport>,
    /// Final status.
    pub status: PostProcessStatus,
    /// Where the final files ended up.
    pub final_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostProcessStatus {
    /// Everything verified and unpacked successfully.
    Complete,
    /// PAR2 verification found damaged/missing files — unpack skipped.
    Damaged {
        healthy: u32,
        damaged: u32,
        missing: u32,
    },
    /// No PAR2 files found, unpacked without verification.
    UnpackedWithoutVerify,
    /// No archives found, files are already in final location.
    NoArchives,
    /// Unpack failed.
    UnpackFailed(String),
}

/// Run the full post-processing pipeline on a downloaded job directory.
pub async fn post_process(config: PostProcessConfig) -> Result<PostProcessReport> {
    post_process_with_progress(config, None).await
}

/// Like [`post_process`], but reports PAR2 **verification** progress as
/// `(done, total)` bytes to `on_verify` (called on a blocking worker).
/// Useful so the UI can show a verify progress bar instead of an
/// indefinite spinner. Pass `None` to run silently.
pub async fn post_process_with_progress(
    config: PostProcessConfig,
    mut on_verify: Option<Box<dyn FnMut(u64, u64) + Send>>,
) -> Result<PostProcessReport> {
    let download_dir = &config.download_dir;

    // Step 1: Find PAR2 files and verify.
    let par2_files = find_par2_files(download_dir);
    let mut par2_set: Option<par2::Par2File> = None;
    let mut verify_report = if config.skip_verify || par2_files.is_empty() {
        debug!("skipping PAR2 verification (no PAR2 files or skip_verify)");
        None
    } else {
        info!(count = par2_files.len(), "verifying PAR2");
        let set = par2::parse_par2_files(&par2_files)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("PAR2 parse: {e}")))?;

        // Fast ParRename (Pillar 2b): restore real names using only the
        // 16 kB MD5 + length — seconds, not minutes — so full verification
        // below runs against correctly-named files.
        match par2::fast_rename_to_par2_names(&set, download_dir) {
            Ok(renamed) => {
                if renamed > 0 {
                    info!(renamed, "PAR2 fast-rename applied");
                }
            }
            Err(e) => {
                debug!(error = %e, "PAR2 fast-rename failed (continuing)");
            }
        }

        let report = match on_verify.as_mut() {
            Some(f) => par2::verify_with_progress(&set, download_dir, Some(f.as_mut())),
            None => par2::verify(&set, download_dir),
        };
        info!(
            healthy = report.healthy,
            damaged = report.damaged,
            missing = report.missing,
            "PAR2 verify done"
        );
        par2_set = Some(set);
        Some(report)
    };

    // Step 2: if verification reports damage or missing files, attempt
    // auto-repair (Pillar 2a) using the recovery slices *before* giving up.
    if let Some(ref vr) = verify_report {
        if vr.damaged > 0 || vr.missing > 0 {
            if let Some(set) = par2_set.as_ref() {
                info!(
                    damaged = vr.damaged,
                    missing = vr.missing,
                    recovery_slices = vr.recovery_slices,
                    "attempting PAR2 repair"
                );
                match par2::repair(set, download_dir) {
                    Ok(rep) if rep.total_slices_repaired > 0 => {
                        info!(
                            repaired_slices = rep.total_slices_repaired,
                            "PAR2 repair: reconstructing bad slices"
                        );
                        // Re-verify after repair.
                        let re = par2::verify(set, download_dir);
                        info!(
                            healthy = re.healthy,
                            damaged = re.damaged,
                            missing = re.missing,
                            "PAR2 re-verify after repair"
                        );
                        verify_report = Some(re);
                    }
                    Ok(rep) => {
                        warn!(
                            repaired = rep.total_slices_repaired,
                            "PAR2 repair produced no repair"
                        );
                    }
                    Err(e) => {
                        warn!(error = %e, "PAR2 repair failed");
                    }
                }
            }
        }
    }

    // Step 2b: if files are still damaged after repair, skip unpack.
    if let Some(ref vr) = verify_report {
        if vr.damaged > 0 || vr.missing > 0 {
            warn!(
                damaged = vr.damaged,
                missing = vr.missing,
                "files damaged/missing — unpack skipped"
            );
            return Ok(PostProcessReport {
                verify: verify_report.clone(),
                unpack: None,
                status: PostProcessStatus::Damaged {
                    healthy: vr.healthy,
                    damaged: vr.damaged,
                    missing: vr.missing,
                },
                final_dir: download_dir.clone(),
            });
        }
    }

    // Step 3: Restore real file names recorded in the PAR2 set.
    //
    // Obfuscated posts rename files after download (deobfuscation); the PAR2
    // recovery set still knows the true names. Restoring them here gives
    // correct, human-readable names AND proper archive volume names before
    // unpacking. Only runs when verification passed (no early Damaged
    // return above).
    if let Some(ref vr) = verify_report {
        rename_to_par2_names(download_dir, vr)?;
    }

    // Step 3b: Normalize deobfuscated RAR volumes.
    //
    // When no PAR2 provided real names, obfuscated RAR files were named
    // `release.NNN.rar` — not a convention unrar can follow. Rename a
    // multi-volume set to `{release}.rar` + `{release}.r00` … so the whole
    // set can be unpacked.
    let stem = download_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("release");
    unpack::normalize_rar_volumes(download_dir, stem)
        .map_err(|e| CoreError::Other(anyhow::anyhow!("rar normalize: {e}")))?;

    // Step 4: Find archive files and unpack.
    let archives = find_archives(download_dir);
    let unpack_report = if archives.is_empty() {
        debug!("no archives found — files are already in final form");
        None
    } else {
        let unpack_dir = download_dir.join("unpacked");
        let mut last_report = None;
        let mut errors = Vec::new();

        for archive in &archives {
            debug!(archive = %archive.display(), "unpacking");
            match unpack::unpack(archive, &unpack_dir, config.archive_password.as_deref()) {
                Ok(report) => {
                    last_report = Some(report);
                }
                Err(UnpackError::Unsupported(fmt)) => {
                    debug!(format = %fmt, "skipping unsupported archive");
                }
                Err(e) => {
                    errors.push(e.to_string());
                }
            }
        }

        if !errors.is_empty() {
            return Ok(PostProcessReport {
                verify: verify_report,
                unpack: None,
                status: PostProcessStatus::UnpackFailed(errors.join("; ")),
                final_dir: download_dir.clone(),
            });
        }

        last_report
    };

    // Step 5: Move files to category folder.
    let final_dir = if let Some(ref cat) = config.category {
        config.completed_dir.join(cat)
    } else {
        config.completed_dir.clone()
    };
    std::fs::create_dir_all(&final_dir).map_err(CoreError::from)?;

    let source_dir = if archives.is_empty() {
        download_dir.clone()
    } else {
        download_dir.join("unpacked")
    };

    move_files(&source_dir, &final_dir)?;

    // Step 5: Cleanup. Removes archive files, .parts dirs, and par2 files —
    // never the already-final files (e.g. videos) themselves.
    if config.cleanup_archives {
        cleanup_temp_files(download_dir)?;
    }

    let status = if verify_report.is_some() {
        PostProcessStatus::Complete
    } else if archives.is_empty() {
        PostProcessStatus::NoArchives
    } else {
        PostProcessStatus::UnpackedWithoutVerify
    };

    Ok(PostProcessReport {
        verify: verify_report,
        unpack: unpack_report,
        status,
        final_dir,
    })
}

/// Find all .par2 files in a directory (recursively).
fn find_par2_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext.eq_ignore_ascii_case("par2") {
                        files.push(path);
                    }
                }
            }
        }
    }
    files
}

/// Check if a filename is a split archive part (e.g. `.7z.001`, `.7z.002`).
/// Returns true for any `.7z.NNN` suffix where NNN is digits.
fn is_split_archive_part(name: &str) -> bool {
    let name = name.to_lowercase();
    if let Some(stripped) = name.strip_prefix(".7z.") {
        stripped.len() == 3 && stripped.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

/// Check if a filename is the first part of a split 7z archive (`.7z.001`).
fn is_split_7z_first(name: &str) -> bool {
    name.to_lowercase().ends_with(".7z.001")
}

/// Check if a filename is an archive file we should process or skip
/// (rar, rar volumes, 7z, split 7z parts, par2).
fn is_archive_or_par2(name: &str) -> bool {
    let name = name.to_lowercase();
    name.ends_with(".rar")
        || name.ends_with(".7z")
        || is_split_archive_part(&name)
        || rar_volume_base(&name).is_some()
        || name.ends_with(".par2")
}

/// Find all archive files (.rar, .7z, .7z.001) in a directory.
///
/// Multi-volume RAR sets are collapsed to their first volume, which the
/// unrar library then follows by conventional volume naming:
///   - `foo.part01.rar` … `foo.partNN.rar` → only `foo.part01.rar`
///   - `foo.rar` + `foo.r00` … → only `foo.rar`
///
/// Truly independent archives (different base names, no volume parts) are
/// all returned so each one gets unpacked. RAR volume parts (`.rNN`) are
/// never returned themselves — they're followed from the first volume.
/// For split 7z, only the first part (.7z.001) is returned.
fn find_archives(dir: &Path) -> Vec<PathBuf> {
    let mut sevenz: Vec<PathBuf> = Vec::new();
    // `.partNN.rar` volumes: (base, part number, path)
    let mut part_volumes: Vec<(String, u32, PathBuf)> = Vec::new();
    // plain `.rar` files: (base, path)
    let mut rar_mains: Vec<(String, PathBuf)> = Vec::new();
    // `.rNN` parts: (base, path)
    let mut rnn_parts: Vec<(String, PathBuf)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.to_string_lossy().to_lowercase();

            // Skip .par2 files
            if name.ends_with(".par2") {
                continue;
            }

            // Split 7z: only keep .7z.001, skip other parts
            if is_split_7z_first(&name) {
                sevenz.push(path);
                continue;
            }
            if is_split_archive_part(&name) {
                continue;
            }
            if name.ends_with(".7z") {
                sevenz.push(path);
                continue;
            }

            // `.partNN.rar` multi-volume naming.
            if let Some((base, num)) = parse_part_rar(&name) {
                part_volumes.push((base, num, path));
                continue;
            }
            // `.rNN` continuation volumes.
            if let Some(base) = rar_volume_base(&name) {
                rnn_parts.push((base, path));
                continue;
            }
            if name.ends_with(".rar") {
                let base = name.strip_suffix(".rar").unwrap_or(&name).to_string();
                rar_mains.push((base, path));
            }
        }
    }

    let mut archives = sevenz;

    // `.partNN.rar`: keep only the first part (lowest number) per base.
    let mut part_firsts: std::collections::HashMap<String, (u32, PathBuf)> =
        std::collections::HashMap::new();
    for (base, num, path) in part_volumes {
        match part_firsts.get(&base) {
            Some((best_num, _)) if *best_num <= num => {}
            _ => {
                part_firsts.insert(base, (num, path));
            }
        }
    }
    for (_, path) in part_firsts.values() {
        archives.push(path.clone());
    }
    // `.rar` files with `.rNN` siblings are the main volume of a set
    // (returned so unrar can follow); `.rar` files without volume parts
    // are independent archives (each returned).
    let rnn_bases: std::collections::HashSet<String> =
        rnn_parts.into_iter().map(|(base, _)| base).collect();
    for (base, path) in rar_mains {
        if rnn_bases.contains(&base) || !part_firsts.contains_key(&base) {
            archives.push(path);
        }
    }

    archives.sort();
    archives
}

/// If `name` (lowercase) is `basename.part###.rar`, return `(basename,
/// part number)`.
fn parse_part_rar(name: &str) -> Option<(String, u32)> {
    let stem = name.strip_suffix(".rar")?;
    let idx = stem.rfind(".part")?;
    let base = &stem[..idx];
    let numstr = &stem[idx + ".part".len()..];
    if numstr.is_empty() || !numstr.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((base.to_string(), numstr.parse().ok()?))
}

/// If `name` (lowercase) is a RAR volume part (`foo.r00`, `foo.r01`, …),
/// return its base (`foo`).
fn rar_volume_base(name: &str) -> Option<String> {
    let name = name.to_lowercase();
    let bytes = name.as_bytes();
    let len = bytes.len();
    if len >= 4
        && bytes[len - 1].is_ascii_digit()
        && bytes[len - 2].is_ascii_digit()
        && bytes[len - 3] == b'r'
        && bytes[len - 4] == b'.'
    {
        Some(String::from_utf8_lossy(&bytes[..len - 4]).into_owned())
    } else {
        None
    }
}

/// Rename downloaded files to the names recorded in the PAR2 set, but only
/// when they differ from the current on-disk name (deobfuscated posts were
/// renamed after download — the PAR2 knows their true names). Handles
/// collisions with a numeric suffix.
fn rename_to_par2_names(dir: &Path, report: &VerifyReport) -> Result<()> {
    for (src, real) in &report.matches {
        let real = sanitize_par2_name(real);
        if real.is_empty() {
            continue;
        }
        let src_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if src_name == real {
            continue;
        }
        let dest = unique_child(dir, &real);
        if dest == *src {
            continue;
        }
        if src.exists() {
            std::fs::rename(src, &dest).map_err(|e| {
                CoreError::Other(anyhow::anyhow!(
                    "renaming {} -> {}: {e}",
                    src.display(),
                    dest.display()
                ))
            })?;
            debug!(from = %src.display(), to = %dest.display(), "renamed to PAR2 name");
        }
    }
    Ok(())
}

/// Clean a name from a PAR2 File Description packet for use as a file name:
/// take the basename (PAR2 names may contain subdirectory components) and
/// replace anything that could act as a path separator.
fn sanitize_par2_name(name: &str) -> String {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

/// A path to a not-yet-existing child of `dir`, appending a numeric suffix
/// on collision.
fn unique_child(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 2;
    loop {
        let alt = dir.join(format!("{name}.{n}"));
        if !alt.exists() {
            return alt;
        }
        n += 1;
    }
}

/// Move all files from `source_dir` to `dest_dir`.
fn move_files(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    if !source_dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(source_dir).map_err(CoreError::from)? {
        let entry = entry.map_err(CoreError::from)?;
        let path = entry.path();
        let name = entry.file_name();
        let dest = dest_dir.join(&name);

        if path.is_dir() {
            // Don't move the unpacked dir into itself.
            if path == dest_dir {
                continue;
            }
            // Don't move .parts directories (temp).
            if path.extension().and_then(|e| e.to_str()) == Some("parts") {
                continue;
            }
            // Don't move the unpacked dir.
            if path.file_name().and_then(|n| n.to_str()) == Some("unpacked") {
                // Move files from inside unpacked instead.
                move_files(&path, dest_dir)?;
                continue;
            }
        }

        // Don't move archive files, split parts, or par2 files.
        let name_lower = path.to_string_lossy().to_lowercase();
        if is_archive_or_par2(&name_lower) {
            continue;
        }

        // Self-move: when source_dir == dest_dir (in-place post-processing)
        // dest IS path. The old remove-then-rename sequence deleted the
        // file itself and then failed "No such file or directory" —
        // destroying the downloaded payload. Guard on canonical identity:
        // `path == dest` alone misses symlinks / trailing components.
        if path == dest {
            continue;
        }
        if path.exists() && dest.exists() {
            let same = std::fs::canonicalize(&path).ok() == std::fs::canonicalize(&dest).ok();
            if same {
                continue;
            }
        }

        if dest.exists() {
            std::fs::remove_file(&dest).map_err(|e| {
                CoreError::Other(anyhow::anyhow!(
                    "removing existing dest {}: {e}",
                    dest.display()
                ))
            })?;
        }
        std::fs::rename(&path, &dest).map_err(|e| {
            CoreError::Other(anyhow::anyhow!(
                "moving {} -> {}: {e}",
                path.display(),
                dest.display()
            ))
        })?;
        debug!(from = %path.display(), to = %dest.display(), "moved file");
    }

    Ok(())
}

/// Clean up temporary files: .parts directories, archive files, par2 files.
fn cleanup_temp_files(dir: &Path) -> Result<()> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.to_string_lossy().to_lowercase();
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_lowercase();

            if file_name.ends_with(".parts") && path.is_dir() {
                debug!(path = %path.display(), "removing parts dir");
                let _ = std::fs::remove_dir_all(&path);
            } else if is_archive_or_par2(&name) {
                debug!(path = %path.display(), "removing archive/par2");
                let _ = std::fs::remove_file(&path);
            } else if file_name == "unpacked" && path.is_dir() {
                debug!(path = %path.display(), "removing unpacked dir");
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod self_move_tests {
    use super::move_files;

    #[test]
    fn in_place_move_is_a_noop_and_preserves_files() {
        // Regression: with source_dir == dest_dir (nzbkodi post-processes
        // in place), remove-then-rename deleted the payload itself and
        // failed "No such file or directory" — the 57 GB Ghost in the
        // Shell mkv was destroyed this way.
        let tmp = std::env::temp_dir().join("turbonzb-move-in-place-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let movie = tmp.join("movie.mkv");
        std::fs::write(&movie, b"payload").unwrap();
        std::fs::write(tmp.join("set.par2"), b"par2").unwrap();

        move_files(&tmp, &tmp).expect("in-place move must succeed");

        assert_eq!(std::fs::read(&movie).unwrap(), b"payload");
        assert!(tmp.join("set.par2").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tempfile_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("turbonzb-pp-test-{}-{n}", std::process::id()))
    }

    #[test]
    fn find_archives_collapses_part_rar_sets() {
        // `.partNN.rar` multi-volume sets → only the first part is unpacked.
        let dir = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        for n in 1..=5 {
            std::fs::write(dir.join(format!("Show.1080p.part{n:02}.rar")), b"x").unwrap();
        }
        let archives = find_archives(&dir);
        assert_eq!(
            archives,
            vec![dir.join("Show.1080p.part01.rar")],
            "only the first part of a .partNN set is unpacked"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_archives_keeps_independent_rars() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Episode1.rar"), b"x").unwrap();
        std::fs::write(dir.join("Episode2.rar"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let archives = find_archives(&dir);
        // Sorted, both independent archives kept, non-archive ignored.
        assert_eq!(
            archives,
            vec![dir.join("Episode1.rar"), dir.join("Episode2.rar")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_archives_keeps_only_main_of_rn_set() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Set.rar"), b"x").unwrap();
        std::fs::write(dir.join("Set.r00"), b"x").unwrap();
        std::fs::write(dir.join("Set.r01"), b"x").unwrap();
        let archives = find_archives(&dir);
        assert_eq!(archives, vec![dir.join("Set.rar")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_archives_mixes_independent_and_part_set() {
        let dir = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Movie.part01.rar"), b"x").unwrap();
        std::fs::write(dir.join("Movie.part02.rar"), b"x").unwrap();
        std::fs::write(dir.join("sample.rar"), b"x").unwrap();
        let archives = find_archives(&dir);
        assert_eq!(
            archives,
            vec![dir.join("Movie.part01.rar"), dir.join("sample.rar")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn obfuscated_files_verified_and_renamed_to_par2_names() {
        // Mirrors the real failure: files were deobfuscated (named after
        // the release), so PAR2 verification by name finds nothing. It must
        // match by content, then restore the real names from the PAR2 and
        // move the files out.
        let dir = tempfile_dir();
        let completed = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&completed).unwrap();

        let contents = [
            b"episode-one-content".to_vec(),
            b"episode-two-content".to_vec(),
        ];
        let real_names = ["Mr.Robot.S04E01.1080p.mkv", "Mr.Robot.S04E02.1080p.mkv"];
        for (i, data) in contents.iter().enumerate() {
            std::fs::write(dir.join(format!("Mr.Robot.S04.1080p.{i:03}.mkv")), data).unwrap();
        }
        // PAR2 whose File Description packets carry the *real* names.
        let par2 = crate::par2::build_par2_set(&[
            (&contents[0], real_names[0]),
            (&contents[1], real_names[1]),
        ]);
        std::fs::write(dir.join("Mr.Robot.S04.1080p.002.par2"), &par2).unwrap();

        let cfg = PostProcessConfig {
            download_dir: dir.clone(),
            completed_dir: completed.clone(),
            category: None,
            cleanup_archives: true,
            archive_password: None,
            skip_verify: false,
        };
        let report = futures::executor::block_on(post_process(cfg)).unwrap();
        assert_eq!(report.status, PostProcessStatus::Complete);
        assert_eq!(report.verify.as_ref().map(|v| v.healthy), Some(2));
        assert_eq!(report.verify.as_ref().map(|v| v.missing), Some(0));

        // Files moved out with their real names.
        for real in &real_names {
            assert!(
                completed.join(real).exists(),
                "{real} should be in completed dir"
            );
            let moved = std::fs::read(completed.join(real)).unwrap();
            assert!(
                contents.iter().any(|c| c == &moved),
                "content of {real} must match"
            );
        }
        // No leftover mkv or par2 in the download dir.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&completed);
    }
    #[test]
    fn damaged_file_auto_repaired_during_post_process() {
        // A download with PAR2 recovery files but a corrupted data file must be
        // auto-repaired by post_process (Pillar 2a) and then unpacked/moved —
        // not marked "damaged, manual repair needed".
        let original = (0u8..=255).cycle().take(40_000).collect::<Vec<_>>();
        let filename = "Movie.2025.1080p.mkv";
        // > 16k so the actual (padded) slices carry recovery data.
        let par2_bytes = crate::par2::build_par2_set(&[(original.as_slice(), filename)]);

        let dir = tempfile_dir();
        let completed = tempfile_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&completed).unwrap();

        // Corrupt the download partway through.
        let mut damaged = original.clone();
        for b in &mut damaged[20_000..] {
            *b ^= 0xFF;
        }
        std::fs::write(dir.join(filename), &damaged).unwrap();
        std::fs::write(dir.join("Movie.2025.1080p.par2"), &par2_bytes).unwrap();

        let cfg = PostProcessConfig {
            download_dir: dir.clone(),
            completed_dir: completed.clone(),
            category: None,
            cleanup_archives: true,
            archive_password: None,
            skip_verify: false,
        };
        let report = futures::executor::block_on(post_process(cfg)).unwrap();
        assert!(
            matches!(report.status, PostProcessStatus::Complete),
            "auto-repair should let post-processing complete, got {:?}",
            report.status
        );
        // The repaired file must be byte-identical and moved to completed.
        assert_eq!(
            std::fs::read(completed.join(filename)).unwrap(),
            original,
            "repaired file must be byte-for-byte original"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&completed);
    }
}
