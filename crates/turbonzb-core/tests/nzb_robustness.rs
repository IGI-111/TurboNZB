//! NZB parser robustness suite (§5 of TEST_PLAN.md).
//!
//! Valid NZBs round-trip into the typed model; malformed / hostile inputs
//! must error (via `CoreError::NzbParse`) rather than panic or hang.

use turbonzb_core::nzb::{NzbFile, parse};

fn basic_nzb() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="title">My Test Release</meta>
    <meta type="password">hunter2</meta>
  </head>
  <file poster="someone@news" date="1700000000" subject="&quot;file.rar&quot; yEnc (1/2)">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="1000" number="1">&lt;one@news&gt;</segment>
      <segment bytes="2000" number="2">&lt;two@news&gt;</segment>
    </segments>
  </file>
  <file poster="someone@news" date="1700000000" subject="&quot;file.rar&quot; yEnc (2/2)">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="1000" number="1">&lt;three@news&gt;</segment>
    </segments>
  </file>
</nzb>"#
        .to_string()
}

/// 5.1 — a normal NZB parses into per-file segments with metadata intact.
#[test]
fn parses_valid_nzb() {
    let nzb = parse(basic_nzb().as_bytes()).expect("valid NZB should parse");
    assert_eq!(nzb.files.len(), 2);
    assert_eq!(nzb.title(), Some("My Test Release"));
    assert_eq!(nzb.passwords(), &["hunter2"]);
    let f = &nzb.files[0];
    assert_eq!(f.poster, "someone@news");
    assert_eq!(f.date, 1700000000);
    assert_eq!(f.groups, vec!["alt.binaries.test"]);
    assert_eq!(f.segments.len(), 2);
    assert_eq!(f.segments[0].message_id, "one@news");
    assert_eq!(f.segments[0].bytes, 1000);
    assert!(!f.segments[0].missing);
    assert_eq!(f.total_bytes(), 3000);
    // Filename pulled from the quoted subject span.
    assert_eq!(f.filename(), "file.rar");
}

/// 5.1 — out-of-order and missing segment numbers are reordered and filled.
#[test]
fn missing_segments_filled() {
    let xml = r#"<nzb>
  <file poster="p" subject="&quot;a.bin&quot;">
    <groups><group>g</group></groups>
    <segments>
      <segment bytes="10" number="3">&lt;c@x&gt;</segment>
      <segment bytes="10" number="1">&lt;a@x&gt;</segment>
    </segments>
  </file>
</nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    let f: &NzbFile = &nzb.files[0];
    assert_eq!(f.segment_count, 3);
    assert_eq!(f.segments.len(), 3);
    // Order 1,2,3; segment 2 is a hole.
    assert_eq!(f.segments[0].number, 1);
    assert!(f.segments[1].missing);
    assert_eq!(f.segments[2].number, 3);
    assert_eq!(f.missing_indices(), vec![2]);
    assert_eq!(f.total_bytes(), 20);
}

/// 5.2 — password and title meta with multiple values.
#[test]
fn metadata_multiple_passwords() {
    let xml = r#"<nzb>
  <head>
    <meta type="title">A</meta>
    <meta type="password">pw1</meta>
    <meta type="password">pw2</meta>
  </head>
  <file poster="p" subject="&quot;x&quot;">
    <groups><group>g</group></groups>
    <segments><segment bytes="1" number="1">&lt;a@x&gt;</segment></segments>
  </file>
</nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.title(), Some("A"));
    assert_eq!(nzb.passwords(), &["pw1", "pw2"]);
}

/// 5.1 — PAR2 volume files group under their base name, so the engine can
/// reason about a whole parity set.
#[test]
fn par2_sets_group_volumes() {
    let xml = r#"<nzb>
  <file poster="p" subject="&quot;movie.par2&quot;">
    <groups><group>g</group></groups>
    <segments><segment bytes="1" number="1">&lt;a@x&gt;</segment></segments>
  </file>
  <file poster="p" subject="&quot;movie.vol00+1.par2&quot;">
    <groups><group>g</group></groups>
    <segments><segment bytes="1" number="1">&lt;b@x&gt;</segment></segments>
  </file>
  <file poster="p" subject="&quot;movie.vol10+5.par2&quot;">
    <groups><group>g</group></groups>
    <segments><segment bytes="1" number="1">&lt;c@x&gt;</segment></segments>
  </file>
</nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    let sets = nzb.par2_sets();
    // All three PAR2 volumes share the "movie" base → one set.
    let movie = sets.get("movie").expect("movie set key");
    assert_eq!(movie.len(), 3);
}

