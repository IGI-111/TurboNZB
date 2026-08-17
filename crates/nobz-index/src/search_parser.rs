//! Parser for Newznab search XML responses.
//!
//! Newznab search results are RSS 2.0 with a `newznab` namespace for extra
//! attributes.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::types::*;

/// A parsed Newznab error (`<error code="200" description="..."/>`).
pub struct NewznabError {
    pub code: u16,
    pub description: String,
}

pub fn parse_error(xml: &str) -> Option<NewznabError> {
    let trimmed = xml.trim_start();
    if !trimmed.starts_with("<?xml") && !trimmed.starts_with("<error") {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    loop {
        let event = reader.read_event_into(&mut buf).ok()?;
        if let Event::Empty(ref e) | Event::Start(ref e) = event {
            if e.name().as_ref() == b"error" {
                let mut code = 0u16;
                let mut description = String::new();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"code" => code = attr_to_u16(&attr.value),
                        b"description" => description = attr_to_string(&attr.value),
                        _ => {}
                    }
                }
                return Some(NewznabError { code, description });
            }
        }
        if matches!(event, Event::Eof) {
            return None;
        }
        buf.clear();
    }
}

pub fn parse_search_results(xml: &str, indexer: &str) -> quick_xml::Result<Vec<SearchResult>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut results = Vec::new();
    let mut buf = Vec::new();

    let mut in_item = false;
    let mut current_tag = String::new();
    let mut item = ItemBuilder {
        indexer: indexer.to_string(),
        ..Default::default()
    };

    loop {
        let event = reader.read_event_into(&mut buf)?;
        match event {
            Event::Start(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "item" => {
                        in_item = true;
                        item = ItemBuilder {
                            indexer: indexer.to_string(),
                            ..Default::default()
                        };
                    }
                    _ if in_item => {
                        current_tag = name.clone();
                        if name == "enclosure" {
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"url" => item.nzb_url = attr_to_string(&attr.value),
                                    b"length" => item.size = attr_to_u64(&attr.value),
                                    _ => {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Empty(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if in_item && name == "enclosure" {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"url" => item.nzb_url = attr_to_string(&attr.value),
                            b"length" => item.size = attr_to_u64(&attr.value),
                            _ => {}
                        }
                    }
                }
                // newznab:attr can be self-closing
                if in_item && name.ends_with("attr") {
                    let mut attr_name = String::new();
                    let mut attr_value = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"name" => attr_name = attr_to_string(&attr.value),
                            b"value" => attr_value = attr_to_string(&attr.value),
                            _ => {}
                        }
                    }
                    item.attrs.insert(attr_name, attr_value);
                }
            }
            Event::Text(ref text) if in_item && !current_tag.is_empty() => {
                let text = text.unescape().unwrap_or_default().into_owned();
                match current_tag.as_str() {
                    "title" => item.title.push_str(&text),
                    "guid" => item.guid.push_str(&text),
                    "pubDate" => item.post_date_str.push_str(&text),
                    "category" => item.category_name.push_str(&text),
                    _ => {}
                }
            }
            Event::End(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "item" => {
                        if in_item {
                            let built = item.build();
                            item = ItemBuilder::default();
                            results.push(built);
                        }
                        in_item = false;
                    }
                    _ if in_item => {
                        current_tag.clear();
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(results)
}

#[derive(Default)]
struct ItemBuilder {
    title: String,
    guid: String,
    nzb_url: String,
    size: u64,
    post_date_str: String,
    category_name: String,
    indexer: String,
    attrs: BTreeMap<String, String>,
}

impl ItemBuilder {
    fn build(self) -> SearchResult {
        let category: u32 = self
            .attrs
            .get("category")
            .and_then(|v| v.parse::<u32>().ok())
            .map(cats::normalize)
            .unwrap_or(0);

        let files: u32 = self
            .attrs
            .get("files")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let grabs: u32 = self
            .attrs
            .get("grabs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let password: PasswordStatus = self
            .attrs
            .get("password")
            .and_then(|v| v.parse::<u32>().ok())
            .map(PasswordStatus::from)
            .unwrap_or(PasswordStatus::Unknown);

        let tv = {
            let season = self.attrs.get("season").and_then(|v| v.parse().ok());
            let episode = self.attrs.get("episode").and_then(|v| v.parse().ok());
            let rage_id = self.attrs.get("rageid").and_then(|v| v.parse().ok());
            let tvdb_id = self.attrs.get("tvdbid").and_then(|v| v.parse().ok());
            let tvmaze_id = self.attrs.get("tvmazeid").and_then(|v| v.parse().ok());
            let title = self.attrs.get("tvtitle").cloned();
            let air_date = self.attrs.get("tvairdate").cloned();

            if season.is_some()
                || episode.is_some()
                || rage_id.is_some()
                || tvdb_id.is_some()
                || tvmaze_id.is_some()
                || title.is_some()
            {
                Some(TvInfo {
                    season,
                    episode,
                    rage_id,
                    tvdb_id,
                    tvmaze_id,
                    title,
                    air_date,
                })
            } else {
                None
            }
        };

        let movie = {
            let imdb_id = self.attrs.get("imdb").cloned();
            let imdb_score = self.attrs.get("imdbscore").cloned();
            let imdb_year = self.attrs.get("imdbyear").and_then(|v| v.parse().ok());
            let genre = self.attrs.get("genre").cloned();

            if imdb_id.is_some() || imdb_score.is_some() || imdb_year.is_some() || genre.is_some() {
                Some(MovieInfo {
                    imdb_id,
                    imdb_score,
                    imdb_year,
                    genre,
                })
            } else {
                None
            }
        };

        let post_date = parse_rfc2822(&self.post_date_str).unwrap_or(0);

        let nzb_url = if self.nzb_url.is_empty() && self.guid.starts_with("http") {
            self.guid.clone()
        } else {
            self.nzb_url
        };

        SearchResult {
            title: self.title,
            guid: self.guid,
            nzb_url,
            size: self.size,
            post_date,
            category,
            category_name: self.category_name,
            grabs,
            files,
            password,
            indexer: self.indexer,
            tv,
            movie,
        }
    }
}

fn attr_to_string(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

fn attr_to_u16(v: &[u8]) -> u16 {
    String::from_utf8_lossy(v).parse().unwrap_or(0)
}

fn attr_to_u64(v: &[u8]) -> u64 {
    String::from_utf8_lossy(v).parse().unwrap_or(0)
}

/// Best-effort RFC 2822 date parser → Unix timestamp.
fn parse_rfc2822(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }

    let day: u32 = parts[1].trim_end_matches(',').parse().ok()?;
    let year: i32 = parts[3].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };

    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u32 = time_parts[0].parse().ok()?;
    let min: u32 = time_parts[1].parse().ok()?;
    let sec: u32 = time_parts[2].parse().ok()?;

    let days = days_from_civil(year, month, day);
    let ts = days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + sec as i64;

    let tz = parts[5];
    if tz.len() >= 5 && (tz.starts_with('+') || tz.starts_with('-')) {
        let sign = if tz.starts_with('+') { 1 } else { -1 };
        let tz_h: i64 = tz[1..3].parse().unwrap_or(0);
        let tz_m: i64 = tz[3..5].parse().unwrap_or(0);
        let offset = sign * (tz_h * 3600 + tz_m * 60);
        return Some((ts - offset) as u64);
    }

    Some(ts as u64)
}

/// Howard Hinnant's days_from_civil algorithm. Returns days since 1970-01-01.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146097 + doe as i64 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PasswordStatus;

    #[test]
    fn test_parse_error_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<error code="100" description="Incorrect user credentials"/>"#;

        let err = parse_error(xml).unwrap();
        assert_eq!(err.code, 100);
        assert_eq!(err.description, "Incorrect user credentials");
    }

    #[test]
    fn test_parse_error_not_present() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Example</title>
  </channel>
</rss>"#;

        assert!(parse_error(xml).is_none());
    }

    #[test]
    fn test_parse_search_results_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>example.com API</title>
    <link>https://example.com/</link>
    <description>API Results</description>
    <newznab:response offset="0" total="1"/>
    <item>
      <title>Some.Release.S01E05.1080p.WEB.x264</title>
      <guid>abc123def456</guid>
      <pubDate>Sun, 06 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV &gt; HD</category>
      <enclosure url="https://example.com/nzb/abc123def456" length="1234567890" type="application/x-nzb"/>
      <newznab:attr name="category" value="5040"/>
      <newznab:attr name="size" value="1234567890"/>
      <newznab:attr name="files" value="45"/>
      <newznab:attr name="grabs" value="3"/>
      <newznab:attr name="password" value="0"/>
      <newznab:attr name="season" value="1"/>
      <newznab:attr name="episode" value="5"/>
    </item>
  </channel>
</rss>"#;

        let results = parse_search_results(xml, "test_indexer").unwrap();
        assert_eq!(results.len(), 1);

        let r = &results[0];
        assert_eq!(r.title, "Some.Release.S01E05.1080p.WEB.x264");
        assert_eq!(r.guid, "abc123def456");
        assert_eq!(r.nzb_url, "https://example.com/nzb/abc123def456");
        assert_eq!(r.size, 1234567890);
        assert_eq!(r.category, 5000); // normalized from 5040
        assert_eq!(r.category_name, "TV > HD");
        assert_eq!(r.grabs, 3);
        assert_eq!(r.files, 45);
        assert_eq!(r.password, PasswordStatus::None);
        assert_eq!(r.indexer, "test_indexer");
        assert_eq!(r.post_date, 1275841763);

        let tv = r.tv.as_ref().unwrap();
        assert_eq!(tv.season, Some(1));
        assert_eq!(tv.episode, Some(5));
    }

    #[test]
    fn test_parse_search_results_multiple() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>example.com API</title>
    <link>https://example.com/</link>
    <description>API Results</description>
    <newznab:response offset="0" total="2"/>
    <item>
      <title>Release.One</title>
      <guid>guid1</guid>
      <pubDate>Sun, 06 Jun 2010 17:29:23 +0100</pubDate>
      <category>Movies &gt; HD</category>
      <enclosure url="https://example.com/nzb/guid1" length="1000000000" type="application/x-nzb"/>
      <newznab:attr name="category" value="2040"/>
      <newznab:attr name="size" value="1000000000"/>
      <newznab:attr name="imdb" value="tt0058935"/>
      <newznab:attr name="imdbscore" value="8.5"/>
    </item>
    <item>
      <title>Release.Two</title>
      <guid>https://example.com/details/guid2</guid>
      <pubDate>Mon, 07 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV</category>
      <newznab:attr name="category" value="5000"/>
      <newznab:attr name="password" value="1"/>
    </item>
  </channel>
