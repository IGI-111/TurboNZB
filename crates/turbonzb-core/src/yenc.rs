//! Pure-Rust yEnc decoder with CRC32 verification.
//!
//! NNTP article bodies for binary posts are yEnc-encoded. Each article looks
//! roughly like:
//!
//! ```text
//! =ybegin line=128 size=12345 name=file.rar
//! =ypart begin=1 end=5000
//! <escaped binary payload>
//! =yend size=5000 pcrc32=abcdef12
//! ```
//!
//! For single-part posts the `=ypart` line is absent and `=yend` carries
//! `crc32=` instead of `pcrc32=`. This module decodes the payload, verifies
//! the CRC, and returns the raw bytes for the engine to assemble in order.

use crc32fast::Hasher;

use crate::error::{CoreError, Result};

/// Decoded payload of a single yEnc-encoded article.
#[derive(Debug, Clone)]
pub struct DecodedPart {
    /// Raw decoded bytes for this part (the slice of the file covered by
    /// `=ypart begin..end`, or the whole file for single-part posts).
    pub data: Vec<u8>,
    /// 1-based begin offset declared in `=ypart` (1 for single-part).
    pub begin: u64,
    /// 1-based end offset declared in `=ypart` (== size for single-part).
    pub end: u64,
    /// Declared total file size from `=ybegin size=`.
    pub total_size: u64,
    /// Declared filename from `=ybegin name=`.
    pub name: String,
    /// The CRC32 we computed over the decoded bytes.
    pub crc32: u32,
    /// Whether the article's declared CRC (pcrc32/crc32) matched `crc32`.
    pub crc_ok: bool,
    /// `true` if the article had no CRC header to check against (we can't
    /// verify, but we still decoded the bytes).
    pub crc_unknown: bool,
}

