//! Download engine: worker pool that fetches articles across persistent
//! NNTP connections, yEnc-decodes them, and assembles files on disk.
//!
//! The engine maintains a pool of `max_connections` workers, each owning
//! a persistent NNTP connection that's reused across articles (no per-
//! article TLS handshake). All pending segments across all files are
//! flattened into a single shared work queue; workers pop segments and
//! fetch them continuously until the queue is empty. This keeps the
//! download pipe full with no gaps between files.
//!
//! Resume safety: segment state is persisted to the queue DB after each
//! article fetch. Only `Pending` segments are fetched; `Done`, `Missing`,
//! `CrcMismatch`, and `Failed` are skipped. On restart, `reset_failed_segments`
//! can be called to retry transient failures.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
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

/// The download engine. Owns the server list and concurrency limit.
pub struct Engine {
    servers: Vec<ServerConfig>,
    /// Concurrency limit = number of worker tasks.
    max_connections: usize,
}

impl Engine {
    /// Create an engine for a set of servers. `total_connections` is the
    /// number of worker tasks to spawn — each owns a persistent NNTP
    /// connection that's reused across articles.
    pub fn new(servers: Vec<ServerConfig>, total_connections: usize) -> Self {
        Self {
            servers,
            max_connections: total_connections,
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
        // Ensure the job is in 'downloading' state. The caller should have
        // already claimed the download slot via claim_download_slot, but
        // we set it here too for standalone use (CLI, tests).
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

        // Flatten all segments across all files into a single work queue.
        // Each item is (file, segment) so the worker knows which file's
        // parts directory to write to.
        let work_queue: Arc<Mutex<VecDeque<(QueueFile, QueueSegment)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let total_segments: usize = {
            let mut wq = work_queue.lock().await;
            let mut count = 0usize;
            for (file, segments) in &pending {
                let _ = tx.send(ProgressEvent::FileStarted {
                    filename: file.filename.clone(),
                    segments: segments.len() as u32,
                });
                for seg in segments {
                    wq.push_back((file.clone(), seg.clone()));
                    count += 1;
                }
            }
            count
        };
        tracing::info!(total_segments, "engine: work queue populated");

        // Per-server connection pool. Workers check out a connection,
        // use it, and return it. Broken connections are dropped.
        let pool: Arc<ConnectionPool> = Arc::new(ConnectionPool::new(&self.servers));

        // Shared counters for completed/failed files.
        let completed = Arc::new(Mutex::new(0usize));
        let failed = Arc::new(Mutex::new(0usize));

        // Write-behind buffer: workers send segment state updates to this
        // channel instead of writing to the DB individually. A batch
        // writer task flushes them in a single transaction periodically,
        // drastically reducing fsync contention.
        let (state_tx, state_rx) = mpsc::unbounded_channel::<(i64, u32, SegmentState)>();

        // Spawn the batch writer task.
        let writer_queue = Arc::clone(&queue);
        let writer = tokio::spawn(async move {
            let mut rx = state_rx;
            let mut buffer: Vec<(i64, u32, SegmentState)> = Vec::with_capacity(256);
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    // Drain all pending updates without blocking.
                    Some(update) = rx.recv(), if !rx.is_closed() => {
                        buffer.push(update);
                        // Batch-drain: try to get more without blocking.
                        while let Ok(update) = rx.try_recv() {
                            buffer.push(update);
                        }
                        // Flush immediately if buffer is large.
                        if buffer.len() >= 200 {
                            flush_batch(&writer_queue, &mut buffer).await;
                        }
                    }
                    _ = interval.tick() => {
                        if !buffer.is_empty() {
                            flush_batch(&writer_queue, &mut buffer).await;
                        }
                    }
                }

                // If the channel is closed and buffer is drained, exit.
                if rx.is_closed() && buffer.is_empty() {
                    break;
                }
            }
            // Final flush.
            if !buffer.is_empty() {
                flush_batch(&writer_queue, &mut buffer).await;
            }
        });

        // Spawn worker tasks.
        let mut workers: JoinSet<Result<()>> = JoinSet::new();
        for worker_id in 0..self.max_connections {
            let engine = Arc::clone(&self);
            let queue = Arc::clone(&queue);
            let pool = Arc::clone(&pool);
            let work_queue = Arc::clone(&work_queue);
            let tx = tx.clone();
            let state_tx = state_tx.clone();
            let output_dir = job.output_dir.clone();
            workers.spawn(async move {
                engine
                    .run_worker(
                        worker_id,
                        &queue,
                        &pool,
                        &work_queue,
                        &output_dir,
                        &tx,
                        &state_tx,
                    )
                    .await
            });
        }

