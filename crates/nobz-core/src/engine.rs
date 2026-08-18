//! Download engine: job scheduler that dispatches article fetches across a
//! pool of NNTP connections, yEnc-decodes them, and assembles files on disk.
//!
//! M3 scope: SQLite-backed resume. The engine loads pending segments from
//! the queue DB, writes per-segment state after each fetch, and can resume
//! a killed job at the article level. Server fallback tries servers in
//! priority order; if all return 430/423, the segment is marked missing
//! (hopeless) and not retried on restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::error::{CoreError, Result};
use crate::nntp::{NntpClient, ServerConfig, StatResult};
use crate::queue::{QueueFile, QueueManager, QueueSegment, SegmentState};
use crate::yenc;

/// Progress events emitted by [`Engine::run_job`]. The CLI/GUI consumes
/// these to render a progress bar / activity panel.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A file started downloading.
    FileStarted { filename: String, segments: u32 },
    /// A segment finished (ok or not).
    SegmentDone {
        filename: String,
        segment: u32,
        status: SegmentState,
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

    /// Run a job to completion, reading pending segments from the queue and
    /// writing per-segment state back. Emits progress events on `tx`.
    ///
    /// Only segments in `Pending` state (non-missing) are fetched. Segments
    /// already `Done`, `Missing`, `CrcMismatch`, or `Failed` are skipped —
    /// this is what makes the engine restart-safe.
    pub async fn run_job(
        self: Arc<Self>,
        queue: Arc<QueueManager>,
        job_id: i64,
        tx: mpsc::UnboundedSender<ProgressEvent>,
    ) -> Result<()> {
        // Mark job as downloading.
        queue
            .set_job_state(job_id, crate::queue::JobState::Downloading)
            .await?;

        let job = queue.get_job(job_id).await?;
        tracing::info!(
            job_id,
            output_dir = %job.output_dir.display(),
            total_segments = job.total_segments,
            segments_done = job.segments_done,
            total_bytes = job.total_bytes,
            "engine: starting job"
        );
        tokio::fs::create_dir_all(&job.output_dir)
            .await
            .map_err(CoreError::from)?;

        // Get all files with pending segments.
        let pending = queue.pending_segments(job_id).await?;
        tracing::info!(
            pending_files = pending.len(),
            "engine: pending segments loaded"
        );

        let mut completed = 0usize;
        let mut failed = 0usize;

        for (file, segments) in &pending {
            let _ = tx.send(ProgressEvent::FileStarted {
                filename: file.filename.clone(),
                segments: segments.len() as u32,
            });

            let outcome = self
                .download_file(&queue, file, segments, &job.output_dir, &tx)
                .await;
            match outcome {
                Ok(o) => {
                    if o.missing == 0 && o.crc_mismatches == 0 {
                        completed += 1;
                    } else {
                        failed += 1;
                    }
                    let _ = tx.send(ProgressEvent::FileCompleted {
                        filename: file.filename.clone(),
                        path: o.path,
                        missing: o.missing,
                        crc_mismatches: o.crc_mismatches,
                    });
                }
                Err(e) => {
                    failed += 1;
                    let _ = tx.send(ProgressEvent::ArticleError {
                        filename: file.filename.clone(),
                        segment: 0,
                        error: e.to_string(),
                    });
                }
            }
        }

        // Determine final job state.
        let final_state = if failed == 0 {
            crate::queue::JobState::Complete
        } else {
            crate::queue::JobState::Failed
        };
        queue.set_job_state(job_id, final_state).await?;

        let _ = tx.send(ProgressEvent::JobFinished { completed, failed });
        Ok(())
    }

    async fn download_file(
        self: &Arc<Self>,
        queue: &Arc<QueueManager>,
        file: &QueueFile,
        pending_segments: &[QueueSegment],
        output_dir: &Path,
        tx: &mpsc::UnboundedSender<ProgressEvent>,
    ) -> Result<FileOutcome> {
        let filename = file.filename.clone();
        let final_path = output_dir.join(&filename);
        let parts_dir = output_dir.join(format!("{}.parts", filename));

        // Create a temp dir for per-segment decoded bytes.
        tokio::fs::create_dir_all(&parts_dir)
            .await
            .map_err(CoreError::from)?;

        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for seg in pending_segments {
            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("semaphore closed: {e}")))?;
            let servers = self.servers.clone();
            let msg_id = seg.message_id.clone();
            let seg_num = seg.number;
            let file_id = file.id;
            let queue = Arc::clone(queue);
            let filename_clone = filename.clone();
            let parts_dir = parts_dir.clone();
            let tx = tx.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let fetch_result = fetch_with_fallback(&servers, &msg_id).await;
                let state = match &fetch_result {
                    Ok(body) => {
                        match yenc::decode_article(body) {
                            Ok(decoded) => {
                                let seg_state = if decoded.crc_ok || decoded.crc_unknown {
                                    SegmentState::Done
                                } else {
                                    SegmentState::CrcMismatch
                                };
                                // Write decoded bytes to a per-segment file
                                // so they survive a restart.
                                if seg_state == SegmentState::Done {
                                    let part_path = parts_dir.join(format!("seg{seg_num:06}"));
                                    tokio::fs::write(&part_path, &decoded.data)
                                        .await
                                        .map_err(CoreError::from)?;
                                }
                                seg_state
                            }
                            Err(e) => {
                                let _ = tx.send(ProgressEvent::ArticleError {
                                    filename: filename_clone.clone(),
                                    segment: seg_num,
                                    error: e.to_string(),
                                });
                                SegmentState::Failed
                            }
                        }
                    }
                    Err(FetchError::Missing) => SegmentState::Missing,
                    Err(FetchError::Other(e)) => {
                        let _ = tx.send(ProgressEvent::ArticleError {
                            filename: filename_clone.clone(),
                            segment: seg_num,
                            error: e.to_string(),
                        });
                        SegmentState::Failed
                    }
                };

                // Persist the segment state to the queue DB.
                if let Err(e) = queue.set_segment_state(file_id, seg_num, state).await {
                    tracing::error!(error = %e, "failed to persist segment state");
                }

                let bytes = match &fetch_result {
                    Ok(b) => b.len() as u64,
                    Err(_) => 0,
                };
                let _ = tx.send(ProgressEvent::SegmentDone {
                    filename: filename_clone,
                    segment: seg_num,
                    status: state,
                    bytes,
                });
                Ok(())
            });
        }

        // Wait for every segment task.
        while let Some(res) = tasks.join_next().await {
            res.map_err(|e| CoreError::Other(anyhow::anyhow!("task panicked: {e}")))??;
        }

        // Refresh job-level aggregate counters now that all segments
        // for this file are done. We skip this during per-segment updates
        // to avoid 3 extra queries per segment.
        if let Err(e) = queue.refresh_job_counts(file.id).await {
            tracing::warn!(error = %e, "failed to refresh job counts");
        }

        // Assemble the file from per-segment part files.
        let all_segments = queue.list_segments(file.id).await?;
        let mut out = tokio::fs::File::create(&final_path)
            .await
            .map_err(CoreError::from)?;
        use tokio::io::AsyncWriteExt;
        let mut missing = 0u32;
        let mut crc_mismatches = 0u32;

        for n in 1..=file.segment_count {
            let seg = all_segments.iter().find(|s| s.number == n);
            match seg {
                Some(s)
                    if s.state == SegmentState::Done
                        || (s.state == SegmentState::Pending && s.missing) =>
                {
                    if s.missing {
                        missing += 1;
                        continue;
                    }
                    // Read the decoded bytes from the part file.
                    let part_path = parts_dir.join(format!("seg{n:06}"));
                    match tokio::fs::read(&part_path).await {
                        Ok(data) => {
                            out.write_all(&data).await.map_err(CoreError::from)?;
                        }
                        Err(_) => {
                            // Part file missing — segment was done in a
                            // previous run but the part file was deleted.
                            missing += 1;
                        }
                    }
                }
                Some(s) if s.state == SegmentState::Missing || s.missing => {
                    missing += 1;
                }
                Some(s) if s.state == SegmentState::CrcMismatch => {
                    crc_mismatches += 1;
                }
                Some(s) if s.state == SegmentState::Failed => {
                    missing += 1;
                }
                _ => {
                    missing += 1;
                }
            }
        }
        out.flush().await.map_err(CoreError::from)?;
        drop(out);

        // Clean up parts dir if the file is complete (no holes).
        if missing == 0 && crc_mismatches == 0 {
            let _ = tokio::fs::remove_dir_all(&parts_dir).await;
        }

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

