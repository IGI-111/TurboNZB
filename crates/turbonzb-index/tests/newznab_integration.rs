//! Integration tests for the Newznab client and aggregator using a mock
//! HTTP server.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use turbonzb_index::aggregate::SearchAggregator;
use turbonzb_index::newznab::NewznabClient;
use turbonzb_index::types::*;

const CAPS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps version="0.1" title="MockIndex" email="test@mock.com" url="http://mock/">
  <server version="1.0.0" />
  <limits max="100" default="50"/>
  <retention days="365"/>
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
    </category>
  </categories>
</caps>"#;

const SEARCH_XML_A: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>MockIndex A</title>
    <link>http://mock-a/</link>
    <description>API Results</description>
    <newznab:response offset="0" total="2"/>
    <item>
      <title>Common.Release.1080p</title>
      <guid>guid-a-1</guid>
      <pubDate>Sun, 06 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV &gt; HD</category>
      <enclosure url="http://mock-a/nzb/guid-a-1" length="1000000000" type="application/x-nzb"/>
      <newznab:attr name="category" value="5040"/>
      <newznab:attr name="size" value="1000000000"/>
      <newznab:attr name="password" value="0"/>
    </item>
    <item>
      <title>Unique.Release.A</title>
      <guid>guid-a-2</guid>
      <pubDate>Mon, 07 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV</category>
      <enclosure url="http://mock-a/nzb/guid-a-2" length="2000000000" type="application/x-nzb"/>
      <newznab:attr name="category" value="5000"/>
      <newznab:attr name="size" value="2000000000"/>
      <newznab:attr name="password" value="0"/>
    </item>
  </channel>
</rss>"#;

const SEARCH_XML_B: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>MockIndex B</title>
    <link>http://mock-b/</link>
    <description>API Results</description>
    <newznab:response offset="0" total="2"/>
    <item>
      <title>common.release.1080p</title>
      <guid>guid-b-1</guid>
      <pubDate>Sun, 06 Jun 2010 18:29:23 +0100</pubDate>
      <category>TV &gt; HD</category>
      <enclosure url="http://mock-b/nzb/guid-b-1" length="1005000000" type="application/x-nzb"/>
      <newznab:attr name="category" value="5040"/>
      <newznab:attr name="size" value="1005000000"/>
      <newznab:attr name="password" value="0"/>
    </item>
    <item>
      <title>Unique.Release.B</title>
      <guid>guid-b-2</guid>
      <pubDate>Tue, 08 Jun 2010 17:29:23 +0100</pubDate>
      <category>TV</category>
      <enclosure url="http://mock-b/nzb/guid-b-2" length="3000000000" type="application/x-nzb"/>
      <newznab:attr name="category" value="5000"/>
      <newznab:attr name="size" value="3000000000"/>
      <newznab:attr name="password" value="0"/>
    </item>
  </channel>
</rss>"#;

/// A minimal HTTP/1.1 mock server that responds to Newznab API queries.
struct MockServer {
    addr: SocketAddr,
}

impl MockServer {
    async fn start(responses: Vec<(String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(responses);

        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let responses = Arc::clone(&responses);
                tokio::spawn(async move {
                    // Read the request line + headers
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();

                    // Parse the request line to get the path
                    let path = request.lines().next().unwrap_or("");
                    let path_parts: Vec<&str> = path.split_whitespace().collect();
                    let url = path_parts.get(1).copied().unwrap_or("/");

                    let body = responses
                        .iter()
                        .find_map(|(pattern, resp)| {
                            if url.contains(pattern) {
                                Some(resp.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| {
                            "<error code=\"202\" description=\"No such function\"/>".to_string()
                        });

                    let http_response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );

                    sock.write_all(http_response.as_bytes()).await.ok();
                });
            }
        });

        Self { addr }
    }

    fn url(&self) -> String {
        format!("http://{}/api", self.addr)
    }
}