        // Wait for all workers to finish.
        let mut worker_errors = 0usize;
        while let Some(res) = workers.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "worker error");
                    worker_errors += 1;
                }
                Err(e) => {
                    tracing::error!(error = %e, "worker panic");
                    worker_errors += 1;
                }
            }
        }
        tracing::info!(worker_errors, "engine: all workers finished");

        // Drop the last state_tx sender so the writer task's channel closes
        // and it flushes any remaining buffered updates.
        drop(state_tx);
        writer.await.ok();

        // Assemble all files now that all segments are downloaded.
        let mut completed_count = 0usize;
        let mut failed_count = 0usize;
        for (file, _segments) in &pending {
            let outcome = assemble_file(&queue, file, &job.output_dir, &tx).await;
            match outcome {
                Ok(o) => {
                    if o.missing == 0 && o.crc_mismatches == 0 {
                        completed_count += 1;
                    } else {
                        failed_count += 1;
                    }
                    let _ = tx.send(ProgressEvent::FileCompleted {
                        filename: file.filename.clone(),
                        path: o.path,
                        missing: o.missing,
                        crc_mismatches: o.crc_mismatches,
                    });
                }
                Err(e) => {
                    failed_count += 1;
                    let _ = tx.send(ProgressEvent::ArticleError {
                        filename: file.filename.clone(),
                        segment: 0,
                        error: e.to_string(),
                    });
                }
            }
        }

        *completed.lock().await = completed_count;
        *failed.lock().await = failed_count;

        // Determine final job state.
        let final_state = if failed_count == 0 {
            crate::queue::JobState::Complete
        } else {
            crate::queue::JobState::Failed
        };
        queue.set_job_state(job_id, final_state).await?;

        let _ = tx.send(ProgressEvent::JobFinished {
            completed: completed_count,
            failed: failed_count,
        });
        Ok(())
    }

    /// A worker loop: continuously pops segments from the shared queue
    /// and fetches them using pooled connections until the queue is empty.
    #[allow(clippy::too_many_arguments)]
    async fn run_worker(
        &self,
        worker_id: usize,
        _queue: &Arc<QueueManager>,
        pool: &Arc<ConnectionPool>,
        work_queue: &Arc<Mutex<VecDeque<(QueueFile, QueueSegment)>>>,
        output_dir: &Path,
        tx: &mpsc::UnboundedSender<ProgressEvent>,
        state_tx: &mpsc::UnboundedSender<(i64, u32, SegmentState)>,
    ) -> Result<()> {
        let parts_dirs: Arc<Mutex<std::collections::HashMap<String, PathBuf>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        loop {
            // Pop the next segment from the queue.
            let (file, seg) = {
                let mut wq = work_queue.lock().await;
                match wq.pop_front() {
                    Some(item) => item,
                    None => break, // Queue empty — worker is done.
                }
            };

            tracing::trace!(
                worker_id,
                file = %file.filename,
                segment = seg.number,
                "worker: processing segment"
            );

            // Ensure the parts directory exists for this file.
            let parts_dir = {
                let mut dirs = parts_dirs.lock().await;
                if let Some(p) = dirs.get(&file.filename) {
                    p.clone()
                } else {
                    let p = output_dir.join(format!("{}.parts", file.filename));
                    tokio::fs::create_dir_all(&p)
                        .await
                        .map_err(CoreError::from)?;
                    dirs.insert(file.filename.clone(), p.clone());
                    p
                }
            };

            // Fetch the article using the connection pool.
            let fetch_result = pool_fetch_with_fallback(pool, &self.servers, &seg.message_id).await;

            let state = match &fetch_result {
                Ok(body) => match yenc::decode_article(body) {
                    Ok(decoded) => {
                        let seg_state = if decoded.crc_ok || decoded.crc_unknown {
                            SegmentState::Done
                        } else {
                            SegmentState::CrcMismatch
                        };
                        if seg_state == SegmentState::Done {
                            let part_path = parts_dir.join(format!("seg{:06}", seg.number));
                            tokio::fs::write(&part_path, &decoded.data)
                                .await
                                .map_err(CoreError::from)?;
                        }
                        seg_state
                    }
                    Err(e) => {
                        let _ = tx.send(ProgressEvent::ArticleError {
                            filename: file.filename.clone(),
                            segment: seg.number,
                            error: e.to_string(),
                        });
                        SegmentState::Failed
                    }
                },
                Err(PoolFetchError::Missing) => SegmentState::Missing,
                Err(PoolFetchError::Other(e)) => {
                    let _ = tx.send(ProgressEvent::ArticleError {
                        filename: file.filename.clone(),
                        segment: seg.number,
                        error: e.to_string(),
                    });
                    SegmentState::Failed
                }
            };

            // Queue the segment state update for the batch writer (no
            // individual DB write — the writer flushes periodically).
            let _ = state_tx.send((file.id, seg.number, state));

            let bytes = match &fetch_result {
                Ok(b) => b.len() as u64,
                Err(_) => 0,
            };
            let _ = tx.send(ProgressEvent::SegmentDone {
                filename: file.filename.clone(),
                segment: seg.number,
                status: state,
                bytes,
            });
        }

        tracing::info!(worker_id, "worker: queue empty, exiting");
        Ok(())
    }
}

