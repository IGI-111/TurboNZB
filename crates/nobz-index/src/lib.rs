//! Nobz Newznab client and multi-indexer aggregation.

#![forbid(unsafe_code)]

pub mod newznab;

pub use newznab::{NewznabClient, SearchProvider};
