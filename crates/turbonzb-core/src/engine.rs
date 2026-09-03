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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::error::{CoreError, Result};
use crate::nntp::{NntpClient, ServerConfig};
use crate::queue::{QueueFile, QueueManager, QueueSegment, SegmentState};
use crate::yenc;

/// Aggregate download performance counters for one job run. Shared between
/// workers, the connection pool, and a sampler task that logs a summary
/// every couple of seconds so real-world behavior can be inspected without
/// a profiler attached.
#[derive(Default)]
pub struct PerfStats {
    /// Articles processed.
    pub articles: AtomicU64,
    /// Raw (yEnc) bytes received.
    pub bytes: AtomicU64,
    /// Microseconds spent waiting to pop work from the shared queue.
    pub queue_wait_us: AtomicU64,
    /// Microseconds spent acquiring a connection from the pool.
    pub acquire_us: AtomicU64,
    /// Microseconds spent on the full BODY exchange (acquire + cmd + read).
    pub fetch_us: AtomicU64,
    /// Microseconds spent decoding yEnc.
    pub decode_us: AtomicU64,
    /// Microseconds spent writing segment parts to disk.
    pub write_us: AtomicU64,
    /// Connections established (TLS handshake counts).
    pub conn_created: AtomicU64,
    /// Connections dropped due to errors.
    pub conn_dropped: AtomicU64,
    // Full-article BODY round-trip time buckets. These reveal whether the
    // server is dribbling data (high % in the big buckets ⇒ server-side
    // throttle) or the client is creating gaps (high % in small buckets
    // while throughput stays low ⇒ client-side serialization).
    pub fetch_le_20ms: AtomicU64,
    pub fetch_le_100ms: AtomicU64,
    pub fetch_le_500ms: AtomicU64,
    pub fetch_le_2000ms: AtomicU64,
    pub fetch_gt_2000ms: AtomicU64,
}

impl PerfStats {
    fn bucket(&self, us: u64) {
        if us <= 20_000 {
            self.fetch_le_20ms.fetch_add(1, Ordering::Relaxed);
        } else if us <= 100_000 {
            self.fetch_le_100ms.fetch_add(1, Ordering::Relaxed);
        } else if us <= 500_000 {
            self.fetch_le_500ms.fetch_add(1, Ordering::Relaxed);
        } else if us <= 2_000_000 {
            self.fetch_le_2000ms.fetch_add(1, Ordering::Relaxed);
        } else {
            self.fetch_gt_2000ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of all fetch-time buckets, for per-run deltas.
    fn fetch_buckets(&self) -> [u64; 5] {
        [
            self.fetch_le_20ms.load(Ordering::Relaxed),
            self.fetch_le_100ms.load(Ordering::Relaxed),
            self.fetch_le_500ms.load(Ordering::Relaxed),
            self.fetch_le_2000ms.load(Ordering::Relaxed),
            self.fetch_gt_2000ms.load(Ordering::Relaxed),
        ]
    }
}

/// The download engine. Owns the server list and concurrency limit.
pub struct Engine {
    servers: Vec<ServerConfig>,
    /// Concurrency limit = number of worker tasks.
    max_connections: usize,
    /// Shared connection pool, created eagerly in [`Engine::new`] so it can
    /// be warmed up and kept alive across jobs (connection establishment is
    /// the expensive, throttle-prone part of talking to a provider — we hold
    /// onto connections as long as possible and never rebuild them per job).
    pool: Arc<std::sync::Mutex<Option<Arc<ConnectionPool>>>>,
    /// Shared performance counters.
    stats: Arc<PerfStats>,
}

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

impl Engine {
    /// Create an engine for a set of servers. `total_connections` is the
    /// number of worker tasks to spawn — each borrows a live connection
    /// from the (persistent) pool.
    pub fn new(servers: Vec<ServerConfig>, total_connections: usize) -> Self {
        let stats = Arc::new(PerfStats::default());
        let pool = Arc::new(ConnectionPool::new(&servers, Arc::clone(&stats)));
        Self {
            servers,
            max_connections: total_connections,
            pool: Arc::new(std::sync::Mutex::new(Some(pool))),
            stats,
        }
    }

    /// Current total live connection count (idle in pool + in use).
    pub fn active_connections(&self) -> usize {
        self.pool
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|p| p.active_count()))
            .unwrap_or(0)
    }

    /// The shared performance counters (for the final summary etc).
    pub fn stats(&self) -> Arc<PerfStats> {
        Arc::clone(&self.stats)
    }

    /// A clone of the shared connection pool, if created.
    pub fn pool(&self) -> Option<Arc<ConnectionPool>> {
        self.pool.lock().ok().and_then(|g| g.clone())
    }

    /// Server list (for pool warm-up / keep-alive).
    pub fn servers(&self) -> &[ServerConfig] {
        &self.servers
    }

