//! Nobz downloader engine.
//!
//! NNTP client, yEnc decoder, NZB parsing, queue persistence, job scheduler,
//! PAR2 verify/repair, and archive unpacking. Designed to be usable as a
//! library independent of any GUI frontend.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod engine;
pub mod error;
pub mod nntp;
pub mod nzb;
pub mod queue;
pub mod yenc;

pub use error::{CoreError, Result};
