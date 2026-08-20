//! TurboNBZ Newznab client and multi-indexer aggregation.
//!
//! Provides a typed async Newznab client, a `SearchProvider` trait for
//! abstraction, and a `SearchAggregator` that fans out searches across
//! multiple indexers in parallel, deduplicates results, and merges them
//! with per-indexer badges.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod aggregate;
pub mod caps_parser;
pub mod error;
pub mod newznab;
pub mod search_parser;
pub mod types;

pub use aggregate::{AggregatedResult, SearchAggregator};
pub use error::{IndexError, Result};
pub use newznab::{NewznabClient, SearchProvider};
pub use types::*;
