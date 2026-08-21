//! PAR2 verify-only: parse PAR2 files, verify downloaded files against
//! their checksums, and report missing/corrupt files.
//!
//! PAR2 files consist of packets with a fixed header and variable body.
//! The key packets for verification are:
//!
//! - **Main packet** (`PAR 2.0\0Main\0\0\0\0`): slice size + file IDs in the
//!   recovery set.
//! - **File Description packet** (`PAR 2.0\0FileDesc`): per-file metadata —
//!   File ID, full-file MD5, MD5 of first 16 kB, length, filename.
//! - **Input File Slice Checksum** (`PAR 2.0\0IFSC\0\0\0\0`): per-slice
//!   MD5 + CRC32 for each slice of a file.
//! - **Recovery Slice packet** (`PAR 2.0\0RecvSlic`): recovery data with
//!   an exponent — only needed for repair, not verify.
//!
//! Verification: for each file in the recovery set, compute the MD5 of the
//! full file and the MD5 of the first 16 kB, then compare against the File
//! Description packet. If the full-file MD5 matches, the file is healthy. If
//! only the 16 kB MD5 matches but the full MD5 differs, the file is damaged
//! and could potentially be repaired (v2). If neither matches, the file is
//! missing or unrecognizable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use tracing::debug;

/// The magic sequence at the start of every PAR2 packet.
const PAR2_MAGIC: [u8; 8] = *b"PAR2\0PKT";

/// Packet type identifiers (16-byte ASCII, null-padded).
const TYPE_MAIN: [u8; 16] = *b"PAR 2.0\0Main\0\0\0\0";
const TYPE_FILE_DESC: [u8; 16] = *b"PAR 2.0\0FileDesc";
const TYPE_IFSC: [u8; 16] = *b"PAR 2.0\0IFSC\0\0\0\0";
const TYPE_RECV_SLICE: [u8; 16] = *b"PAR 2.0\0RecvSlic";
const TYPE_CREATOR: [u8; 16] = *b"PAR 2.0\0Creator\0";

/// A parsed PAR2 file: the collection of packets relevant to verification.
#[derive(Debug, Default)]
pub struct Par2File {
    /// Recovery set ID (MD5 of the main packet body).
    pub set_id: [u8; 16],
    /// Slice size in bytes (from the main packet).
    pub slice_size: u64,
    /// File IDs in the recovery set.
    pub recovery_file_ids: Vec<[u8; 16]>,
    /// File descriptions keyed by File ID.
    pub file_descriptions: HashMap<[u8; 16], FileDescription>,
    /// Slice checksums keyed by File ID.
    pub slice_checksums: HashMap<[u8; 16], Vec<SliceChecksum>>,
    /// Number of recovery slices available.
    pub recovery_count: u32,
}

/// A file description from a PAR2 File Description packet.
#[derive(Debug, Clone)]
pub struct FileDescription {
    /// The 16-byte File ID.
    pub file_id: [u8; 16],
    /// MD5 hash of the entire file.
    pub md5_full: [u8; 16],
    /// MD5 hash of the first 16 kB of the file.
    pub md5_16k: [u8; 16],
    /// File length in bytes.
    pub length: u64,
    /// File name (ASCII).
    pub filename: String,
}

/// A per-slice checksum from an IFSC packet.
#[derive(Debug, Clone)]
pub struct SliceChecksum {
    pub md5: [u8; 16],
    pub crc32: u32,
}

/// Result of verifying a single file against its PAR2 description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyStatus {
    /// File exists and its full MD5 matches — fully healthy.
    Ok,
    /// File exists and its 16 kB MD5 matches but the full MD5 differs —
    /// damaged, potentially repairable (v2).
    Damaged,
    /// File does not exist on disk.
    Missing,
    /// File exists but neither the 16 kB nor full MD5 matches — wrong file
    /// or severely damaged.
    Unrecognized,
}

/// Overall verification result for a recovery set.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    /// Per-file status, keyed by filename.
    pub files: Vec<(String, VerifyStatus)>,
    /// Number of healthy files.
    pub healthy: u32,
    /// Number of damaged files.
    pub damaged: u32,
    /// Number of missing files.
    pub missing: u32,
    /// Number of recovery slices available (for potential repair in v2).
    pub recovery_slices: u32,
    /// Whether the set could be repaired (damaged <= recovery_slices).
    pub repairable: bool,
    /// Matches between on-disk files and recovery-set files, keyed by the
    /// name recorded in the PAR2 (which may differ from the on-disk name
    /// when files were deobfuscated/renamed after download).
    pub matches: Vec<(PathBuf, String)>,
}