    /// Spawn the background pool keeper: paces connection warm-up (so we
    /// never blast a provider's setup throttle) and NOOPs idle connections
    /// every 30s so they stay alive across jobs and idle periods.
    pub fn spawn_pool_keeper(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let mut warm = tokio::time::interval(std::time::Duration::from_millis(350));
            warm.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut keep = tokio::time::interval(std::time::Duration::from_secs(30));
            keep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = warm.tick() => {
                        if let Some(p) = engine.pool() {
                            p.try_open_one(engine.servers()).await;
                        }
                    }
                    _ = keep.tick() => {
                        if let Some(p) = engine.pool() {
                            p.keep_idle_alive().await;
                        }
                    }
                }
            }
        });
    }

    /// Try one connection per server. Returns an error string if EVERY
    /// server failed with a deterministic DNS-style error (bad hostname),
    /// so the job can fail fast instead of retrying each segment.
    async fn precheck_server_dns(&self) -> Option<String> {
        let mut last_err: Option<String> = None;
        let mut any_ok = false;
        for s in &self.servers {
            match NntpClient::connect(s).await {
                Ok(c) => {
                    drop(c);
                    any_ok = true;
                    break;
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if any_ok {
            return None;
        }
        let err = last_err?;
        let low = err.to_lowercase();
        if low.contains("lookup")
            || low.contains("name or service not known")
            || low.contains("resolve")
            || low.contains("dns")
        {
            Some(err)
        } else {
            // Not a deterministic DNS failure (e.g. refused/timeout) —
            // let the normal retry logic handle it.
            None
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
        self.run_job_inner(queue, job_id, tx, None).await
    }

    /// Like [`Engine::run_job`], but stops gracefully when `cancel` is set:
    /// workers stop pulling new work, completed segments' state is flushed
    /// and already-written bytes reach the file, then the job returns
    /// **before** file finalization — so a paused job's already-downloaded
    /// segments are persisted and NOT re-fetched when it's resumed.
    pub async fn run_job_cancellable(
        self: Arc<Self>,
        queue: Arc<QueueManager>,
        job_id: i64,
        tx: mpsc::UnboundedSender<ProgressEvent>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        self.run_job_inner(queue, job_id, tx, Some(cancel)).await
    }

    /// Shared implementation of both entry points.
    async fn run_job_inner(
        self: Arc<Self>,
        queue: Arc<QueueManager>,
        job_id: i64,
        tx: mpsc::UnboundedSender<ProgressEvent>,
        cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<()> {
        // Ensure the job is in 'downloading' state. The caller should have
        // already claimed the download slot via claim_download_slot, but
        // we set it here too for standalone use (CLI, tests).
        queue
            .set_job_state(job_id, crate::queue::JobState::Downloading)
            .await?;
        // Clear stale error from a previous (failed) attempt — it's being
        // redownloaded now.
        queue.set_job_error(job_id, None).await?;

        // Fail fast if every configured server is unreachable for a
        // *deterministic* reason (DNS resolution failure — e.g. a typo in
        // the server hostname). Transient errors (refused/timeout) are
        // left to the per-segment retry logic. Only probe when no
        // connections are already established (a warm pool proves DNS OK).
        {
            let pool_opt = self.pool();
            let need_probe = pool_opt.map(|p| p.active_count() == 0).unwrap_or(true);
            if need_probe {
                if let Some(err) = self.precheck_server_dns().await {
                    let msg =
                        format!("Cannot reach NNTP server — check the hostname in Settings: {err}");
                    queue
                        .set_job_state(job_id, crate::queue::JobState::Failed)
                        .await?;
                    queue.set_job_error(job_id, Some(&msg)).await?;
                    let _ = tx.send(ProgressEvent::ArticleError {
                        filename: String::new(),
                        segment: 0,
                        error: msg,
                    });
                    let _ = tx.send(ProgressEvent::JobFinished {
                        completed: 0,
                        failed: 0,
                    });
                    return Ok(());
                }
            }
        }

        let t0 = std::time::Instant::now();

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
        if let Ok((done, missing, pending_here, crc, failed)) =
            queue.segment_state_counts(job_id).await
        {
            tracing::info!(
                pending = pending_here,
                done,
                missing,
                crc_mismatches = crc,
                failed,
                "engine: segment state breakdown on job start"
            );
        }
        tracing::info!(
            pending_files = pending.len(),
            "engine: pending segments loaded"
        );

        // Flatten all segments across all files into a single work queue.
        // Each item is (file_index, segment) so the worker can look up
        // the file via the shared `files` Arc without cloning the full
        // QueueFile for every segment (saves ~150 bytes per segment).
        let files: Arc<Vec<QueueFile>> = Arc::new(pending.iter().map(|(f, _)| f.clone()).collect());
        let work_queue: Arc<Mutex<VecDeque<(usize, QueueSegment)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let total_segments: usize = {
            let mut wq = work_queue.lock().await;
            let mut count = 0usize;
            for (file_idx, (file, segments)) in pending.iter().enumerate() {
                let _ = tx.send(ProgressEvent::FileStarted {
                    filename: file.filename.clone(),
                    segments: segments.len() as u32,
                });
                for seg in segments {
                    wq.push_back((file_idx, seg.clone()));
                    count += 1;
                }
            }
            count
        };
        tracing::info!(total_segments, "engine: work queue populated");

        // Shared pool (warmed + kept alive by the keeper task) and perf
        // counters are owned by the engine, so they persist across jobs.
        let pool = self.pool.lock().expect("pool lock").clone();
        let Some(pool) = pool else {
            // Pool dropped (shouldn't happen) — fall back to creating one.
            unreachable!("pool should never be cleared after new()")
        };
        let stats = Arc::clone(&self.stats);
        // Snapshot cumulative counters so the end-of-run perf summary reports
        // just this run's bytes/articles, not the process-wide total
        // (stats are shared across runs and jobs).
        let stats_articles_start = stats.articles.load(Ordering::Relaxed);
        let stats_bytes_start = stats.bytes.load(Ordering::Relaxed);
        let stats_conn_created_start = stats.conn_created.load(Ordering::Relaxed);
        let stats_conn_dropped_start = stats.conn_dropped.load(Ordering::Relaxed);
        let stats_fetch_buckets_start = stats.fetch_buckets();

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
        // Per-segment attempt counter (keyed by segment DB id) so that a
        // permanently unreachable server fails the job instead of retrying
        // every segment forever.
        let seg_attempts: Arc<Mutex<std::collections::HashMap<i64, u32>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Direct-write (Pillar 3): one output file per NZB file, segments
        // `pwrite`d at their offset. `writers` maps a file id to the sender
        // of its dedicated writer task; `writer_tasks` holds those tasks so we
        // can wait for them to drain before finalizing each file.
        let writers: Arc<tokio::sync::Mutex<OutputWriterMap>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let writer_tasks: Arc<tokio::sync::Mutex<WriterTaskMap>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let mut workers: JoinSet<Result<()>> = JoinSet::new();
        for worker_id in 0..self.max_connections {
            let engine = Arc::clone(&self);
            let queue = Arc::clone(&queue);
            let pool = Arc::clone(&pool);
            let work_queue = Arc::clone(&work_queue);
            let files = Arc::clone(&files);
            let stats = Arc::clone(&stats);
            let seg_attempts = Arc::clone(&seg_attempts);
            let writers = Arc::clone(&writers);
            let writer_tasks = Arc::clone(&writer_tasks);
            let cancel = cancel.clone();
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
                        &files,
                        &stats,
                        &seg_attempts,
                        &writers,
                        &writer_tasks,
                        &output_dir,
                        &tx,
                        &state_tx,
                        cancel.as_ref(),
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

        // Close every per-file writer channel: dropping the registry drops
        // all senders, so each writer task drains its writes and exits.
        drop(writers);
        let mut writer_paths: std::collections::HashMap<i64, PathBuf> =
            std::collections::HashMap::new();
        {
            let mut tasks = writer_tasks.lock().await;
            for (file_id, handle) in tasks.drain() {
                match handle.await {
                    Ok(Ok(path)) => {
                        writer_paths.insert(file_id, path);
                    }
                    Ok(Err(e)) => {
                        tracing::error!(file_id, error = %e, "file writer task error");
                    }
                    Err(e) => {
                        tracing::error!(file_id, error = %e, "file writer task panic");
                    }
                }
            }
        }

        // On cancel (pause): state is flushed and files drained above; stop
        // before finalizing so resume re-fetches nothing already downloaded.
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            // Refresh the job's aggregate counters (segments_done,
            // downloaded_bytes, files_done) so the UI shows the real,
            // persisted progress instead of 0 when the job sits Queued.
            for (file, _segments) in &pending {
                let _ = queue.refresh_job_counts(file.id).await;
            }
            log_perf_summary(
                &stats,
                t0,
                stats_articles_start,
                stats_bytes_start,
                stats_conn_created_start,
                stats_conn_dropped_start,
                &stats_fetch_buckets_start,
            );
            tracing::info!(
                job_id,
                "engine: cancelled — state persisted, finalize skipped"
            );
            return Ok(());
        }

        // Finalize all files now that their writer tasks have drained.
        let mut completed_count = 0usize;
        let mut failed_count = 0usize;
        for (file, _segments) in &pending {
            let outcome = finalize_file(
                &queue,
                file,
                &job.output_dir,
                &job.name,
                job.file_count,
                &writer_paths,
                &tx,
            )
            .await;
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
        let (final_state, error_msg) = if failed_count == 0 {
            (crate::queue::JobState::Complete, None)
        } else {
            (
                crate::queue::JobState::Failed,
                Some(format!(
                    "{failed_count} of {file_count_total} files with missing/corrupt segments",
                    file_count_total = pending.len()
                )),
            )
        };
        queue.set_job_state(job_id, final_state).await?;
        // Persist the human-readable reason (or clear a stale one).
        queue.set_job_error(job_id, error_msg.as_deref()).await?;

        let _ = tx.send(ProgressEvent::JobFinished {
            completed: completed_count,
            failed: failed_count,
        });

        // Final perf summary (opt-in; only visible with
        // RUST_LOG=turbonzb_core=info).
        log_perf_summary(
            &stats,
            t0,
            stats_articles_start,
            stats_bytes_start,
            stats_conn_created_start,
            stats_conn_dropped_start,
            &stats_fetch_buckets_start,
        );

        // NOTE: pool is intentionally kept alive across jobs — connection
        // establishment is the throttled, expensive part, so we hold.

        Ok(())
    }

    /// A worker loop: continuously pops batches of segments and fetches
    /// them with pipelined BODY commands until the queue is empty.
    #[allow(clippy::too_many_arguments)]
    async fn run_worker(
        &self,
        worker_id: usize,
        queue: &Arc<QueueManager>,
        pool: &Arc<ConnectionPool>,
        work_queue: &Arc<Mutex<VecDeque<(usize, QueueSegment)>>>,
        files: &Arc<Vec<QueueFile>>,
        stats: &Arc<PerfStats>,
        seg_attempts: &Arc<Mutex<std::collections::HashMap<i64, u32>>>,
        writers: &Arc<tokio::sync::Mutex<OutputWriterMap>>,
        writer_tasks: &Arc<tokio::sync::Mutex<WriterTaskMap>>,
        output_dir: &Path,
        tx: &mpsc::UnboundedSender<ProgressEvent>,
        state_tx: &mpsc::UnboundedSender<(i64, u32, SegmentState)>,
        cancel: Option<&Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<()> {
        // Files whose real name (from `=ybegin name=`) we've already
        // persisted, to avoid re-writing the DB on every segment.
        let mut name_latched: std::collections::HashSet<i64> = std::collections::HashSet::new();

        loop {
            // Graceful stop on pause — stop pulling new work (in-flight batch
            // above is still written out and its state still recorded).
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                break;
            }
            // Pop a batch of up to PIPELINE segments (timed — high average
            // = workers fighting for work). Batching lets us send several
            // BODY commands on one connection before reading responses
            // (Pillar 1a — command pipelining).
            let batch: Vec<(usize, QueueSegment)> = {
                let t = std::time::Instant::now();
                let mut wq = work_queue.lock().await;
                let mut b = Vec::with_capacity(PIPELINE);
                while b.len() < PIPELINE {
                    match wq.pop_front() {
                        Some(item) => b.push(item),
                        None => break,
                    }
                }
                stats
                    .queue_wait_us
                    .fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                if b.is_empty() {
                    break; // Queue empty — worker is done.
                }
                b
            };

            // Bounded retry: count attempts per segment; a server that is
            // permanently unreachable must eventually fail the job rather
            // than retrying forever (which burns CPU and floods logs).
            let mut to_fetch: Vec<(usize, QueueSegment)> = Vec::with_capacity(batch.len());
            let mut failed_items: Vec<(usize, QueueSegment)> = Vec::new();
            for (file_idx, seg) in batch {
                let attempts = {
                    let mut map = seg_attempts.lock().await;
                    let e = map.entry(seg.id).or_insert(0);
                    *e += 1;
                    *e
                };
                if attempts > MAX_SEGMENT_ATTEMPTS {
                    failed_items.push((file_idx, seg));
                } else {
                    to_fetch.push((file_idx, seg));
                }
            }
            for (file_idx, seg) in failed_items {
                let file = &files[file_idx];
                let _ = state_tx.send((file.id, seg.number, SegmentState::Failed));
                stats.articles.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(ProgressEvent::SegmentDone {
                    filename: file.filename.clone(),
                    segment: seg.number,
                    status: SegmentState::Failed,
                    bytes: 0,
                });
                let _ = tx.send(ProgressEvent::ArticleError {
                    filename: file.filename.clone(),
                    segment: seg.number,
                    error: format!(
                        "fetch failed after {MAX_SEGMENT_ATTEMPTS} attempts (server unreachable?)"
                    ),
                });
            }
            if to_fetch.is_empty() {
                continue;
            }

            // Pipeline the BODY requests on pooled connections (window = the
            // batch size, bounded by PIPELINE), decoding straight into the
            // segment buffer as each body streams in (no intermediate copy).
            let t_fetch = std::time::Instant::now();
            let results = pipeline_fetch(pool, &self.servers, &to_fetch, stats, cancel).await;
            let fetch_us = t_fetch.elapsed().as_micros() as u64;
            stats.fetch_us.fetch_add(fetch_us, Ordering::Relaxed);
            stats.bucket(fetch_us);

            let mut requeue: Vec<(usize, QueueSegment)> = Vec::new();
            for (pos, (file_idx, seg)) in to_fetch.iter().enumerate() {
                let file = &files[*file_idx];
                let outcome = match results.get(pos) {
                    Some(o) => o,
                    None => {
                        requeue.push((*file_idx, seg.clone()));
                        continue;
                    }
                };
                let (seg_state, bytes, write_data) = match outcome {
                    FetchOutcome::Decoded(decoded) => {
                        let seg_state = if decoded.crc_ok || decoded.crc_unknown {
                            SegmentState::Done
                        } else {
                            SegmentState::CrcMismatch
                        };
                        if seg_state == SegmentState::Done {
                            // Obfuscated posts put a hash in the subject but
                            // the real filename in `=ybegin name=`. Latch the
                            // real name (once per file) so the finalized
                            // file is identifiable on disk.
                            let real = sanitize_yenc_name(&decoded.name);
                            if !real.is_empty()
                                && real != file.filename
                                && file.yenc_name.as_deref() != Some(real.as_str())
                                && name_latched.insert(file.id)
                            {
                                if let Err(e) = queue.set_file_yenc_name(file.id, &real).await {
                                    tracing::warn!(
                                        file_id = file.id,
                                        error = %e,
                                        "failed to record yenc filename"
                                    );
                                }
                            }
                            (
                                seg_state,
                                seg.bytes,
                                Some((decoded.begin, decoded.data.clone())),
                            )
                        } else {
                            (seg_state, seg.bytes, None)
                        }
                    }
                    FetchOutcome::Missing => (SegmentState::Missing, 0, None),
                    FetchOutcome::Retry => {
                        requeue.push((*file_idx, seg.clone()));
                        continue;
                    }
                };

                // Direct write: send the decoded bytes to this file's
                // dedicated writer task, which pwrite's them at their
                // offset into the single output file (Pillar 3).
                if let Some((begin, data)) = write_data {
                    // =ypart begin= is header-derived; a corrupt header
                    // with a huge begin would seek a sparse write far
                    // beyond the file. Bound it to 2^48 (256 TB of
                    // positional offset is still nonsense for a release
                    // file, but the yenc layer already rejects absurd
                    // ranges — this is defense in depth).
                    const MAX_WRITE_OFFSET: u64 = 1 << 48;
                    if begin > MAX_WRITE_OFFSET {
                        tracing::warn!(begin, "implausible write offset — dropping segment");
                        let _ = state_tx.send((file.id, seg.number, SegmentState::CrcMismatch));
                        let _ = tx.send(ProgressEvent::ArticleError {
                            filename: file.filename.clone(),
                            segment: seg.number,
                            error: "implausible write offset (corrupt article header)".into(),
                        });
                        continue;
                    }
                    let offset = begin.saturating_sub(1);
                    if !send_to_file_writer(
                        writers,
                        writer_tasks,
                        stats,
                        file.id,
                        &file.filename,
                        output_dir,
                        offset,
                        data,
                    )
                    .await
                    {
                        // Bytes could not be handed to a writer — don't mark
                        // the segment Done (that would record it as complete
                        // while its data is absent). Re-queue so it's
                        // re-decoded and re-written once a writer is healthy.
                        tracing::warn!(
                            file_id = file.id,
                            offset,
                            "file writer unavailable; requeuing segment"
                        );
                        requeue.push((*file_idx, seg.clone()));
                        continue;
                    }
                }

                stats.articles.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(bytes, Ordering::Relaxed);
                let _ = state_tx.send((file.id, seg.number, seg_state));
                let _ = tx.send(ProgressEvent::SegmentDone {
                    filename: file.filename.clone(),
                    segment: seg.number,
                    status: seg_state,
                    bytes,
                });
            }

            // Re-queue segments that hit transient connection errors so they
            // get a fresh attempt (short backoff so a dead server doesn't
            // cause a hot retry loop).
            if !requeue.is_empty() {
                // Backoff so a dead server doesn't hot-spin — unless we're
                // just winding down for a pause, where latency matters.
                if !cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                let mut wq = work_queue.lock().await;
                for (file_idx, seg) in requeue.into_iter() {
                    wq.push_front((file_idx, seg));
                }
            }
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
    /// One gate per server: a permit represents the right to have one
    /// connection checked out (idle in deque OR actively in use). Permits
    /// are bounded by each server's configured max, so `get` *waits* for
    /// a free slot instead of stampeding new connections when the deque is
    /// momentarily empty — which providers at their connection cap reject.
    gate: Vec<Arc<tokio::sync::Semaphore>>,
    /// Total number of live connections (idle in pool + actively in use).
    /// Incremented on connect, decremented when a connection is dropped.
    active: std::sync::atomic::AtomicUsize,
    /// Bounds how many connections are being *established* at once.
    /// Opening dozens of TLS handshakes simultaneously trips providers'
    /// connection-bombardment protection (they RST the excess, producing the
    /// "connection closed mid-response" / churn storm), so we ramp up a few
    /// at a time instead of stampeding.
    connect_gate: tokio::sync::Semaphore,
    /// Performance counters (connection create/drop).
    stats: Arc<PerfStats>,
}

/// Max simultaneous connection establishments across all servers.
const MAX_CONCURRENT_CONNECTS: usize = 4;

impl ConnectionPool {
    fn new(servers: &[ServerConfig], stats: Arc<PerfStats>) -> Self {
        let server_pools = servers
            .iter()
            .map(|_| Mutex::new(VecDeque::new()))
            .collect();
        let gate = servers
            .iter()
            .map(|s| {
                Arc::new(tokio::sync::Semaphore::new(
                    s.max_connections.max(1) as usize
                ))
            })
            .collect();
        Self {
            servers: server_pools,
            gate,
            active: std::sync::atomic::AtomicUsize::new(0),
            connect_gate: tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTS),
            stats,
        }
    }

    /// Current total live connection count (idle + in use).
    pub fn active_count(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check out a connection for `server_idx`, waiting for a free slot
    /// if all are in use. Returns the connection plus its slot permit.
    async fn get(
        &self,
        server_idx: usize,
        servers: &[ServerConfig],
    ) -> Result<(NntpClient, tokio::sync::OwnedSemaphorePermit)> {
        // Wait for the right to have one connection checked out.
        let permit = self.gate[server_idx]
            .clone()
            .acquire_owned()
            .await
            .expect("pool gate semaphore never closed");
        {
            let mut pool = self.servers[server_idx].lock().await;
            if let Some(conn) = pool.pop_front() {
                return Ok((conn, permit));
            }
        }
        // Deque empty but we hold a slot — create a new connection. Paced
        // via connect_gate so we never burst dozens of handshakes at once.
        let _connect_permit = self
            .connect_gate
            .acquire()
            .await
            .expect("connect gate closed");
        {
            // Re-check the deque: another waiter may have returned a pooled
            // connection while we waited on the gate.
            let mut pool = self.servers[server_idx].lock().await;
            if let Some(conn) = pool.pop_front() {
                return Ok((conn, permit));
            }
        }
        self.active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.stats.conn_created.fetch_add(1, Ordering::Relaxed);
        match NntpClient::connect(&servers[server_idx]).await {
            Ok(conn) => Ok((conn, permit)),
            Err(e) => {
                self.active
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                // Dropping the permit frees the slot (connection never opened).
                Err(e)
            }
        }
    }

    /// Return a healthy connection to the pool for reuse (releases its slot).
    async fn put(
        &self,
        server_idx: usize,
        conn: NntpClient,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        // Push the connection BEFORE releasing the permit so a waiting worker
        // that grabs the slot also finds the connection available.
        self.servers[server_idx].lock().await.push_back(conn);
        drop(permit);
    }

    /// Drop a connection (releases its slot).
    fn drop_connection(&self, permit: tokio::sync::OwnedSemaphorePermit) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.stats.conn_dropped.fetch_add(1, Ordering::Relaxed);
        drop(permit);
    }

    /// Open one new connection and place it in the idle pool — unless the
    /// pool is already at full capacity. The caller paces this (a keeper
    /// task) so connection establishment doesn't burst into a provider's
    /// setup throttle.
    async fn try_open_one(&self, servers: &[ServerConfig]) {
        let capacity: usize = servers
            .iter()
            .map(|s| s.max_connections.max(1) as usize)
            .sum();
        if self.active_count() >= capacity {
            return;
        }
        for (idx, srv) in servers.iter().enumerate() {
            if let Ok(conn) = NntpClient::connect(srv).await {
                self.active
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.stats.conn_created.fetch_add(1, Ordering::Relaxed);
                self.servers[idx].lock().await.push_back(conn);
                return;
            }
            // connect failed — try the next server; caller retries later.
        }
    }

    /// Keep idle connections alive: send a `NOOP` on exactly ONE idle
    /// connection per call (the keeper paces this), so a busy download
    /// is never starved: at most one connection is briefly checked out.
    async fn keep_idle_alive(&self) {
        // Drain one connection from the first server that has any idle.
        let found: Option<(usize, NntpClient)> = {
            let mut found = None;
            for (i, pool) in self.servers.iter().enumerate() {
                let Some(conn) = pool.lock().await.pop_front() else {
                    continue;
                };
                found = Some((i, conn));
                break;
            }
            found
        };
        let Some((idx, mut conn)) = found else { return };
        if conn.noop().await.is_ok() {
            self.servers[idx].lock().await.push_back(conn);
        } else {
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            self.stats.conn_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// How many times a segment is retried before being marked failed. Bounds
/// the retry loop when a server is permanently unreachable.
const MAX_SEGMENT_ATTEMPTS: u32 = 4;

/// Outcome of fetching a single article.
/// How many BODY commands to pipeline on one connection before reading
/// responses. Modest and bounded: even a server that doesn't support
/// pipelining responds per command as it reads them, and 4 small commands
/// fit comfortably in any input buffer, so we never overflow a server's
/// flow control (Pillar 1a).
const PIPELINE: usize = 4;

/// How often in-flight BODY reads re-check the pause flag while streaming.
/// Small enough that pressing Pause interrupts a stalled batch almost
/// immediately; large enough that the wakeup cost is negligible.
const CANCEL_CHECK_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Outcome of fetching a single article via the pipelined path.
#[derive(Clone)]
enum FetchOutcome {
    /// Decoded article (CRC already checked). Caller decides Done/Mismatch.
    Decoded(yenc::DecodedPart),
    /// Article genuinely missing (430/423 on every server) — mark Missing.
    Missing,
    /// A connection-level problem — caller should re-queue (retry later).
    Retry,
}

/// A single positional write job for a per-file writer task: `data` goes at
/// byte `offset` of the file.
struct WriteJob {
    offset: u64,
    data: Vec<u8>,
}

/// The on-disk path a file's segments are written to (and resumed from).
///
/// - Obfuscated-by-subject files (a bare hex token) are written to a
///   stable temp path and only renamed once content is available to sniff a
///   real extension — their final name isn't known until then.
/// - Everything else is written directly to its real filename so a partial
///   file resumes in place across runs (no data is ever lost on resume).
fn write_target_for(file_id: i64, output_dir: &Path, filename: &str) -> PathBuf {
    if is_obfuscated_name(filename) {
        output_dir.join(format!(".turbonzb-{file_id}.partial"))
    } else {
        output_dir.join(filename)
    }
}

/// Open (create-or-resume) the per-file output file for writing. Tries
/// `primary` first; if that path is blocked — e.g. it already exists as a
/// **directory** (left over from a previous run) — falls back to `fallback`
/// (a stable per-file temp path) so the download never dies on a naming
/// collision. Returns the path actually opened.
async fn open_writer_fd(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false) // never truncate — resume a partial file in place
        .write(true)
        .read(true)
        .open(path)
        .await
}

async fn open_writer_file(primary: &Path, fallback: &Path) -> Result<(PathBuf, tokio::fs::File)> {
    match open_writer_fd(primary).await {
        Ok(f) => Ok((primary.to_path_buf(), f)),
        Err(_) => match open_writer_fd(fallback).await {
            Ok(f) => Ok((fallback.to_path_buf(), f)),
            Err(e) => Err(CoreError::from(e)),
        },
    }
}

/// Dedicated per-file writer task (Pillar 3). Receives decoded segments via
/// `rx` and `pwrite`s each at its offset into a single output file, opened
/// without truncation so a partial file from a previous run is resumed in
/// place.
///
/// A single task owns the `File`, so no locking is needed across workers and
/// positional seek+write is safe. Returns the path actually written to once
/// the channel closes.
async fn file_writer_task(
    primary: PathBuf,
    fallback: PathBuf,
    stats: Arc<PerfStats>,
    mut rx: mpsc::UnboundedReceiver<WriteJob>,
) -> Result<PathBuf> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let (path, mut file) = open_writer_file(&primary, &fallback).await?;
    let mut max_end: u64 = 0;
    while let Some(job) = rx.recv().await {
        let t_write = std::time::Instant::now();
        file.seek(std::io::SeekFrom::Start(job.offset))
            .await
            .map_err(CoreError::from)?;
        file.write_all(&job.data).await.map_err(CoreError::from)?;
        stats
            .write_us
            .fetch_add(t_write.elapsed().as_micros() as u64, Ordering::Relaxed);
        max_end = max_end.max(job.offset + job.data.len() as u64);
    }
    // Grow the file to cover the furthest byte written (never shrink — a
    // resumed file may already be larger from a prior run; writes beyond
    // EOF create sparse holes, keeping resume cheap).
    let cur = file.metadata().await.map_err(CoreError::from)?.len();
    if max_end > cur {
        file.set_len(max_end).await.map_err(CoreError::from)?;
    }
    file.sync_all().await.map_err(CoreError::from)?;
    Ok(path)
}

/// Sends a decoded segment to its file's writer task, creating the task on
/// first use. Returns `false` if the write could not be handed off (writer
/// channel closed) — in which case a fresh writer is created for subsequent
/// segments, and the caller should RE-QUEUE this segment (its data is
/// consumed by the failed send, so it must be re-cut to re-send). The
/// caller must NOT mark the segment Done unless this returns `true`
/// (otherwise its bytes are silently lost).
#[allow(clippy::too_many_arguments)]
async fn send_to_file_writer(
    writers: &Arc<tokio::sync::Mutex<OutputWriterMap>>,
    writer_tasks: &Arc<tokio::sync::Mutex<WriterTaskMap>>,
    stats: &Arc<PerfStats>,
    file_id: i64,
    filename: &str,
    output_dir: &Path,
    offset: u64,
    data: Vec<u8>,
) -> bool {
    let primary = write_target_for(file_id, output_dir, filename);
    let fallback = output_dir.join(format!(".turbonzb-{file_id}.partial"));

    let sender = {
        let mut reg = writers.lock().await;
        if let Some(s) = reg.get(&file_id) {
            s.clone()
        } else {
            let (txw, rxw) = mpsc::unbounded_channel();
            let st = Arc::clone(stats);
            let handle = tokio::spawn(file_writer_task(primary.clone(), fallback.clone(), st, rxw));
            writer_tasks.lock().await.insert(file_id, handle);
            reg.insert(file_id, txw.clone());
            txw
        }
    };

    if sender.send(WriteJob { offset, data }).is_ok() {
        return true;
    }

    // The writer task died (e.g. its open failed) — replace it so future
    // segments have a live writer, and report failure for THIS one.
    let (txw, rxw) = mpsc::unbounded_channel();
    let st = Arc::clone(stats);
    let handle = tokio::spawn(file_writer_task(primary, fallback, st, rxw));
    writer_tasks.lock().await.insert(file_id, handle);
    writers.lock().await.insert(file_id, txw);
    false
}

/// Pipelined fetch of a batch of articles using the gated connection pool,
/// trying servers in priority order (Pillar 1a).
///
/// For each server, a connection is checked out, up to `PIPELINE` BODY
/// commands are written back-to-back, then the responses are read in order —
/// so articles stream with no per-article command round-trip gap. Each body
/// is yEnc-decoded straight from the wire (no intermediate `Vec`) via
/// `NntpClient::read_body_decoded` (Pillar 1b).
///
/// The returned `Vec<FetchOutcome>` is aligned with `items`:
/// - `Decoded` — fetched and decoded on some server.
/// - `Missing` — 430/423 on every server, no connection error.
/// - `Retry` — a connection broke while trying; caller re-queues.
async fn pipeline_fetch(
    pool: &Arc<ConnectionPool>,
    servers: &[ServerConfig],
    items: &[(usize, QueueSegment)],
    stats: &PerfStats,
    cancel: Option<&Arc<AtomicBool>>,
) -> Vec<FetchOutcome> {
    let cancelled =
        |cancel: Option<&Arc<AtomicBool>>| cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let mut results: Vec<FetchOutcome> = vec![FetchOutcome::Missing; items.len()];
    let mut unresolved: Vec<usize> = (0..items.len()).collect();
    let mut conn_error: Vec<bool> = vec![false; items.len()];

    'server_loop: for (idx, _srv) in servers.iter().enumerate() {
        if unresolved.is_empty() || cancelled(cancel) {
            break;
        }
        let t_acquire = std::time::Instant::now();
        let (mut client, permit) = match pool.get(idx, servers).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(server_idx = idx, error = %e, "connect failed; trying next");
                // Still-unresolved items saw a connection problem.
                for &i in &unresolved {
                    conn_error[i] = true;
                }
                continue;
            }
        };
        stats
            .acquire_us
            .fetch_add(t_acquire.elapsed().as_micros() as u64, Ordering::Relaxed);

        // Window = still-unresolved items, capped at PIPELINE.
        let window: Vec<usize> = unresolved.iter().take(PIPELINE).copied().collect();

        // Write all commands first…
        let mut write_ok = true;
        for &i in &window {
            if cancelled(cancel) {
                // Aborting: treat the window as connection-broken so the
                // connection is dropped (it may hold a partial response).
                for &j in &window {
                    conn_error[j] = true;
                }
                write_ok = false;
                break;
            }
            if let Err(e) = client.send_body(&items[i].1.message_id).await {
                tracing::warn!(server_idx = idx, error = %e, "BODY send error; dropping connection");
                for &j in &window {
                    conn_error[j] = true;
                }
                write_ok = false;
                break;
            }
        }

        // …then read all responses in order (only if every command was sent).
        // Each read races the pause flag (checked every CANCEL_CHECK_POLL) so
        // a user pressing Pause interrupts in-flight bodies instead of
        // waiting for the whole batch to stream in.
        let mut read_ok = write_ok;
        if write_ok {
            'window: for &i in &window {
                let read = client.read_body_decoded();
                tokio::pin!(read);
                let outcome = loop {
                    tokio::select! {
                        biased;
                        r = &mut read => break Some(r),
                        _ = tokio::time::sleep(CANCEL_CHECK_POLL) => {
                            if cancelled(cancel) {
                                break None;
                            }
                        }
                    }
                };
                match outcome {
                    Some(Ok(Ok(part))) => {
                        results[i] = FetchOutcome::Decoded(part);
                    }
                    Some(Ok(Err(_))) => {
                        // Absent here — keep unresolved so the next server
                        // (in priority order) can try it.
                    }
                    Some(Err(e)) => {
                        tracing::warn!(server_idx = idx, error = %e, "BODY read error; dropping connection");
                        for &j in &window {
                            conn_error[j] = true;
                        }
                        read_ok = false;
                        break 'window;
                    }
                    None => {
                        // Cancelled mid-read: drop the connection (partial
                        // body in flight makes it unusable anyway).
                        for &j in &window {
                            conn_error[j] = true;
                        }
                        read_ok = false;
                        break 'window;
                    }
                }
            }
        }

        if read_ok && write_ok {
            // Healthy — return the connection to the pool for reuse.
            pool.put(idx, client, permit).await;
        } else {
            // Broken — drop it (the client is dropped here too).
            pool.drop_connection(permit);
            continue 'server_loop;
        }
        // Only 430s remain unresolved → try the next server for them.
        unresolved.retain(|i| matches!(results[*i], FetchOutcome::Missing));
    }

    // Finalize: items unresolved after every server are Missing unless a
    // connection error occurred anywhere while trying them (then Retry).
    for (i, r) in results.iter_mut().enumerate() {
        if matches!(r, FetchOutcome::Missing) && conn_error[i] {
            *r = FetchOutcome::Retry;
        }
    }
    results
}

#[derive(Debug)]
struct FileOutcome {
    path: PathBuf,
    missing: u32,
    crc_mismatches: u32,
}

/// Clean a filename from a `=ybegin name=` header for use as a file name:
/// strip whitespace and anything that could act as a path separator.
fn sanitize_yenc_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect::<String>()
}

/// True if a file name gives no human-readable information — i.e. it's a
/// poster-generated obfuscation token with no extractable read name.
///
/// Detects two shapes:
/// - the classic bare **all-hex** blob of ≥ 16 chars, and
/// - a **long bare alphanumeric token** with no extension / separator
///   (e.g. `0kfagna8bx9e9x5ux2un9kh`) — many obfuscators use arbitrary
///   mixed-case alphanumerics rather than hex.
///
/// Both mean the NZB subject and the yEnc header carry no usable real name,
/// so the file must be renamed to the release name (with a sniffed
/// extension) after download.
fn is_obfuscated_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let lower = stem.to_ascii_lowercase();
    if stem.len() >= 16 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    // A long token made only of letters/digits (no dot, space or separator)
    // carries no readable structure — treat as obfuscation.
    stem.len() >= 12 && stem.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Sniff a file extension from the first bytes of a file's content.
fn sniff_ext(head: &[u8]) -> Option<&'static str> {
    let ext = if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        "mkv"
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        "mp4"
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"AVI " {
        "avi"
    } else if head.starts_with(b"OggS") {
        "ogv"
    } else if head.starts_with(b"Rar!\x1a\x07") {
        "rar"
    } else if head.starts_with(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]) {
        "7z"
    } else if head.starts_with(b"PK\x03\x04") {
        "zip"
    } else if head.starts_with(b"PAR2") {
        "par2"
    } else {
        return None;
    };
    Some(ext)
}

