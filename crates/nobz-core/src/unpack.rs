//! Archive unpacking: RAR (via `unrar` crate, C bindings) and 7z (via
//! `sevenz-rust2`, pure Rust). Password-protected archives are supported.
//!
//! ZIP is not handled here — Usenet releases almost never use ZIP. If needed,
//! it can be added later via the `zip` crate.

use std::path::{Path, PathBuf};

use tracing::debug;

/// The result of an unpack operation.
#[derive(Debug, Clone)]
pub struct UnpackReport {
    /// Files extracted from the archive.
    pub extracted_files: Vec<String>,
    /// Total bytes extracted.
    pub total_bytes: u64,
    /// Whether the archive was password-protected.
    pub was_encrypted: bool,
}

/// Errors during unpacking.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    #[error("unrar error: {0}")]
    Unrar(String),

    #[error("7z error: {0}")]
    Sevenz(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported archive format: {0}")]
    Unsupported(String),
}

/// Unpack a single archive file to `dest_dir`. The format is auto-detected
/// by file extension. Password is used if the archive is encrypted.
///
/// Handles split archives (`.7z.001`, `.7z.002`, ...) by concatenating the
/// parts into a single temp file before unpacking.
pub fn unpack(
    archive_path: &Path,
    dest_dir: &Path,
    password: Option<&str>,
) -> Result<UnpackReport, UnpackError> {
    let filename = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_lowercase();

    // Detect split 7z: .7z.001, .7z.002, etc.
    if filename.ends_with(".7z.001") {
        return unpack_split_7z(archive_path, dest_dir, password);
    }

    // Detect other split archives: .001, .002 (not .7z.001 — already handled)
    // These are generic split files; check if the parent name looks like an archive.
    if let Some(ext) = archive_path.extension().and_then(|e| e.to_str()) {
        if ext.chars().all(|c| c.is_ascii_digit()) && ext.len() == 3 && ext != "001" {
            // Only handle if it's part of a .7z or .rar set
            return Err(UnpackError::Unsupported(format!(
                "split file part .{ext} — reassemble manually"
            )));
        }
    }

    let ext = archive_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    // Handle multi-part RAR: .part01.rar, .part001.rar, .rar
    let is_rar = ext == "rar"
        || archive_path
            .to_string_lossy()
            .to_lowercase()
            .contains(".part")
            && ext == "rar";

    if is_rar {
        unpack_rar(archive_path, dest_dir, password)
    } else if ext == "7z" {
        unpack_7z(archive_path, dest_dir, password)
    } else {
        Err(UnpackError::Unsupported(format!(
            "unknown archive format: {ext}"
        )))
    }
}

/// True if a filename is a RAR volume part following the conventional
/// `file.r00`, `file.r01`, … scheme.
pub fn is_rar_part_name(name: &str) -> bool {
    let name = name.to_lowercase();
    let bytes = name.as_bytes();
    let len = bytes.len();
    // `\.r\d{2}$` and not `.rar` (which is `r` + `a` + `r`).
    len >= 4
        && bytes[len - 1].is_ascii_digit()
        && bytes[len - 2].is_ascii_digit()
        && bytes[len - 3] == b'r'
        && bytes[len - 4] == b'.'
}

