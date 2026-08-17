//! Download engine: job scheduler that dispatches article fetches across a
//! pool of NNTP connections, yEnc-decodes them, and assembles files on disk.
//!
//! M1 scope: a single server, multiple connections, per-article hopeless
//! tracking, and per-file assembly. Server fallback (try server B when server
//! A returns 430) lands in M3 alongside SQLite-backed resume.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::error::{CoreError, Result};
use crate::nntp::{NntpClient, ServerConfig, StatResult};
use crate::nzb::{Nzb, NzbFile};
use crate::yenc;

/// A download job derived from a parsed NZB.
#[derive(Debug, Clone)]
pub struct DownloadJob {
    /// Display name (NZB title or first-file subject).
    pub name: String,
    /// Where to write decoded files.
    pub output_dir: PathBuf,
    /// The files to fetch.
    pub files: Vec<NzbFile>,
}

impl DownloadJob {
    /// Build a job from an [`Nzb`] document.
    pub fn from_nzb(nzb: &Nzb, output_dir: impl Into<PathBuf>) -> Self {
        let name = nzb
            .title()
            .map(str::to_string)
            .or_else(|| nzb.files.first().map(|f| f.filename()))
            .unwrap_or_else(|| "nobz-download".into());
        Self {
            name,
            output_dir: output_dir.into(),
            files: nzb.files.clone(),
        }
    }
}

/// Per-article outcome recorded by the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArticleStatus {
    /// Decoded and written successfully; CRC matched.
    Ok,
    /// Decoded and written, but the article's CRC didn't match (corrupt on
    /// the server). PAR2 repair (M4) can recover this.
    CrcMismatch,
    /// The article was missing on every configured server.
    Missing,
}

/// Progress events emitted by [`Engine::run`]. The CLI/GUI consumes these to
/// render a progress bar / activity panel.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A file started downloading.
    FileStarted { filename: String, segments: u32 },
    /// A segment finished (ok or not).
    SegmentDone {
        filename: String,
        segment: u32,
        status: ArticleStatus,
        bytes: u64,
    },
    /// A file finished — all segments attempted, file assembled on disk.
    FileCompleted {
        filename: String,
        path: PathBuf,
        missing: u32,
        crc_mismatches: u32,
    },
    /// The whole job is done.
    JobFinished { completed: usize, failed: usize },
    /// A non-fatal error for one article (the engine keeps going).
    ArticleError {
        filename: String,
        segment: u32,
        error: String,
    },
}

/// The download engine. Owns the server pool and dispatches articles.
pub struct Engine {
    servers: Vec<ServerConfig>,
    /// Concurrency limit across the whole engine.
    semaphore: Arc<Semaphore>,
}

impl Engine {
    /// Create an engine for a set of servers. `total_connections` caps the
    /// number of simultaneous NNTP connections regardless of how many servers
    /// are configured.
    pub fn new(servers: Vec<ServerConfig>, total_connections: usize) -> Self {
        Self {
            servers,
            semaphore: Arc::new(Semaphore::new(total_connections)),
        }
    }

    /// Run a job to completion, emitting progress events on `tx`.
    ///
    /// Each file's segments are fanned out across connections; segments are
    /// written to `<output_dir>/<filename>.partNN` and then concatenated into
    /// `<output_dir>/<filename>` once all are present.
    pub async fn run(
        self,
        job: DownloadJob,
        tx: mpsc::UnboundedSender<ProgressEvent>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(&job.output_dir)
            .await
            .map_err(CoreError::from)?;

        let mut completed = 0usize;
        let mut failed = 0usize;

        for file in &job.files {
            let _ = tx.send(ProgressEvent::FileStarted {
                filename: file.filename(),
                segments: file.segment_count,
            });

            let outcome = self.download_file(file, &job.output_dir, &tx).await;
            match outcome {
                Ok(o) => {
                    if o.missing == 0 && o.crc_mismatches == 0 {
                        completed += 1;
                    } else {
                        // File assembled but with holes/corruption — count as
                        // failed for M1 reporting; M4 PAR2 repair may recover.
                        failed += 1;
                    }
                    let _ = tx.send(ProgressEvent::FileCompleted {
                        filename: file.filename(),
                        path: o.path,
                        missing: o.missing,
                        crc_mismatches: o.crc_mismatches,
                    });
                }
                Err(e) => {
                    failed += 1;
                    let _ = tx.send(ProgressEvent::ArticleError {
                        filename: file.filename(),
                        segment: 0,
                        error: e.to_string(),
                    });
                }
            }
        }

        let _ = tx.send(ProgressEvent::JobFinished { completed, failed });
        Ok(())
    }