/// Per-server connection pool. Each server has its own deque of idle
/// connections. Workers check out a connection, use it, and return it.
/// Broken connections are dropped (not returned to the pool).
pub struct ConnectionPool {
    /// One deque per server, indexed by server priority order.
    servers: Vec<Mutex<VecDeque<NntpClient>>>,
}

impl ConnectionPool {
    fn new(servers: &[ServerConfig]) -> Self {
        let server_pools = servers
            .iter()
            .map(|_| Mutex::new(VecDeque::new()))
            .collect();
        Self {
            servers: server_pools,
        }
    }

    /// Get a connection from the pool for `server_idx`, or create a new
    /// one if the pool is empty.
    async fn get(&self, server_idx: usize, servers: &[ServerConfig]) -> Result<NntpClient> {
        {
            let mut pool = self.servers[server_idx].lock().await;
            if let Some(conn) = pool.pop_front() {
                return Ok(conn);
            }
        }
        // Pool empty — create a new connection.
        NntpClient::connect(&servers[server_idx]).await
    }

    /// Return a healthy connection to the pool for later reuse.
    async fn put(&self, server_idx: usize, conn: NntpClient) {
        let mut pool = self.servers[server_idx].lock().await;
        pool.push_back(conn);
    }
}

/// Error type for pool-aware fetch.
#[derive(Debug)]
enum PoolFetchError {
    /// Article missing on all servers (430/423 from every server).
    Missing,
    Other(CoreError),
}

/// Fetch an article using the connection pool. Tries servers in priority
/// order. Connections are reused across calls — no per-article TLS handshake.
///
/// On success, the connection is returned to the pool. On connection
/// error, the connection is dropped (not returned) so the next fetch
/// creates a fresh one.
async fn pool_fetch_with_fallback(
    pool: &Arc<ConnectionPool>,
    servers: &[ServerConfig],
    message_id: &str,
) -> std::result::Result<Vec<u8>, PoolFetchError> {
    let mut all_missing = true;
    let mut last_error: Option<CoreError> = None;

    for (idx, _server) in servers.iter().enumerate() {
        // Get a connection (from pool or create new).
        let mut client = match pool.get(idx, servers).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(server_idx = idx, error = %e, "connect failed; trying next");
                last_error = Some(e);
                all_missing = false;
                continue;
            }
        };

        match client.body(message_id).await {
            Ok(Ok(body)) => {
                // Success — return connection to pool for reuse.
                pool.put(idx, client).await;
                return Ok(body.bytes);
            }
            Ok(Err(StatResult::Missing)) => {
                // Article not on this server — return connection, try next.
                pool.put(idx, client).await;
                continue;
            }
            Ok(Err(StatResult::Present)) => {
                // BODY never returns Present; protocol oddity. Return conn.
                pool.put(idx, client).await;
                continue;
            }
            Err(e) => {
                // Connection error — drop the connection (don't return to
                // pool). The next fetch will create a fresh one.
                tracing::warn!(server_idx = idx, error = %e, "BODY error; dropping connection");
                last_error = Some(e);
                all_missing = false;
                continue;
            }
        }
    }

    if all_missing {
        Err(PoolFetchError::Missing)
    } else {
        Err(PoolFetchError::Other(last_error.unwrap_or_else(|| {
            CoreError::Nntp(format!("no server could fetch {message_id}"))
        })))
    }
}

#[derive(Debug)]
struct FileOutcome {
    path: PathBuf,
    missing: u32,
    crc_mismatches: u32,
}

/// Assemble a file from per-segment part files. Reads all segments from
/// the DB, concatenates the done parts in order, and writes the final
/// file. Also calls `refresh_job_counts` to update aggregate counters.
async fn assemble_file(
    queue: &Arc<QueueManager>,
    file: &QueueFile,
    output_dir: &Path,
    _tx: &mpsc::UnboundedSender<ProgressEvent>,
) -> Result<FileOutcome> {
    // Refresh job-level aggregate counters now that all segments for this
    // file are done.
    if let Err(e) = queue.refresh_job_counts(file.id).await {
        tracing::warn!(error = %e, "failed to refresh job counts");
    }

    let filename = file.filename.clone();
    let final_path = output_dir.join(&filename);
    let parts_dir = output_dir.join(format!("{}.parts", filename));

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
                let part_path = parts_dir.join(format!("seg{n:06}"));
                match tokio::fs::read(&part_path).await {
                    Ok(data) => {
                        out.write_all(&data).await.map_err(CoreError::from)?;
                    }
                    Err(_) => {
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

/// Flush buffered segment state updates in a single DB transaction.
async fn flush_batch(queue: &Arc<QueueManager>, buffer: &mut Vec<(i64, u32, SegmentState)>) {
    if buffer.is_empty() {
        return;
    }
    tracing::trace!(count = buffer.len(), "flushing segment state batch");
    if let Err(e) = queue.set_segment_states_batch(buffer).await {
        tracing::error!(error = %e, "batch segment state write failed");
    }
    buffer.clear();
}

// Re-export mpsc for the public API.
pub use tokio::sync::mpsc;
