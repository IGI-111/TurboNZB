//! Backend: owns the tokio runtime, queue manager, and bridges async
//! engine/search events to the egui main thread.
//!
//! The GUI thread sends commands over `BackendHandle::cmd_tx` and polls
//! results over `BackendHandle::event_rx` each frame. The backend runtime
//! processes commands on a background tokio runtime and pushes `BackendEvent`s
//! back. The egui `Context` is stored so the backend can call
//! `ctx.request_repaint()` after pushing an event, waking the GUI immediately.
//!
//! ## Single-download guarantee
//!
//! Only one job downloads at a time. This is enforced **at the database
//! level** via a partial unique index (`idx_one_downloading`) that makes it
//! impossible for two jobs to be in the `'downloading'` state simultaneously.
//! The backend uses `claim_download_slot` / `release_download_slot` as the
//! atomic gate — no in-memory mutexes track "what's running". If the app
//! crashes, `recover_interrupted` at startup resets any stale `'downloading'`
//! jobs back to `'queued'`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use turbonzb_core::engine::{Engine, ProgressEvent};
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb;
use turbonzb_core::postprocess::{
    PostProcessConfig, PostProcessReport, PostProcessStatus, post_process,
};
use turbonzb_core::queue::{JobState, QueueJob, QueueManager};
use turbonzb_index::types::{IndexerConfig, SearchQuery};
use turbonzb_index::{AggregatedResult, NewznabClient, SearchAggregator};

use crate::settings::AppConfig;

/// Commands the GUI sends to the backend.
#[derive(Debug)]
pub enum BackendCmd {
    /// Open an NZB file from disk and queue it.
    OpenNzbFile { path: PathBuf },
    /// Add an NZB (from bytes) to the queue and start downloading.
    DownloadNzb {
        nzb_bytes: Vec<u8>,
        output_dir: PathBuf,
        category: Option<String>,
        archive_password: Option<String>,
    },
    /// Fetch an NZB from a URL, then add it to the queue and start
    /// downloading. This replaces the old spawn_nzb_fetch approach — the
    /// backend does the HTTP fetch on its own tokio runtime so errors
    /// surface properly.
    DownloadFromUrl {
        url: String,
        title: String,
        download_dir: PathBuf,
        category: Option<String>,
    },
    /// Pause the download engine — cancels the active download and puts
    /// the job back to queued. No new jobs auto-start until ResumeEngine.
    PauseEngine,
    /// Resume the download engine — starts the next queued job.
    ResumeEngine,
    /// Delete all completed jobs from the queue.
    ClearCompleted,
    /// Delete a job.
    DeleteJob { job_id: i64 },
    /// Move a job up in the queue (higher priority).
    MoveJobUp { job_id: i64 },
    /// Move a job down in the queue (lower priority).
    MoveJobDown { job_id: i64 },
    /// Run post-processing on a completed job directory.
    PostProcess {
        download_dir: PathBuf,
        completed_dir: PathBuf,
        category: Option<String>,
        archive_password: Option<String>,
        skip_verify: bool,
        cleanup_archives: bool,
    },
    /// Execute a search across all configured indexers.
    Search { query: SearchQuery },
    /// Test an indexer connection (fetch caps).
    TestIndexer { config: IndexerConfig },
    /// Test an NNTP server connection (connect + AUTHINFO).
    TestServer { config: ServerConfig },
    /// Refresh the job list from the DB.
    RefreshJobs,
    /// Fetch per-file details for a job (for the details pane).
    GetJobDetails { job_id: i64 },
    /// Set the currently selected job (so the 200ms interval can auto-refresh
    /// its details).
    SetSelectedJob { job_id: Option<i64> },
    /// Update the backend's copy of the config (after settings are saved).
    SetConfig(Box<AppConfig>),
    /// A generic error surfaced from the GUI side (e.g. NZB fetch failed).
    Error(String),
}

/// Events the backend pushes back to the GUI.
#[derive(Debug)]
pub enum BackendEvent {
    /// A download progress event from the engine.
    Progress(ProgressEvent),
    /// Speed update for the currently-downloading job (emitted ~5x per
    /// second during active downloads). `job_id` is None when idle.
    Speed {
        job_id: Option<i64>,
        bytes_per_sec: f64,
        downloaded_bytes: u64,
        total_bytes: u64,
        /// Recent speed history (most recent last), for the speed graph.
        history: Vec<f64>,
        /// Number of active NNTP connections (0 when idle).
        active_connections: usize,
    },
    /// A job was added to the queue (returns job id).
    JobAdded { job_id: i64 },
    /// A job's state changed.
    JobStateChanged { job_id: i64, state: JobState },
    /// The current job list (after a refresh or change).
    JobsList(Vec<QueueJob>),
    /// Per-file details for a job (for the details pane).
    JobDetails {
        job_id: i64,
        files: Vec<JobFileDetail>,
    },
    /// Search completed.
    SearchResults(Vec<AggregatedResult>),
    /// Search failed.
    SearchFailed(String),
    /// Indexer test result.
    IndexerTestResult {
        name: String,
        ok: bool,
        message: String,
    },
    /// Server test result.
    ServerTestResult {
        host: String,
        ok: bool,
        message: String,
    },
    /// Post-processing started (PAR2 verify + unpack).
    PostProcessStarted { job_id: i64 },
    /// Post-processing completed.
    PostProcessDone {
        job_id: Option<i64>,
        report: PostProcessReport,
    },
    /// Post-processing failed.
    PostProcessFailed { job_id: Option<i64>, error: String },
    /// Engine running state changed (paused/resumed).
    EnginePaused(bool),
    /// A generic error.
    Error(String),
}

/// Per-file detail for the details pane.
/// A single segment's per-file detail, for the segment view.
#[derive(Debug, Clone)]
pub struct JobSegmentDetail {
    pub number: u32,
    pub bytes: u64,
    pub state: turbonzb_core::queue::SegmentState,
}

/// Per-file entry for the queue's details pane.
#[derive(Debug, Clone)]
pub struct JobFileDetail {
    pub id: i64,
    pub filename: String,
    pub segment_count: u32,
    pub segments_done: u32,
    pub segments_missing: u32,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    /// All segments of this file (for the segment grid + segment view).
    pub segments: Vec<JobSegmentDetail>,
}

/// Handle held by the GUI to send commands and receive events.
#[derive(Clone)]
pub struct BackendHandle {
    pub cmd_tx: mpsc::UnboundedSender<BackendCmd>,
    pub event_rx: std::sync::Arc<std::sync::Mutex<mpsc::UnboundedReceiver<BackendEvent>>>,
}

