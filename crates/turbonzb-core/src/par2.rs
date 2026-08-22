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
    /// Number of recovery slices available (== len of `recovery_slices`).
    pub recovery_count: u32,
    /// Parsed recovery slices (exponent + data), for repair (Pillar 2a).
    pub recovery_slices: Vec<RecoverySlice>,
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

/// A parsed recovery slice (from a `PAR 2.0\0RecvSlic` packet): the
/// recovery-block exponent and its data (zero-padded to `slice_size`).
#[derive(Debug, Clone)]
pub struct RecoverySlice {
    pub exponent: u32,
    /// Recovery data, `slice_size` bytes (zero-padded tail for short
    /// recovery sets).
    pub data: Vec<u8>,
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
            TYPE_RECV_SLICE => {
                par2.recovery_count += 1;
                parse_recv_slice_packet(body, par2);
            }
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

/// Parse a recovery slice packet (after the 64-byte header). The body is a
/// 4-byte little-endian exponent followed by `slice_size` bytes of recovery
/// data. Only the data is stored — the actual RS math is done lazily during
/// repair.
fn parse_recv_slice_packet(body: &[u8], par2: &mut Par2File) {
    if body.len() <= 4 {
        return;
    }
    let exponent = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let data = body[4..].to_vec();
    par2.recovery_slices.push(RecoverySlice { exponent, data });
}

/// Verify downloaded files against a parsed PAR2 set.
///
/// Files are matched **by content** (length + MD5), not by filename: the
/// on-disk name may differ from the name recorded in the PAR2 (deobfuscated
/// posts rename files after download), so name-based lookup would report
/// every file missing. This mirrors how PAR2 clients actually verify — they
/// match on hashes and tolerate renamed files.
pub fn verify(par2: &Par2File, dir: &Path) -> VerifyReport {
    verify_with_progress(par2, dir, None)
}

/// Like [`verify`], but reports hashing progress as a fraction of total
/// candidate bytes processed. `progress` is called as `(done, total)` and
/// may be called many times per file; pass `None` to skip reporting.
pub fn verify_with_progress(
    par2: &Par2File,
    dir: &Path,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> VerifyReport {
    verify_impl(par2, dir, progress.unwrap_or(&mut |_, _| {}))
}

fn verify_impl(
    par2: &Par2File,
    dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> VerifyReport {
    let mut candidates = gather_candidates(dir);
    let total: u64 = candidates.iter().map(|c| c.len).sum();
    let mut hashed: u64 = 0;

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

        let (status, matched_path) = match_file(&mut candidates, desc, &mut |n| {
            hashed += n;
            progress(hashed, total);
        });
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
    on_hash: &mut dyn FnMut(u64),
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
            on_hash(cand.len);
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

/// Fast PAR2 rename (ParRename, Pillar 2b).
///
/// For each file in the recovery set, find an on-disk candidate matching on
/// **length + 16 kB MD5** (NOT the whole-file MD5) and, if the on-disk
/// name differs, rename it to the name recorded in the File Description.
/// This restores real names in seconds — even for huge files — without
/// hashing the entire release. Full verification (whole-file MD5) then runs
/// on the renamed, correctly-named set.
///
/// Matches are exclusive (each candidate is used once). Returns the number
/// of renames performed.
pub fn fast_rename_to_par2_names(par2: &Par2File, dir: &Path) -> Result<usize, ParseError> {
    let mut candidates = gather_candidates(dir);
    let mut renames = 0usize;

    for file_id in &par2.recovery_file_ids {
        let desc = match par2.file_descriptions.get(file_id) {
            Some(d) => d,
            None => continue,
        };
        let real = desc.filename.rsplit('/').next().unwrap_or(&desc.filename).to_string();
        let real = real.trim().to_string();
        if real.is_empty() {
            continue;
        }

        // Find an unused candidate matching on length + 16k MD5.
        let mut matched: Option<PathBuf> = None;
        for cand in candidates.iter_mut() {
            if cand.used || cand.len != desc.length {
                continue;
            }
            if cand.md5_16k.is_none() {
                // A file shorter than 16k still hashes fully for the
                // first-16k comparison.
                cand.md5_16k = std::fs::read(&cand.path)
                    .ok()
                    .map(|d| md5_16k(&d));
            }
            if cand.md5_16k == Some(desc.md5_16k) {
                cand.used = true;
                matched = Some(cand.path.clone());
                break;
            }
        }

        let Some(src) = matched else { continue };
        let src_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if src_name == real {
            continue;
        }
        let dest = child_avoiding(dir, &real, &src);
        if dest == src {
            continue;
        }
        std::fs::rename(&src, &dest).map_err(ParseError::Io)?;
        renames += 1;
    }

    Ok(renames)
}

/// A child of `dir` named `name` that doesn't exist and isn't `ignore`.
/// Appends a numeric suffix on collision.
fn child_avoiding(dir: &Path, name: &str, ignore: &Path) -> PathBuf {
    let candidate = dir.join(name);
    if candidate.exists() && candidate != ignore {
        let mut n = 2;
        loop {
            let alt = dir.join(format!("{name}.{n}"));
            if !alt.exists() {
                return alt;
            }
            n += 1;
        }
    }
    candidate
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
    let mut main_body = Vec::new();
    main_body.extend_from_slice(&slice_size.to_le_bytes());
    main_body.extend_from_slice(&(details.len() as u32).to_le_bytes()); // # files in set
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

    const RECOVERY_SLICES: u32 = 2; // emit enough to repair one bad slice

    // Main packet.
    write_packet(&mut out, set_id, TYPE_MAIN, &main_body);

    // Input slices in global order (file order × slice index), zero-padded —
    // the same ordering repair() reconstructs with.
    let slice_size_us = slice_size as usize;
    let mut input_blocks: Vec<Vec<u8>> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new(); // (first_block, count) per file
    for data in files.iter().map(|(d, _)| *d) {
        let begin = input_blocks.len();
        let n_slices = if data.is_empty() {
            0
        } else {
            data.len().div_ceil(slice_size_us)
        };
        for s in 0..n_slices {
            let start = s * slice_size_us;
            let take = data.len().saturating_sub(start).min(slice_size_us);
            let mut sl = vec![0u8; slice_size_us];
            sl[..take].copy_from_slice(&data[start..start + take]);
            input_blocks.push(sl);
        }
        ranges.push((begin, n_slices));
    }

    let gf = Gf16::new();
    let logbases = input_logbases(input_blocks.len());

    // File Description + IFSC packets, plus recovery slices.
    for (i, entry) in details.iter().enumerate() {
        let (begin, count) = ranges[i];

        // File Description packet.
        let mut file_desc_body = Vec::new();
        file_desc_body.extend_from_slice(&entry.file_id);
        file_desc_body.extend_from_slice(&entry.md5_full);
        file_desc_body.extend_from_slice(&entry.md5_16k);
        file_desc_body.extend_from_slice(&(entry.len as u64).to_le_bytes());
        file_desc_body.extend_from_slice(entry.name.as_bytes());
        while file_desc_body.len() % 4 != 0 {
            file_desc_body.push(0);
        }
        write_packet(&mut out, set_id, TYPE_FILE_DESC, &file_desc_body);

        // IFSC packet (per-slice MD5 + CRC32 of the zero-padded slice).
        let mut ifsc_body = Vec::new();
        ifsc_body.extend_from_slice(&entry.file_id);
        for block in &input_blocks[begin..begin + count] {
            let mut m = Md5::new();
            m.update(block);
            let md5: [u8; 16] = m.finalize().into();
            let mut c = crc32fast::Hasher::new();
            c.update(block);
            let crc = c.finalize();
            ifsc_body.extend_from_slice(&md5);
            ifsc_body.extend_from_slice(&crc.to_le_bytes());
        }
        write_packet(&mut out, set_id, TYPE_IFSC, &ifsc_body);
    }

    // Recovery slice packets (exponents 0..RECOVERY_SLICES-1). PAR2
    // interprets each slice as a sequence of little-endian 16-bit GF(2^16)
    // words; recovery word k of block e is the GF sum over input blocks of
    // input_word_k * coeff, where coeff = 2^(logbase*e).
    let slice_words = slice_size_us / 2;
    let input_words: Vec<Vec<u16>> = input_blocks.iter().map(|sl| bytes_to_words(sl)).collect();
    for e in 0..RECOVERY_SLICES {
        let mut acc = vec![0u16; slice_words];
        for (b, block) in input_words.iter().enumerate() {
            let coeff = gf.pow_logbase(logbases[b], e);
            if coeff == 0 {
                continue;
            }
            for (x, d) in acc.iter_mut().zip(block.iter()) {
                *x ^= gf.mul(*d, coeff);
            }
        }
        let data = words_to_bytes(&acc);
        let mut rec_body = Vec::with_capacity(4 + slice_size_us);
        rec_body.extend_from_slice(&e.to_le_bytes());
        rec_body.extend_from_slice(&data);
        write_packet(&mut out, set_id, TYPE_RECV_SLICE, &rec_body);
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


/// ===== GF(2^16) Reed-Solomon for PAR2 repair (Pillar 2a) =====
///
/// PAR2 computes recovery data over the Galois field GF(2^16) with the
/// primitive polynomial `0x1100B` (x^16 + x^12 + x^3 + x + 1) and the
/// primitive element 2 — matched bit-for-bit with par2cmdline. Field
/// elements here happen to be single bytes: every byte of every slice /
/// recovery block is combined independently in this field.
const GF_POLY: u32 = 0x1100B;
/// Multiplicative group order of GF(2^16).
const GF_ORDER: u32 = 65535;

/// The GF(2^16) field, with precomputed antilog (exp) and log tables.
pub struct Gf16 {
    exp: Vec<u16>,
    log: Vec<u16>,
}

impl Gf16 {
    pub fn new() -> Self {
        let mut exp = vec![0u16; 65536];
        let mut log = vec![0u16; 65536];
        let mut x: u32 = 1; // 2^0
        for i in 0..GF_ORDER {
            exp[i as usize] = x as u16;
            log[x as usize] = i as u16;
            // Multiply by the generator (2): shift left, reduce mod poly
            // when bit 16 is set.
            x <<= 1;
            if x & 0x10000 != 0 {
                x ^= GF_POLY;
            }
            x &= 0xFFFF;
        }
        Self { exp, log }
    }

    fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            return 0;
        }
        let l = (self.log[a as usize] as u32 + self.log[b as usize] as u32) % GF_ORDER;
        self.exp[l as usize]
    }

    fn inverse(&self, a: u16) -> u16 {
        debug_assert!(a != 0);
        self.exp[((GF_ORDER - self.log[a as usize] as u32) % GF_ORDER) as usize]
    }

    /// `base^e` where `base = 2^logbase`, i.e. `2^(logbase*e)`.
    fn pow_logbase(&self, logbase: u32, e: u32) -> u16 {
        self.exp[((logbase * e) % GF_ORDER) as usize]
    }
}

impl Default for Gf16 {
    fn default() -> Self {
        Self::new()
    }
}

/// The `logbase` exponent assigned to the `i`-th input block: the i-th
/// non-negative integer coprime with 65535 (i.e. not divisible by 3, 5, 17
/// or 257 — the prime factors of 65535). Matches par2cmdline.
pub fn input_logbases(count: usize) -> Vec<u32> {
    let mut v = Vec::with_capacity(count);
    let mut n: u32 = 0;
    while v.len() < count {
        if n % 3 != 0 && n % 5 != 0 && n % 17 != 0 && n % 257 != 0 {
            v.push(n);
        }
        n += 1;
    }
    v
}

/// Convert a byte slice (little-endian, `slice_size` bytes) to GF(2^16)
/// words. If `bytes` has an odd count, the last byte is the low byte of a
/// word with a zero high byte (shouldn't happen: slice sizes are even).
fn bytes_to_words(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Convert GF(2^16) words back to little-endian bytes.
fn words_to_bytes(words: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(words.len() * 2);
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

/// A single input block (slice) of the recovery set.
struct Block {
    slice_index: usize,
    /// Data as little-endian GF(2^16) words (zero-padded to slice size).
    data: Vec<u16>,
    good: bool,
    file_len: usize,
}

/// Per-file repair outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairStatus {
    /// No slices needed repair.
    Ok,
    /// All missing/corrupt slices were reconstructed.
    Repaired,
    /// Not enough recovery data to repair every bad slice.
    Unrepairable { repaired: u32, need: u32 },
}

/// Result of a PAR2 repair attempt.
#[derive(Debug, Clone, Default)]
pub struct RepairReport {
    /// Per-file status, keyed by the file's basename on disk.
    pub files: Vec<(String, RepairStatus)>,
    /// Total slices reconstructed.
    pub total_slices_repaired: u32,
}

/// (basename, file length) -> reconstructed slices, for the write-back pass.
type SliceMap = std::collections::BTreeMap<(String, usize), Vec<(usize, Vec<u8>)>>;

/// Repair damaged/missing files in `dir` using the PAR2 recovery set.
///
/// Slices whose per-slice checksum (IFSC) doesn't match on disk (or whose
/// file is absent) are reconstructed from the recovery slices via RS
/// erasure decoding over GF(2^16), then written back into the file at
/// their slice offsets — filling the sparse holes the direct-write engine
/// leaves for missing segments (Pillar 3 integration).
pub fn repair(par2: &Par2File, dir: &Path) -> Result<RepairReport, ParseError> {
    let gf = Gf16::new();
    let slice_size = par2.slice_size.max(1) as usize;
    if par2.recovery_slices.is_empty() {
        return Ok(RepairReport::default());
    }

    // Build the ordered input-block list and read on-disk content.
    let mut blocks: Vec<Block> = Vec::new();
    // (fi, on-disk-name) for each global input block.
    let mut block_file: Vec<(usize, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    for (fi, file_id) in par2.recovery_file_ids.iter().enumerate() {
        let Some(desc) = par2.file_descriptions.get(file_id) else {
            continue;
        };
        let name = par2_basename(&desc.filename);
        if names.len() <= fi {
            names.resize(fi + 1, String::new());
        }
        names[fi] = name.clone();

        let slice_count = desc.length.div_ceil(slice_size as u64) as usize;
        let on_disk = std::fs::read(dir.join(&name)).ok();
        let ifsc = par2.slice_checksums.get(file_id).cloned();
        for s in 0..slice_count {
            let start = s * slice_size;
            let mut slice = vec![0u8; slice_size];
            let mut good = false;
            if let Some(content) = &on_disk {
                let take = content.len().saturating_sub(start).min(slice_size);
                slice[..take].copy_from_slice(&content[start..start + take]);
                good = ifsc
                    .as_ref()
                    .map(|v| slice_matches(v, s, &slice))
                    .unwrap_or(true);
            }
            blocks.push(Block {
                slice_index: s,
                data: bytes_to_words(&slice),
                good,
                file_len: desc.length as usize,
            });
            block_file.push((fi, name.clone()));
        }
    }

    let logbases = input_logbases(blocks.len());
    let bad: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| !b.good)
        .map(|(i, _)| i)
        .collect();

    let mut report: Vec<(String, RepairStatus)> = names
        .iter()
        .filter(|s| !s.is_empty())
        .map(|n| (n.clone(), RepairStatus::Ok))
        .collect();
    if bad.is_empty() {
        return Ok(RepairReport::default());
    }

    let need = bad.len() as u32;
    if par2.recovery_slices.len() < bad.len() {
        for &bj in &bad {
            set_status(&mut report, &block_file[bj].1, RepairStatus::Unrepairable { repaired: 0, need });
        }
        return Ok(RepairReport { files: report, total_slices_repaired: 0 });
    }

    // Select one recovery slice per missing column.
    let used: Vec<(u32, &Vec<u8>)> = par2
        .recovery_slices
        .iter()
        .take(bad.len())
        .map(|r| (r.exponent, &r.data))
        .collect();

    // For each selected recovery slice: R'_e = R_e XOR (sum over good
    // blocks g of good_g * base_g^e). Leaves only the bad-blocks' terms.
    let mut mat: Vec<Vec<u16>> = Vec::with_capacity(bad.len());
    let mut rhs: Vec<Vec<u16>> = Vec::with_capacity(bad.len());
    for (e, rdata) in &used {
        let mut row = bytes_to_words(rdata);
        for (i, b) in blocks.iter().enumerate() {
            if b.good {
                let coeff = gf.pow_logbase(logbases[i], *e);
                if coeff == 0 {
                    continue;
                }
                for (x, d) in row.iter_mut().zip(b.data.iter()) {
                    *x ^= gf.mul(*d, coeff);
                }
            }
        }
        rhs.push(row);
        mat.push(bad.iter().map(|&bj| gf.pow_logbase(logbases[bj], *e)).collect());
    }

    if !gauss_jordan(&gf, &mut mat, &mut rhs) {
        for &bj in &bad {
            set_status(&mut report, &block_file[bj].1, RepairStatus::Unrepairable { repaired: 0, need });
        }
        return Ok(RepairReport { files: report, total_slices_repaired: 0 });
    }

    // Recombine solved word slices into bytes and overlay them at their slice
    // offsets (filling sparse holes left by missing segments).
    let mut by_file: SliceMap = std::collections::BTreeMap::new();
    for (c, &bj) in bad.iter().enumerate() {
        let (name, file_len) = (block_file[bj].1.clone(), blocks[bj].file_len);
        by_file
            .entry((name, file_len))
            .or_default()
            .push((blocks[bj].slice_index, words_to_bytes(&rhs[c])));
    }

    let mut total = 0u32;
    for ((name, file_len), slices) in by_file {
        let path = dir.join(&name);
        let mut content = std::fs::read(&path).unwrap_or_else(|_| vec![0u8; file_len]);
        if content.len() < file_len {
            content.resize(file_len, 0);
        }
        for (s, data) in &slices {
            let start = s * slice_size;
            let n = file_len.saturating_sub(start).min(slice_size);
            let take = n.min(slice_size.min(data.len()));
            content[start..start + take].copy_from_slice(&data[..take]);
        }
        std::fs::write(&path, &content).map_err(ParseError::Io)?;
        set_status(&mut report, &name, RepairStatus::Repaired);
        total += slices.len() as u32;
    }

    Ok(RepairReport { files: report, total_slices_repaired: total })
}

/// Basename for on-disk use (strip any PAR2 subdirectory component).
fn par2_basename(name: &str) -> String {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .trim()
        .to_string()
}

fn set_status(report: &mut [(String, RepairStatus)], name: &str, st: RepairStatus) {
    if let Some(entry) = report.iter_mut().find(|(n, _)| n == name) {
        entry.1 = st;
    }
}

/// Whether an on-disk (zero-padded) slice matches its IFSC checksum.
fn slice_matches(ifsc: &[SliceChecksum], s: usize, slice: &[u8]) -> bool {
    match ifsc.get(s) {
        Some(c) => md5_bytes(slice) == c.md5,
        None => true,
    }
}

fn md5_bytes(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().into()
}

/// Solve `mat * x = rhs` in place by Gauss-Jordan over GF(2^16), leaving
/// the solution in `rhs`. Returns `false` if the matrix is singular.
/// Iterate a linear equation system over GF(2^16) in place, leaving the
/// solution in `rhs`. Returns false if the matrix is singular.
#[allow(clippy::needless_range_loop)]
fn gauss_jordan(gf: &Gf16, mat: &mut [Vec<u16>], rhs: &mut [Vec<u16>]) -> bool {
    let n = mat.len();
    for col in 0..n {
        let piv = match (col..n).find(|&r| mat[r][col] != 0) {
            Some(p) => p,
            None => return false,
        };
        mat.swap(col, piv);
        rhs.swap(col, piv);

        let inv = gf.inverse(mat[col][col]);
        for k in col..n {
            mat[col][k] = gf.mul(mat[col][k], inv);
        }
        for b in rhs[col].iter_mut() {
            *b = gf.mul(*b, inv);
        }

        for r in 0..n {
            if r != col && mat[r][col] != 0 {
                let factor = mat[r][col];
                for k in col..n {
                    mat[r][k] ^= gf.mul(mat[col][k], factor);
                }
                let pivot = rhs[col].clone();
                for (x, y) in rhs[r].iter_mut().zip(pivot.iter()) {
                    *x ^= gf.mul(*y, factor);
                }
            }
        }
    }
    true
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

    #[tokio::test]
    async fn test_repair_reconstructs_corrupted_file() {
        // A single file whose data is corrupted past the first 16k: PAR2
        // repair must reconstruct it from recovery slices and restore it.
        let original = (0u8..=255).cycle().take(40_000).collect::<Vec<_>>();
        let filename = "release.mkv";

        let par2_data = build_par2_set(&[(original.as_slice(), filename)]);
        let par2 = parse_par2_bytes(&par2_data).unwrap();
        assert!(par2.recovery_slices.len() >= 1, "must carry recovery slices");
        assert!(par2.slice_checksums.contains_key(&par2.recovery_file_ids[0]));

        let tmp = std::env::temp_dir().join(format!("turbonzb-par2-repair-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Corrupt from 20_000 onward (damages slices past the first).
        let mut damaged = original.clone();
        for b in &mut damaged[20_000..] {
            *b ^= 0xFF;
        }
        std::fs::write(tmp.join(filename), &damaged).unwrap();

        // Initial verify reports damaged.
        let before = verify(&par2, &tmp);
        assert!(before.damaged > 0);

        // Repair.
        let rep = repair(&par2, &tmp).unwrap();
        assert!(rep.total_slices_repaired >= 1, "should reconstruct at least one slice");
        assert_eq!(
            rep.files.iter().find(|(n, _)| n == filename).map(|(_, st)| st),
            Some(&RepairStatus::Repaired)
        );

        // After repair the file must be intact and verify clean.
        let after_data = std::fs::read(tmp.join(filename)).unwrap();
        assert_eq!(after_data, original, "file must be byte-for-byte original after repair");
        let after = verify(&par2, &tmp);
        assert_eq!(after.healthy, 1);
        assert_eq!(after.damaged, 0);
        assert_eq!(after.missing, 0);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[tokio::test]
    async fn test_repair_recovers_missing_file() {
        // A missing file (not on disk at all) is entirely reconstructable.
        let content = b"whole file is gone and must come back".repeat(2);
        let filename = "gone.bin";
        let par2_data = build_par2_set(&[(content.as_slice(), filename)]);
        let par2 = parse_par2_bytes(&par2_data).unwrap();

        let tmp = std::env::temp_dir().join("turbonzb-par2-repair-missing");
        std::fs::create_dir_all(&tmp).unwrap();

        let rep = repair(&par2, &tmp).unwrap();
        assert_eq!(
            rep.files.iter().find(|(n, _)| n == filename).map(|(_, st)| st),
            Some(&RepairStatus::Repaired)
        );
        let after_data = std::fs::read(tmp.join(filename)).unwrap();
        assert_eq!(after_data, content);
        let after = verify(&par2, &tmp);
        assert_eq!(after.healthy, 1);

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