/// Unpack a RAR archive. Multi-volume sets are followed automatically by the
/// unrar library as long as the sibling volumes follow the conventional
/// naming (`file.r00`, `file.r01`, … or `file.partN.rar`) — which the
/// PAR2-name restore / volume normalization above guarantees.
fn unpack_rar(
    archive_path: &Path,
    dest_dir: &Path,
    password: Option<&str>,
) -> Result<UnpackReport, UnpackError> {
    debug!(archive = %archive_path.display(), "unpacking RAR");

    std::fs::create_dir_all(dest_dir)?;

    let opened = if let Some(pw) = password {
        unrar::Archive::with_password(archive_path, pw)
            .open_for_processing()
            .map_err(|e| UnpackError::Unrar(e.to_string()))?
    } else {
        unrar::Archive::new(archive_path)
            .open_for_processing()
            .map_err(|e| UnpackError::Unrar(e.to_string()))?
    };

    let mut extracted_files = Vec::new();
    let mut total_bytes = 0u64;
    let mut was_encrypted = false;

    let mut current = opened;
    loop {
        match current.read_header() {
            Ok(header_result) => match header_result {
                Some(archive_with_entry) => {
                    let entry = archive_with_entry.entry();
                    let filename = entry.filename.to_string_lossy().to_string();
                    let size = entry.unpacked_size;

                    if entry.is_encrypted() {
                        was_encrypted = true;
                    }

                    debug!(file = %filename, size, "extracting");

                    match archive_with_entry.extract_with_base(dest_dir) {
                        Ok(extracted) => {
                            extracted_files.push(filename);
                            total_bytes += size;
                            current = extracted;
                        }
                        Err(e) => {
                            // The next volume is requested here when an
                            // entry's data continues into it. If it's
                            // missing (incomplete set), stop gracefully
                            // with what we have.
                            if e.to_string().to_lowercase().contains("next volume") {
                                break;
                            }
                            return Err(UnpackError::Unrar(e.to_string()));
                        }
                    }
                }
                None => break,
            },
            Err(e) => {
                if e.to_string().to_lowercase().contains("next volume") {
                    break;
                }
                return Err(UnpackError::Unrar(e.to_string()));
            }
        }
    }

    Ok(UnpackReport {
        extracted_files,
        total_bytes,
        was_encrypted,
    })
}

/// Rename deobfuscated RAR files (`name.NNN.rar` — the engine's generic
/// fallback when no real name is available) to proper RAR volume names
/// (`{stem}.rar`, `{stem}.r00`, …) so the whole set can be unpacked.
///
/// Volume membership ("is this a multi-volume set, and which file is the
/// first?") is determined via the unrar library, which parses the actual
/// RAR headers. Files that were already given real names (e.g. from PAR2)
/// are left untouched.
pub fn normalize_rar_volumes(dir: &Path, stem: &str) -> Result<(), UnpackError> {
    let stem = if stem.trim().is_empty() { "release" } else { stem };
    let stem = stem
        .trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect::<String>();

    // Collect generic-named rar files: `anything.NNN.rar` where NNN is digits.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_lowercase();
            if !lower.ends_with(".rar") || !looks_generic_rar_name(&lower) {
                continue;
            }
            candidates.push(path);
        }
    }
    // Sort by name → engine file order (posting order).
    candidates.sort();

    // Classify via unrar. Skip files it can't open.
    struct Classified {
        path: PathBuf,
        first: bool,
    }
    let mut classified: Vec<Classified> = Vec::new();
    for path in &candidates {
        match unrar::Archive::new(path).open_for_listing() {
            Ok(opened) => match opened.volume_info() {
                unrar::VolumeInfo::First => classified.push(Classified {
                    path: path.clone(),
                    first: true,
                }),
                unrar::VolumeInfo::Subsequent => classified.push(Classified {
                    path: path.clone(),
                    first: false,
                }),
                unrar::VolumeInfo::None => {}
            },
            Err(e) => {
                debug!(path = %path.display(), error = %e, "unrar classify skipped");
            }
        }
    }
    if classified.is_empty() {
        // No rar here is a volume set — independent archives are fine as-is.
        return Ok(());
    }

    // The first volume becomes `{stem}.rar`, everything else `{stem}.rNN`.
    // If no file is explicitly marked first, assume posting order (sorted).
    let first_idx = classified
        .iter()
        .position(|c| c.first)
        .unwrap_or_default();
    let main_src = classified[first_idx].path.clone();
    let main_dst = dir.join(format!("{stem}.rar"));

    if main_src != main_dst {
        if main_dst.exists() {
            std::fs::remove_file(&main_dst)?;
        }
        std::fs::rename(&main_src, &main_dst)?;
        debug!(from = %main_src.display(), to = %main_dst.display(), "renamed rar first volume");
    }

    let mut n = 0u32;
    for (i, c) in classified.iter().enumerate() {
        if i == first_idx {
            continue;
        }
        // Skip any volume already conventionally named (`file.rNN`).
        if is_rar_part_name(
            c.path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default(),
        ) {
            continue;
        }
        let dst = dir.join(format!("{stem}.r{n:02}"));
        if c.path == dst {
            n += 1;
            continue;
        }
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        std::fs::rename(&c.path, &dst)?;
        debug!(from = %c.path.display(), to = %dst.display(), "renamed rar volume");
        n += 1;
    }

    Ok(())
}