</rss>"#;

        let results = parse_search_results(xml, "test_indexer").unwrap();
        assert_eq!(results.len(), 2);

        let r1 = &results[0];
        assert_eq!(r1.title, "Release.One");
        assert_eq!(r1.category, 2000); // normalized from 2040
        let m = r1.movie.as_ref().unwrap();
        assert_eq!(m.imdb_id.as_deref(), Some("tt0058935"));

        let r2 = &results[1];
        assert_eq!(r2.title, "Release.Two");
        assert_eq!(r2.password, PasswordStatus::Rar);
        assert_eq!(r2.nzb_url, "https://example.com/details/guid2");
    }

    #[test]
    fn test_parse_search_results_empty() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>example.com API</title>
    <link>https://example.com/</link>
    <description>API Results</description>
    <newznab:response offset="0" total="0"/>
  </channel>
</rss>"#;

        let results = parse_search_results(xml, "test_indexer").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_rfc2822() {
        // 2010-06-06 17:29:23 +0100 = 2010-06-06 16:29:23 UTC = 1275841763
        let ts = parse_rfc2822("Sun, 06 Jun 2010 17:29:23 +0100").unwrap();
        assert_eq!(ts, 1275841763);

        // GMT — same UTC time
        let ts2 = parse_rfc2822("Sun, 06 Jun 2010 16:29:23 GMT").unwrap_or(0);
        assert_eq!(ts2, 1275841763);

        // Negative offset: -0100 means UTC 18:29:23 = 1275841763 + 2*3600
        let ts3 = parse_rfc2822("Sun, 06 Jun 2010 17:29:23 -0100").unwrap();
        assert_eq!(ts3, 1275841763 + 2 * 3600);

        assert!(parse_rfc2822("").is_none());
        assert!(parse_rfc2822("not a date").is_none());
    }
}
