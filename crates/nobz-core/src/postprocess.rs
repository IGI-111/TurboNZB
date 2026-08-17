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
    /// Directory to move unpacked files to (e.g. ~/Downloads/nobz/tv).
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
    let download_dir = &config.download_dir;

    // Step 1: Find PAR2 files and verify.
    let par2_files = find_par2_files(download_dir);
    let verify_report = if config.skip_verify || par2_files.is_empty() {
        debug!("skipping PAR2 verification (no PAR2 files or skip_verify)");
        None
    } else {
        info!(count = par2_files.len(), "verifying PAR2");
        let par2 = par2::parse_par2_files(&par2_files)
            .map_err(|e| CoreError::Other(anyhow::anyhow!("PAR2 parse: {e}")))?;
        let report = par2::verify(&par2, download_dir);
        info!(
            healthy = report.healthy,
            damaged = report.damaged,
            missing = report.missing,
            "PAR2 verify done"
        );
        Some(report)
    };

    // Step 2: Check if we can proceed to unpack.
    if let Some(ref vr) = verify_report {
        if vr.damaged > 0 || vr.missing > 0 {
            warn!(
                damaged = vr.damaged,
                missing = vr.missing,
                "files damaged/missing — skipping unpack (repair deferred to v2)"
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

    // Step 3: Find archive files and unpack.
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

    // Step 4: Move files to category folder.
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

    // Step 5: Cleanup.
    if config.cleanup_archives && !archives.is_empty() {
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
/// (rar, 7z, split 7z parts, par2).
fn is_archive_or_par2(name: &str) -> bool {
    let name = name.to_lowercase();
    name.ends_with(".rar")
        || name.ends_with(".7z")
        || is_split_archive_part(&name)
        || name.ends_with(".par2")
}

/// Find all archive files (.rar, .7z, .7z.001) in a directory.
/// For split 7z, only the first part (.7z.001) is returned.
fn find_archives(dir: &Path) -> Vec<PathBuf> {
    let mut archives = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = path.to_string_lossy().to_lowercase();

                // Skip .par2 files
                if name.ends_with(".par2") {
                    continue;
                }

                // Split 7z: only keep .7z.001, skip other parts
                if is_split_7z_first(&name) {
                    archives.push(path);
                    continue;
                }
                if is_split_archive_part(&name) {
                    continue;
                }

                if name.ends_with(".rar") || name.ends_with(".7z") {
                    archives.push(path);
                }
            }
        }
    }
    // Sort to process .rar before .r00 etc (multi-part).
    archives.sort();
    // Only keep the first .rar (the main volume); the unrar library handles
    // multi-part automatically.
    if let Some(first_rar) = archives.iter().position(|a| {
        a.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("rar"))
            == Some(true)
    }) {
        // Keep only the main .rar file, drop other .rar parts.
        let main = archives[first_rar].clone();
        archives.retain(|a| {
            a.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("7z"))
                == Some(true)
                || a == &main
        });
    }
    archives
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

        if dest.exists() {
            std::fs::remove_file(&dest).map_err(CoreError::from)?;
        }
        std::fs::rename(&path, &dest).map_err(CoreError::from)?;
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

            if name.ends_with(".parts") && path.is_dir() {
                debug!(path = %path.display(), "removing parts dir");
                let _ = std::fs::remove_dir_all(&path);
            } else if is_archive_or_par2(&name) {
                debug!(path = %path.display(), "removing archive/par2");
                let _ = std::fs::remove_file(&path);
            } else if name == "unpacked" && path.is_dir() {
                debug!(path = %path.display(), "removing unpacked dir");
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(())
}