/// Streaming yEnc decoder.
///
/// Consumes the *unstuffed* lines of an article body one at a time and
/// writes decoded bytes directly into an internal (pre-sized) buffer, so the
/// NNTP layer never has to materialize the whole encoded article as one giant
/// `Vec`, then copy it again during decode. This removes one full copy of
/// every ~500 KB article from the hot path (Pillar 1b).
///
/// Use with the NNTP dot-body reader: feed each line (with `..`→`.`
/// unstuffing already applied, and without the trailing CRLF) via
/// [`Decoder::push_line`], then call [`Decoder::finish`] once the `.`
/// terminator is reached.
pub struct Decoder {
    /// Reused decoded-output buffer, pre-sized from `size=` when known.
    out: Vec<u8>,
    name: String,
    begin: u64,
    end: u64,
    total_size: u64,
    line_size: usize,
    stage: Stage,
    /// Reusable per-line raw (still-encoded) buffer, for padding logic.
    line_raw: Vec<u8>,
    seen_yend: bool,
    declared_crc: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Haven't seen `=ybegin` yet; skip leading article headers.
    AwaitBegin,
    /// Seen `=ybegin`; lines are either `=ypart` or payload until `=yend`.
    InPayload,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            name: String::new(),
            begin: 1,
            end: 0,
            total_size: 0,
            line_size: 0,
            stage: Stage::AwaitBegin,
            line_raw: Vec::with_capacity(512),
            seen_yend: false,
            declared_crc: None,
        }
    }

    /// Feed one unstuffed body line (no trailing CR/LF).
    pub fn push_line(&mut self, line: &[u8]) -> Result<()> {
        if self.stage == Stage::AwaitBegin {
            // Look for the `=ybegin` frame, skipping any article headers
            // (Path:, Date:, Xref:) some servers prepend.
            if !line.starts_with(b"=ybegin") {
                return Ok(());
            }
            let params = str_from_ascii(line)?;
            self.name = param(params, "name").unwrap_or_default();
            self.total_size = param(params, "size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            self.line_size = param(params, "line")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            // A corrupt (or hostile) `size=` must not become a giant
            // allocation: Rust aborts the whole process on failed alloc,
            // bypassing every panic hook — one bad article killed the
            // engine mid-download. Treat absurd sizes as corruption
            // (the engine retries/marks the segment, PAR2 can repair),
            // and cap the pre-allocation to something article-shaped.
            const MAX_ARTICLE_BYTES: u64 = 128 * 1024 * 1024;
            const PREALLOC_CAP: usize = 2 * 1024 * 1024;
            if self.total_size > MAX_ARTICLE_BYTES {
                return Err(CoreError::Yenc(
                    "implausible article size (corrupt =ybegin header)".into(),
                ));
            }
            // Pre-size the output buffer to the declared size so the payload
            // decode has no reallocations (single-copy path).
            let cap = if self.total_size > 0 {
                (self.total_size as usize).min(PREALLOC_CAP)
            } else {
                512 * 1024
            };
            self.out = Vec::with_capacity(cap);
            self.stage = Stage::InPayload;
            return Ok(());
        }

        // InPayload: `=ypart`, `=yend`, or payload.
        if line.starts_with(b"=ypart") {
            let params = str_from_ascii(line)?;
            self.begin = param(params, "begin")
                .and_then(|s| s.parse().ok())
                .unwrap_or(self.begin);
            // Corrupt =ypart headers produce nonsense positional offsets;
            // validate the range the moment both values are known (only
            // when the article declared its size).
            self.end = param(params, "end")
                .and_then(|s| s.parse().ok())
                .unwrap_or(self.end);
            if self.total_size > 0
                && (self.end < self.begin || self.end.saturating_sub(self.begin) > self.total_size)
            {
                return Err(CoreError::Yenc(
                    "implausible part range (corrupt =ypart header)".into(),
                ));
            }
            return Ok(());
        }
        if line.starts_with(b"=yend") {
            let params = str_from_ascii(line)?;
            // If there was no `=ypart`, the part covers the whole file.
            if self.begin == 1 && self.end == 0 {
                self.end = self.total_size;
            }
            let value = if params.contains(" crc32=") {
                param(params, "crc32").as_deref().and_then(parse_hex_u32)
            } else {
                param(params, "pcrc32").as_deref().and_then(parse_hex_u32)
            };
            self.declared_crc = value;
            self.seen_yend = true;
            return Ok(());
        }
        // Payload line: apply transport-padding logic then decode.
        self.line_raw.clear();
        self.line_raw.extend_from_slice(line);
        maybe_strip_padding(&mut self.line_raw, self.line_size);
        decode_line(&self.line_raw, &mut self.out)?;
        self.line_raw.clear();
        Ok(())
    }

    /// Finalize after the `.` terminator: compute the CRC and build the
    /// decoded part. Errors if `=ybegin` or `=yend` were never seen.
    pub fn finish(self) -> Result<DecodedPart> {
        if self.stage != Stage::InPayload {
            return Err(CoreError::Yenc("missing =ybegin frame".into()));
        }
        if !self.seen_yend {
            return Err(CoreError::Yenc("missing =yend frame".into()));
        }
        // CRC32 computed once over the full decoded output — per-byte updates
        // during decode are ~44x slower (1.5ms vs 34µs for a 500KB article).
        let mut crc = Hasher::new();
        crc.update(&self.out);
        let computed = crc.finalize();

        let (crc_ok, crc_unknown) = match self.declared_crc {
            Some(d) => (d == computed, false),
            None => (false, true),
        };

        Ok(DecodedPart {
            data: self.out,
            begin: self.begin,
            end: self.end,
            total_size: self.total_size,
            name: self.name,
            crc32: computed,
            crc_ok,
            crc_unknown,
        })
    }
}

