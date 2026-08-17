//! NZB XML parser → segment graph.
//!
//! Produces a typed representation of an NZB file: a list of `NzbFile`s, each
//! with an ordered list of `Segment`s. PAR2 sets are detected by subject
//! prefix matching (the convention used by SABnzbd and friends: a `.par2`
//! file with a `.vol` suffix implies a parity set anchored on the base name).
//! Missing segments are reported as `Segment::missing` holes in the
//! `1..=num_segments` ordering.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::{Event, attributes::Attribute};
use quick_xml::name::QName;

use crate::error::{CoreError, Result};

/// Top-level NZB document.
#[derive(Debug, Clone, Default)]
pub struct Nzb {
    /// `<meta>` entries from `<head>`, keyed by `type` attribute.
    pub meta: BTreeMap<String, Vec<String>>,
    /// All `<file>` entries, in document order.
    pub files: Vec<NzbFile>,
}

impl Nzb {
    /// Convenience: the `<meta type="title">` value, if present.
    pub fn title(&self) -> Option<&str> {
        self.meta
            .get("title")
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// Convenience: the `<meta type="password">` values, if any.
    pub fn passwords(&self) -> &[String] {
        self.meta.get("password").map(Vec::as_slice).unwrap_or(&[])
    }

    /// Group all files into PAR2 sets, keyed by the parity-set base name.
    ///
    /// Files whose subject does not match a `.par2` pattern are assigned to
    /// their own singleton set keyed by their subject — they still need
    /// downloading, they just don't participate in parity repair.
    pub fn par2_sets(&self) -> BTreeMap<String, Vec<&NzbFile>> {
        let mut sets: BTreeMap<String, Vec<&NzbFile>> = BTreeMap::new();
        for f in &self.files {
            let key = par2_set_key(&f.subject).unwrap_or_else(|| f.filename());
            sets.entry(key).or_default().push(f);
        }
        sets
    }
}

/// A single `<file>` entry inside an NZB.
#[derive(Debug, Clone, Default)]
pub struct NzbFile {
    /// `poster` attribute.
    pub poster: String,
    /// Unix epoch seconds from the `date` attribute.
    pub date: u64,
    /// Raw `subject` attribute — used for filename guessing and PAR2 grouping.
    pub subject: String,
    /// Newsgroups listed under `<groups>`.
    pub groups: Vec<String>,
    /// Segments in declared order. Holes (missing numbers in
    /// `1..=expected_count`) are represented as `Segment::missing == true`.
    pub segments: Vec<Segment>,
    /// Highest `number` seen across segments, used to size the hole check.
    pub segment_count: u32,
}

impl NzbFile {
    /// Total byte size across all known segments.
    pub fn total_bytes(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| !s.missing)
            .map(|s| s.bytes)
            .sum()
    }

    /// Indices (1-based) that are absent from the NZB.
    pub fn missing_indices(&self) -> Vec<u32> {
        let have: std::collections::BTreeSet<u32> = self
            .segments
            .iter()
            .filter(|s| !s.missing)
            .map(|s| s.number)
            .collect();
        (1..=self.segment_count)
            .filter(|n| !have.contains(n))
            .collect()
    }

    /// Best-effort filename extracted from the subject.
    ///
    /// NZB subjects conventionally look like `"Some File.rar" (1/10)`, so we
    /// take the first quoted span; otherwise we fall back to the full subject.
    pub fn filename(&self) -> String {
        let s = &self.subject;
        if let (Some(open), Some(close)) = (s.find('"'), s.rfind('"')) {
            if open < close {
                return s[open + 1..close].to_string();
            }
        }
        s.split_whitespace().next().unwrap_or(s).to_string()
    }
}

/// One `<segment>` of a file. A `missing` segment is a hole synthesized from
/// the declared `segment_count` vs the actual segment numbers seen.
#[derive(Debug, Clone, Default)]
pub struct Segment {
    /// 1-based segment number from the `number` attribute.
    pub number: u32,
    /// Declared byte size from the `bytes` attribute.
    pub bytes: u64,
    /// The `Message-ID` of the article (without angle brackets).
    pub message_id: String,
    /// `true` for synthesized holes — there is no article to fetch.
    pub missing: bool,
}

