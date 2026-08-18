//! Backend: owns the tokio runtime, queue manager, and bridges async
//! engine/search events to the egui main thread.
//!
//! The GUI thread sends commands over `BackendHandle::cmd_tx` and polls
//! results over `BackendHandle::event_rx` each frame. The backend runtime
//! processes commands on a background tokio runtime and pushes `BackendEvent`s
//! back. The egui `Context` is stored so the backend can call
//! `ctx.request_repaint()` after pushing an event, waking the GUI immediately.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nobz_core::engine::{Engine, ProgressEvent};
use nobz_core::nntp::ServerConfig;
use nobz_core::nzb;
use nobz_core::postprocess::{PostProcessConfig, PostProcessReport, post_process};
use nobz_core::queue::{JobState, QueueJob, QueueManager};
use nobz_index::types::{IndexerConfig, SearchQuery};
use nobz_index::{AggregatedResult, NewznabClient, SearchAggregator};
use tokio::sync::mpsc;
use tracing::warn;

use crate::settings::AppConfig;

/// Commands the GUI sends to the backend.
#[derive(Debug)]
pub enum BackendCmd {
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
    /// Resume a paused/failed job.
    ResumeJob { job_id: i64 },
    /// Pause a job.
    PauseJob { job_id: i64 },
    /// Delete a job.
    DeleteJob { job_id: i64 },
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
    /// Set the currently selected job (so the 500ms interval can auto-refresh
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
    /// Download speed update (emitted ~2x per second during active downloads).
    Speed {
        job_id: i64,
        bytes_per_sec: f64,
        downloaded_bytes: u64,
        total_bytes: u64,
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
    /// Post-processing completed.
    PostProcessDone {
        job_id: Option<i64>,
        report: PostProcessReport,
    },
    /// Post-processing failed.
    PostProcessFailed { job_id: Option<i64>, error: String },
    /// A generic error.
    Error(String),
}

/// Per-file detail for the details pane.
#[derive(Debug, Clone)]
pub struct JobFileDetail {
    pub filename: String,
    pub segment_count: u32,
    pub segments_done: u32,
    pub segments_missing: u32,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
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

/// The backend owns the tokio runtime and the queue manager. It is created
/// once at startup and runs for the lifetime of the app.
pub struct Backend {
    queue: Arc<QueueManager>,
    config: Arc<std::sync::RwLock<AppConfig>>,
    ctx: Option<egui::Context>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    /// Track which job is currently downloading (only one at a time in v1).
    current_job: Arc<std::sync::Mutex<Option<i64>>>,
    /// Speed tracker: job_id → (start_time, bytes_seen_so_far).
    speed: Arc<std::sync::Mutex<HashMap<i64, (Instant, u64)>>>,
    /// Cancellation tokens for running jobs, so pause can stop them.
    cancel_tokens: Arc<std::sync::Mutex<HashMap<i64, tokio_util::sync::CancellationToken>>>,
    /// Currently selected job (for auto-refreshing details in the 500ms interval).
    selected_job: Arc<std::sync::Mutex<Option<i64>>>,
}

impl Backend {
    /// Spawn the backend: starts a tokio runtime, opens the queue DB, and
    /// spawns a command-processing task. Returns a handle for the GUI.
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
        let db_path = {
            let c = config.read().expect("config lock");
            c.db_path.clone()
        };

        let queue =
            runtime.block_on(async { QueueManager::open(&db_path).await.expect("queue db open") });
        let queue = Arc::new(queue);

        let handle = BackendHandle {
            cmd_tx,
            event_rx: std::sync::Arc::new(std::sync::Mutex::new(event_rx)),
        };