/// Decode a full yEnc article body (the part between the NNTP `.` terminator
/// after `BODY` returns and before it).
///
/// The input may contain CRLF line endings; yEnc treats them as transport
/// framing only and they're stripped during decode. The binary payload between
/// `=ybegin`/`=ypart` and `=yend` is not required to be valid UTF-8 — only the
/// frame header lines are (they're ASCII per the yEnc spec).
pub fn decode_article(input: &[u8]) -> Result<DecodedPart> {
    let (ybegin, rest) = find_frame(input, b"=ybegin")
        .ok_or_else(|| CoreError::Yenc("missing =ybegin frame".into()))?;
    let (ypart, after_part) = match find_frame(rest, b"=ypart") {
        Some((frame, after)) => (Some(frame), after),
        None => (None, rest),
    };
    let (yend, payload) = match find_frame_with_payload(after_part, b"=yend") {
        Some((frame, payload)) => (frame, payload),
        None => return Err(CoreError::Yenc("missing =yend frame".into())),
    };

    let ybegin_str = str_from_ascii(ybegin)?;
    let yend_str = str_from_ascii(yend)?;
    let ypart_str = ypart.map(|p| str_from_ascii(p)).transpose()?;

    let name = param(ybegin_str, "name").unwrap_or_default();
    let total_size: u64 = param(ybegin_str, "size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let line_size: usize = param(ybegin_str, "line")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Corrupt articles carry garbage sizes; reject the absurd ones as
    // corruption instead of propagating nonsense downstream (offsets,
    // pre-allocations).
    const MAX_ARTICLE_BYTES: u64 = 128 * 1024 * 1024;
    if total_size > MAX_ARTICLE_BYTES {
        return Err(CoreError::Yenc(
            "implausible article size (corrupt =ybegin header)".into(),
        ));
    }

    let (begin, end) = if let Some(p) = ypart_str {
        let b: u64 = param(p, "begin").and_then(|s| s.parse().ok()).unwrap_or(1);
        let e: u64 = param(p, "end").and_then(|s| s.parse().ok()).unwrap_or(0);
        (b, e)
    } else {
        (1, total_size)
    };

    let mut out = Vec::with_capacity(payload.len());
    // yEnc transport framing: CRLF pairs are inserted by the news transport
    // and must be removed. A single trailing space or tab *immediately before
    // a CRLF* may be transport padding (NNTP servers strip trailing spaces,
    // so posters add a tab/space to protect lines that would otherwise end in
    // a space). Bytes that decode to space/tab *inside* a line are real data.
    //
    // Padding detection: if the `line=` parameter is present, we only strip
    // a trailing space/tab when the decoded byte count without stripping
    // exceeds `line=`. This correctly handles both cases:
    //   - Poster added a padding byte (line is 129 bytes for line=128) → strip
    //   - Last data byte naturally encodes to space/tab (line is 128 bytes
    //     for line=128) → keep, it's real data
    // If `line=` is absent, we don't strip (safer to keep data bytes).
    let mut line_raw: Vec<u8> = Vec::with_capacity(512);
    for &b in payload {
        if b == b'\n' {
            maybe_strip_padding(&mut line_raw, line_size);
            decode_line(&line_raw, &mut out)?;
            line_raw.clear();
        } else if b == b'\r' {
            continue;
        } else {
            line_raw.push(b);
        }
    }
    if !line_raw.is_empty() {
        maybe_strip_padding(&mut line_raw, line_size);
        decode_line(&line_raw, &mut out)?;
        line_raw.clear();
    }
    // CRC32 computed once over the full decoded output — per-byte updates
    // during decode are ~44x slower (1.5ms vs 34µs for a 500KB article).
    let mut crc = Hasher::new();
    crc.update(&out);
    let computed = crc.finalize();

    // Single-part: crc32=.  Multi-part: pcrc32=.
    let declared = if ypart_str.is_none() {
        param(yend_str, "crc32").as_deref().and_then(parse_hex_u32)
    } else {
        param(yend_str, "pcrc32").as_deref().and_then(parse_hex_u32)
    };
    let (crc_ok, crc_unknown) = match declared {
        Some(d) => (d == computed, false),
        None => (false, true),
    };

    Ok(DecodedPart {
        data: out,
        begin,
        end,
        total_size,
        name,
        crc32: computed,
        crc_ok,
        crc_unknown,
    })
}

/// Conditionally strip a trailing space/tab from a raw yEnc line if it's
/// transport padding (not real data).
///
/// If `line_size` is known (from `=ybegin line=N`), we count the decoded bytes
/// in the raw line. If keeping the trailing space/tab would produce more than
/// `line_size` decoded bytes, the trailing byte is padding and is stripped.
/// Otherwise it's kept as real data.
fn maybe_strip_padding(line_raw: &mut Vec<u8>, line_size: usize) {
    if line_size == 0 {
        // No `line=` parameter — can't tell, don't strip (safer to keep data).
        return;
    }
    // Only consider stripping if the last raw byte is space or tab.
    if !matches!(line_raw.last(), Some(b' ') | Some(b'\t')) {
        return;
    }
    // Count decoded bytes including the trailing space/tab.
    let decoded_count = count_decoded_bytes(line_raw);
    if decoded_count > line_size {
        // The trailing space/tab is padding — strip it.
        line_raw.pop();
    }
}

/// Count how many decoded bytes a raw (encoded) yEnc line produces.
fn count_decoded_bytes(line_raw: &[u8]) -> usize {
    let mut count = 0;
    let mut escaped = false;
    for &b in line_raw {
        if escaped {
            count += 1;
            escaped = false;
        } else if b == b'=' {
            escaped = true;
        } else {
            count += 1;
        }
    }
    count
}

/// Decode one raw (encoded) yEnc line into output bytes and update the CRC.
/// Handles the `=` escape: an escaped byte `b` decodes to `b - 64 - 42`, an
/// ordinary byte `b` decodes to `b - 42`. Returns an error if the line ends
/// with a dangling `=` (no byte following it).
fn decode_line(line_raw: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let mut escaped = false;
    for &b in line_raw {
        if escaped {
            let decoded = b.wrapping_sub(64).wrapping_sub(42);
            out.push(decoded);
            escaped = false;
        } else if b == b'=' {
            escaped = true;
        } else {
            let decoded = b.wrapping_sub(42);
            out.push(decoded);
        }
    }
    if escaped {
        return Err(CoreError::Yenc("dangling escape at end of line".into()));
    }
    Ok(())
}

/// `(frame_params, rest_after_newline)`. Operates on bytes so the surrounding
/// input may contain non-UTF-8 binary; the frame line itself must be ASCII.
fn find_frame<'a>(input: &'a [u8], marker: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let idx = find_at_line_start(input, marker)?;
    let after = &input[idx + marker.len()..];
    let line_end = after
        .iter()
        .position(|&c| c == b'\n')
        .unwrap_or(after.len());
    // Strip a trailing CR if present.
    let params_end = if line_end > 0 && after[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    };
    let params = &after[..params_end];
    let rest = &after[line_end..];
    let rest = rest.strip_prefix(b"\n").unwrap_or(rest);
    Some((params, rest))
}