/// Parse an NZB document from XML bytes.
pub fn parse(xml: &[u8]) -> Result<Nzb> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut nzb = Nzb::default();
    let mut state = State::Root;
    let mut current_file: Option<NzbFile> = None;
    let mut current_segment: Option<Segment> = None;
    let mut meta_key: Option<String> = None;
    let mut text_buf = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name());
                text_buf.clear();
                match (&state, name.as_ref()) {
                    (State::Root, "nzb") => state = State::InNzb,
                    (State::InNzb, "head") => state = State::InHead,
                    (State::InNzb, "file") => {
                        let mut f = NzbFile::default();
                        for attr in e.attributes() {
                            let attr = attr.map_err(|e| CoreError::NzbParse(e.to_string()))?;
                            match local_name(attr.key).as_ref() {
                                "poster" => f.poster = attr_value(&attr),
                                "date" => f.date = attr_value(&attr).parse().unwrap_or(0),
                                "subject" => f.subject = attr_value(&attr),
                                _ => {}
                            }
                        }
                        current_file = Some(f);
                        state = State::InFile;
                    }
                    (State::InFile, "groups") => state = State::InGroups,
                    (State::InFile, "segments") => state = State::InSegments,
                    (State::InHead, "meta") => {
                        meta_key = e.attributes().find_map(|a| {
                            a.ok().and_then(|a| {
                                if local_name(a.key) == "type" {
                                    Some(attr_value(&a))
                                } else {
                                    None
                                }
                            })
                        });
                        state = State::InMeta;
                    }
                    (State::InGroups, "group") => state = State::InGroup,
                    (State::InSegments, "segment") => {
                        let mut seg = Segment::default();
                        for attr in e.attributes() {
                            let attr = attr.map_err(|e| CoreError::NzbParse(e.to_string()))?;
                            match local_name(attr.key).as_ref() {
                                "bytes" => seg.bytes = attr_value(&attr).parse().unwrap_or(0),
                                "number" => seg.number = attr_value(&attr).parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        current_segment = Some(seg);
                        state = State::InSegment;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                text_buf.push_str(
                    &t.unescape()
                        .map_err(|e| CoreError::NzbParse(e.to_string()))?,
                );
            }
            Ok(Event::CData(t)) => {
                text_buf.push_str(&String::from_utf8_lossy(t.as_ref()));
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name());
                match (&state, name.as_ref()) {
                    (State::InNzb, "nzb") => state = State::Root,
                    (State::InHead, "head") => state = State::InNzb,
                    (State::InMeta, "meta") => {
                        if let Some(k) = meta_key.take() {
                            nzb.meta
                                .entry(k)
                                .or_default()
                                .push(text_buf.trim().to_string());
                        }
                        state = State::InHead;
                    }
                    (State::InGroup, "group") => {
                        if let Some(f) = current_file.as_mut() {
                            f.groups.push(text_buf.trim().to_string());
                        }
                        state = State::InGroups;
                    }
                    (State::InGroups, "groups") => state = State::InFile,
                    (State::InSegment, "segment") => {
                        if let Some(mut s) = current_segment.take() {
                            // Strip angle brackets if present (Message-IDs are
                            // conventionally wrapped in `<...>`).
                            let mid = text_buf.trim();
                            s.message_id = mid
                                .strip_prefix('<')
                                .and_then(|m| m.strip_suffix('>'))
                                .unwrap_or(mid)
                                .to_string();
                            if let Some(f) = current_file.as_mut() {
                                f.segment_count = f.segment_count.max(s.number);
                                f.segments.push(s);
                            }
                        }
                        state = State::InSegments;
                    }
                    (State::InSegments, "segments") => state = State::InFile,
                    (State::InFile, "file") => {
                        if let Some(mut f) = current_file.take() {
                            // Sort segments by number and fill missing holes so
                            // the engine can skip them without re-deriving.
                            f.segments.sort_by_key(|s| s.number);
                            let mut full: Vec<Segment> =
                                Vec::with_capacity(f.segment_count as usize);
                            for n in 1..=f.segment_count {
                                if let Some(idx) = f.segments.iter().position(|s| s.number == n) {
                                    full.push(f.segments[idx].clone());
                                } else {
                                    full.push(Segment {
                                        number: n,
                                        bytes: 0,
                                        message_id: String::new(),
                                        missing: true,
                                    });
                                }
                            }
                            f.segments = full;
                            nzb.files.push(f);
                        }
                        state = State::InNzb;
                    }
                    _ => {}
                }
                text_buf.clear();
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(CoreError::NzbParse(e.to_string())),
        }
        buf.clear();
    }

    Ok(nzb)
}

#[derive(Debug, Clone, Copy)]
enum State {
    Root,
    InNzb,
    InHead,
    InMeta,
    InFile,
    InGroups,
    InGroup,
    InSegments,
    InSegment,
}

fn local_name(name: QName<'_>) -> String {
    // QName::local_name() already strips any namespace prefix.
    let s = std::str::from_utf8(name.local_name().into_inner()).unwrap_or("");
    s.to_string()
}

