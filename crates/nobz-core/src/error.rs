//! Central error type for `nobz-core`.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("NNTP protocol error: {0}")]
    Nntp(String),

    #[error("NNTP connection failed: {0}")]
    NntpConnect(String),

    #[error("NNTP authentication failed")]
    NtpAuthFailed,

    #[error("yEnc decode error: {0}")]
    Yenc(String),

    #[error("yEnc CRC mismatch: expected {expected:#010x}, got {actual:#010x}")]
    YencCrc { expected: u32, actual: u32 },

    #[error("NZB parse error: {0}")]
    NzbParse(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),

    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),

    #[error("other: {0}")]
    Other(#[from] anyhow::Error),
}