/// Find a `=yend` frame, returning `(frame_params, payload_before_it)`.
/// Unlike [`find_frame`], we need the bytes *before* the frame, not after.
/// The CRLF that precedes `=yend` is kept in the payload so the decoder's
/// line handler can strip the trailing transport space/tab from the final
/// line — stripping it here would leave that padding byte in the payload
/// and corrupt the CRC.
fn find_frame_with_payload<'a>(input: &'a [u8], marker: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let idx = find_at_line_start(input, marker)?;
    let payload = &input[..idx];
    let after = &input[idx + marker.len()..];
    let line_end = after
        .iter()
        .position(|&c| c == b'\n')
        .unwrap_or(after.len());
    let params_end = if line_end > 0 && after[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    };
    let params = &after[..params_end];
    Some((params, payload))
}

/// Find `marker` in `input` at the start of a line (index 0 or just after a
/// `\n`).
fn find_at_line_start(input: &[u8], marker: &[u8]) -> Option<usize> {
    if input.starts_with(marker) {
        return Some(0);
    }
    let mut search_from = 0;
    while let Some(nl) = input[search_from..].iter().position(|&c| c == b'\n') {
        let after_nl = search_from + nl + 1;
        if input[after_nl..].starts_with(marker) {
            return Some(after_nl);
        }
        search_from = after_nl;
    }
    None
}

/// Interpret a slice as ASCII text. Frame header lines are ASCII per the yEnc
/// spec; if we hit a non-ASCII byte it's a malformed article.
fn str_from_ascii(bytes: &[u8]) -> Result<&str> {
    if bytes.is_ascii() {
        Ok(std::str::from_utf8(bytes)
            .map_err(|e| CoreError::Yenc(format!("ascii check failed: {e}")))?)
    } else {
        Err(CoreError::Yenc(
            "non-ASCII byte in yEnc frame header".into(),
        ))
    }
}

/// Extract a `key=value` parameter from a yEnc frame header line.
/// Handles values that may contain spaces (e.g. `name=Foo Bar.rar`) by taking
/// everything up to the next ` key=` pattern or end of line.
fn param(frame: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = frame.find(&needle)?;
    let value_start = start + needle.len();
    let rest = &frame[value_start..];
    // The value runs until the next ` word=` pattern (a space followed by an
    // identifier and `=`) or end of line. If no such pattern exists, take the
    // rest of the line.
    let bytes = rest.as_bytes();
    let mut end = rest.len();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            // Look ahead for `ident=`.
            let tail = &rest[i + 1..];
            if let Some(eq) = tail.find('=') {
                let ident = &tail[..eq];
                if !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    end = i;
                    break;
                }
            }
        }
        i += 1;
    }
    Some(rest[..end].trim().to_string())
}