/// A file on disk that could satisfy a recovery-set entry. Hashes are
/// computed lazily (only when a description's length matches) and cached so
/// each file is read at most once.
struct Candidate {
    path: PathBuf,
    len: u64,
    full_md5: Option<[u8; 16]>,
    md5_16k: Option<[u8; 16]>,
    used: bool,
}

/// Parse one or more PAR2 files from disk. All files should belong to the
/// same recovery set (same set ID).
pub fn parse_par2_files(paths: &[impl AsRef<Path>]) -> Result<Par2File, ParseError> {
    let mut par2 = Par2File::default();
    let mut set_id_seen: Option<[u8; 16]> = None;

    for path in paths {
        let data = std::fs::read(path.as_ref()).map_err(ParseError::Io)?;
        parse_packets(&data, &mut par2, &mut set_id_seen)?;
    }

    Ok(par2)
}

/// Parse a single PAR2 file from bytes.
pub fn parse_par2_bytes(data: &[u8]) -> Result<Par2File, ParseError> {
    let mut par2 = Par2File::default();
    let mut set_id_seen = None;
    parse_packets(data, &mut par2, &mut set_id_seen)?;
    Ok(par2)
}

fn parse_packets(
    data: &[u8],
    par2: &mut Par2File,
    set_id_seen: &mut Option<[u8; 16]>,
) -> Result<(), ParseError> {
    let mut pos = 0;

    while pos + 64 <= data.len() {
        // Find the next magic sequence.
        let remaining = &data[pos..];
        let magic_pos = remaining.windows(8).position(|w| w == PAR2_MAGIC);

        let offset = match magic_pos {
            Some(p) => p,
            None => break,
        };

        pos += offset;

        // Packet header: 8 magic + 8 length + 16 md5 + 16 set_id + 16 type = 64
        if pos + 64 > data.len() {
            break;
        }

        let packet_len = u64::from_le_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;

        if packet_len < 64 || pos + packet_len > data.len() {
            // Damaged packet — skip forward past the magic.
            pos += 8;
            continue;
        }

        let set_id: [u8; 16] = data[pos + 32..pos + 48].try_into().unwrap();
        let pkt_type: [u8; 16] = data[pos + 48..pos + 64].try_into().unwrap();

        // Track the set ID (all packets should share it).
        if set_id_seen.is_none() {
            *set_id_seen = Some(set_id);
        }
        if par2.set_id == [0u8; 16] {
            par2.set_id = set_id;
        }

        let body = &data[pos + 64..pos + packet_len];

        match pkt_type {
            TYPE_MAIN => parse_main_packet(body, par2)?,
            TYPE_FILE_DESC => parse_file_desc_packet(body, par2),
            TYPE_IFSC => parse_ifsc_packet(body, par2),
            TYPE_RECV_SLICE => par2.recovery_count += 1,
            TYPE_CREATOR => { /* informational, skip */ }
            _ => { /* unknown packet type, skip */ }
        }

        pos += packet_len;
    }

    Ok(())
}

fn parse_main_packet(body: &[u8], par2: &mut Par2File) -> Result<(), ParseError> {
    if body.len() < 12 {
        return Err(ParseError::Truncated("main packet too short"));
    }

    par2.slice_size = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let recovery_count = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;

    let file_id_start = 12;
    let recovery_end = file_id_start + recovery_count * 16;
    if body.len() < recovery_end {
        return Err(ParseError::Truncated("main packet file IDs truncated"));
    }

    par2.recovery_file_ids.clear();
    for i in 0..recovery_count {
        let start = file_id_start + i * 16;
        let id: [u8; 16] = body[start..start + 16].try_into().unwrap();
        par2.recovery_file_ids.push(id);
    }

    // Non-recovery set file IDs (if any).
    let non_recovery_start = recovery_end;
    let remaining = body.len() - non_recovery_start;
    let non_recovery_count = remaining / 16;
    for i in 0..non_recovery_count {
        let start = non_recovery_start + i * 16;
        let id: [u8; 16] = body[start..start + 16].try_into().unwrap();
        par2.recovery_file_ids.push(id);
    }

    Ok(())
}

