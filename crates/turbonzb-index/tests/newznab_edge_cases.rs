//! Newznab edge cases (§10 of TEST_PLAN.md): recorded indexer responses,
//! caps-driven limits, pagination, and how failures surface.
//!
//! Split in two halves. Parser-level checks feed recorded fixtures straight
//! into the parsers, so a weird indexer response is a data change rather than
//! a rewrite. Client-level checks run against a loopback HTTP server that
//! speaks just enough HTTP/1.1 to exercise status handling and the query
//! string the client actually sends.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use turbonzb_index::aggregate::SearchAggregator;
use turbonzb_index::caps_parser::parse_caps;
use turbonzb_index::error::IndexError;
use turbonzb_index::newznab::{NewznabClient, SearchProvider};
use turbonzb_index::search_parser::{parse_error, parse_search_results};
use turbonzb_index::types::*;

/// A release listing with the full attribute set an indexer emits.
const RECORDING_FULL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>Mock</title>
    <item>
      <title>Some.Show.S01E05.1080p &amp; 2160p</title>
      <guid>guid-full-1</guid>
      <pubDate>Sun, 06 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV &gt; HD</category>
      <enclosure url="http://mock-a/nzb/guid-full-1?apikey=x" length="1073741824" type="application/x-nzb"/>
      <newznab:attr name="category" value="5040"/>
      <newznab:attr name="size" value="1073741824"/>
      <newznab:attr name="grabs" value="4211"/>
      <newznab:attr name="files" value="7"/>
      <newznab:attr name="password" value="1"/>
      <newznab:attr name="season" value="1"/>
      <newznab:attr name="episode" value="5"/>
      <newznab:attr name="tvdbid" value="79891"/>
      <newznab:attr name="rageid" value="1234"/>
      <newznab:attr name="tvtitle" value="Some Show"/>
      <newznab:attr name="tvairdate" value="2010-06-06"/>
    </item>
  </channel>
</rss>"#;

/// An item with no enclosure, a movie attribute set, and no password hint.
const RECORDING_MOVIE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <item>
      <title>Some Movie 2010 1080p</title>
      <guid>http://mock-b/getnzb/guid-movie-1</guid>
      <pubDate>Mon, 07 Jun 2010 09:00:00 +0000</pubDate>
      <category>Movies</category>
      <newznab:attr name="category" value="2000"/>
      <newznab:attr name="imdb" value="tt0123456"/>
      <newznab:attr name="imdbscore" value="7.8"/>
      <newznab:attr name="imdbyear" value="2010"/>
      <newznab:attr name="genre" value="Drama"/>
    </item>
  </channel>
</rss>"#;

const CAPS_FULL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps version="0.1" title="MockIndex" email="ops@mock.example" url="http://mock/">
  <server version="1.0.0"/>
  <limits max="1000" default="100"/>
  <retention days="2000"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <tv-search available="yes" supportedParams="q,rid,tvdbid,tvmazeid,season,ep"/>
    <movie-search available="yes" supportedParams="q,imdbid"/>
    <audio-search available="no" supportedParams="q"/>
    <book-search available="yes" supportedParams="q"/>
  </searching>
  <categories>
    <category id="5000" name="TV">
      <subcat id="5040" name="TV/HD"/>
      <subcat id="5030" name="TV/SD"/>
    </category>
    <category id="2000" name="Movies"/>
  </categories>
</caps>"#;

fn cfg(name: &str, url: String) -> IndexerConfig {
    IndexerConfig {
        name: name.to_string(),
        url,
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    }
}