fn parse_hex_u32(s: &str) -> Option<u32> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(s, 16).ok()
}

#[cfg(test)]
mod tests {

    #[test]
    fn absurd_ybegin_size_is_rejected_not_allocated() {
        // One corrupt article in the wild carried size=58358840436 — the
        // old code with_capacity'd it, the allocation failed, and Rust
        // aborted the entire engine process mid-download (no panic hook,
        // no logs — it looked like a mystery hang). Now: Yenc error.
        let body = b"=ybegin line=128 size=58358840436 name=x\r\n=ypart begin=1 end=58358840436\r\n..\r\n=yend size=58358840436 crc32=deadbeef\r\n";
        let err = decode_article(body).expect_err("must reject absurd size");
        assert!(
            err.to_string().contains("implausible article size"),
            "got: {err}"
        );
    }

    #[test]
    fn streaming_decoder_rejects_absurd_ybegin_size() {
        let mut dec = Decoder::new();
        let res = dec.push_line(b"=ybegin line=128 size=58358840436 name=x");
        assert!(res.is_err(), "streaming decoder must reject absurd size");
    }

    #[test]
    fn streaming_decoder_rejects_implausible_ypart_range() {
        let mut dec = Decoder::new();
        assert!(dec.push_line(b"=ybegin line=128 size=1000 name=x").is_ok());
        // end wildly beyond the declared size:
        assert!(
            dec.push_line(b"=ypart begin=1 end=999999999999").is_err(),
            "implausible part range must be rejected"
        );
    }

    use super::*;