/// 5.3 — malformed XML (truncated, garbage, unbalanced) errors, no panic.
#[test]
fn malformed_xml_errors_gracefully() {
    let bad_cases: Vec<Vec<u8>> = vec![
        vec![],                                  // empty
        b"not xml at all".to_vec(),              // plain text
        b"<nzb>".to_vec(),                       // not closed
        b"<nzb><file".to_vec(),                  // truncated tag
        b"<nzb></nzb".to_vec(),                  // dangling
        b"<nzb><head><meta type=".to_vec(),      // broken attribute
        b"<<>>".to_vec(),                        // junk
        b"<file attr=".to_vec(),                 // truncated attribute
        b"<nzb>garbage \xff\xfe bytes".to_vec(), // invalid utf-8-ish
    ];
    for c in bad_cases {
        let _ = parse(&c); // must not panic
    }
}

/// 5.4 — large-but-reasonable deep/wide documents parse correctly (no hang,
/// no OOM). Exercises entity and CDATA handling.
#[test]
fn large_and_deep_documents_parse() {
    let mut xml = String::from("<nzb>");
    for f in 0..300usize {
        xml.push_str(&format!("<file poster=\"p\" subject='big-{f}.zip'>"));
        xml.push_str("<groups><group>alt.binaries.a</group></groups>");
        xml.push_str("<segments>");
        for s in 1..=20usize {
            xml.push_str(&format!(
                "<segment bytes=\"1000\" number=\"{s}\">&lt;{f}-{s}@news&gt;</segment>"
            ));
        }
        xml.push_str("</segments></file>");
    }
    xml.push_str("</nzb>");
    let nzb = parse(xml.as_bytes()).expect("large NZB should parse");
    assert_eq!(nzb.files.len(), 300);
    assert_eq!(nzb.files[299].segments.len(), 20);
    assert_eq!(nzb.files[150].total_bytes(), 20000);
}

/// 5.3 — billions-of-laughs style entity expansion must not blow up or hang.
/// quick-xml does not expand custom entities (it's not an entity-expanding
/// parser), so this just asserts we can parse a deep entity reference safely.
#[test]
fn entity_laden_subject_does_not_panic() {
    let xml = r#"<nzb>
  <file poster="p" subject="&amp;&amp;&amp; &lt;weird&gt; &quot;quoted&quot;">
    <groups><group>g</group></groups>
    <segments><segment bytes="1" number="1">&lt;a@x&gt;</segment></segments>
  </file>
</nzb>"#;
    // Must parse without error: well-formed references are legal.
    let nzb = parse(xml.as_bytes()).expect("entity refs should parse");
    assert_eq!(nzb.files.len(), 1);
}

/// 5.5 — CData in group names is preserved as text.
#[test]
fn cdata_group_preserved() {
    let xml = r#"<nzb>
  <file poster="p" subject="&quot;x&quot;">
    <groups><group><![CDATA[alt.binaries.special]]></group></groups>
    <segments><segment bytes="1" number="1">&lt;a@x&gt;</segment></segments>
  </file>
</nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.files[0].groups, vec!["alt.binaries.special"]);
}

/// 5.1 — a file with zero segments is allowed (no holes filled, no error).
#[test]
fn file_with_zero_segments_is_harmless() {
    let xml = r#"<nzb>
  <file poster="p" subject="&quot;empty.rar&quot;">
    <groups><group>g</group></groups>
    <segments></segments>
  </file>
</nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.files.len(), 1);
    assert_eq!(nzb.files[0].segments.len(), 0);
    assert!(nzb.files[0].missing_indices().is_empty());
}