#[tokio::test]
async fn test_caps_auto_detect() {
    let server = MockServer::start(vec![("t=caps".to_string(), CAPS_XML.to_string())]).await;

    let client = NewznabClient::new(IndexerConfig {
        name: "mock".to_string(),
        url: server.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    });

    let caps = client.caps().await.unwrap();

    assert_eq!(caps.title, "MockIndex");
    assert_eq!(caps.server_version, "1.0.0");
    assert_eq!(caps.retention_days, Some(365));
    assert!(caps.search.unwrap().available);
    assert!(caps.tv_search.unwrap().available);
    assert!(!caps.audio_search.unwrap().available);
    assert_eq!(caps.categories.len(), 1);
}

#[tokio::test]
async fn test_search_single_indexer() {
    let server = MockServer::start(vec![
        ("t=caps".to_string(), CAPS_XML.to_string()),
        ("t=search".to_string(), SEARCH_XML_A.to_string()),
    ])
    .await;

    let client = NewznabClient::new(IndexerConfig {
        name: "mock-a".to_string(),
        url: server.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    });

    let query = SearchQuery::text("test query");
    let results = client.search(&query).await.unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Common.Release.1080p");
    assert_eq!(results[1].title, "Unique.Release.A");
    assert_eq!(results[0].indexer, "mock-a");
}

#[tokio::test]
async fn test_search_tvsearch() {
    let server = MockServer::start(vec![
        ("t=caps".to_string(), CAPS_XML.to_string()),
        ("t=tvsearch".to_string(), SEARCH_XML_A.to_string()),
    ])
    .await;

    let client = NewznabClient::new(IndexerConfig {
        name: "mock-a".to_string(),
        url: server.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    });

    let query = SearchQuery::tv("Some Show", 1, 5);
    let results = client.search(&query).await.unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn test_aggregator_dedupes_and_merges() {
    let server_a = MockServer::start(vec![
        ("t=caps".to_string(), CAPS_XML.to_string()),
        ("t=search".to_string(), SEARCH_XML_A.to_string()),
    ])
    .await;
    let server_b = MockServer::start(vec![
        ("t=caps".to_string(), CAPS_XML.to_string()),
        ("t=search".to_string(), SEARCH_XML_B.to_string()),
    ])
    .await;

    let client_a = NewznabClient::new(IndexerConfig {
        name: "indexer_a".to_string(),
        url: server_a.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    });
    let client_b = NewznabClient::new(IndexerConfig {
        name: "indexer_b".to_string(),
        url: server_b.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 1,
    });

    let mut aggregator = SearchAggregator::new(10);
    aggregator.add_provider(Box::new(client_a));
    aggregator.add_provider(Box::new(client_b));

    let query = SearchQuery::text("test");
    let results = aggregator.search(&query).await;

    // We expect 3 results: the common release deduped (2 sources), plus unique-a and unique-b
    assert_eq!(results.len(), 3);

    let common = results
        .iter()
        .find(|r| r.result.title.to_lowercase().contains("common"))
        .expect("common release not found");
    assert_eq!(common.sources.len(), 2);
    assert!(common.sources.contains(&"indexer_a".to_string()));
    assert!(common.sources.contains(&"indexer_b".to_string()));
}

#[tokio::test]
async fn test_aggregator_handles_timeout() {
    let server_a = MockServer::start(vec![
        ("t=caps".to_string(), CAPS_XML.to_string()),
        ("t=search".to_string(), SEARCH_XML_A.to_string()),
    ])
    .await;

    // Create a "server" that never accepts connections
    let dead_addr = "127.0.0.1:1";

    let client_a = NewznabClient::new(IndexerConfig {
        name: "alive".to_string(),
        url: server_a.url(),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 5,
        priority: 0,
    });
    let client_b = NewznabClient::new(IndexerConfig {
        name: "dead".to_string(),
        url: format!("http://{dead_addr}/api"),
        api_key: "test".to_string(),
        max_concurrent: 1,
        timeout_s: 1,
        priority: 1,
    });

    let mut aggregator = SearchAggregator::new(2);
    aggregator.add_provider(Box::new(client_a));
    aggregator.add_provider(Box::new(client_b));

    let query = SearchQuery::text("test");
    let results = aggregator.search(&query).await;

    // Only the alive indexer should return results
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.result.indexer == "alive"));
}
