//! Shared types for search providers, Newznab indexers, and aggregation.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Which Newznab search function to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchType {
    /// `t=search` — generic text search.
    Search,
    /// `t=tvsearch` — TV search with optional season/episode/ids.
    TvSearch,
    /// `t=movie` — movie search with optional imdb id.
    Movie,
    /// `t=music` — music search with optional artist/album/label/year.
    Music,
    /// `t=book` — book search with optional author/title.
    Book,
}

impl SearchType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::TvSearch => "tvsearch",
            Self::Movie => "movie",
            Self::Music => "music",
            Self::Book => "book",
        }
    }
}

/// Normalized Newznab category tree (standard IDs only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Category {
    pub id: u32,
    pub name: &'static str,
}

pub mod cats {
    use super::Category;

    pub const CONSOLE: Category = Category {
        id: 1000,
        name: "Console",
    };
    pub const MOVIES: Category = Category {
        id: 2000,
        name: "Movies",
    };
    pub const AUDIO: Category = Category {
        id: 3000,
        name: "Audio",
    };
    pub const PC: Category = Category {
        id: 4000,
        name: "PC",
    };
    pub const TV: Category = Category {
        id: 5000,
        name: "TV",
    };
    pub const XXX: Category = Category {
        id: 6000,
        name: "XXX",
    };
    pub const BOOKS: Category = Category {
        id: 7000,
        name: "Books",
    };
    pub const OTHER: Category = Category {
        id: 8000,
        name: "Other",
    };

    pub const ALL: &[Category] = &[CONSOLE, MOVIES, AUDIO, PC, TV, XXX, BOOKS, OTHER];

    /// Normalize a site-specific or sub-category id to its top-level parent.
    pub fn normalize(id: u32) -> u32 {
        (id / 1000) * 1000
    }
}

/// A single search query, normalized across all search types.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub ty: SearchType,
    pub q: Option<String>,
    pub limit: u32,
    pub offset: u32,
    pub max_age_days: Option<u32>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub categories: Vec<u32>,

    // TV-specific
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub tvdb_id: Option<u32>,
    pub tvmaze_id: Option<u32>,
    pub rage_id: Option<u32>,

    // Movie-specific
    pub imdb_id: Option<String>,

    // Music-specific
    pub artist: Option<String>,
    pub album: Option<String>,
    pub label: Option<String>,
    pub year: Option<u32>,

    // Book-specific
    pub author: Option<String>,
    pub title: Option<String>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            ty: SearchType::Search,
            q: None,
            limit: 100,
            offset: 0,
            max_age_days: None,
            min_size: None,
            max_size: None,
            categories: Vec::new(),
            season: None,
            episode: None,
            tvdb_id: None,
            tvmaze_id: None,
            rage_id: None,
            imdb_id: None,
            artist: None,
            album: None,
            label: None,
            year: None,
            author: None,
            title: None,
        }
    }
}

impl SearchQuery {
    /// Simple text search.
    pub fn text(q: impl Into<String>) -> Self {
        Self {
            q: Some(q.into()),
            ..Default::default()
        }
    }

    /// TV search by title + season + episode.
    pub fn tv(q: impl Into<String>, season: u32, episode: u32) -> Self {
        Self {
            ty: SearchType::TvSearch,
            q: Some(q.into()),
            season: Some(season),
            episode: Some(episode),
            ..Default::default()
        }
    }

    /// Movie search by imdb id (e.g. "tt0058935" or "0058935").
    pub fn movie(imdb: impl Into<String>) -> Self {
        Self {
            ty: SearchType::Movie,
            imdb_id: Some(imdb.into()),
            ..Default::default()
        }
    }
}

/// A single result from a search, already normalized.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Title of the release.
    pub title: String,
    /// Globally unique identifier from the indexer (used for `t=get`).
    pub guid: String,
    /// Direct URL to fetch the NZB (enclosure url).
    pub nzb_url: String,
    /// Size in bytes (0 if unknown).
    pub size: u64,
    /// Usenet post date (Unix timestamp, 0 if unknown).
    pub post_date: u64,
    /// Normalized top-level category id (e.g. 5000 for TV).
    pub category: u32,
    /// Human-readable category from the indexer (e.g. "TV > HD").
    pub category_name: String,
    /// Number of grabs/downloads (0 if unknown).
    pub grabs: u32,
    /// Number of files in the release (0 if unknown).
    pub files: u32,
    /// Whether the release is password protected.
    pub password: PasswordStatus,
    /// The name of the indexer that returned this result.
    pub indexer: String,
    /// TV-specific attributes (if any).
    pub tv: Option<TvInfo>,
    /// Movie-specific attributes (if any).
    pub movie: Option<MovieInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordStatus {
    None,
    Rar,
    InnerArchive,
    Unknown,
}

impl From<u32> for PasswordStatus {
    fn from(v: u32) -> Self {
        match v {
            0 => Self::None,
            1 => Self::Rar,
            2 => Self::InnerArchive,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TvInfo {
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub rage_id: Option<u32>,
    pub tvdb_id: Option<u32>,
    pub tvmaze_id: Option<u32>,
    pub title: Option<String>,
    pub air_date: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MovieInfo {
    pub imdb_id: Option<String>,
    pub imdb_score: Option<String>,
    pub imdb_year: Option<u32>,
    pub genre: Option<String>,
}

/// Capabilities of a Newznab indexer, parsed from `t=caps`.
#[derive(Debug, Clone, Default)]
pub struct IndexerCaps {
    pub server_version: String,
    pub protocol_version: String,
    pub title: String,
    pub email: String,
    pub url: String,
    pub retention_days: Option<u32>,
    pub max_results: u32,
    pub default_results: u32,
    /// Supported search types and their supported params.
    pub search: Option<SearchCaps>,
    pub tv_search: Option<SearchCaps>,
    pub movie_search: Option<SearchCaps>,
    pub audio_search: Option<SearchCaps>,
    pub book_search: Option<SearchCaps>,
    /// All categories advertised by the indexer.
    pub categories: Vec<CapsCategory>,
}

#[derive(Debug, Clone)]
pub struct SearchCaps {
    pub available: bool,
    pub supported_params: BTreeSet<String>,
}

impl SearchCaps {
    /// Parse the `supportedParams="q,rid,season"` attribute.
    pub fn parse_params(s: &str) -> BTreeSet<String> {
        s.split(',')
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct CapsCategory {
    pub id: u32,
    pub name: String,
    pub subcats: Vec<CapsCategory>,
}

/// Configuration for a single indexer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerConfig {
    /// Display name for this indexer (e.g. "NinjaCentral").
    pub name: String,
    /// Base URL including path to api (e.g. "https://api.ninjacentral.com/api").
    pub url: String,
    /// API key for authentication.
    pub api_key: String,
    /// Number of simultaneous search requests allowed.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Per-indexer timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Priority for result tie-breaking (lower = higher priority).
    #[serde(default)]
    pub priority: u32,
}

fn default_max_concurrent() -> u32 {
    1
}

fn default_timeout() -> u64 {
    15
}