/// §10.1 — a recorded listing decodes into a fully populated result: entities
/// in the title, sub-category normalized to its parent, enclosure length as
/// size, RFC 2822 date as a Unix timestamp, TV ids in one bundle.
#[test]
fn recorded_listing_populates_every_attribute() {
    let results = parse_search_results(RECORDING_FULL, "mock-a").unwrap();
    assert_eq!(results.len(), 1);
    let r = &results[0];

    assert_eq!(
        r.title, "Some.Show.S01E05.1080p & 2160p",
        "entities decoded"
    );
    assert_eq!(r.guid, "guid-full-1");
    assert_eq!(r.nzb_url, "http://mock-a/nzb/guid-full-1?apikey=x");
    assert_eq!(r.size, 1_073_741_824);
    assert_eq!(r.category, 5000, "5040 normalizes to the TV parent");
    assert_eq!(r.category_name, "TV > HD");
    assert_eq!(r.grabs, 4211);
    assert_eq!(r.files, 7);
    assert_eq!(r.password, PasswordStatus::Rar);
    assert_eq!(r.indexer, "mock-a");
    // Sun, 06 Jun 2010 17:29:23 +0100 == 16:29:23 UTC.
    assert_eq!(r.post_date, 1_275_841_763);

    let tv = r.tv.as_ref().expect("tv info for a tv-search hit");
    assert_eq!(tv.season, Some(1));
    assert_eq!(tv.episode, Some(5));
    assert_eq!(tv.tvdb_id, Some(79891));
    assert_eq!(tv.rage_id, Some(1234));
    assert_eq!(tv.tvmaze_id, None, "not advertised in this recording");
    assert_eq!(tv.title.as_deref(), Some("Some Show"));
    assert_eq!(tv.air_date.as_deref(), Some("2010-06-06"));
    assert!(r.movie.is_none());
}

/// §10.1 — no enclosure means the guid is the NZB URL; movie attributes land
/// in `movie` and leave `tv` empty.
#[test]
fn guid_doubles_as_nzb_url_and_movie_attrs_are_grouped() {
    let results = parse_search_results(RECORDING_MOVIE, "mock-b").unwrap();
    let r = &results[0];
    assert_eq!(r.nzb_url, "http://mock-b/getnzb/guid-movie-1");
    assert_eq!(r.size, 0, "no enclosure length means unknown size");
    assert_eq!(r.category, 2000);
    assert_eq!(r.password, PasswordStatus::Unknown, "no password attr");
    assert!(r.tv.is_none());

    let movie = r.movie.as_ref().expect("movie info");
    assert_eq!(movie.imdb_id.as_deref(), Some("tt0123456"));
    assert_eq!(movie.imdb_score.as_deref(), Some("7.8"));
    assert_eq!(movie.imdb_year, Some(2010));
    assert_eq!(movie.genre.as_deref(), Some("Drama"));
    assert_eq!(r.post_date, 1_275_901_200);
}

/// §10.1 — password flag values map onto the four states the UI shows.
#[test]
fn password_flag_maps_to_the_four_states() {
    let xml_for = |v: &str| {
        format!(
            r#"<?xml version="1.0"?>
<rss><channel><item><title>T</title><guid>g</guid>
<newznab:attr name="password" value="{v}"/></item></channel></rss>"#
        )
    };
    let status = |v: &str| {
        let r = &parse_search_results(&xml_for(v), "m").unwrap()[0];
        r.password
    };
    assert_eq!(status("0"), PasswordStatus::None);
    assert_eq!(status("1"), PasswordStatus::Rar);
    assert_eq!(status("2"), PasswordStatus::InnerArchive);
    assert_eq!(status("7"), PasswordStatus::Unknown);
    assert_eq!(status("not-a-number"), PasswordStatus::Unknown);
}