fn attr_value(attr: &Attribute<'_>) -> String {
    attr.unescape_value()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(attr.value.as_ref()).into_owned())
}

/// Derive the PAR2-set key for a subject, returning `None` if the file is not
/// part of a parity set.
///
/// Conventional PAR2 subjects look like:
///   `"Some Release.par2"`
///   `"Some Release.vol000+01.par2"`
/// We strip the `.vol...` and `.par2` suffixes to get the anchor name, so all
/// `.vol` files plus the base `.par2` land in one bucket.
fn par2_set_key(subject: &str) -> Option<String> {
    let lower = subject.to_ascii_lowercase();
    if !lower.contains(".par2") {
        return None;
    }
    // Pull the quoted filename if there is one, else the whole subject.
    let raw = subject
        .find('"')
        .and_then(|open| {
            subject[open + 1..]
                .find('"')
                .map(|_| subject.split('"').nth(1).unwrap_or(subject))
        })
        .unwrap_or(subject);
    let lower_raw = raw.to_ascii_lowercase();
    let vol_idx = lower_raw.find(".vol");
    let stem = if let Some(i) = vol_idx {
        &raw[..i]
    } else {
        &raw[..raw.to_ascii_lowercase().find(".par2")?]
    };
    Some(stem.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="title">Demo Release</meta>
    <meta type="password">hunter2</meta>
  </head>
  <file poster="poster@example.com" date="1700000000" subject="&quot;Demo.part1.rar&quot; (1/3)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">a@x</segment>
      <segment bytes="200" number="2">b@x</segment>
      <segment bytes="300" number="3">c@x</segment>
    </segments>
  </file>
  <file poster="poster@example.com" date="1700000001" subject="&quot;Demo.part2.rar&quot; (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">d@x</segment>
      <!-- segment 2 is missing on purpose -->
      <segment bytes="300" number="3">f@x</segment>
    </segments>
  </file>
  <file poster="poster@example.com" date="1700000002" subject="&quot;Demo.par2&quot;">
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="50" number="1">p@x</segment></segments>
  </file>
  <file poster="poster@example.com" date="1700000003" subject="&quot;Demo.vol000+01.par2&quot;">
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="50" number="1">v@x</segment></segments>
  </file>
</nzb>"#;

    #[test]
    fn parses_meta() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(nzb.title(), Some("Demo Release"));
        assert_eq!(nzb.passwords(), &["hunter2".to_string()]);
    }

    #[test]
    fn fills_missing_segments() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        let part2 = &nzb.files[1];
        assert_eq!(part2.segment_count, 3);
        assert_eq!(part2.segments.len(), 3);
        assert!(!part2.segments[0].missing);
        assert!(part2.segments[1].missing, "segment 2 should be a hole");
        assert!(!part2.segments[2].missing);
        assert_eq!(part2.missing_indices(), vec![2]);
    }

    #[test]
    fn extracts_filename_from_quoted_subject() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(nzb.files[0].filename(), "Demo.part1.rar");
        assert_eq!(nzb.files[2].filename(), "Demo.par2");
    }

    #[test]
    fn groups_par2_set() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        let sets = nzb.par2_sets();
        let par2 = sets.get("Demo").expect("Demo par2 set");
        assert_eq!(par2.len(), 2, "base .par2 and .vol file should group");
        // Non-par2 files get their own singleton sets.
        assert!(sets.contains_key("Demo.part1.rar"));
    }

    #[test]
    fn total_bytes_excludes_missing() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        let part2 = &nzb.files[1];
        // 100 + 300, missing segment contributes 0.
        assert_eq!(part2.total_bytes(), 400);
    }

    #[test]
    fn strips_message_id_brackets() {
        let nzb = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(nzb.files[0].segments[0].message_id, "a@x");
    }

    #[test]
    fn handles_empty_nzb() {
        let nzb = parse(b"<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\"></nzb>").unwrap();
        assert!(nzb.files.is_empty());
    }

    #[test]
    fn handles_namespaced_attributes() {
        // Some NZBs wrap everything in a default namespace; attributes use
        // unprefixed names per the spec, but be defensive anyway.
        let xml = br#"<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
            <file subject="&quot;x.rar&quot;" date="1" poster="p">
              <segments><segment bytes="1" number="1">m@i</segment></segments>
            </file></nzb>"#;
        let nzb = parse(xml).unwrap();
        assert_eq!(nzb.files.len(), 1);
        assert_eq!(nzb.files[0].poster, "p");
    }
}