/// True for the engine's generic deobfuscated-name pattern: `*.NNN.rar`.
fn looks_generic_rar_name(name: &str) -> bool {
    let name = name.to_lowercase();
    let Some(stem) = name.strip_suffix(".rar") else {
        return false;
    };
    let Some(dot) = stem.rfind('.') else {
        return false;
    };
    let digits = &stem[dot + 1..];
    digits.len() == 3 && digits.chars().all(|c| c.is_ascii_digit())
}

/// Unpack a split 7z archive (`.7z.001`, `.7z.002`, ...) by concatenating
/// all parts into a temp file, then decompressing.
fn unpack_split_7z(
    first_part: &Path,
    dest_dir: &Path,
    password: Option<&str>,
) -> Result<UnpackReport, UnpackError> {
    debug!(archive = %first_part.display(), "unpacking split 7z");

    std::fs::create_dir_all(dest_dir)?;

    // Find all parts: .7z.001, .7z.002, .7z.003, ...
    let base = first_part.to_string_lossy();
    let base = base.strip_suffix(".001").ok_or_else(|| {
        UnpackError::Unsupported("split 7z first part must end with .7z.001".into())
    })?;

    let mut parts = Vec::new();
    for i in 1..=999 {
        let part_path = format!("{base}.{i:03}");
        let path = Path::new(&part_path);
        if path.exists() {
            parts.push(path.to_path_buf());
        } else {
            break;
        }
    }

    debug!(parts = parts.len(), "found split 7z parts");

    // Concatenate into a temp file.
    let tmp = dest_dir.join(".nobz-7z-tmp.dat");
    {
        let mut out = std::fs::File::create(&tmp)?;
        use std::io::Write;
        for part in &parts {
            debug!(part = %part.display(), "concatenating");
            let data = std::fs::read(part)?;
            out.write_all(&data)?;
        }
        out.flush()?;
    }

    // Decompress the concatenated file.
    let result = if let Some(pw) = password {
        sevenz_rust2::decompress_file_with_password(&tmp, dest_dir, sevenz_rust2::Password::new(pw))
            .map_err(|e| UnpackError::Sevenz(e.to_string()))
    } else {
        sevenz_rust2::decompress_file(&tmp, dest_dir)
            .map_err(|e| UnpackError::Sevenz(e.to_string()))
    };

    // Clean up the temp file.
    let _ = std::fs::remove_file(&tmp);

    result?;

    let extracted_files = list_extracted_files(dest_dir);
    Ok(UnpackReport {
        extracted_files,
        total_bytes: 0,
        was_encrypted: password.is_some(),
    })
}
/// Unpack a 7z archive using `sevenz-rust2`.
fn unpack_7z(
    archive_path: &Path,
    dest_dir: &Path,
    password: Option<&str>,
) -> Result<UnpackReport, UnpackError> {
    debug!(archive = %archive_path.display(), "unpacking 7z");

    std::fs::create_dir_all(dest_dir)?;

    if let Some(pw) = password {
        sevenz_rust2::decompress_file_with_password(
            archive_path,
            dest_dir,
            sevenz_rust2::Password::new(pw),
        )
        .map_err(|e| UnpackError::Sevenz(e.to_string()))?;
    } else {
        sevenz_rust2::decompress_file(archive_path, dest_dir)
            .map_err(|e| UnpackError::Sevenz(e.to_string()))?;
    }

    // Scan the output directory for extracted files.
    let extracted_files = list_extracted_files(dest_dir);

    Ok(UnpackReport {
        extracted_files,
        total_bytes: 0,
        was_encrypted: password.is_some(),
    })
}

/// Recursively list files in a directory.
fn list_extracted_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(list_extracted_files(&path));
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                files.push(name.to_string());
            }
        }
    }
    files
}