    async fn download_file(
        &self,
        file: &NzbFile,
        output_dir: &Path,
        tx: &mpsc::UnboundedSender<ProgressEvent>,
    ) -> Result<FileOutcome> {
        let filename = file.filename();
        let final_path = output_dir.join(&filename);

        // Per-segment results keyed by segment number.
        let results: Arc<Mutex<HashMap<u32, SegmentResult>>> = Arc::new(Mutex::new(HashMap::new()));

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for seg in &file.segments {
            if seg.missing {
                // Synthesized hole from the NZB — nothing to fetch.
                results.lock().await.insert(
                    seg.number,
                    SegmentResult {
                        status: ArticleStatus::Missing,
                        bytes: Vec::new(),
                    },
                );
                let _ = tx.send(ProgressEvent::SegmentDone {
                    filename: filename.clone(),
                    segment: seg.number,
                    status: ArticleStatus::Missing,
                    bytes: 0,
                });
                continue;
            }
            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("semaphore closed: {e}")))?;
            let servers = self.servers.clone();
            let msg_id = seg.message_id.clone();
            let seg_num = seg.number;
            let results = results.clone();
            let filename_clone = filename.clone();
            let tx = tx.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let bytes = fetch_with_fallback(&servers, &msg_id).await;
                let status = match &bytes {
                    Ok(b) => {
                        // Decode yEnc.
                        match yenc::decode_article(b) {
                            Ok(decoded) => {
                                let status = if decoded.crc_ok || decoded.crc_unknown {
                                    ArticleStatus::Ok
                                } else {
                                    ArticleStatus::CrcMismatch
                                };
                                results.lock().await.insert(
                                    seg_num,
                                    SegmentResult {
                                        status: status.clone(),
                                        bytes: decoded.data,
                                    },
                                );
                                status
                            }
                            Err(e) => {
                                let _ = tx.send(ProgressEvent::ArticleError {
                                    filename: filename_clone.clone(),
                                    segment: seg_num,
                                    error: e.to_string(),
                                });
                                results.lock().await.insert(
                                    seg_num,
                                    SegmentResult {
                                        status: ArticleStatus::Missing,
                                        bytes: Vec::new(),
                                    },
                                );
                                ArticleStatus::Missing
                            }
                        }
                    }
                    Err(FetchError::Missing) => {
                        results.lock().await.insert(
                            seg_num,
                            SegmentResult {
                                status: ArticleStatus::Missing,
                                bytes: Vec::new(),
                            },
                        );
                        ArticleStatus::Missing
                    }
                    Err(FetchError::Other(e)) => {
                        let _ = tx.send(ProgressEvent::ArticleError {
                            filename: filename_clone.clone(),
                            segment: seg_num,
                            error: e.to_string(),
                        });
                        results.lock().await.insert(
                            seg_num,
                            SegmentResult {
                                status: ArticleStatus::Missing,
                                bytes: Vec::new(),
                            },
                        );
                        ArticleStatus::Missing
                    }
                };
                let _ = tx.send(ProgressEvent::SegmentDone {
                    filename: filename_clone,
                    segment: seg_num,
                    status,
                    bytes: bytes.as_deref().map(|b| b.len() as u64).unwrap_or(0),
                });
                Ok(())
            });
        }

        // Wait for every segment task.
        while let Some(res) = tasks.join_next().await {
            res.map_err(|e| CoreError::Other(anyhow::anyhow!("task panicked: {e}")))??;
        }

        // Assemble the file in segment-number order.
        let results = results.lock().await;
        let mut out = tokio::fs::File::create(&final_path)
            .await
            .map_err(CoreError::from)?;
        use tokio::io::AsyncWriteExt;
        let mut missing = 0u32;
        let mut crc_mismatches = 0u32;
        for n in 1..=file.segment_count {
            match results.get(&n) {
                Some(r) => {
                    out.write_all(&r.bytes).await.map_err(CoreError::from)?;
                    match r.status {
                        ArticleStatus::Missing => missing += 1,
                        ArticleStatus::CrcMismatch => crc_mismatches += 1,
                        ArticleStatus::Ok => {}
                    }
                }
                None => missing += 1,
            }
        }
        out.flush().await.map_err(CoreError::from)?;
        drop(out);

        Ok(FileOutcome {
            path: final_path,
            missing,
            crc_mismatches,
        })
    }
}

#[derive(Debug)]
struct FileOutcome {
    path: PathBuf,
    missing: u32,
    crc_mismatches: u32,
}

#[derive(Debug, Clone)]
struct SegmentResult {
    status: ArticleStatus,
    bytes: Vec<u8>,
}

#[derive(Debug)]
enum FetchError {
    Missing,
    Other(CoreError),
}

/// Try each server in priority order until one returns the article body, or
/// all report it missing. M3 will add per-article hopeless tracking and
/// SQLite persistence; for M1 this is a stateless fan-out.
async fn fetch_with_fallback(
    servers: &[ServerConfig],
    message_id: &str,
) -> std::result::Result<Vec<u8>, FetchError> {
    let mut last_missing = true;
    for server in servers {
        let mut client = match NntpClient::connect(server).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(server = %server.host, error = %e, "connect failed; trying next");
                continue;
            }
        };
        match client.body(message_id).await {
            Ok(Ok(body)) => return Ok(body.bytes),
            Ok(Err(StatResult::Missing)) => {
                last_missing = true;
                continue;
            }
            Ok(Err(StatResult::Present)) => {
                // BODY never returns Present; treat as a protocol error.
                continue;
            }
            Err(e) => {
                tracing::warn!(server = %server.host, error = %e, "BODY error; trying next");
                continue;
            }
        }
    }
    if last_missing {
        Err(FetchError::Missing)
    } else {
        Err(FetchError::Other(CoreError::Nntp(format!(
            "no server could fetch {message_id}"
        ))))
    }
}