        let backend = Self {
            queue: Arc::clone(&queue),
            config: Arc::clone(&config),
            ctx: Some(ctx),
            event_tx,
            current_job: Arc::new(std::sync::Mutex::new(None)),
            speed: Arc::new(std::sync::Mutex::new(HashMap::new())),
            cancel_tokens: Arc::new(std::sync::Mutex::new(HashMap::new())),
            selected_job: Arc::new(std::sync::Mutex::new(None)),
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
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                BackendCmd::DownloadNzb {
                    nzb_bytes,
                    output_dir,
                    category,
                    archive_password,
                } => {
                    self.handle_download(nzb_bytes, output_dir, category, archive_password)
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
                BackendCmd::ResumeJob { job_id } => {
                    self.handle_resume(job_id).await;
                }
                BackendCmd::PauseJob { job_id } => {
                    self.handle_pause(job_id).await;
                }
                BackendCmd::DeleteJob { job_id } => {
                    self.handle_delete(job_id).await;
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

    async fn handle_download(
        &self,
        nzb_bytes: Vec<u8>,
        output_dir: PathBuf,
        category: Option<String>,
        archive_password: Option<String>,
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
        for (i, f) in nzb.files.iter().enumerate() {
            tracing::debug!(
                file = i,
                filename = %f.filename(),
                segment_count = f.segment_count,
                actual_segments = f.segments.len(),
                "NZB file"
            );
        }

        let pw = archive_password.or_else(|| nzb.passwords().first().map(|s| s.to_string()));

        let job_id = match self.queue.add_job(&nzb, &output_dir, 0).await {
            Ok(id) => id,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("Queue add error: {e}")));
                return;
            }
        };
        self.emit(BackendEvent::JobAdded { job_id });

        // Refresh the queue immediately so the new job shows up.
        self.handle_refresh_jobs().await;

        // Store the password for post-processing later.
        {
            let mut current = self.current_job.lock().expect("current_job lock");
            *current = Some(job_id);
        }

        // Only one download at a time — if another job is already
        // downloading, this job stays queued until the current one
        // finishes (and the engine task picks up the next queued job).
        let already_downloading = {
            let tokens = self.cancel_tokens.lock().expect("cancel_tokens lock");
            !tokens.is_empty()
        };

        if already_downloading {
            tracing::info!(job_id, "another job is downloading — queuing");
            return;
        }

        // Spawn the engine as a separate task so the command loop
        // continues processing RefreshJobs/Pause/etc. while downloading.
        self.spawn_engine(job_id, category, pw);
    }

