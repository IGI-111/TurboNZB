//! Archive unpacking: RAR (via `unrar` crate, C bindings) and 7z (via
//! `sevenz-rust2`, pure Rust). Password-protected archives are supported.
//!
//! ZIP is not handled here — Usenet releases almost never use ZIP. If needed,
//! it can be added later via the `zip` crate.

use std::path::Path;

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

/// Unpack a RAR archive using the `unrar` crate (C bindings, statically linked).
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

                    let extracted = archive_with_entry
                        .extract_with_base(dest_dir)
                        .map_err(|e| UnpackError::Unrar(e.to_string()))?;

                    extracted_files.push(filename);
                    total_bytes += size;
                    current = extracted;
                }
                None => break,
            },
            Err(e) => return Err(UnpackError::Unrar(e.to_string())),
        }
    }

    Ok(UnpackReport {
        extracted_files,
        total_bytes,
        was_encrypted,
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