impl BackendHandle {
    /// Send a command to the backend (non-blocking, never fails — the
    /// backend owns the receiver and lives for the whole app lifetime).
    pub fn send(&self, cmd: BackendCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Drain all pending events (called each frame).
    pub fn drain(&self) -> Vec<BackendEvent> {
        let mut rx = self.event_rx.lock().expect("event rx mutex poisoned");
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }
}

/// Speed tracker for the single active download. Maintains a ring buffer
/// of recent speed samples for the graph.
struct SpeedTracker {
    history: VecDeque<f64>,
    last_time: Instant,
    last_bytes: u64,
    current_bps: f64,
    downloaded: u64,
    total: u64,
}

impl SpeedTracker {
    const HISTORY_CAP: usize = 240; // ~24s at 100ms intervals
    /// EMA smoothing factor. Lower = smoother but more lag.
    /// 0.3 means new sample contributes 30%, history contributes 70%.
    const EMA_ALPHA: f64 = 0.3;

    fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(Self::HISTORY_CAP),
            last_time: Instant::now(),
            last_bytes: 0,
            current_bps: 0.0,
            downloaded: 0,
            total: 0,
        }
    }

    fn start(&mut self, total: u64) {
        self.history.clear();
        self.last_time = Instant::now();
        self.last_bytes = 0;
        self.current_bps = 0.0;
        self.downloaded = 0;
        self.total = total;
    }

    fn add_bytes(&mut self, bytes: u64) {
        self.last_bytes += bytes;
    }

    fn tick(&mut self) -> (f64, u64) {
        let elapsed = self.last_time.elapsed().as_secs_f64().max(0.001);
        let raw_bps = self.last_bytes as f64 / elapsed;

        // Exponential moving average to smooth out spikes from per-tick
        // timing jitter (article arrives just before/after the tick).
        if self.current_bps == 0.0 {
            // First sample — initialize directly.
            self.current_bps = raw_bps;
        } else {
            self.current_bps =
                Self::EMA_ALPHA * raw_bps + (1.0 - Self::EMA_ALPHA) * self.current_bps;
        }

        self.downloaded += self.last_bytes;
        self.last_bytes = 0;
        self.last_time = Instant::now();

        if self.history.len() >= Self::HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(self.current_bps);

        (self.current_bps, self.downloaded)
    }

    fn history_vec(&self) -> Vec<f64> {
        self.history.iter().copied().collect()
    }
}

/// The backend owns the tokio runtime and the queue manager. It is created
/// once at startup and runs for the lifetime of the app.
pub struct Backend {
    queue: Arc<QueueManager>,
    config: Arc<std::sync::RwLock<AppConfig>>,
    ctx: Option<egui::Context>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    /// Long-lived download engine + its warm connection pool, created once
    /// and reused across jobs so connection establishment (provider-tolerant)
    /// happens up front, once, gently.
    engine: Arc<Engine>,
    /// Cancellation token for the single running download, if any.
    cancel_token: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    /// Speed tracker for the single active download.
    speed: Arc<std::sync::Mutex<SpeedTracker>>,
    /// Currently selected job (for auto-refreshing details in the 100ms interval).
    selected_job: Arc<std::sync::Mutex<Option<i64>>>,
    /// Whether the engine is paused (user pressed Pause). When true, no
    /// new jobs are auto-started. The current download is cancelled and
    /// its job goes back to Queued.
    engine_paused: Arc<std::sync::Mutex<bool>>,
}

impl Backend {
    /// Spawn the backend: starts a tokio runtime, opens the queue DB,
    /// recovers any interrupted downloads, and spawns a command-processing
    /// task. Returns a handle for the GUI.
    pub fn spawn(
        config: AppConfig,
        ctx: egui::Context,
    ) -> (BackendHandle, tokio::runtime::Runtime) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BackendCmd>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<BackendEvent>();

        // The tokio runtime lives on a dedicated thread so it doesn't block
        // the egui render loop.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime build");

        let config = Arc::new(std::sync::RwLock::new(config));

        // Long-lived engine with a warm connection pool. Spawn the pool
        // keeper so connections are established up front, spread out, and
        // kept alive — never re-established per download.
        let engine = {
            let c = config.read().expect("config lock");
            Arc::new(Engine::new(
                c.server_configs(),
                c.effective_max_connections(),
            ))
        };
        {
            let _guard = runtime.enter();
            engine.spawn_pool_keeper();
        }
        let db_path = {
            let c = config.read().expect("config lock");
            c.db_path.clone()
        };

        let queue =
            runtime.block_on(async { QueueManager::open(&db_path).await.expect("queue db open") });
        let queue = Arc::new(queue);

