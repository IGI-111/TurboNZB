//! Central error type for `turbonzb-index`.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("Newznab API error {code}: {description}")]
    Api { code: u16, description: String },

    #[error("Newznab caps parse error: {0}")]
    CapsParse(String),

    #[error("Newznab search response parse error: {0}")]
    SearchParse(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),

    #[error("indexer '{indexer}' request timed out")]
    Timeout { indexer: String },

    #[error("indexer '{indexer}' rate limited, retry after {retry_after_s}s")]
    RateLimited { indexer: String, retry_after_s: u64 },

    #[error("other: {0}")]
    Other(#[from] anyhow::Error),
}