#[derive(Debug)]
enum FetchError {
    /// Article missing on all servers (430/423 from every server).
    Missing,
    Other(CoreError),
}

/// Try each server in priority order until one returns the article body, or
/// all report it missing. If all servers return 430/423, the segment is
/// hopeless and [`FetchError::Missing`] is returned — the engine marks it
/// as `SegmentState::Missing` so it won't be retried on restart.
async fn fetch_with_fallback(
    servers: &[ServerConfig],
    message_id: &str,
) -> std::result::Result<Vec<u8>, FetchError> {
    let mut all_missing = true;
    let mut last_error: Option<CoreError> = None;

    for server in servers {
        let mut client = match NntpClient::connect(server).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(server = %server.host, error = %e, "connect failed; trying next");
                last_error = Some(e);
                all_missing = false; // connection error ≠ missing article
                continue;
            }
        };
        match client.body(message_id).await {
            Ok(Ok(body)) => return Ok(body.bytes),
            Ok(Err(StatResult::Missing)) => {
                // 430/423 — article not on this server, try next.
                continue;
            }
            Ok(Err(StatResult::Present)) => {
                // BODY never returns Present; treat as a protocol error.
                continue;
            }
            Err(e) => {
                tracing::warn!(server = %server.host, error = %e, "BODY error; trying next");
                last_error = Some(e);
                all_missing = false;
                continue;
            }
        }
    }

    if all_missing {
        Err(FetchError::Missing)
    } else {
        Err(FetchError::Other(last_error.unwrap_or_else(|| {
            CoreError::Nntp(format!("no server could fetch {message_id}"))
        })))
    }
}