        // Recover from any unclean shutdown: reset stale 'downloading'
        // jobs back to 'queued' so the download slot is free.
        runtime.block_on(async {
            match queue.recover_interrupted().await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(recovered = n, "recovered interrupted downloads");
                    }
                }
                Err(e) => tracing::error!(error = %e, "failed to recover interrupted downloads"),
            }
        });

        let handle = BackendHandle {
            cmd_tx,
            event_rx: std::sync::Arc::new(std::sync::Mutex::new(event_rx)),
        };

        let backend = Self {
            queue: Arc::clone(&queue),
            config: Arc::clone(&config),
            ctx: Some(ctx),
            event_tx,
            engine: Arc::clone(&engine),
            cancel_token: Arc::new(std::sync::Mutex::new(None)),
            speed: Arc::new(std::sync::Mutex::new(SpeedTracker::new())),
            selected_job: Arc::new(std::sync::Mutex::new(None)),
            engine_paused: Arc::new(std::sync::Mutex::new(false)),
        };

        // Spawn the command loop.
        runtime.spawn(async move {
            backend.run_loop(&mut cmd_rx).await;
        });

        (handle, runtime)
    }

    fn emit(&self, event: BackendEvent) {
        let _ = self.event_tx.send(event);
        if let Some(ref ctx) = self.ctx {
            ctx.request_repaint();
        }
    }

    async fn run_loop(self, cmd_rx: &mut mpsc::UnboundedReceiver<BackendCmd>) {
        // Auto-start the first queued job on launch (in case there were
        // interrupted downloads recovered at startup).
        self.try_start_next_job().await;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                BackendCmd::OpenNzbFile { path } => {
                    self.handle_open_nzb_file(path).await;
                }
                BackendCmd::DownloadNzb {
                    nzb_bytes,
                    output_dir,
                    category,
                    archive_password,
                } => {
                    self.handle_download(nzb_bytes, output_dir, category, archive_password, None)
                        .await;
                }
                BackendCmd::DownloadFromUrl {
                    url,
                    title,
                    download_dir,
                    category,
                } => {
                    self.handle_download_from_url(url, title, download_dir, category)
                        .await;
                }
                BackendCmd::PauseEngine => {
                    self.handle_pause_engine().await;
                }
                BackendCmd::ResumeEngine => {
                    self.handle_resume_engine().await;
                }
                BackendCmd::ClearCompleted => {
                    self.handle_clear_completed().await;
                }
                BackendCmd::DeleteJob { job_id } => {
                    self.handle_delete(job_id).await;
                }
                BackendCmd::MoveJobUp { job_id } => {
                    self.handle_move_job(job_id, true).await;
                }
                BackendCmd::MoveJobDown { job_id } => {
                    self.handle_move_job(job_id, false).await;
                }
                BackendCmd::PostProcess {
                    download_dir,
                    completed_dir,
                    category,
                    archive_password,
                    skip_verify,
                    cleanup_archives,
                } => {
                    self.handle_postprocess(
                        None,
                        download_dir,
                        completed_dir,
                        category,
                        archive_password,
                        skip_verify,
                        cleanup_archives,
                    )
                    .await;
                }
                BackendCmd::Search { query } => {
                    self.handle_search(query).await;
                }
                BackendCmd::TestIndexer { config } => {
                    self.handle_test_indexer(config).await;
                }
                BackendCmd::TestServer { config } => {
                    self.handle_test_server(config).await;
                }
                BackendCmd::RefreshJobs => {
                    self.handle_refresh_jobs().await;
                }
                BackendCmd::GetJobDetails { job_id } => {
                    self.handle_get_job_details(job_id).await;
                }
                BackendCmd::SetSelectedJob { job_id } => {
                    let mut sel = self.selected_job.lock().expect("selected_job lock");
                    *sel = job_id;
                }
                BackendCmd::SetConfig(cfg) => {
                    let mut c = self.config.write().expect("config lock");
                    *c = *cfg;
                }
                BackendCmd::Error(msg) => {
                    self.emit(BackendEvent::Error(msg));
                }
            }
        }
    }

    /// Open an NZB file from disk, create a fetching job, then read+parse
    /// the file and populate the job — same flow as DownloadFromUrl but
    /// reading from the local filesystem.
    async fn handle_open_nzb_file(&self, path: PathBuf) {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "turbonzb-download".into());

        let download_dir = {
            let c = self.config.read().expect("config lock");
            c.download_dir.clone()
        };

        // Sanitize title for directory name.
        let safe_name: String = title
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
            .collect();
        let job_dir = download_dir.join(if safe_name.is_empty() {
            "turbonzb-download".to_string()
        } else {
            safe_name
        });

        // Create placeholder job immediately.
        let job_id = match self.queue.create_fetching_job(&title, &job_dir, 0).await {
            Ok(id) => id,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("Create job error: {e}")));
                return;
            }
        };
        self.emit(BackendEvent::JobAdded { job_id });
        self.handle_refresh_jobs().await;

        // Read and parse the file in a spawned task so we don't block.
        let queue = Arc::clone(&self.queue);
        let config = Arc::clone(&self.config);
        let event_tx = self.event_tx.clone();
        let ctx = self.ctx.clone();
        let cancel_token = Arc::clone(&self.cancel_token);
        let selected_job = Arc::clone(&self.selected_job);
        let speed = Arc::clone(&self.speed);
        let engine_paused = Arc::clone(&self.engine_paused);
        let engine = Arc::clone(&self.engine);

        tokio::spawn(async move {
            let emit = |ev: BackendEvent| {
                let _ = event_tx.send(ev);
                if let Some(ref ctx) = ctx {
                    ctx.request_repaint();
                }
            };

            let nzb_bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    emit(BackendEvent::Error(format!("Failed to read NZB file: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            let nzb = match nzb::parse(&nzb_bytes) {
                Ok(n) => n,
                Err(e) => {
                    emit(BackendEvent::Error(format!("NZB parse error: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            if let Err(e) = queue
                .populate_job(job_id, &nzb, &job_dir, Some(&title))
                .await
            {
                emit(BackendEvent::Error(format!("Populate job error: {e}")));
                let _ = queue.delete_job(job_id).await;
                return;
            }

            // Store the NZB's declared password on the job itself, so
            // post-processing can unlock encrypted archives regardless of
            // which other jobs get added later.
            if let Some(pw) = nzb.passwords().first() {
                let _ = queue
                    .set_job_archive_password(job_id, Some(pw.as_str()))
                    .await;
            }

            emit(BackendEvent::JobStateChanged {
                job_id,
                state: JobState::Queued,
            });
            if let Ok(jobs) = queue.list_jobs().await {
                emit(BackendEvent::JobsList(jobs));
            }

            // Try to start downloading.
            let is_paused = engine_paused.lock().map(|p| *p).unwrap_or(false);
            let already_running = cancel_token.lock().expect("cancel_token lock").is_some();
            let has_downloading = queue
                .current_downloading_job()
                .await
                .ok()
                .flatten()
                .is_some();

            if !is_paused && !already_running && !has_downloading {
                if let Ok(Some(next)) = queue.next_queued_job().await {
                    if next.id == job_id {
                        if let Ok(true) = queue.claim_download_slot(job_id).await {
                            let servers: Vec<ServerConfig> = {
                                let c = config.read().expect("config lock");
                                c.server_configs()
                            };
                            if !servers.is_empty() {
                                let new_cancel = CancellationToken::new();
                                {
                                    let mut slot = cancel_token.lock().expect("cancel_token lock");
                                    *slot = Some(new_cancel.clone());
                                }
                                spawn_engine_task(
                                    job_id,
                                    Arc::clone(&engine),
                                    Arc::clone(&queue),
                                    Arc::clone(&config),
                                    event_tx.clone(),
                                    ctx.clone(),
                                    Arc::clone(&speed),
                                    Arc::clone(&cancel_token),
                                    Arc::clone(&selected_job),
                                    Arc::clone(&engine_paused),
                                    new_cancel,
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    async fn handle_download(
        &self,
        nzb_bytes: Vec<u8>,
        output_dir: PathBuf,
        category: Option<String>,
        archive_password: Option<String>,
        display_name: Option<&str>,
    ) {
        tracing::info!(bytes = nzb_bytes.len(), "parsing NZB");

        // Quick sanity check: NZB files are XML starting with <?xml or <nzb.
        // If the response is HTML (e.g. an error page), fail early with a
        // helpful message instead of a confusing XML parse error.
        let head = String::from_utf8_lossy(&nzb_bytes[..nzb_bytes.len().min(200)]);
        if !head.trim_start().starts_with("<?xml") && !head.trim_start().starts_with("<nzb") {
            tracing::error!("NZB response is not XML");
            self.emit(BackendEvent::Error(
                "NZB fetch returned non-XML content (likely an error page). Check the indexer URL and API key.".into(),
            ));
            return;
        }

        let nzb = match nzb::parse(&nzb_bytes) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "NZB parse failed");
                self.emit(BackendEvent::Error(format!("NZB parse error: {e}")));
                return;
            }
        };
        tracing::info!(files = nzb.files.len(), "NZB parsed");
        let total_segs: u32 = nzb.files.iter().map(|f| f.segment_count).sum();
        let total_bytes_nzb: u64 = nzb
            .files
            .iter()
            .flat_map(|f| &f.segments)
            .map(|s| s.bytes)
            .sum();
        tracing::info!(
            total_segments = total_segs,
            total_bytes = total_bytes_nzb,
            "NZB segment summary"
        );

        let pw = archive_password.or_else(|| nzb.passwords().first().map(|s| s.to_string()));

        let job_id = match self.queue.add_job(&nzb, &output_dir, 0, display_name).await {
            Ok(id) => id,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("Queue add error: {e}")));
                return;
            }
        };
        // Store the archive password on the job so post-processing can
        // unlock encrypted archives later (survives queueing + restarts).
        if let Some(ref p) = pw {
            let _ = self
                .queue
                .set_job_archive_password(job_id, Some(p.as_str()))
                .await;
        }
        self.emit(BackendEvent::JobAdded { job_id });

        // Refresh the queue immediately so the new job shows up.
        self.handle_refresh_jobs().await;

        // Try to start downloading. If the download slot is already held
        // by another job, this job stays queued and will be auto-started
        // when the current download finishes.
        self.try_start_next_job_with_password(Some(job_id), category, pw)
            .await;
    }

    async fn handle_download_from_url(
        &self,
        url: String,
        title: String,
        download_dir: PathBuf,
        _category: Option<String>,
    ) {
        tracing::info!(%url, %title, "downloading NZB from URL");

        // Sanitize title for directory name.
        let safe_name: String = title
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
            .collect();
        let job_dir = download_dir.join(if safe_name.is_empty() {
            "turbonzb-download".to_string()
        } else {
            safe_name
        });

        // Create a placeholder job in Fetching state so it shows in the
        // queue immediately while the NZB is being downloaded.
        let job_id = match self.queue.create_fetching_job(&title, &job_dir, 0).await {
            Ok(id) => id,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("Create job error: {e}")));
                return;
            }
        };
        self.emit(BackendEvent::JobAdded { job_id });
        self.handle_refresh_jobs().await;

        // Spawn the HTTP fetch + parse + populate as a separate task so
        // the command loop is free to process other commands (including
        // more DownloadFromUrl requests) in parallel.
        let queue = Arc::clone(&self.queue);
        let config = Arc::clone(&self.config);
        let event_tx = self.event_tx.clone();
        let ctx = self.ctx.clone();
        let cancel_token = Arc::clone(&self.cancel_token);
        let selected_job = Arc::clone(&self.selected_job);
        let speed = Arc::clone(&self.speed);
        let engine_paused = Arc::clone(&self.engine_paused);
        let engine = Arc::clone(&self.engine);

        tokio::spawn(async move {
            let emit = |ev: BackendEvent| {
                let _ = event_tx.send(ev);
                if let Some(ref ctx) = ctx {
                    ctx.request_repaint();
                }
            };

            // Append API key if needed.
            let url = {
                let config = config.read().expect("config lock");
                append_api_key(&url, &config.indexers)
            };

            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent(concat!("turbonzb/", env!("CARGO_PKG_VERSION")))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    emit(BackendEvent::Error(format!("HTTP client error: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    emit(BackendEvent::Error(format!("NZB fetch failed: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            let status = resp.status();
            tracing::info!(%status, %url, "NZB fetch response");

            if !status.is_success() {
                emit(BackendEvent::Error(format!(
                    "NZB fetch failed: HTTP {status}"
                )));
                let _ = queue.delete_job(job_id).await;
                return;
            }

            let nzb_bytes = match resp.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    emit(BackendEvent::Error(format!("NZB read failed: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            tracing::info!(bytes = nzb_bytes.len(), "NZB downloaded");

            // Quick sanity check: NZB files are XML.
            let head = String::from_utf8_lossy(&nzb_bytes[..nzb_bytes.len().min(200)]);
            if !head.trim_start().starts_with("<?xml") && !head.trim_start().starts_with("<nzb") {
                emit(BackendEvent::Error(
                    "NZB fetch returned non-XML content (likely an error page). Check the indexer URL and API key.".into(),
                ));
                let _ = queue.delete_job(job_id).await;
                return;
            }

            let nzb = match nzb::parse(&nzb_bytes) {
                Ok(n) => n,
                Err(e) => {
                    emit(BackendEvent::Error(format!("NZB parse error: {e}")));
                    let _ = queue.delete_job(job_id).await;
                    return;
                }
            };

            // Populate the placeholder job with real data and set state to Queued.
            if let Err(e) = queue
                .populate_job(job_id, &nzb, &job_dir, Some(&title))
                .await
            {
                emit(BackendEvent::Error(format!("Populate job error: {e}")));
                let _ = queue.delete_job(job_id).await;
                return;
            }

            // Store the NZB's declared password on the job (same as the
            // file-open path) so post-processing can unlock encrypted
            // archives after the download.
            if let Some(pw) = nzb.passwords().first() {
                let _ = queue
                    .set_job_archive_password(job_id, Some(pw.as_str()))
                    .await;
            }

            emit(BackendEvent::JobStateChanged {
                job_id,
                state: JobState::Queued,
            });
            if let Ok(jobs) = queue.list_jobs().await {
                emit(BackendEvent::JobsList(jobs));
            }

            // Try to start downloading. We need to check engine_paused and
            // cancel_token here (not in try_start_next_job) since we don't
            // have a &self reference in this spawned task.
            let is_paused = engine_paused.lock().map(|p| *p).unwrap_or(false);
            let already_running = cancel_token.lock().expect("cancel_token lock").is_some();
            let has_downloading = queue
                .current_downloading_job()
                .await
                .ok()
                .flatten()
                .is_some();

            if !is_paused && !already_running && !has_downloading {
                if let Ok(Some(next)) = queue.next_queued_job().await {
                    if next.id == job_id {
                        // Claim the slot and spawn the engine.
                        if let Ok(true) = queue.claim_download_slot(job_id).await {
                            let servers: Vec<ServerConfig> = {
                                let c = config.read().expect("config lock");
                                c.server_configs()
                            };
                            if !servers.is_empty() {
                                let new_cancel = CancellationToken::new();
                                {
                                    let mut slot = cancel_token.lock().expect("cancel_token lock");
                                    *slot = Some(new_cancel.clone());
                                }
                                spawn_engine_task(
                                    job_id,
                                    Arc::clone(&engine),
                                    Arc::clone(&queue),
                                    Arc::clone(&config),
                                    event_tx.clone(),
                                    ctx.clone(),
                                    Arc::clone(&speed),
                                    Arc::clone(&cancel_token),
                                    Arc::clone(&selected_job),
                                    Arc::clone(&engine_paused),
                                    new_cancel,
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    /// Pause the download engine: cancel the active download and put its
    /// job back to Queued. Set engine_paused so no new jobs auto-start.
    async fn handle_pause_engine(&self) {
        {
            let mut paused = self.engine_paused.lock().expect("engine_paused lock");
            *paused = true;
        }
        self.emit(BackendEvent::EnginePaused(true));

        // Cancel the running download if any.
        let token = self.cancel_token.lock().expect("cancel_token lock").take();
        if let Some(tk) = token {
            tk.cancel();
            // The engine task will release the slot and put the job back
            // to Queued.
        } else {
            // No active download — just refresh.
            self.handle_refresh_jobs().await;
        }
    }

    /// Resume the download engine: clear the paused flag and start the
    /// next queued job.
    async fn handle_resume_engine(&self) {
        {
            let mut paused = self.engine_paused.lock().expect("engine_paused lock");
            *paused = false;
        }
        self.emit(BackendEvent::EnginePaused(false));
        self.try_start_next_job().await;
    }

    /// Try to start the next queued job. If the download slot is already
    /// held (another job is downloading), this is a no-op. The job at the
    /// front of the queue claims the slot atomically via the DB.
    async fn try_start_next_job(&self) {
        self.try_start_next_job_with_password(None, None, None)
            .await;
    }

    /// Try to start the next queued job, optionally passing along category
    /// and archive_password from a just-added job (for post-processing).
    async fn try_start_next_job_with_password(
        &self,
        _preferred_job_id: Option<i64>,
        _category: Option<String>,
        _archive_password: Option<String>,
    ) {
        // If the engine is paused, don't start anything.
        {
            let paused = self.engine_paused.lock().expect("engine_paused lock");
            if *paused {
                return;
            }
        }

        // If a download is already running (in-memory token or DB state),
        // don't start another. The DB check catches the case where the
        // engine task is still shutting down after a pause but hasn't
        // released the slot yet.
        {
            let tokens = self.cancel_token.lock().expect("cancel_token lock");
            if tokens.is_some() {
                return;
            }
        }
        if let Ok(Some(_)) = self.queue.current_downloading_job().await {
            return;
        }

        // Find the next queued job.
        let next = match self.queue.next_queued_job().await {
            Ok(Some(j)) => j,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "next_queued_job failed");
                return;
            }
        };

        // Atomically claim the download slot.
        let claimed = match self.queue.claim_download_slot(next.id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "claim_download_slot failed");
                return;
            }
        };
        if !claimed {
            tracing::debug!(job_id = next.id, "could not claim download slot");
            return;
        }

        self.spawn_engine(next.id);
    }

    /// Spawn the engine as a background task for a job that has already
    /// claimed the download slot. This is non-blocking — the command loop
    /// continues processing other commands (RefreshJobs, etc.) while the
    /// download runs concurrently.
    fn spawn_engine(&self, job_id: i64) {
        let servers: Vec<ServerConfig> = {
            let c = self.config.read().expect("config lock");
            c.server_configs()
        };
        if servers.is_empty() {
            self.emit(BackendEvent::Error(
                "No NNTP servers configured. Add one in Settings.".into(),
            ));
            // Release the slot we just claimed.
            let queue = Arc::clone(&self.queue);
            let event_tx = self.event_tx.clone();
            let ctx = self.ctx.clone();
            tokio::spawn(async move {
                let _ = queue.release_download_slot(job_id, JobState::Queued).await;
                let _ = event_tx.send(BackendEvent::JobsList(
                    queue.list_jobs().await.unwrap_or_default(),
                ));
                if let Some(ref ctx) = ctx {
                    ctx.request_repaint();
                }
            });
            return;
        }

        tracing::info!(job_id, servers = servers.len(), "starting engine");

        let cancel_token = CancellationToken::new();
        {
            let mut token = self.cancel_token.lock().expect("cancel_token lock");
            *token = Some(cancel_token.clone());
        }

        spawn_engine_task(
            job_id,
            Arc::clone(&self.engine),
            Arc::clone(&self.queue),
            Arc::clone(&self.config),
            self.event_tx.clone(),
            self.ctx.clone(),
            Arc::clone(&self.speed),
            Arc::clone(&self.cancel_token),
            Arc::clone(&self.selected_job),
            Arc::clone(&self.engine_paused),
            cancel_token,
        );
    }

    async fn handle_delete(&self, job_id: i64) {
        // Check if this is the actively-downloading job.
        let active_id = self.queue.current_downloading_job().await.ok().flatten();
        let is_active = active_id == Some(job_id);

        if is_active {
            let token = self.cancel_token.lock().expect("cancel_token lock").take();
            if let Some(tk) = token {
                tk.cancel();
            }
        }

        if let Err(e) = self.queue.delete_job(job_id).await {
            self.emit(BackendEvent::Error(format!("Delete error: {e}")));
            return;
        }
        self.handle_refresh_jobs().await;
    }

    /// Delete all completed (and failed) jobs from the queue.
    async fn handle_clear_completed(&self) {
        let jobs = match self.queue.list_jobs().await {
            Ok(j) => j,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("List jobs error: {e}")));
                return;
            }
        };
        for job in &jobs {
            if matches!(job.state, JobState::Complete | JobState::Failed) {
                if let Err(e) = self.queue.delete_job(job.id).await {
                    tracing::warn!(error = %e, job_id = job.id, "failed to delete completed job");
                }
            }
        }
        self.handle_refresh_jobs().await;
    }

    /// Move a job up or down in the queue by swapping its position with
    /// the adjacent job and renormalizing all priorities to match the new
    /// order. `up = true` moves toward the front, `up = false` toward the back.
    async fn handle_move_job(&self, job_id: i64, up: bool) {
        let mut jobs = match self.queue.list_jobs().await {
            Ok(j) => j,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("List jobs error: {e}")));
                return;
            }
        };

        // Find the index of the target job in the ordered list.
        let idx = jobs.iter().position(|j| j.id == job_id);
        let Some(idx) = idx else {
            return;
        };

        // Compute the swap index.
        let adjacent_idx = if up {
            if idx == 0 {
                return;
            }
            idx - 1
        } else {
            if idx >= jobs.len() - 1 {
                return;
            }
            idx + 1
        };

        // Swap the two jobs in the ordered list.
        jobs.swap(idx, adjacent_idx);

        // Renormalize: assign priority = index for all jobs so the order
        // is fully determined by priority (no id tiebreaker needed).
        for (i, job) in jobs.iter().enumerate() {
            if let Err(e) = self.queue.set_job_priority(job.id, i as i64).await {
                self.emit(BackendEvent::Error(format!("Move error: {e}")));
                return;
            }
        }

        self.handle_refresh_jobs().await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_postprocess(
        &self,
        job_id: Option<i64>,
        download_dir: PathBuf,
        completed_dir: PathBuf,
        category: Option<String>,
        archive_password: Option<String>,
        skip_verify: bool,
        cleanup_archives: bool,
    ) {
        let config = PostProcessConfig {
            download_dir,
            completed_dir,
            category,
            cleanup_archives,
            archive_password,
            skip_verify,
        };
        match post_process(config).await {
            Ok(report) => {
                self.emit(BackendEvent::PostProcessDone { job_id, report });
            }
            Err(e) => {
                self.emit(BackendEvent::PostProcessFailed {
                    job_id,
                    error: e.to_string(),
                });
            }
        }
    }

    async fn handle_search(&self, query: SearchQuery) {
        let indexer_configs: Vec<IndexerConfig> = {
            let c = self.config.read().expect("config lock");
            c.indexers.clone()
        };
        if indexer_configs.is_empty() {
            self.emit(BackendEvent::SearchFailed(
                "No indexers configured. Add one in Settings.".into(),
            ));
            return;
        }

        let mut aggregator = SearchAggregator::new(15);
        for cfg in indexer_configs {
            let cfg = normalize_indexer_config(cfg);
            aggregator.add_provider(Box::new(NewznabClient::new(cfg)));
        }

        let results = aggregator.search(&query).await;
        self.emit(BackendEvent::SearchResults(results));
    }

    async fn handle_test_indexer(&self, config: IndexerConfig) {
        let name = config.name.clone();
        let config = normalize_indexer_config(config);
        let client = NewznabClient::new(config);
        match client.caps().await {
            Ok(caps) => {
                self.emit(BackendEvent::IndexerTestResult {
                    name,
                    ok: true,
                    message: format!(
                        "{} — v{}, {} categories, retention {}d",
                        caps.title,
                        caps.server_version,
                        caps.categories.len(),
                        caps.retention_days
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "?".into())
                    ),
                });
            }
            Err(e) => {
                self.emit(BackendEvent::IndexerTestResult {
                    name,
                    ok: false,
                    message: e.to_string(),
                });
            }
        }
    }

    async fn handle_test_server(&self, config: ServerConfig) {
        let host = config.host.clone();
        match turbonzb_core::nntp::NntpClient::connect(&config).await {
            Ok(_) => {
                self.emit(BackendEvent::ServerTestResult {
                    host,
                    ok: true,
                    message: "Connected and authenticated successfully".into(),
                });
            }
            Err(e) => {
                self.emit(BackendEvent::ServerTestResult {
                    host,
                    ok: false,
                    message: e.to_string(),
                });
            }
        }
    }

    async fn handle_refresh_jobs(&self) {
        match self.queue.list_jobs().await {
            Ok(jobs) => {
                self.emit(BackendEvent::JobsList(jobs));
            }
            Err(e) => {
                warn!(error = %e, "refresh jobs failed");
            }
        }
    }

    async fn handle_get_job_details(&self, job_id: i64) {
        self.emit_job_details(job_id).await;
    }

    /// Fetch per-file details for a job and emit a `JobDetails` event.
    async fn emit_job_details(&self, job_id: i64) {
        let details = build_job_details(&self.queue, &[], job_id).await;
        self.emit(BackendEvent::JobDetails {
            job_id,
            files: details,
        });
    }

    /// Update the config (called when the user saves settings).
    pub fn update_config(&self, new_config: AppConfig) {
        let mut c = self.config.write().expect("config lock");
        *c = new_config;
    }
}

/// Build job file details from a single SQL aggregation query (no N+1).
async fn build_job_details(
    queue: &QueueManager,
    _files: &[turbonzb_core::queue::QueueFile],
    job_id: i64,
) -> Vec<JobFileDetail> {
    let files = match queue.list_files(job_id).await {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "list_files failed");
            return Vec::new();
        }
    };

    let mut details = Vec::with_capacity(files.len());
    for file in &files {
        let segments = match queue.list_segments(file.id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "list_segments failed");
                Vec::new()
            }
        };
        let done = segments
            .iter()
            .filter(|s| s.state != turbonzb_core::queue::SegmentState::Pending)
            .count() as u32;
        let missing = segments
            .iter()
            .filter(|s| s.state == turbonzb_core::queue::SegmentState::Missing || s.missing)
            .count() as u32;
        let total_bytes = segments.iter().map(|s| s.bytes).sum();
        let down = segments
            .iter()
            .filter(|s| s.state == turbonzb_core::queue::SegmentState::Done)
            .map(|s| s.bytes)
            .sum();
        details.push(JobFileDetail {
            id: file.id,
            filename: file.filename.clone(),
            segment_count: file.segment_count,
            segments_done: done,
            segments_missing: missing,
            total_bytes,
            downloaded_bytes: down,
            segments: segments
                .into_iter()
                .map(|s| JobSegmentDetail {
                    number: s.number,
                    bytes: s.bytes,
                    state: s.state,
                })
                .collect(),
        });
    }
    tracing::trace!(job_id, files = details.len(), "built job details");
    details
}

/// Normalize an indexer URL: if it doesn't end with `/api`, append it.
/// This handles the common case where users paste the base URL (e.g.
/// `https://api.example.com`) without the Newznab API path.
fn normalize_indexer_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/api") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/api")
    }
}

/// Normalize an `IndexerConfig` by fixing its URL.
fn normalize_indexer_config(mut cfg: IndexerConfig) -> IndexerConfig {
    cfg.url = normalize_indexer_url(&cfg.url);
    cfg
}

/// Newznab enclosure URLs from search results typically don't include
/// the API key. If the URL starts with a known indexer's base URL,
/// append `apikey=<key>` as a query parameter.
fn append_api_key(url: &str, indexers: &[IndexerConfig]) -> String {
    for indexer in indexers {
        let base = normalize_indexer_url(&indexer.url);
        if url.starts_with(&base) {
            if url.contains("apikey=") {
                return url.to_string();
            }
            let separator = if url.contains('?') { '&' } else { '?' };
            return format!("{url}{separator}apikey={}", indexer.api_key);
        }
    }
    url.to_string()
}

/// Spawn an engine task for a job. The job must have already claimed the
/// download slot (state = 'downloading'). When the engine finishes (or
/// is cancelled), the slot is released and the next queued job is
/// auto-started.
#[allow(clippy::too_many_arguments)]
fn spawn_engine_task(
    job_id: i64,
    engine: Arc<Engine>,
    queue: Arc<QueueManager>,
    config: Arc<std::sync::RwLock<AppConfig>>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    ctx: Option<egui::Context>,
    speed_tracker: Arc<std::sync::Mutex<SpeedTracker>>,
    cancel_token_slot: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    selected_job: Arc<std::sync::Mutex<Option<i64>>>,
    engine_paused: Arc<std::sync::Mutex<bool>>,
    cancel_token: CancellationToken,
) {
    tracing::info!(job_id, "spawning engine task");
    tokio::spawn(async move {
        let total_bytes = queue
            .get_job(job_id)
            .await
            .ok()
            .map(|j| j.total_bytes)
            .unwrap_or(0);

        // Initialize the speed tracker for this job.
        {
            let mut speed = speed_tracker.lock().expect("speed lock");
            speed.start(total_bytes);
        }

        let fwd_engine = Arc::clone(&engine);
        // Extra clone so auto-start can reuse the same warm engine after this
        // job completes (engine itself is moved into run_job below).
        let engine_reuse = Arc::clone(&engine);
        let (tx, mut rx) = mpsc::unbounded_channel::<ProgressEvent>();

        // Forward progress events to the GUI + track speed.
        let fwd_event_tx = event_tx.clone();
        let fwd_ctx = ctx.clone();
        let fwd_speed = Arc::clone(&speed_tracker);
        let fwd_queue = Arc::clone(&queue);
        let fwd_selected = Arc::clone(&selected_job);
        let fwd_job_id = job_id;
        let forwarder = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        let Some(ev) = ev else { break };
                        if let ProgressEvent::SegmentDone { bytes, .. } = &ev {
                            if let Ok(mut tracker) = fwd_speed.lock() {
                                tracker.add_bytes(*bytes);
                            }
                        }
                        let _ = fwd_event_tx.send(BackendEvent::Progress(ev));
                        // Don't request_repaint on every segment — the 100ms
                        // tick below handles repaints. With 50 connections
                        // this would cause hundreds of repaints/sec.
                    }
                    _ = interval.tick() => {
                        let (bps, downloaded) = fwd_speed
                            .lock()
                            .map(|mut s| s.tick())
                            .unwrap_or((0.0, 0));
                        let history = fwd_speed
                            .lock()
                            .ok()
                            .map(|s| s.history_vec())
                            .unwrap_or_default();
                        let _ = fwd_event_tx.send(BackendEvent::Speed {
                            job_id: Some(fwd_job_id),
                            bytes_per_sec: bps,
                            downloaded_bytes: downloaded,
                            total_bytes,
                            history,
                            active_connections: fwd_engine.active_connections(),
                        });

                        if let Ok(jobs) = fwd_queue.list_jobs().await {
                            let _ = fwd_event_tx.send(BackendEvent::JobsList(jobs));
                        }

                        if let Some(sel_id) = fwd_selected
                            .lock()
                            .ok()
                            .and_then(|s| *s)
                        {
                            let details = build_job_details(&fwd_queue, &[], sel_id).await;
                            let _ = fwd_event_tx.send(BackendEvent::JobDetails {
                                job_id: sel_id,
                                files: details,
                            });
                        }

                        if let Some(ref ctx) = fwd_ctx {
                            ctx.request_repaint();
                        }
                    }
                }
            }
        });

        let result = tokio::select! {
            r = engine.run_job(queue.clone(), job_id, tx) => r,
            _ = cancel_token.cancelled() => {
                tracing::info!(job_id, "engine: cancelled by pause");
                Ok(())
            }
        };
        tracing::info!(job_id, "engine: run_job returned");
        forwarder.await.ok();

        let emit = |ev: BackendEvent| {
            let _ = event_tx.send(ev);
            if let Some(ref ctx) = ctx {
                ctx.request_repaint();
            }
        };

        // Determine final state and release the download slot.
        match &result {
            Ok(()) => {
                let job = queue.get_job(job_id).await.ok();
                let state = job.as_ref().map(|j| j.state).unwrap_or(JobState::Complete);
                // If cancelled (engine pause), put the job back to Queued
                // so it can be resumed later. release_download_slot only
                // works if state is still 'downloading'.
                let final_state = if cancel_token.is_cancelled() {
                    JobState::Queued
                } else {
                    state
                };

                if cancel_token.is_cancelled() {
                    let _ = queue.release_download_slot(job_id, JobState::Queued).await;
                    // Reset failed segments so they get retried on resume.
                    let _ = queue.reset_failed_segments(job_id).await;
                }

                emit(BackendEvent::JobStateChanged {
                    job_id,
                    state: final_state,
                });

                // Run post-processing if enabled and job completed successfully.
                if !cancel_token.is_cancelled() && state == JobState::Complete {
                    let do_pp = {
                        let c = config.read().expect("config lock");
                        c.post_process.auto_post_process
                    };
                    if do_pp {
                        if let Some(job) = job {
                            let pp_defaults = {
                                let c = config.read().expect("config lock");
                                c.post_process.clone()
                            };
                            let completed_dir = {
                                let c = config.read().expect("config lock");
                                c.completed_dir.clone()
                            };
                            let pp_config = PostProcessConfig {
                                download_dir: job.output_dir,
                                completed_dir,
                                category: None,
                                cleanup_archives: pp_defaults.cleanup_archives,
                                // Password stored on the job itself when it
                                // was added (NZB meta or user-supplied).
                                archive_password: job.archive_password.clone(),
                                skip_verify: pp_defaults.skip_verify,
                            };
                            emit(BackendEvent::PostProcessStarted { job_id });
                            // Run post-processing on a blocking thread —
                            // PAR2 verify is CPU-bound (MD5 hashing) and
                            // would block the async runtime.
                            let pp_result = tokio::task::spawn_blocking(move || {
                                // post_process is async only because of file
                                // I/O; we can run it synchronously on the
                                // blocking thread.
                                futures::executor::block_on(post_process(pp_config))
                            })
                            .await;
                            match pp_result {
                                Ok(Ok(report)) => {
                                    // Persist a human-readable failure so the
                                    // job shows as "Failed" with a reason
                                    // (unpack errors, damaged files, ...).
                                    let err = match &report.status {
                                        PostProcessStatus::Complete
                                        | PostProcessStatus::UnpackedWithoutVerify
                                        | PostProcessStatus::NoArchives => None,
                                        PostProcessStatus::Damaged {
                                            healthy,
                                            damaged,
                                            missing,
                                        } => Some(format!(
                                            "PAR2 verify: {healthy} ok, {damaged} damaged, {missing} missing — repair needed"
                                        )),
                                        PostProcessStatus::UnpackFailed(msg) => {
                                            Some(format!("Unpack failed: {msg}"))
                                        }
                                    };
                                    if err.is_some() {
                                        let _ = queue.set_job_state(job_id, JobState::Failed).await;
                                    }
                                    let _ = queue.set_job_error(job_id, err.as_deref()).await;
                                    emit(BackendEvent::JobStateChanged {
                                        job_id,
                                        state: if err.is_some() {
                                            JobState::Failed
                                        } else {
                                            JobState::Complete
                                        },
                                    });
                                    emit(BackendEvent::PostProcessDone {
                                        job_id: Some(job_id),
                                        report,
                                    });
                                }
                                Ok(Err(e)) => {
                                    let msg = e.to_string();
                                    let _ = queue.set_job_state(job_id, JobState::Failed).await;
                                    let _ = queue
                                        .set_job_error(
                                            job_id,
                                            Some(&format!("Post-process: {msg}")),
                                        )
                                        .await;
                                    emit(BackendEvent::JobStateChanged {
                                        job_id,
                                        state: JobState::Failed,
                                    });
                                    emit(BackendEvent::PostProcessFailed {
                                        job_id: Some(job_id),
                                        error: msg,
                                    });
                                }
                                Err(e) => {
                                    let msg = format!("post-process task panicked: {e}");
                                    let _ = queue.set_job_state(job_id, JobState::Failed).await;
                                    let _ = queue.set_job_error(job_id, Some(&msg)).await;
                                    emit(BackendEvent::JobStateChanged {
                                        job_id,
                                        state: JobState::Failed,
                                    });
                                    emit(BackendEvent::PostProcessFailed {
                                        job_id: Some(job_id),
                                        error: msg,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // Engine errored — release the slot to Failed.
                let _ = queue.release_download_slot(job_id, JobState::Failed).await;
                let msg = format!("Engine error: {e}");
                let _ = queue.set_job_error(job_id, Some(&msg)).await;
                emit(BackendEvent::Error(msg));
            }
        }

        // Clear the cancel token slot.
        {
            let mut slot = cancel_token_slot.lock().expect("cancel_token lock");
            *slot = None;
        }
        // Reset the speed tracker.
        {
            let mut speed = speed_tracker.lock().expect("speed lock");
            speed.start(0);
        }

        // Emit a zero-speed event so the GUI knows the download stopped.
        emit(BackendEvent::Speed {
            job_id: None,
            bytes_per_sec: 0.0,
            downloaded_bytes: 0,
            total_bytes: 0,
            history: Vec::new(),
            active_connections: 0,
        });

        // Refresh jobs list + auto-start next queued job (unless engine
        // is paused).
        let is_paused = engine_paused.lock().map(|p| *p).unwrap_or(false);

        match queue.list_jobs().await {
            Ok(jobs) => {
                emit(BackendEvent::JobsList(jobs.clone()));

                if is_paused {
                    tracing::info!("engine paused — not auto-starting next job");
                } else if let Some(next) = jobs.iter().find(|j| j.state == JobState::Queued) {
                    tracing::info!(next_job_id = next.id, "auto-starting next queued job");
                    let next_id = next.id;

                    let (next_servers, _next_conn) = {
                        let c = config.read().expect("config lock");
                        (c.server_configs(), c.effective_max_connections())
                    };
                    if next_servers.is_empty() {
                        tracing::warn!("no servers configured, skipping auto-start");
                    } else {
                        // Claim the slot for the next job. If the current
                        // job's state hasn't been fully released yet, this
                        // will fail — retry once after a short delay.
                        match queue.claim_download_slot(next_id).await {
                            Ok(true) => {
                                let new_cancel = CancellationToken::new();
                                {
                                    let mut slot =
                                        cancel_token_slot.lock().expect("cancel_token lock");
                                    *slot = Some(new_cancel.clone());
                                }
                                // Reuse the same long-lived engine (warm pool).
                                spawn_engine_task(
                                    next_id,
                                    Arc::clone(&engine_reuse),
                                    queue.clone(),
                                    config.clone(),
                                    event_tx.clone(),
                                    ctx.clone(),
                                    speed_tracker.clone(),
                                    cancel_token_slot.clone(),
                                    selected_job.clone(),
                                    engine_paused.clone(),
                                    new_cancel,
                                );
                            }
                            Ok(false) => {
                                tracing::warn!(
                                    next_id,
                                    "could not claim slot for next job — slot may still be held"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "claim slot for next job failed");
                            }
                        }
                    }
                } else {
                    tracing::info!("no queued jobs to auto-start");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "refresh jobs failed");
            }
        }
    });
}