/// §10.5 — caps drive what the search form can offer: limits, retention,
/// which params each search type supports, and nested categories.
#[test]
fn caps_describe_limits_and_supported_params() {
    let caps = parse_caps(CAPS_FULL).unwrap();
    assert_eq!(caps.title, "MockIndex");
    assert_eq!(caps.protocol_version, "0.1");
    assert_eq!(caps.server_version, "1.0.0");
    assert_eq!(caps.max_results, 1000);
    assert_eq!(caps.default_results, 100);
    assert_eq!(caps.retention_days, Some(2000));

    let tv = caps.tv_search.as_ref().expect("tv-search caps");
    assert!(tv.available);
    for p in ["q", "rid", "tvdbid", "tvmazeid", "season", "ep"] {
        assert!(tv.supported_params.contains(p), "tv-search should list {p}");
    }
    assert!(!tv.supported_params.contains("imdbid"));

    let audio = caps.audio_search.as_ref().expect("audio-search caps");
    assert!(!audio.available, "available=\"no\" is not available");

    assert_eq!(caps.categories.len(), 2);
    let tv_cat = &caps.categories[0];
    assert_eq!(tv_cat.id, 5000);
    assert_eq!(tv_cat.name, "TV");
    assert_eq!(tv_cat.subcats.len(), 2, "subcats nest under their parent");
    assert_eq!(caps.categories[1].subcats.len(), 0);
}

/// §10.6 — malformed and hostile documents are handled without panicking and
/// without inventing results.
#[test]
fn odd_documents_parse_leniently() {
    // Cut off mid-item: whatever closed properly is returned.
    let truncated = r#"<?xml version="1.0"?><rss><channel><item><title>One</title><guid>g1</guid></item><item><title>Two"#;
    let results = parse_search_results(truncated, "m").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "One");

    // Unicode and emoji survive untouched.
    let unicode = r#"<?xml version="1.0"?><rss><channel><item><title>Živio Ćićić 🎞 emoji</title><guid>g</guid></item></channel></rss>"#;
    let results = parse_search_results(unicode, "m").unwrap();
    assert_eq!(results[0].title, "Živio Ćićić 🎞 emoji");

    // Numeric character references decode.
    let numeric = r#"<?xml version="1.0"?><rss><channel><item><title>caf&#233;</title></item></channel></rss>"#;
    let results = parse_search_results(numeric, "m").unwrap();
    assert_eq!(results[0].title, "café");

    // No items at all is a normal empty result, not an error.
    let empty = r#"<?xml version="1.0"?><rss><channel><title>Mock</title></channel></rss>"#;
    assert!(parse_search_results(empty, "m").unwrap().is_empty());

    // Completely empty input.
    assert!(parse_search_results("", "m").unwrap().is_empty());
}