/// Build a recognizable name for an obfuscated file from the job (release)
/// name, with a content-sniffed extension. Multi-file jobs get a numeric
/// suffix so each file stays distinguishable.
fn obfuscated_final_name(
    job_name: &str,
    file_index: u32,
    file_count: u32,
    ext: Option<&str>,
) -> String {
    let stem = sanitize_yenc_name(job_name);
    let stem = if stem.is_empty() {
        format!("file{:03}", file_index + 1)
    } else {
        stem
    };
    let mut name = if file_count > 1 {
        format!("{stem}.{file_index:03}")
    } else {
        stem.clone()
    };
    if let Some(ext) = ext {
        name.push('.');
        name.push_str(ext);
    }
    name
}

/// Avoid clobbering an existing file from a previous run.
async fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        let mut n = 2;
        loop {
            let alt = dir.join(format!("{name}.{n}"));
            if !tokio::fs::try_exists(&alt).await.unwrap_or(false) {
                return alt;
            }
            n += 1;
        }
    }
    candidate
}

/// Assemble a file from per-segment part files. Reads all segments from
/// the DB, concatenates the done parts in order, and writes the final
/// file. Also calls `refresh_job_counts` to update aggregate counters.
///
/// Files whose name is obfuscated (bare hash in both subject and yEnc
/// header) are renamed to the job's release name, with the extension
/// sniffed from the assembled content and a numeric suffix for multi-file
/// jobs — the article data itself carries no readable name.
async fn finalize_file(
    queue: &Arc<QueueManager>,
    file: &QueueFile,
    output_dir: &Path,
    job_name: &str,
    file_count: u32,
    writer_paths: &std::collections::HashMap<i64, PathBuf>,
    _tx: &mpsc::UnboundedSender<ProgressEvent>,
) -> Result<FileOutcome> {
    // Reload the file so a real name latched during the download (from the
    // yEnc headers) is picked up even though we were handed a snapshot.
    let file = queue.get_file(file.id).await?;

    // Refresh job-level aggregate counters now that all segments for this
    // file are done.
    if let Err(e) = queue.refresh_job_counts(file.id).await {
        tracing::warn!(error = %e, "failed to refresh job counts");
    }

    let base_name = file
        .yenc_name
        .clone()
        .unwrap_or_else(|| file.filename.clone());
    let obfuscated = is_obfuscated_name(&base_name);

    // The direct-write path wrote every segment into a single stable file:
    // either directly at `file.filename`, or at a `.partial` temp path for
    // content-sniff (obfuscated subject) files. Finalize renames it *once*
    // if the real name differs; otherwise it's already at its final path.
    let on_disk = writer_paths
        .get(&file.id)
        .cloned()
        .unwrap_or_else(|| write_target_for(file.id, output_dir, &file.filename));
    if !tokio::fs::try_exists(&on_disk).await.unwrap_or(false) {
        tokio::fs::write(&on_disk, [])
            .await
            .map_err(CoreError::from)?;
    }

    // Count missing / CRC-mismatched segments from the DB (segment state),
    // not from the disk — the single output file may have sparse holes
    // where segments are missing/corrupt.
    let all_segments = queue.list_segments(file.id).await?;
    let mut missing = 0u32;
    let mut crc_mismatches = 0u32;
    for n in 1..=file.segment_count {
        match all_segments.iter().find(|s| s.number == n) {
            Some(s) if s.state == SegmentState::Missing || s.missing => missing += 1,
            Some(s) if s.state == SegmentState::CrcMismatch => crc_mismatches += 1,
            Some(s) if s.state == SegmentState::Failed => missing += 1,
            _ => {}
        }
    }

    let final_path = if obfuscated {
        // Sniff the content for a real extension and pick a unique
        // release-based name before renaming once.
        use tokio::io::AsyncReadExt;
        let mut fh = tokio::fs::File::open(&on_disk)
            .await
            .map_err(CoreError::from)?;
        let mut head = [0u8; 16];
        let n = fh.read(&mut head).await.map_err(CoreError::from)?;
        drop(fh);
        let ext = sniff_ext(&head[..n]);
        let name = obfuscated_final_name(job_name, file.file_index, file_count, ext);
        let dest = unique_path(output_dir, &name).await;
        if dest != on_disk {
            tokio::fs::rename(&on_disk, &dest)
                .await
                .map_err(CoreError::from)?;
        }
        dest
    } else {
        let mut dest = output_dir.join(&base_name);
        if dest != on_disk {
            // The file was written under its subject name or `.partial` but
            // the yEnc header revealed a better (real) name — rename once.
            if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
                let meta = tokio::fs::metadata(&dest).await;
                if meta.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
                    // The intended name is occupied by a directory — pick a
                    // unique file name rather than failing the whole file.
                    dest = unique_path(output_dir, &base_name).await;
                } else {
                    tokio::fs::remove_file(&dest)
                        .await
                        .map_err(CoreError::from)?;
                }
            }
            if dest != on_disk {
                tokio::fs::rename(&on_disk, &dest)
                    .await
                    .map_err(CoreError::from)?;
            }
        }
        dest
    };

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