    async fn handle_download_from_url(
        &self,
        url: String,
        title: String,
        download_dir: PathBuf,
        category: Option<String>,
    ) {
        tracing::info!(%url, %title, "downloading NZB from URL");

        // Newznab enclosure URLs from search results typically don't
        // include the API key. We need to append it. Find the matching
        // indexer by checking if the URL starts with the indexer's base URL.
        let url = {
            let config = self.config.read().expect("config lock");
            append_api_key(&url, &config.indexers)
        };

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("nobz/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("HTTP client error: {e}")));
                return;
            }
        };

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("NZB fetch failed: {e}")));
                return;
            }
        };

        let status = resp.status();
        tracing::info!(%status, %url, "NZB fetch response");

        if !status.is_success() {
            self.emit(BackendEvent::Error(format!(
                "NZB fetch failed: HTTP {status}"
            )));
            return;
        }

        let nzb_bytes = match resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                self.emit(BackendEvent::Error(format!("NZB read failed: {e}")));
                return;
            }
        };

        tracing::info!(bytes = nzb_bytes.len(), "NZB downloaded");

        // Sanitize title for directory name.
        let safe_name: String = title
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
            .collect();
        let job_dir = download_dir.join(if safe_name.is_empty() {
            "nobz-download".to_string()
        } else {
            safe_name
        });

        self.handle_download(nzb_bytes, job_dir, category, None)
            .await;
    }

    async fn handle_resume(&self, job_id: i64) {
        // Set job back to queued so the engine picks it up.
        let _job = match self.queue.get_job(job_id).await {
            Ok(j) => j,
            Err(e) => {
                self.emit(BackendEvent::Error(format!("Get job error: {e}")));
                return;
            }
        };
        if let Err(e) = self.queue.set_job_state(job_id, JobState::Queued).await {
            self.emit(BackendEvent::Error(format!("Set job state error: {e}")));
            return;
        }
        {
            let mut current = self.current_job.lock().expect("current_job lock");
            *current = Some(job_id);
        }
        self.spawn_engine(job_id, None, None);
    }

    /// Spawn the engine as a background task. This is non-blocking — the
    /// command loop continues processing other commands (RefreshJobs, etc.)
    /// while the download runs concurrently.
    fn spawn_engine(
        &self,
        job_id: i64,
        _category: Option<String>,
        _archive_password: Option<String>,
    ) {
        let servers: Vec<ServerConfig> = {
            let c = self.config.read().expect("config lock");
            c.server_configs()
        };
        let max_conn = {
            let c = self.config.read().expect("config lock");
            c.max_connections
        };
        if servers.is_empty() {
            self.emit(BackendEvent::Error(
                "No NNTP servers configured. Add one in Settings.".into(),
            ));
            return;
        }

        tracing::info!(job_id, servers = servers.len(), max_conn, "starting engine");

        let cancel_token = tokio_util::sync::CancellationToken::new();
        {
            let mut tokens = self.cancel_tokens.lock().expect("cancel_tokens lock");
            tokens.insert(job_id, cancel_token.clone());
        }
        {
            let mut speed = self.speed.lock().expect("speed lock");
            speed.insert(job_id, (Instant::now(), 0));
        }

        spawn_engine_task(
            job_id,
            PathBuf::new(), // output_dir not needed, engine reads from DB
            servers,
            max_conn,
            Arc::clone(&self.queue),
            Arc::clone(&self.config),
            self.event_tx.clone(),
            self.ctx.clone(),
            Arc::clone(&self.speed),
            Arc::clone(&self.current_job),
            Arc::clone(&self.cancel_tokens),
            Arc::clone(&self.selected_job),
            cancel_token,
        );
    }

    async fn handle_pause(&self, job_id: i64) {
        // Set state to Paused in the DB.
        if let Err(e) = self.queue.set_job_state(job_id, JobState::Paused).await {
            self.emit(BackendEvent::Error(format!("Pause error: {e}")));
            return;
        }

        // Cancel the running engine task if any.
        let cancelled = {
            let tokens = self.cancel_tokens.lock().expect("cancel_tokens lock");
            if let Some(token) = tokens.get(&job_id) {
                token.cancel();
                true
            } else {
                false
            }
        };

        if cancelled {
            // The engine task will set the state and emit events.
            tracing::info!(job_id, "pause: cancel token fired");
        } else {
            // No running task — just emit the state change.
            self.emit(BackendEvent::JobStateChanged {
                job_id,
                state: JobState::Paused,
            });
            self.handle_refresh_jobs().await;
        }
    }

    async fn handle_delete(&self, job_id: i64) {
        if let Err(e) = self.queue.delete_job(job_id).await {
            self.emit(BackendEvent::Error(format!("Delete error: {e}")));
            return;
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
        match nobz_core::nntp::NntpClient::connect(&config).await {
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
        let files = match self.queue.list_files(job_id).await {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "list_files failed");
                return;
            }
        };

        let mut details = Vec::with_capacity(files.len());
        for file in &files {
            let segments = match self.queue.list_segments(file.id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "list_segments failed");
                    continue;
                }
            };
            let segments_done = segments
                .iter()
                .filter(|s| s.state != nobz_core::queue::SegmentState::Pending)
                .count() as u32;
            let segments_missing = segments
                .iter()
                .filter(|s| s.missing || s.state == nobz_core::queue::SegmentState::Missing)
                .count() as u32;
            let total_bytes: u64 = segments.iter().map(|s| s.bytes).sum();
            let downloaded_bytes: u64 = segments
                .iter()
                .filter(|s| s.state == nobz_core::queue::SegmentState::Done)
                .map(|s| s.bytes)
                .sum();
            details.push(JobFileDetail {
                filename: file.filename.clone(),
                segment_count: file.segment_count,
                segments_done,
                segments_missing,
                total_bytes,
                downloaded_bytes,
            });
        }

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