fn parse_file_desc_packet(body: &[u8], par2: &mut Par2File) {
    if body.len() < 56 {
        return;
    }

    let file_id: [u8; 16] = body[0..16].try_into().unwrap();
    let md5_full: [u8; 16] = body[16..32].try_into().unwrap();
    let md5_16k: [u8; 16] = body[32..48].try_into().unwrap();
    let length = u64::from_le_bytes(body[48..56].try_into().unwrap());
    let filename = String::from_utf8_lossy(&body[56..])
        .trim_end_matches('\0')
        .to_string();

    par2.file_descriptions.insert(
        file_id,
        FileDescription {
            file_id,
            md5_full,
            md5_16k,
            length,
            filename,
        },
    );
}

fn parse_ifsc_packet(body: &[u8], par2: &mut Par2File) {
    if body.len() < 16 {
        return;
    }

    let file_id: [u8; 16] = body[0..16].try_into().unwrap();
    let checksum_data = &body[16..];
    let slice_count = checksum_data.len() / 20; // 16 bytes MD5 + 4 bytes CRC32

    let mut checksums = Vec::with_capacity(slice_count);
    for i in 0..slice_count {
        let start = i * 20;
        let md5: [u8; 16] = checksum_data[start..start + 16].try_into().unwrap();
        let crc32 = u32::from_le_bytes(checksum_data[start + 16..start + 20].try_into().unwrap());
        checksums.push(SliceChecksum { md5, crc32 });
    }

    par2.slice_checksums.insert(file_id, checksums);
}

/// Verify downloaded files against a parsed PAR2 set.
///
/// Files are matched **by content** (length + MD5), not by filename: the
/// on-disk name may differ from the name recorded in the PAR2 (deobfuscated
/// posts rename files after download), so name-based lookup would report
/// every file missing. This mirrors how PAR2 clients actually verify — they
/// match on hashes and tolerate renamed files.
pub fn verify(par2: &Par2File, dir: &Path) -> VerifyReport {
    let mut candidates = gather_candidates(dir);

    let mut files = Vec::new();
    let mut healthy = 0u32;
    let mut damaged = 0u32;
    let mut missing = 0u32;
    let mut matches = Vec::new();

    for file_id in &par2.recovery_file_ids {
        let desc = match par2.file_descriptions.get(file_id) {
            Some(d) => d,
            None => continue,
        };

        let (status, matched_path) = match_file(&mut candidates, desc);
        if let Some(path) = matched_path {
            matches.push((path, desc.filename.clone()));
        }
        match status {
            VerifyStatus::Ok => healthy += 1,
            VerifyStatus::Damaged => damaged += 1,
            VerifyStatus::Missing => missing += 1,
            VerifyStatus::Unrecognized => damaged += 1,
        }

        files.push((desc.filename.clone(), status));
    }

    let repairable = damaged <= par2.recovery_count;

    debug!(
        healthy,
        damaged,
        missing,
        recovery_slices = par2.recovery_count,
        repairable,
        "PAR2 verify complete"
    );

    VerifyReport {
        files,
        healthy,
        damaged,
        missing,
        recovery_slices: par2.recovery_count,
        repairable,
        matches,
    }
}

/// Gather top-level files in `dir`, excluding `.par2` files themselves.
fn gather_candidates(dir: &Path) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_par2 = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("par2"))
                .unwrap_or(false);
            if is_par2 {
                continue;
            }
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            candidates.push(Candidate {
                path,
                len,
                full_md5: None,
                md5_16k: None,
                used: false,
            });
        }
    }
    candidates
}

/// Try to match `desc` against the currently unused candidates. On a match
/// the matching candidate is marked used and its on-disk path returned (so
/// the caller can map it back to the real, PAR2-recorded name).
fn match_file(
    candidates: &mut [Candidate],
    desc: &FileDescription,
) -> (VerifyStatus, Option<PathBuf>) {
    for cand in candidates.iter_mut() {
        if cand.used {
            continue;
        }
        // Quick length check before reading anything.
        if cand.len != desc.length {
            continue;
        }
        // Read (once) and hash this candidate.
        if cand.full_md5.is_none() {
            if let Ok(data) = std::fs::read(&cand.path) {
                cand.full_md5 = Some(md5_full(&data));
                cand.md5_16k = if data.len() >= 16 * 1024 {
                    Some(md5_16k(&data))
                } else {
                    None
                };
            }
        }
        let Some(full) = cand.full_md5 else {
            continue;
        };

        if full == desc.md5_full {
            cand.used = true;
            return (VerifyStatus::Ok, Some(cand.path.clone()));
        }
        // Damaged: same length and leading data, but full MD5 differs.
        if cand.md5_16k == Some(desc.md5_16k) {
            cand.used = true;
            return (VerifyStatus::Damaged, Some(cand.path.clone()));
        }
        // Length matches but content is unrelated — keep looking.
    }

    (VerifyStatus::Missing, None)
}

