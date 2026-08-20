//! TurboNBZ downloader engine.
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
pub mod par2;
pub mod postprocess;
pub mod queue;
pub mod unpack;
pub mod yenc;

pub use error::{CoreError, Result};
pub use postprocess::{PostProcessConfig, PostProcessReport, PostProcessStatus};
pub use queue::{
    JobFileStats, JobState, QueueFile, QueueJob, QueueManager, QueueSegment, SegmentState,
};