/// Spawn an engine task for a job. This is a free function so it can be
/// called both from `Backend::spawn_engine` and from the auto-start-next
/// logic at the end of a previous engine task.
#[allow(clippy::too_many_arguments)]
fn spawn_engine_task(
    job_id: i64,
    _output_dir: PathBuf,
    servers: Vec<ServerConfig>,
    max_conn: usize,
    queue: Arc<QueueManager>,
    config: Arc<std::sync::RwLock<AppConfig>>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    ctx: Option<egui::Context>,
    speed_tracker: Arc<std::sync::Mutex<HashMap<i64, (Instant, u64)>>>,
    current_job: Arc<std::sync::Mutex<Option<i64>>>,
    cancel_tokens: Arc<std::sync::Mutex<HashMap<i64, tokio_util::sync::CancellationToken>>>,
    selected_job: Arc<std::sync::Mutex<Option<i64>>>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    tracing::info!(
        job_id,
        servers = servers.len(),
        max_conn,
        "spawning engine task"
    );
    tokio::spawn(async move {
        let total_bytes = queue
            .get_job(job_id)
            .await
            .ok()
            .map(|j| j.total_bytes)
            .unwrap_or(0);

        let engine = Arc::new(Engine::new(servers, max_conn));
        let (tx, mut rx) = mpsc::unbounded_channel::<ProgressEvent>();

        // Forward progress events to the GUI + track speed.
        let fwd_event_tx = event_tx.clone();
        let fwd_ctx = ctx.clone();
        let fwd_speed = Arc::clone(&speed_tracker);
        let fwd_queue = Arc::clone(&queue);
        let fwd_selected = Arc::clone(&selected_job);
        let forwarder = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        let Some(ev) = ev else { break };
                        if let ProgressEvent::SegmentDone { bytes, .. } = &ev {
                            if let Ok(mut tracker) = fwd_speed.lock() {
                                if let Some(entry) = tracker.get_mut(&job_id) {
                                    entry.1 += bytes;
                                }
                            }
                        }
                        let _ = fwd_event_tx.send(BackendEvent::Progress(ev));
                        if let Some(ref ctx) = fwd_ctx {
                            ctx.request_repaint();
                        }
                    }
                    _ = interval.tick() => {
                        let (bps, downloaded) = fwd_speed
                            .lock()
                            .ok()
                            .and_then(|s| {
                                s.get(&job_id).map(|(start, bytes)| {
                                    let elapsed = start.elapsed().as_secs_f64().max(0.1);
                                    (*bytes as f64 / elapsed, *bytes)
                                })
                            })
                            .unwrap_or((0.0, 0));
                        let _ = fwd_event_tx.send(BackendEvent::Speed {
                            job_id,
                            bytes_per_sec: bps,
                            downloaded_bytes: downloaded,
                            total_bytes,
                        });

                        if let Ok(jobs) = fwd_queue.list_jobs().await {
                            let _ = fwd_event_tx.send(BackendEvent::JobsList(jobs));
                        }

                        if let Some(sel_id) = fwd_selected
                            .lock()
                            .ok()
                            .and_then(|s| *s)
                        {
                            if let Ok(files) = fwd_queue.list_files(sel_id).await {
                                let mut details = Vec::with_capacity(files.len());
                                for file in &files {
                                    if let Ok(segs) = fwd_queue.list_segments(file.id).await {
                                        let seg_done = segs
                                            .iter()
                                            .filter(|s| s.state
                                                != nobz_core::queue::SegmentState::Pending)
                                            .count() as u32;
                                        let seg_missing = segs
                                            .iter()
                                            .filter(|s| {
                                                s.missing
                                                    || s.state
                                                        == nobz_core::queue::SegmentState::Missing
                                            })
                                            .count() as u32;
                                        let tb: u64 = segs.iter().map(|s| s.bytes).sum();
                                        let db: u64 = segs
                                            .iter()
                                            .filter(|s| {
                                                s.state
                                                    == nobz_core::queue::SegmentState::Done
                                            })
                                            .map(|s| s.bytes)
                                            .sum();
                                        details.push(JobFileDetail {
                                            filename: file.filename.clone(),
                                            segment_count: file.segment_count,
                                            segments_done: seg_done,
                                            segments_missing: seg_missing,
                                            total_bytes: tb,
                                            downloaded_bytes: db,
                                        });
                                    }
                                }
                                let _ = fwd_event_tx.send(BackendEvent::JobDetails {
                                    job_id: sel_id,
                                    files: details,
                                });
                            }
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
                let _ = queue.set_job_state(job_id, JobState::Paused).await;
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

        match result {
            Ok(()) => {
                let job = queue.get_job(job_id).await.ok();
                let state = job.as_ref().map(|j| j.state).unwrap_or(JobState::Complete);
                emit(BackendEvent::JobStateChanged { job_id, state });

                let do_pp = {
                    let c = config.read().expect("config lock");
                    c.post_process.auto_post_process
                };
                if do_pp && state == JobState::Complete {
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
                            archive_password: None,
                            skip_verify: pp_defaults.skip_verify,
                        };
                        match post_process(pp_config).await {
                            Ok(report) => {
                                emit(BackendEvent::PostProcessDone {
                                    job_id: Some(job_id),
                                    report,
                                });
                            }
                            Err(e) => {
                                emit(BackendEvent::PostProcessFailed {
                                    job_id: Some(job_id),
                                    error: e.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            Err(e) => {
                emit(BackendEvent::Error(format!("Engine error: {e}")));
            }
        }

        {
            let mut current = current_job.lock().expect("current_job lock");
            *current = None;
        }
        {
            let mut speed = speed_tracker.lock().expect("speed lock");
            speed.remove(&job_id);
        }
        {
            cancel_tokens
                .lock()
                .expect("cancel_tokens lock")
                .remove(&job_id);
        }

        // Refresh jobs list + auto-start next queued job.
        match queue.list_jobs().await {
            Ok(jobs) => {
                emit(BackendEvent::JobsList(jobs.clone()));

                if let Some(next) = jobs.iter().find(|j| j.state == JobState::Queued) {
                    tracing::info!(next_job_id = next.id, "auto-starting next queued job");
                    let next_id = next.id;
                    let next_output = next.output_dir.clone();

                    let (next_servers, next_conn) = {
                        let c = config.read().expect("config lock");
                        (c.server_configs(), c.max_connections)
                    };
                    if next_servers.is_empty() {
                        tracing::warn!("no servers configured, skipping auto-start");
                    } else {
                        let cancel = tokio_util::sync::CancellationToken::new();
                        cancel_tokens
                            .lock()
                            .expect("cancel_tokens lock")
                            .insert(next_id, cancel.clone());
                        speed_tracker
                            .lock()
                            .expect("speed lock")
                            .insert(next_id, (Instant::now(), 0));
                        current_job
                            .lock()
                            .expect("current_job lock")
                            .replace(next_id);

                        spawn_engine_task(
                            next_id,
                            next_output,
                            next_servers,
                            next_conn,
                            queue.clone(),
                            config.clone(),
                            event_tx.clone(),
                            ctx.clone(),
                            speed_tracker.clone(),
                            current_job.clone(),
                            cancel_tokens.clone(),
                            selected_job.clone(),
                            cancel,
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "refresh jobs failed");
            }
        }
    });
}
