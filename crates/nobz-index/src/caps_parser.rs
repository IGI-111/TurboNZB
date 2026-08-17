//! Parser for Newznab `t=caps` XML responses.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::types::{CapsCategory, IndexerCaps, SearchCaps};

pub fn parse_caps(xml: &str) -> quick_xml::Result<IndexerCaps> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut caps = IndexerCaps::default();
    let mut buf = Vec::new();

    let mut in_searching = false;
    let mut in_categories = false;
    let mut current_category: Option<CapsCategory> = None;
    let mut current_subcat: Option<CapsCategory> = None;
    let mut in_subcats = false;

    loop {
        let event = reader.read_event_into(&mut buf)?;
        // Treat Start and Empty the same for attribute extraction
        let start = match &event {
            Event::Start(e) => Some(e),
            Event::Empty(e) => Some(e),
            _ => None,
        };

        if let Some(e) = start {
            match e.name().as_ref() {
                b"caps" => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"version" => caps.protocol_version = attr_to_string(&attr.value),
                            b"title" => caps.title = attr_to_string(&attr.value),
                            b"email" => caps.email = attr_to_string(&attr.value),
                            b"url" => caps.url = attr_to_string(&attr.value),
                            _ => {}
                        }
                    }
                }
                b"server" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"version" {
                            caps.server_version = attr_to_string(&attr.value);
                        }
                    }
                }
                b"limits" => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"max" => caps.max_results = attr_to_u32(&attr.value),
                            b"default" => caps.default_results = attr_to_u32(&attr.value),
                            _ => {}
                        }
                    }
                }
                b"retention" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"days" {
                            caps.retention_days = attr_to_u32(&attr.value).into();
                        }
                    }
                }
                b"searching" => in_searching = true,
                b"search" if in_searching => {
                    caps.search = Some(parse_search_caps(e)?);
                }
                b"tv-search" if in_searching => {
                    caps.tv_search = Some(parse_search_caps(e)?);
                }
                b"movie-search" if in_searching => {
                    caps.movie_search = Some(parse_search_caps(e)?);
                }
                b"audio-search" if in_searching => {
                    caps.audio_search = Some(parse_search_caps(e)?);
                }
                b"book-search" if in_searching => {
                    caps.book_search = Some(parse_search_caps(e)?);
                }
                b"categories" => in_categories = true,
                b"category" if in_categories => {
                    let mut id = 0u32;
                    let mut name = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => id = attr_to_u32(&attr.value),
                            b"name" => name = attr_to_string(&attr.value),
                            _ => {}
                        }
                    }
                    current_category = Some(CapsCategory {
                        id,
                        name,
                        subcats: Vec::new(),
                    });
                }
                b"subcat" if current_category.is_some() => {
                    let mut id = 0u32;
                    let mut name = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => id = attr_to_u32(&attr.value),
                            b"name" => name = attr_to_string(&attr.value),
                            _ => {}
                        }
                    }
                    current_subcat = Some(CapsCategory {
                        id,
                        name,
                        subcats: Vec::new(),
                    });
                    in_subcats = true;
                }
                _ => {}
            }
        }

        // Handle End events for state tracking
        if let Event::End(ref e) = event {
            match e.name().as_ref() {
                b"searching" => in_searching = false,
                b"categories" => in_categories = false,
                b"category" if current_category.is_some() => {
                    if let Some(cat) = current_category.take() {
                        caps.categories.push(cat);
                    }
                }
                b"subcat" if in_subcats => {
                    if let (Some(sc), Some(cat)) =
                        (current_subcat.take(), current_category.as_mut())
                    {
                        cat.subcats.push(sc);
                    }
                    in_subcats = false;
                }
                _ => {}
            }
        }

        // Handle Empty events for subcats (self-closing)
        if let Event::Empty(ref e) = event {
            if in_subcats || e.name().as_ref() == b"subcat" {
                if let (Some(sc), Some(cat)) = (current_subcat.take(), current_category.as_mut()) {
                    cat.subcats.push(sc);
                }
                in_subcats = false;
            }
        }

        if matches!(event, Event::Eof) {
            break;
        }
        buf.clear();
    }

    Ok(caps)
}

fn parse_search_caps(e: &quick_xml::events::BytesStart<'_>) -> quick_xml::Result<SearchCaps> {
    let mut available = false;
    let mut supported_params = std::collections::BTreeSet::new();

    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"available" => available = attr.value.as_ref() == b"yes",
            b"supportedParams" => {
                supported_params = SearchCaps::parse_params(&attr_to_string(&attr.value));
            }
            _ => {}
        }
    }

    Ok(SearchCaps {
        available,
        supported_params,
    })
}

fn attr_to_string(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

fn attr_to_u32(v: &[u8]) -> u32 {
    String::from_utf8_lossy(v).parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_caps_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps version="0.1" title="Example Index" email="info@example.com" url="https://example.com/" image="" type="Site">
  <server version="0.1.0" />
  <limits max="100" default="50"/>
  <retention days="1234"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <tv-search available="yes" supportedParams="q,rid,tvdbid,season,ep"/>
    <movie-search available="yes" supportedParams="q,imdbid"/>
    <audio-search available="no" supportedParams="q"/>
    <book-search available="yes" supportedParams="q"/>
  </searching>
  <categories>
    <category id="5000" name="TV">
      <subcat id="5040" name="TV/HD"/>
      <subcat id="5030" name="TV/SD"/>
    </category>
    <category id="2000" name="Movies">
      <subcat id="2040" name="Movies/HD"/>
    </category>
  </categories>
</caps>"#;

        let caps = parse_caps(xml).unwrap();

        assert_eq!(caps.protocol_version, "0.1");
        assert_eq!(caps.title, "Example Index");
        assert_eq!(caps.email, "info@example.com");
        assert_eq!(caps.url, "https://example.com/");
        assert_eq!(caps.server_version, "0.1.0");
        assert_eq!(caps.max_results, 100);
        assert_eq!(caps.default_results, 50);
        assert_eq!(caps.retention_days, Some(1234));

        let search = caps.search.unwrap();
        assert!(search.available);
        assert!(search.supported_params.contains("q"));

        let tv = caps.tv_search.unwrap();
        assert!(tv.available);
        assert!(tv.supported_params.contains("rid"));
        assert!(tv.supported_params.contains("tvdbid"));
        assert!(tv.supported_params.contains("season"));
        assert!(tv.supported_params.contains("ep"));

        let audio = caps.audio_search.unwrap();
        assert!(!audio.available);

        assert_eq!(caps.categories.len(), 2);
        assert_eq!(caps.categories[0].id, 5000);
        assert_eq!(caps.categories[0].name, "TV");
        assert_eq!(caps.categories[0].subcats.len(), 2);
        assert_eq!(caps.categories[0].subcats[0].id, 5040);
    }

    #[test]
    fn test_parse_caps_minimal() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps version="0.1" title="Minimal" email="" url="https://minimal.com/">
</caps>"#;

        let caps = parse_caps(xml).unwrap();
        assert_eq!(caps.title, "Minimal");
        assert_eq!(caps.max_results, 0);
        assert!(caps.search.is_none());
        assert!(caps.categories.is_empty());
    }
}