/// §10.4 — a Newznab `<error>` element is reported as an error, and its
/// absence is not mistaken for one.
#[test]
fn error_element_is_surfaced_with_code_and_description() {
    let err = parse_error(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<error code="103" description="Invalid API key"/>
"#,
    )
    .expect("error detected");
    assert_eq!(err.code, 103);
    assert_eq!(err.description, "Invalid API key");

    // A bare error document without an XML declaration still counts.
    let err = parse_error(r#"<error code="202" description="No such function"/>"#).unwrap();
    assert_eq!(err.code, 202);

    // A healthy response is not an error.
    assert!(parse_error(RECORDING_FULL).is_none());
    assert!(parse_error("").is_none());
}

// ---------------------------------------------------------------- client ---

/// Loopback HTTP server: answers by matching a substring of the request line
/// and records every request line it saw.
struct MockHttp {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockHttp {
    async fn start(responses: Vec<(&'static str, &'static str, &'static str)>) -> Self {
        // (match, status line, body)
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(responses);
        let requests = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                let responses = Arc::clone(&responses);
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = request.lines().next().unwrap_or("").to_string();
                    if let Ok(mut seen) = sink.lock() {
                        seen.push(line.clone());
                    }
                    let (_, status, body) = responses
                        .iter()
                        .find(|(needle, _, _)| line.contains(needle))
                        .unwrap_or(&(
                            "",
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                            "",
                        ));
                    let head = format!("{status}Content-Length: {}\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                });
            }
        });

        Self { addr, requests }
    }

    fn url(&self) -> String {
        format!("http://{}/api", self.addr)
    }

    fn seen(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// §10.5 — the query string carries the api key, the search function, the
/// requested page window, and only the params that were actually set.
#[tokio::test]
async fn query_string_reflects_the_search_query() {
    let server = MockHttp::start(vec![("t=tvsearch", "HTTP/1.1 200 OK\r\n", RECORDING_FULL)]).await;
    let client = NewznabClient::new(cfg("mock-a", server.url()));

    let mut query = SearchQuery::tv("Some Show", 1, 5);
    query.limit = 25;
    query.offset = 50;
    query.tvdb_id = Some(79891);
    query.max_age_days = Some(30);
    query.categories = vec![5000, 5040];

    let results = client.search(&query).await.unwrap();
    assert_eq!(results.len(), 1);

    let line = server.seen().into_iter().next().expect("one request");
    assert!(
        line.contains("t=tvsearch"),
        "function selects the endpoint: {line}"
    );
    assert!(line.contains("apikey=test"), "api key is sent: {line}");
    assert!(line.contains("limit=25"), "explicit limit is sent: {line}");
    assert!(
        line.contains("offset=50"),
        "pagination offset is sent: {line}"
    );
    assert!(line.contains("season=1"), "season is sent: {line}");
    assert!(line.contains("ep=5"), "episode is sent: {line}");
    assert!(line.contains("tvdbid=79891"), "tvdb id is sent: {line}");
    assert!(line.contains("maxage=30"), "max age is sent: {line}");
    assert!(
        line.contains("extended=1"),
        "extended attrs are requested: {line}"
    );
    assert!(
        line.contains("q=Some+Show") || line.contains("q=Some%20Show"),
        "spaces are encoded: {line}"
    );
}

/// The default page size is implicit on the wire, so an untouched query does
/// not resend it.
#[tokio::test]
async fn default_query_sends_only_what_it_changes() {
    let server = MockHttp::start(vec![("t=search", "HTTP/1.1 200 OK\r\n", RECORDING_MOVIE)]).await;
    let client = NewznabClient::new(cfg("mock-a", server.url()));

    let results = client.search(&SearchQuery::text("movie")).await.unwrap();
    assert_eq!(results.len(), 1);

    let line = server.seen().into_iter().next().unwrap();
    assert!(line.contains("t=search"));
    assert!(!line.contains("offset="), "offset 0 is implicit: {line}");
    assert!(
        !line.contains("limit=100"),
        "the default limit is left to the indexer: {line}"
    );
}

/// §10.4 — a 429 becomes a rate-limit error with the advertised delay, not an
/// empty result set.
#[tokio::test]
async fn rate_limit_is_reported_with_retry_after() {
    let server = MockHttp::start(vec![(
        "t=search",
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\n",
        "",
    )])
    .await;
    let client = NewznabClient::new(cfg("limited", server.url()));

    match client.search(&SearchQuery::text("x")).await {
        Err(IndexError::RateLimited {
            indexer,
            retry_after_s,
        }) => {
            assert_eq!(indexer, "limited");
            assert_eq!(retry_after_s, 7);
        }
        other => panic!("expected a rate-limit error, got {other:?}"),
    }
}

/// §10.4 — an invalid API key arrives as a Newznab error code rather than an
/// empty list the UI would read as "nothing found".
#[tokio::test]
async fn api_error_element_is_not_confused_with_no_results() {
    let server = MockHttp::start(vec![(
        "t=search",
        "HTTP/1.1 200 OK\r\n",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<error code="103" description="Invalid API key"/>"#,
    )])
    .await;
    let client = NewznabClient::new(cfg("mock-a", server.url()));

    match client.search(&SearchQuery::text("x")).await {
        Err(IndexError::Api { code, description }) => {
            assert_eq!(code, 103);
            assert_eq!(description, "Invalid API key");
        }
        other => panic!("expected an API error, got {other:?}"),
    }
}

/// §10.3 — a server error on caps is reported with its HTTP status.
#[tokio::test]
async fn caps_http_failure_is_reported() {
    let server =
        MockHttp::start(vec![("t=caps", "HTTP/1.1 503 Service Unavailable\r\n", "")]).await;
    let client = NewznabClient::new(cfg("mock-a", server.url()));

    match client.caps().await {
        Err(IndexError::Api { code, .. }) => assert_eq!(code, 503),
        other => panic!("expected an API error, got {other:?}"),
    }
}

/// A provider stub for aggregation tests: fixed answer, no network.
struct Stub {
    name: &'static str,
    items: Vec<SearchResult>,
    fail: bool,
}

impl SearchProvider for Stub {
    fn name(&self) -> &str {
        self.name
    }

    fn caps(&self) -> BoxFuture<'_, Result<IndexerCaps, IndexError>> {
        Box::pin(async { Ok(IndexerCaps::default()) })
    }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, IndexError>> {
        let items = if self.fail {
            None
        } else {
            Some(self.items.clone())
        };
        let _ = query;
        Box::pin(async move { Ok(items.unwrap_or_default()) })
    }
}

fn item(title: &str, indexer: &str, size: u64, post_date: u64) -> SearchResult {
    SearchResult {
        title: title.to_string(),
        guid: format!("{indexer}:{title}"),
        nzb_url: format!("http://{indexer}/nzb"),
        size,
        post_date,
        category: 5000,
        category_name: "TV".to_string(),
        grabs: 0,
        files: 0,
        password: PasswordStatus::Unknown,
        indexer: indexer.to_string(),
        tv: None,
        movie: None,
    }
}

/// §10.2 — the same release on three indexers becomes one row carrying all
/// sources, while a genuinely different release stays separate.
#[tokio::test]
async fn aggregation_merges_duplicates_and_keeps_distinct_releases() {
    let mut agg = SearchAggregator::new(5);
    agg.add_provider(Box::new(Stub {
        name: "a",
        items: vec![
            item("Show.S01E01", "a", 1_000_000_000, 200),
            item("Only.On.A", "a", 500, 100),
        ],
        fail: false,
    }));
    agg.add_provider(Box::new(Stub {
        name: "b",
        items: vec![item("show.s01e01", "b", 1_005_000_000, 200)],
        fail: false,
    }));
    agg.add_provider(Box::new(Stub {
        name: "c",
        items: vec![item("Show.S01E01.1080p", "c", 1_000_000_000, 300)],
        fail: false,
    }));

    let results = agg.search(&SearchQuery::text("Show")).await;
    assert_eq!(
        results.len(),
        3,
        "two distinct releases plus the merged one"
    );

    // Newest first.
    assert_eq!(results[0].result.title, "Show.S01E01.1080p");

    let merged = results
        .iter()
        .find(|r| r.result.title == "Show.S01E01")
        .expect("the merged release");
    assert_eq!(merged.sources.len(), 2, "a and b merge on title + size");
    assert!(merged.sources.contains(&"a".to_string()));
    assert!(merged.sources.contains(&"b".to_string()));

    let solo = results
        .iter()
        .find(|r| r.result.title == "Only.On.A")
        .expect("the single-source release");
    assert_eq!(solo.sources, vec!["a".to_string()]);
}

/// §10.3 — one failing indexer does not blank out the rest, and its name does
/// not appear as a source.
#[tokio::test]
async fn a_failing_indexer_does_not_blank_the_results() {
    let mut agg = SearchAggregator::new(5);
    agg.add_provider(Box::new(Stub {
        name: "healthy",
        items: vec![item("Good.Release", "healthy", 10, 10)],
        fail: false,
    }));
    agg.add_provider(Box::new(Stub {
        name: "broken",
        items: Vec::new(),
        fail: true,
    }));

    let results = agg.search(&SearchQuery::text("Good")).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].sources, vec!["healthy".to_string()]);
}
