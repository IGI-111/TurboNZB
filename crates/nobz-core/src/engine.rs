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
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::error::{CoreError, Result};
use crate::nntp::{NntpClient, ServerConfig, StatResult};
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
        let mut workers: JoinSet<Result<()>> = JoinSet::new();
        for worker_id in 0..self.max_connections {
            let engine = Arc::clone(&self);
            let queue = Arc::clone(&queue);
            let pool = Arc::clone(&pool);
            let work_queue = Arc::clone(&work_queue);
            let files = Arc::clone(&files);
            let stats = Arc::clone(&stats);
            let seg_attempts = Arc::clone(&seg_attempts);
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
        // RUST_LOG=nobz_core=info).
        {
            let articles = stats.articles.load(Ordering::Relaxed);
            let bytes = stats.bytes.load(Ordering::Relaxed);
            let wall = t0.elapsed().as_secs_f64();
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
                avg_queue_us = avg(stats.queue_wait_us.load(Ordering::Relaxed)),
                avg_acquire_us = avg(stats.acquire_us.load(Ordering::Relaxed)),
                avg_fetch_us = avg(stats.fetch_us.load(Ordering::Relaxed)),
                avg_decode_us = avg(stats.decode_us.load(Ordering::Relaxed)),
                avg_write_us = avg(stats.write_us.load(Ordering::Relaxed)),
                conn_created = stats.conn_created.load(Ordering::Relaxed),
                conn_dropped = stats.conn_dropped.load(Ordering::Relaxed),
                fetch_bucket_le20ms = stats.fetch_le_20ms.load(Ordering::Relaxed),
                fetch_bucket_20_100ms = stats.fetch_le_100ms.load(Ordering::Relaxed),
                fetch_bucket_100_500ms = stats.fetch_le_500ms.load(Ordering::Relaxed),
                fetch_bucket_500_2000ms = stats.fetch_le_2000ms.load(Ordering::Relaxed),
                fetch_bucket_gt2s = stats.fetch_gt_2000ms.load(Ordering::Relaxed),
                "engine final perf summary"
            );
        }

        // NOTE: pool is intentionally kept alive across jobs — connection
        // establishment is the throttled, expensive part, so we hold.

        Ok(())
    }

    /// A worker loop: continuously pops segments from the shared queue
    /// and fetches them using pooled connections until the queue is empty.
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
        output_dir: &Path,
        tx: &mpsc::UnboundedSender<ProgressEvent>,
        state_tx: &mpsc::UnboundedSender<(i64, u32, SegmentState)>,
    ) -> Result<()> {
        // Per-worker cache of parts directories (no lock needed — each
        // worker has its own HashMap). Most downloads have many segments
        // per file, so after the first segment the dir is already cached.
        // Keyed by file id so a real-name discovery (see below) never
        // strands an already-created parts dir under the old name.
        let mut parts_dirs: std::collections::HashMap<i64, PathBuf> =
            std::collections::HashMap::new();
        // Files whose real name (from `=ybegin name=`) we've already
        // persisted, to avoid re-writing the DB on every segment.
        let mut name_latched: std::collections::HashSet<i64> = std::collections::HashSet::new();

        loop {
            // Pop the next segment (timed — high average = workers
            // fighting for work).
            let (file_idx, seg) = {
                let t = std::time::Instant::now();
                let mut wq = work_queue.lock().await;
                let item = wq.pop_front();
                stats
                    .queue_wait_us
                    .fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                match item {
                    Some(item) => item,
                    None => break, // Queue empty — worker is done.
                }
            };
            let file = &files[file_idx];

            // Bounded retry: count attempts per segment; a server that is
            // permanently unreachable must eventually fail the job rather
            // than retrying forever (which burns CPU and floods logs).
            let attempts = {
                let mut map = seg_attempts.lock().await;
                let e = map.entry(seg.id).or_insert(0);
                *e += 1;
                *e
            };
            if attempts > MAX_SEGMENT_ATTEMPTS {
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
                continue;
            }

            // Ensure the parts directory exists for this file.
            let parts_dir = match parts_dirs.get(&file.id) {
                Some(p) => p.clone(),
                None => {
                    let p = output_dir.join(format!("{}.parts", file.filename));
                    tokio::fs::create_dir_all(&p)
                        .await
                        .map_err(CoreError::from)?;
                    parts_dirs.insert(file.id, p.clone());
                    p
                }
            };

            let t_fetch = std::time::Instant::now();
            let outcome = pool_fetch(pool, &self.servers, &seg.message_id, stats).await;
            let fetch_us = t_fetch.elapsed().as_micros() as u64;
            stats.fetch_us.fetch_add(fetch_us, Ordering::Relaxed);
            stats.bucket(fetch_us);

            let outcome = match outcome {
                ArticleOutcome::Retry => {
                    // Connection problem — put the segment back for another
                    // worker (never mark it failed on a transient issue),
                    // with a short backoff so a dead server doesn't cause
                    // a hot retry loop.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    let mut wq = work_queue.lock().await;
                    wq.push_front((file_idx, seg));
                    continue;
                }
                o => o,
            };

            let (seg_state, bytes) = match outcome {
                ArticleOutcome::OkBytes(body) => {
                    let t_decode = std::time::Instant::now();
                    let state = match yenc::decode_article(&body) {
                        Ok(decoded) => {
                            stats.decode_us.fetch_add(
                                t_decode.elapsed().as_micros() as u64,
                                Ordering::Relaxed,
                            );
                            let seg_state = if decoded.crc_ok || decoded.crc_unknown {
                                SegmentState::Done
                            } else {
                                SegmentState::CrcMismatch
                            };
                            if seg_state == SegmentState::Done {
                                // Obfuscated posts put a hash in the subject
                                // but the real filename in `=ybegin name=`.
                                // Latch the real name (once per file) so the
                                // assembled file is identifiable on disk.
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
                                let part_path = parts_dir.join(format!("seg{:06}", seg.number));
                                let t_write = std::time::Instant::now();
                                tokio::fs::write(&part_path, &decoded.data)
                                    .await
                                    .map_err(CoreError::from)?;
                                stats.write_us.fetch_add(
                                    t_write.elapsed().as_micros() as u64,
                                    Ordering::Relaxed,
                                );
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
                    };
                    // Report the *declared* size of the segment (from the
                    // NZB), not the raw yEnc body length. The body's on-wire
                    // size includes transport overhead (headers, CRLFs,
                    // dot-stuffing), which made live progress overshoot the
                    // job's total before snapping back at completion.
                    (state, seg.bytes)
                }
                ArticleOutcome::Missing => (SegmentState::Missing, 0),
                ArticleOutcome::Retry => unreachable!(),
            };

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
    /// Performance counters (connection create/drop).
    stats: Arc<PerfStats>,
}

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
        // Deque empty but we hold a slot — create a new connection.
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
enum ArticleOutcome {
    /// Article fetched successfully (raw yEnc bytes, dot-unstuffed).
    OkBytes(Vec<u8>),
    /// Article is genuinely missing (430/423) — mark it Missing.
    Missing,
    /// A connection-level problem — caller should retry later (re-queue).
    Retry,
}

/// Fetch one article using the gated connection pool, trying servers in
/// priority order.
///
/// - On success the connection is returned to the pool for reuse.
/// - An article missing (430/423) on every server → `Missing`.
/// - Any connection break (mid-read) → `Retry` so the segment is
///   re-queued rather than failed; the broken connection is dropped.
async fn pool_fetch(
    pool: &Arc<ConnectionPool>,
    servers: &[ServerConfig],
    message_id: &str,
    stats: &PerfStats,
) -> ArticleOutcome {
    let mut saw_missing = false;
    let mut saw_conn_error = false;

    for (idx, _server) in servers.iter().enumerate() {
        let t_acquire = std::time::Instant::now();
        let (mut client, permit) = match pool.get(idx, servers).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(server_idx = idx, error = %e, "connect failed; trying next");
                continue;
            }
        };
        stats
            .acquire_us
            .fetch_add(t_acquire.elapsed().as_micros() as u64, Ordering::Relaxed);

        match client.body(message_id).await {
            Ok(Ok(body)) => {
                pool.put(idx, client, permit).await;
                return ArticleOutcome::OkBytes(body.bytes);
            }
            Ok(Err(StatResult::Missing)) => {
                // Not on this server — return the connection, try the next.
                pool.put(idx, client, permit).await;
                saw_missing = true;
            }
            Ok(Err(StatResult::Present)) => {
                // BODY never returns Present; protocol oddity. Try next.
                pool.put(idx, client, permit).await;
                saw_missing = true;
            }
            Err(e) => {
                // Connection error — drop it, try next server. Next fetch
                // creates a fresh one (under the gate's slot count).
                tracing::warn!(server_idx = idx, error = %e, "BODY error; dropping connection");
                pool.drop_connection(permit);
                saw_conn_error = true;
            }
        }
    }

    if saw_missing && !saw_conn_error {
        ArticleOutcome::Missing
    } else {
        ArticleOutcome::Retry
    }
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

/// Assemble a file from per-segment part files. Reads all segments from
/// the DB, concatenates the done parts in order, and writes the final
/// file. Also calls `refresh_job_counts` to update aggregate counters.
async fn assemble_file(
    queue: &Arc<QueueManager>,
    file: &QueueFile,
    output_dir: &Path,
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

    let filename = file
        .yenc_name
        .clone()
        .unwrap_or_else(|| file.filename.clone());
    let final_path = output_dir.join(&filename);
    let parts_dir = output_dir.join(format!("{}.parts", file.filename));

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
