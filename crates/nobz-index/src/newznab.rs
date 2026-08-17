//! Typed async Newznab client.
//!
//! One code path covers all Newznab-family indexers. Supports caps
//! auto-detection and all search types: `search`, `tvsearch`, `movie`,
//! `music`, `book`.

use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::header;
use tracing::debug;

use crate::error::{IndexError, Result};
use crate::types::*;

/// Async Newznab client. Owns a `reqwest::Client` and indexer config.
#[derive(Clone)]
pub struct NewznabClient {
    config: IndexerConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for NewznabClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewznabClient")
            .field("name", &self.config.name)
            .field("url", &self.config.url)
            .finish()
    }
}

impl NewznabClient {
    pub fn new(config: IndexerConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_s))
            .user_agent(concat!("nobz/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build");
        Self { config, http }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn config(&self) -> &IndexerConfig {
        &self.config
    }

    /// Build the base URL with apikey and t=func.
    fn build_url(&self, func: &str) -> url::Url {
        let mut url = url::Url::parse(&self.config.url).expect("valid indexer url");
        url.query_pairs_mut()
            .append_pair("apikey", &self.config.api_key)
            .append_pair("t", func);
        url
    }

    /// Fetch and parse server capabilities (`t=caps`).
    pub async fn caps(&self) -> Result<IndexerCaps> {
        let url = self.build_url("caps");
        debug!(indexer = %self.config.name, %url, "fetching caps");

        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(IndexError::Api {
                code: status.as_u16(),
                description: format!("HTTP {status}"),
            });
        }
        let text = resp.text().await?;
        let caps = crate::caps_parser::parse_caps(&text)
            .map_err(|e| IndexError::CapsParse(e.to_string()))?;
        Ok(caps)
    }

    /// Execute a search query and return normalized results.
    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        let url = self.build_search_url(query);
        debug!(indexer = %self.config.name, %url, "searching");

        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            return Err(IndexError::RateLimited {
                indexer: self.config.name.clone(),
                retry_after_s: retry_after,
            });
        }
        if !status.is_success() {
            return Err(IndexError::Api {
                code: status.as_u16(),
                description: format!("HTTP {status}"),
            });
        }

        let text = resp.text().await?;

        // Check for Newznab error element first.
        if let Some(err) = crate::search_parser::parse_error(&text) {
            return Err(IndexError::Api {
                code: err.code,
                description: err.description,
            });
        }

        let parsed = crate::search_parser::parse_search_results(&text, &self.config.name)
            .map_err(|e| IndexError::SearchParse(e.to_string()))?;
        Ok(parsed)
    }

    fn build_search_url(&self, q: &SearchQuery) -> url::Url {
        let mut url = self.build_url(q.ty.as_str());

        let mut pairs = url.query_pairs_mut();
        if let Some(ref query) = q.q {
            pairs.append_pair("q", query);
        }
        if q.limit != 100 {
            pairs.append_pair("limit", &q.limit.to_string());
        }
        if q.offset > 0 {
            pairs.append_pair("offset", &q.offset.to_string());
        }
        if let Some(max_age) = q.max_age_days {
            pairs.append_pair("maxage", &max_age.to_string());
        }
        if let Some(min) = q.min_size {
            pairs.append_pair("minsize", &min.to_string());
        }
        if let Some(max) = q.max_size {
            pairs.append_pair("maxsize", &max.to_string());
        }
        if !q.categories.is_empty() {
            let cats: Vec<String> = q.categories.iter().map(|c| c.to_string()).collect();
            pairs.append_pair("cat", &cats.join(","));
        }

        match q.ty {
            SearchType::TvSearch => {
                if let Some(s) = q.season {
                    pairs.append_pair("season", &s.to_string());
                }
                if let Some(e) = q.episode {
                    pairs.append_pair("ep", &e.to_string());
                }
                if let Some(id) = q.tvdb_id {
                    pairs.append_pair("tvdbid", &id.to_string());
                }
                if let Some(id) = q.tvmaze_id {
                    pairs.append_pair("tvmazeid", &id.to_string());
                }
                if let Some(id) = q.rage_id {
                    pairs.append_pair("rid", &id.to_string());
                }
            }
            SearchType::Movie => {
                if let Some(ref imdb) = q.imdb_id {
                    pairs.append_pair("imdbid", imdb);
                }
            }
            SearchType::Music => {
                if let Some(ref a) = q.artist {
                    pairs.append_pair("artist", a);
                }
                if let Some(ref a) = q.album {
                    pairs.append_pair("album", a);
                }
                if let Some(ref l) = q.label {
                    pairs.append_pair("label", l);
                }
                if let Some(y) = q.year {
                    pairs.append_pair("year", &y.to_string());
                }
            }
            SearchType::Book => {
                if let Some(ref a) = q.author {
                    pairs.append_pair("author", a);
                }
                if let Some(ref t) = q.title {
                    pairs.append_pair("title", t);
                }
            }
            SearchType::Search => {}
        }

        // Always request extended attributes.
        pairs.append_pair("extended", "1");
        drop(pairs);
        url
    }
}

/// The trait the UI talks to — not Newznab directly.
///
/// This allows raw HTML search engines (Binsearch-style) and future sources
/// to be added in v2 without touching the GUI.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Fetch capabilities (may be unsupported by some providers).
    fn caps(&self) -> BoxFuture<'_, Result<IndexerCaps>>;

    /// Execute a search.
    fn search<'a>(&'a self, query: &'a SearchQuery) -> BoxFuture<'a, Result<Vec<SearchResult>>>;
}

impl SearchProvider for NewznabClient {
    fn name(&self) -> &str {
        self.config.name.as_str()
    }

    fn caps(&self) -> BoxFuture<'_, Result<IndexerCaps>> {
        Box::pin(self.caps())
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> BoxFuture<'a, Result<Vec<SearchResult>>> {
        Box::pin(self.search(query))
    }
}
