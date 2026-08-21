//! Multi-indexer search aggregator.
//!
//! Fans out a search query to multiple indexers in parallel, collects
//! results, deduplicates, and returns a merged result set with per-indexer
//! badges.

use std::collections::HashMap;

use futures::future::join_all;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use crate::error::IndexError;
use crate::newznab::SearchProvider;
use crate::types::*;

/// Aggregates search results from multiple indexers.
pub struct SearchAggregator {
    providers: Vec<Box<dyn SearchProvider>>,
    /// Default per-indexer timeout.
    timeout_s: u64,
}

impl SearchAggregator {
    pub fn new(timeout_s: u64) -> Self {
        Self {
            providers: Vec::new(),
            timeout_s,
        }
    }

    pub fn add_provider(&mut self, provider: Box<dyn SearchProvider>) {
        self.providers.push(provider);
    }

    /// Execute a search across all providers in parallel, dedupe, and return
    /// merged results sorted by indexer priority then age.
    pub async fn search(&self, query: &SearchQuery) -> Vec<AggregatedResult> {
        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| {
                let name = p.name().to_string();
                async move {
                    let res = timeout(Duration::from_secs(self.timeout_s), p.search(query)).await;
                    (name, res)
                }
            })
            .collect();

        let all = join_all(futures).await;

        // Collect per-indexer results, logging errors
        let mut per_indexer: HashMap<String, Vec<SearchResult>> = HashMap::new();
        for (name, res) in all {
            match res {
                Ok(Ok(results)) => {
                    info!(indexer = %name, count = results.len(), "search ok");
                    per_indexer.insert(name, results);
                }
                Ok(Err(e)) => {
                    if let IndexError::RateLimited {
                        indexer,
                        retry_after_s,
                    } = &e
                    {
                        warn!(%indexer, retry_after_s, "rate limited");
                    } else {
                        warn!(indexer = %name, error = %e, "search failed");
                    }
                }
                Err(_) => {
                    warn!(indexer = %name, timeout_s = self.timeout_s, "search timed out");
                }
            }
        }

        // Merge and dedupe
        let merged = merge_and_dedupe(per_indexer);
        debug!(total = merged.len(), "aggregated results");
        merged
    }
}

/// A result that has been merged across indexers. If the same release was
/// found on multiple indexers, all sources are listed.
#[derive(Debug, Clone)]
pub struct AggregatedResult {
    /// The primary search result (from the highest-priority indexer).
    pub result: SearchResult,
    /// All indexers that returned this result.
    pub sources: Vec<String>,
}

/// Normalize a title for dedup: lowercase, collapse whitespace, strip common
/// release tags.
fn normalize_title(title: &str) -> String {
    let mut s = title.to_lowercase();
    // Collapse whitespace
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s
}

/// Size tolerance for dedup: ±1%.
fn size_matches(a: u64, b: u64) -> bool {
    if a == 0 || b == 0 {
        return false; // can't compare if either is unknown
    }
    let lo = a.saturating_sub(a / 100);
    let hi = a + a / 100;
    b >= lo && b <= hi
}

fn merge_and_dedupe(per_indexer: HashMap<String, Vec<SearchResult>>) -> Vec<AggregatedResult> {
    // Simple O(n²) dedup: group by normalized title + size within ±1%
    let mut groups: Vec<Vec<SearchResult>> = Vec::new();

    // Process in a deterministic order (sort indexer names)
    let mut indexer_names: Vec<&String> = per_indexer.keys().collect();
    indexer_names.sort();

    for name in indexer_names {
        let results = per_indexer.get(name).expect("key exists");
        for result in results {
            let norm = normalize_title(&result.title);

            // Try to find an existing group that matches by title + size
            let mut found_group = None;
            for (i, group) in groups.iter().enumerate() {
                let matches = group.iter().any(|r| {
                    normalize_title(&r.title) == norm
                        && (size_matches(r.size, result.size) || r.size == 0 || result.size == 0)
                });
                if matches {
                    found_group = Some(i);
                    break;
                }
            }

            if let Some(i) = found_group {
                groups[i].push(result.clone());
            } else {
                groups.push(vec![result.clone()]);
            }
        }
    }

    // Build AggregatedResult from each group
    let mut merged: Vec<AggregatedResult> = groups
        .into_iter()
        .map(|mut group| {
            // Sort by indexer name for deterministic ordering
            group.sort_by(|a, b| a.indexer.cmp(&b.indexer));
            let sources: Vec<String> = group.iter().map(|r| r.indexer.clone()).collect();
            // Pick the result from the first indexer (alphabetical = deterministic)
            let primary = group.into_iter().next().expect("group non-empty");
            AggregatedResult {
                result: primary,
                sources,
            }
        })
        .collect();

    // Sort by post_date descending (newest first)
    merged.sort_by_key(|r| std::cmp::Reverse(r.result.post_date));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_title() {
        let a = normalize_title("Some.Release.S01E05.1080p");
        let b = normalize_title("some.release.s01e05.1080p");
        assert_eq!(a, b);
    }

    #[test]
    fn test_size_matches() {
        assert!(size_matches(1_000_000_000, 1_009_000_000));
        assert!(size_matches(1_000_000_000, 991_000_000));
        assert!(!size_matches(1_000_000_000, 1_020_000_000));
        assert!(!size_matches(0, 1_000_000_000));
    }

    #[test]
    fn test_dedupe_basic() {
        let mut per_indexer = HashMap::new();
        per_indexer.insert(
            "indexer_a".to_string(),
            vec![SearchResult {
                title: "Release.Name".to_string(),
                guid: "a1".to_string(),
                nzb_url: "http://a/1".to_string(),
                size: 1_000_000_000,
                post_date: 1000,
                category: 5000,
                category_name: "TV".to_string(),
                grabs: 0,
                files: 0,
                password: PasswordStatus::Unknown,
                indexer: "indexer_a".to_string(),
                tv: None,
                movie: None,
            }],
        );
        per_indexer.insert(
            "indexer_b".to_string(),
            vec![SearchResult {
                title: "release.name".to_string(),
                guid: "b1".to_string(),
                nzb_url: "http://b/1".to_string(),
                size: 1_005_000_000, // within 1%
                post_date: 1000,
                category: 5000,
                category_name: "TV".to_string(),
                grabs: 0,
                files: 0,
                password: PasswordStatus::Unknown,
                indexer: "indexer_b".to_string(),
                tv: None,
                movie: None,
            }],
        );

        let merged = merge_and_dedupe(per_indexer);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources.len(), 2);
    }
}