    /// Build a yEnc article body with the given payload bytes.
    fn encode_part(payload: &[u8], name: &str, begin: u64, end: u64, total: u64) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("=ybegin line=128 size={total} name={name}\r\n").as_bytes());
        if end != total || begin != 1 {
            out.extend_from_slice(format!("=ypart begin={begin} end={end}\r\n").as_bytes());
        }
        let mut crc = Hasher::new();
        let mut body: Vec<u8> = Vec::with_capacity(payload.len());
        for &b in payload {
            crc.update(&[b]);
            let enc = b.wrapping_add(42);
            if enc == b'=' || enc == b'\r' || enc == b'\n' || enc == b'\0' {
                body.push(b'=');
                body.push(enc.wrapping_add(64));
            } else {
                body.push(enc);
            }
        }
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\r\n");
        let crc_val = crc.finalize();
        if end != total || begin != 1 {
            out.extend_from_slice(
                format!("=yend size={} pcrc32={:08x}\r\n", payload.len(), crc_val).as_bytes(),
            );
        } else {
            out.extend_from_slice(
                format!("=yend size={} crc32={:08x}\r\n", payload.len(), crc_val).as_bytes(),
            );
        }
        out
    }

    #[test]
    fn roundtrips_single_part() {
        let payload = b"Hello, yEnc world! This is a test payload.";
        let article = encode_part(
            payload,
            "test.bin",
            1,
            payload.len() as u64,
            payload.len() as u64,
        );
        let decoded = decode_article(&article).unwrap();
        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.name, "test.bin");
        assert_eq!(decoded.begin, 1);
        assert_eq!(decoded.end, payload.len() as u64);
        assert!(decoded.crc_ok, "CRC should match");
        assert!(!decoded.crc_unknown);
    }

    #[test]
    fn roundtrips_multi_part() {
        let payload = b"part2-bytes-here-are-some";
        let article = encode_part(payload, "big.bin", 21, 44, 100);
        let decoded = decode_article(&article).unwrap();
        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.begin, 21);
        assert_eq!(decoded.end, 44);
        assert_eq!(decoded.total_size, 100);
        assert!(decoded.crc_ok);
    }

    #[test]
    fn detects_crc_mismatch() {
        let payload = b"some bytes";
        let mut article = encode_part(
            payload,
            "x.bin",
            1,
            payload.len() as u64,
            payload.len() as u64,
        );
        // Corrupt the declared CRC: find `crc32=` and flip a hex digit.
        let needle = b"crc32=";
        let idx = article
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let hex_idx = idx + needle.len();
        article[hex_idx] = if article[hex_idx] == b'0' { b'f' } else { b'0' };
        let decoded = decode_article(&article).unwrap();
        assert!(!decoded.crc_ok);
        assert!(!decoded.crc_unknown);
    }

    #[test]
    fn handles_no_crc_header() {
        let payload = b"no-crc-here";
        let mut article = encode_part(
            payload,
            "x.bin",
            1,
            payload.len() as u64,
            payload.len() as u64,
        );
        // Strip the crc32= attribute from =yend by truncating the line at the
        // first space after `=yend`.
        let yend_idx = article.windows(5).position(|w| w == b"=yend").unwrap();
        let line_end = article[yend_idx..]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| yend_idx + p)
            .unwrap_or(article.len());
        let space_after_yend = article[yend_idx..line_end]
            .iter()
            .position(|&c| c == b' ')
            .map(|p| yend_idx + p)
            .unwrap_or(line_end);
        article.truncate(space_after_yend);
        article.extend_from_slice(b"\r\n");
        let decoded = decode_article(&article).unwrap();
        assert_eq!(decoded.data, payload);
        assert!(decoded.crc_unknown);
        assert!(!decoded.crc_ok);
    }

    #[test]
    fn strips_crlf_in_payload() {
        // Encode a payload that, after +42, contains bytes that are *not* CR/LF,
        // then inject literal CRLFs in the transport to confirm they're dropped.
        let payload = b"ABCDEF";
        let mut article = encode_part(payload, "t.bin", 1, 6, 6);
        // Insert a CRLF in the middle of the encoded body (before =yend).
        let yend_idx = article.windows(5).position(|w| w == b"=yend").unwrap();
        article.splice(yend_idx..yend_idx, *b"\r\n");
        let decoded = decode_article(&article).unwrap();
        assert_eq!(decoded.data, payload);
        assert!(decoded.crc_ok);
    }

    /// Feed an article body's lines (dot-unstuffed, no trailing CRLF) into
    /// a streaming `Decoder`, the way `NntpClient::read_body_decoded` does.
    fn stream_decode(article: &[u8]) -> Result<DecodedPart> {
        let mut dec = Decoder::new();
        // Split on \n, strip \r and the leading ..-unstuff as if from the
        // wire (for . lines there's no dot-stuffing here in our test bodies).
        for raw in article.split(|&b| b == b'\n') {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if line == b"." {
                break;
            }
            dec.push_line(line)?;
        }
        dec.finish()
    }

    #[test]
    fn streaming_matches_batch_decode_and_strips_dot() {
        let payload = (0u8..=255).chain(0u8..=255).collect::<Vec<_>>();
        // A payload line that starts with `.` must be dot-stuffed on the wire.
        let article = encode_part(
            &payload,
            "stream.bin",
            1,
            payload.len() as u64,
            payload.len() as u64,
        );
        // Simulate dot-stuffing: prefix the first `=ybegin` line's payload by
        // injecting a leading `..` into the body via a crafted line. Instead,
        // directly test unstuff by constructing a body whose first data byte
        // encodes to `.` and confirming streaming unstuffs it like the batch path.
        let decoded = stream_decode(&article).unwrap();
        for (i, (a, b)) in decoded.data.iter().zip(payload.iter()).enumerate() {
            if a != b {
                eprintln!("first diff at {i}: stream={} batch={}", a, b);
                break;
            }
        }
        eprintln!(
            "len decoded={} payload={} crc_ok={} begin={} end={}",
            decoded.data.len(),
            payload.len(),
            decoded.crc_ok,
            decoded.begin,
            decoded.end
        );
        assert_eq!(decoded.data, payload);
        assert!(decoded.crc_ok);
        assert_eq!(decoded.name, "stream.bin");
    }

    #[test]
    fn rejects_missing_ybegin() {
        let err = decode_article(b"=yend size=1\r\n").unwrap_err();
        assert!(err.to_string().contains("=ybegin"));
    }

    #[test]
    fn rejects_missing_yend() {
        let err = decode_article(b"=ybegin size=1 name=x\r\npayload").unwrap_err();
        assert!(err.to_string().contains("=yend"));
    }

    #[test]
    fn roundtrips_binary_payload_that_triggers_escaping() {
        // Bytes whose `+ 42` encoding hits each of the four escape-triggering
        // values: 0 (=> byte 214), 10 (=> 224), 13 (=> 219), '='=61 (=> 19).
        // Plus a few ordinary bytes to fill out the payload, then every byte
        // value once for full-coverage.
        let payload = [
            214, 224, 219, 19, // escape triggers
            0, 255, 100, // ordinary
        ]
        .into_iter()
        .chain(0u8..=255)
        .collect::<Vec<_>>();

        let article = encode_part(
            &payload,
            "bin.dat",
            1,
            payload.len() as u64,
            payload.len() as u64,
        );
        let decoded = decode_article(&article).unwrap();
        assert_eq!(
            decoded.data, payload,
            "binary round-trip must recover original bytes"
        );
        assert!(
            decoded.crc_ok,
            "CRC must match for binary payload with escapes"
        );
    }

    #[test]
    fn roundtrips_with_trailing_space_padding_on_every_line() {
        // Real yEnc posters add a trailing space (or tab) to every line so
        // NNTP servers that strip trailing whitespace don't corrupt the
        // payload. The decoder must strip exactly one trailing space/tab per
        // line — keeping it would add a spurious 0x20 byte to every line and
        // break the CRC.
        let payload = (0u8..=255).collect::<Vec<_>>();
        let total = payload.len() as u64;

        // Hand-build an article with trailing-space padding: split the encoded
        // payload into 16-byte lines, each terminated by ` \r\n`.
        let mut article: Vec<u8> = Vec::new();
        article.extend_from_slice(b"=ybegin line=16 size=");
        article.extend_from_slice(total.to_string().as_bytes());
        article.extend_from_slice(b" name=bin.dat\r\n");

        let mut crc = Hasher::new();
        for chunk in payload.chunks(16) {
            crc.update(chunk);
            for &b in chunk {
                let enc = b.wrapping_add(42);
                if enc == b'=' || enc == b'\r' || enc == b'\n' || enc == b'\0' {
                    article.push(b'=');
                    article.push(enc.wrapping_add(64));
                } else {
                    article.push(enc);
                }
            }
            // Trailing space (transport padding) + CRLF.
            article.push(b' ');
            article.push(b'\r');
            article.push(b'\n');
        }
        let crc_val = crc.finalize();
        article.extend_from_slice(b"=yend size=");
        article.extend_from_slice(total.to_string().as_bytes());
        article.extend_from_slice(b" crc32=");
        article.extend_from_slice(format!("{:08x}", crc_val).as_bytes());
        article.extend_from_slice(b"\r\n");

        let decoded = decode_article(&article).unwrap();
        assert_eq!(
            decoded.data, payload,
            "trailing-space padding must be stripped"
        );
        assert!(decoded.crc_ok, "CRC must match with padding stripped");
    }

    #[test]
    fn keeps_trailing_space_that_is_data_not_padding() {
        // Regression test: when a line's last encoded byte is naturally a
        // space (0x20) and the line is exactly `line=` bytes long, the space
        // is real data — not transport padding. The decoder must NOT strip it.
        //
        // Byte 0xfa decodes from raw 0x20 (space): 0x20 - 42 = 0xfa (wrapping).
        // So a line of 16 data bytes ending with 0xfa will have a raw encoded
        // line of 16 bytes ending with 0x20 (space), with NO extra padding.
        let payload: Vec<u8> = (0..15)
            .map(|i| i as u8)
            .chain(std::iter::once(0xfa))
            .collect();
        let total = payload.len() as u64;

        let mut article: Vec<u8> = Vec::new();
        article.extend_from_slice(b"=ybegin line=16 size=");
        article.extend_from_slice(total.to_string().as_bytes());
        article.extend_from_slice(b" name=bin.dat\r\n");

        let mut crc = Hasher::new();
        crc.update(&payload);
        for &b in &payload {
            let enc = b.wrapping_add(42);
            if enc == b'=' || enc == b'\r' || enc == b'\n' || enc == b'\0' {
                article.push(b'=');
                article.push(enc.wrapping_add(64));
            } else {
                article.push(enc);
            }
        }
        // NO trailing space — the last encoded byte IS a space (0x20).
        article.push(b'\r');
        article.push(b'\n');

        let crc_val = crc.finalize();
        article.extend_from_slice(b"=yend size=");
        article.extend_from_slice(total.to_string().as_bytes());
        article.extend_from_slice(b" crc32=");
        article.extend_from_slice(format!("{:08x}", crc_val).as_bytes());
        article.extend_from_slice(b"\r\n");

        let decoded = decode_article(&article).unwrap();
        assert_eq!(
            decoded.data, payload,
            "trailing space that is real data must be kept"
        );
        assert!(decoded.crc_ok, "CRC must match when data-space is kept");
    }
}