/// Log the aggregated perf counters for a run, including achieved MB/s and the
/// per-article fetch-time buckets (reveals provider throttle vs client gaps).
fn log_perf_summary(
    stats: &PerfStats,
    t0: std::time::Instant,
    start_articles: u64,
    start_bytes: u64,
    start_conn_created: u64,
    start_conn_dropped: u64,
    start_fetch_buckets: &[u64; 5],
) {
    let articles = stats
        .articles
        .load(Ordering::Relaxed)
        .saturating_sub(start_articles);
    let bytes = stats
        .bytes
        .load(Ordering::Relaxed)
        .saturating_sub(start_bytes);
    let conn_created = stats
        .conn_created
        .load(Ordering::Relaxed)
        .saturating_sub(start_conn_created);
    let conn_dropped = stats
        .conn_dropped
        .load(Ordering::Relaxed)
        .saturating_sub(start_conn_dropped);
    let wall = t0.elapsed().as_secs_f64();
    let mbps = if wall > 0.0 {
        bytes as f64 / 1024.0 / 1024.0 / wall
    } else {
        0.0
    };
    let avg = |x: u64| {
        if articles == 0 {
            0.0
        } else {
            x as f64 / articles as f64
        }
    };
    tracing::info!(
        articles,
        bytes,
        elapsed_s = wall,
        throughput_mbps = mbps,
        avg_queue_us = avg(stats.queue_wait_us.load(Ordering::Relaxed)),
        avg_acquire_us = avg(stats.acquire_us.load(Ordering::Relaxed)),
        avg_fetch_us = avg(stats.fetch_us.load(Ordering::Relaxed)),
        avg_decode_us = avg(stats.decode_us.load(Ordering::Relaxed)),
        avg_write_us = avg(stats.write_us.load(Ordering::Relaxed)),
        conn_created,
        conn_dropped,
        fetch_bucket_le20ms = stats
            .fetch_le_20ms
            .load(Ordering::Relaxed)
            .saturating_sub(start_fetch_buckets[0]),
        fetch_bucket_20_100ms = stats
            .fetch_le_100ms
            .load(Ordering::Relaxed)
            .saturating_sub(start_fetch_buckets[1]),
        fetch_bucket_100_500ms = stats
            .fetch_le_500ms
            .load(Ordering::Relaxed)
            .saturating_sub(start_fetch_buckets[2]),
        fetch_bucket_500_2000ms = stats
            .fetch_le_2000ms
            .load(Ordering::Relaxed)
            .saturating_sub(start_fetch_buckets[3]),
        fetch_bucket_gt2s = stats
            .fetch_gt_2000ms
            .load(Ordering::Relaxed)
            .saturating_sub(start_fetch_buckets[4]),
        "engine perf summary"
    );
}

// Re-export mpsc for the public API.
pub use tokio::sync::mpsc;

/// Per-file direct-write sender registry: file id → its writer-task channel.
type OutputWriterMap = std::collections::HashMap<i64, mpsc::UnboundedSender<WriteJob>>;
/// Per-file writer task handles (waited on after workers drain).
type WriterTaskMap = std::collections::HashMap<i64, tokio::task::JoinHandle<Result<PathBuf>>>;