/// Compute MD5 of the first 16 kB of a file.
fn md5_16k(data: &[u8]) -> [u8; 16] {
    let chunk = &data[..data.len().min(16 * 1024)];
    let mut hasher = Md5::new();
    hasher.update(chunk);
    hasher.finalize().into()
}

/// Compute MD5 of the entire file.
fn md5_full(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Write a single PAR2 packet with correct MD5 and padding.
fn write_packet(out: &mut Vec<u8>, set_id: [u8; 16], pkt_type: [u8; 16], body: &[u8]) {
    let total_len = 64 + body.len();
    // Pad body to 4-byte alignment.
    let padded_len = (total_len + 3) & !3;
    let padding = padded_len - total_len;

    // MD5 of (set_id + type + body + padding)
    let mut md5_hasher = Md5::new();
    md5_hasher.update(set_id);
    md5_hasher.update(pkt_type);
    md5_hasher.update(body);
    for _ in 0..padding {
        md5_hasher.update([0]);
    }
    let packet_md5: [u8; 16] = md5_hasher.finalize().into();

    out.extend_from_slice(&PAR2_MAGIC);
    out.extend_from_slice(&(padded_len as u64).to_le_bytes());
    out.extend_from_slice(&packet_md5);
    out.extend_from_slice(&set_id);
    out.extend_from_slice(&pkt_type);
    out.extend_from_slice(body);
    for _ in 0..padding {
        out.push(0);
    }
}

/// Build a minimal single-set PAR2 in memory, describing `files`
/// (`(content, filename)` pairs). Primarily for tests; the resulting
/// archive is parsed by [`parse_par2_bytes`] and verified by [`verify`].
pub fn build_par2_set(files: &[(&[u8], &str)]) -> Vec<u8> {
    struct Entry {
        file_id: [u8; 16],
        md5_16k: [u8; 16],
        md5_full: [u8; 16],
        len: usize,
        name: String,
    }

    // Compute file IDs = MD5 of (md5_16k + length + filename).
    let details: Vec<Entry> = files
        .iter()
        .map(|(data, name)| {
            let md5_16k_val = md5_16k(data);
            let file_id: [u8; 16] = {
                let mut h = Md5::new();
                h.update(md5_16k_val);
                h.update((data.len() as u64).to_le_bytes());
                h.update(name.as_bytes());
                h.finalize().into()
            };
            Entry {
                file_id,
                md5_16k: md5_16k_val,
                md5_full: md5_full(data),
                len: data.len(),
                name: name.to_string(),
            }
        })
        .collect();

    // Build the main packet body.
    let slice_size: u64 = 16 * 1024; // 16 kB slices
    let recovery_count: u32 = 1;
    let mut main_body = Vec::new();
    main_body.extend_from_slice(&slice_size.to_le_bytes());
    main_body.extend_from_slice(&recovery_count.to_le_bytes());
    for entry in &details {
        main_body.extend_from_slice(&entry.file_id);
    }

    // Main packet set ID = MD5 of main body.
    let set_id: [u8; 16] = {
        let mut h = Md5::new();
        h.update(&main_body);
        h.finalize().into()
    };

    let mut out = Vec::new();

    // Main packet.
    write_packet(&mut out, set_id, TYPE_MAIN, &main_body);

    // File Description packets.
    for entry in &details {
        let mut file_desc_body = Vec::new();
        file_desc_body.extend_from_slice(&entry.file_id);
        file_desc_body.extend_from_slice(&entry.md5_full);
        file_desc_body.extend_from_slice(&entry.md5_16k);
        file_desc_body.extend_from_slice(&(entry.len as u64).to_le_bytes());
        file_desc_body.extend_from_slice(entry.name.as_bytes());
        // Pad to 4-byte alignment.
        while file_desc_body.len() % 4 != 0 {
            file_desc_body.push(0);
        }
        write_packet(&mut out, set_id, TYPE_FILE_DESC, &file_desc_body);
    }

    // Creator packet (required by spec).
    let creator_body = b"turbonzb test\0\0\0";
    write_packet(&mut out, set_id, TYPE_CREATOR, creator_body);

    out
}

/// Errors that can occur during PAR2 parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("truncated packet: {0}")]
    Truncated(&'static str),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_parse_and_verify_healthy_file() {
        let file_data = b"Hello, this is a test file for PAR2 verification!";
        let filename = "test.bin";
        let par2_data = build_par2_set(&[(file_data, filename)]);

        let par2 = parse_par2_bytes(&par2_data).unwrap();

        assert_eq!(par2.slice_size, 16 * 1024);
        assert_eq!(par2.recovery_file_ids.len(), 1);
        assert_eq!(par2.file_descriptions.len(), 1);

        let desc = par2
            .file_descriptions
            .get(&par2.recovery_file_ids[0])
            .unwrap();
        assert_eq!(desc.filename, "test.bin");
        assert_eq!(desc.length, file_data.len() as u64);

        // Write the file to a temp dir and verify.
        let tmp = std::env::temp_dir().join("turbonzb-par2-test-healthy");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(filename), file_data).unwrap();

        let report = verify(&par2, &tmp);
        assert_eq!(report.healthy, 1);
        assert_eq!(report.damaged, 0);
        assert_eq!(report.missing, 0);
        assert_eq!(report.files[0].1, VerifyStatus::Ok);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_verify_missing_file() {
        let file_data = b"some data here";
        let filename = "missing.bin";
        let par2_data = build_par2_set(&[(file_data, filename)]);
        let par2 = parse_par2_bytes(&par2_data).unwrap();

        let tmp = std::env::temp_dir().join("turbonzb-par2-test-missing");
        std::fs::create_dir_all(&tmp).unwrap();
        // Don't write the file — it should be missing.

        let report = verify(&par2, &tmp);
        assert_eq!(report.missing, 1);
        assert_eq!(report.healthy, 0);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_verify_matches_renamed_file_by_content() {
        // Regression for obfuscated posts: the on-disk file was renamed
        // (deobfuscated) so its name no longer matches the name recorded
        // in the PAR2. Verification must match by content.
        let file_data = b"Hello, this is a test file for PAR2 verification!";
        let filename = "secret.bin";
        let par2_data = build_par2_set(&[(file_data, filename)]);
        let par2 = parse_par2_bytes(&par2_data).unwrap();

        let tmp = std::env::temp_dir().join("turbonzb-par2-test-renamed");
        std::fs::create_dir_all(&tmp).unwrap();
        // Write the same content under a *different* name.
        std::fs::write(tmp.join("release.000.rar"), file_data).unwrap();

        let report = verify(&par2, &tmp);
        assert_eq!(report.healthy, 1, "must match by content despite rename");
        assert_eq!(report.damaged, 0);
        assert_eq!(report.missing, 0);
        // The real (PAR2-recorded) name must be exposed for renaming.
        assert_eq!(
            report.matches,
            vec![(tmp.join("release.000.rar"), filename.to_string())]
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_verify_damaged_file() {
        let original_data = vec![0xAA; 20_000]; // > 16 kB so 16k hash is meaningful
        let filename = "damaged.bin";
        let par2_data = build_par2_set(&[(original_data.as_slice(), filename)]);
        let par2 = parse_par2_bytes(&par2_data).unwrap();

        let tmp = std::env::temp_dir().join("turbonzb-par2-test-damaged");
        std::fs::create_dir_all(&tmp).unwrap();

        // Damage the file: keep first 16 kB intact, corrupt the rest.
        let mut damaged = original_data.clone();
        for byte in &mut damaged[16 * 1024..] {
            *byte = 0xFF;
        }
        // Adjust length to match (same length, different content).
        std::fs::write(tmp.join(filename), &damaged).unwrap();

        let report = verify(&par2, &tmp);
        // The 16 kB MD5 should match but the full MD5 won't.
        assert_eq!(report.damaged, 1);
        assert_eq!(report.healthy, 0);
        assert_eq!(report.files[0].1, VerifyStatus::Damaged);

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
