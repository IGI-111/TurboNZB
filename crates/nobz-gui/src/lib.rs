//! Nobz GUI library (eframe app).
//!
//! Exposed as a library so the binary is a thin shim and tests can drive the
//! app state directly.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod backend;
pub mod queue_tab;
pub mod search_tab;
pub mod settings;
pub mod settings_tab;
pub mod theme;
pub mod win95_scroll;
pub mod win95_widgets;
pub mod wizard;

pub type Result<T> = std::result::Result<T, anyhow::Error>;

use std::sync::Arc;

use eframe::egui;

use crate::backend::{Backend, BackendEvent, BackendHandle};
use crate::queue_tab::QueueState;
use crate::search_tab::SearchState;
use crate::settings::AppConfig;
use crate::settings_tab::SettingsState;
use crate::theme::{Icons, apply_theme};
use crate::win95_widgets::{Win95TabButton, status_segment};
use crate::wizard::Wizard;
use nobz_core::queue::JobState;

const ORG_NAME: &str = "nobz";
const APP_NAME: &str = "nobz";

/// Tab identifiers for the main view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Search,
    Queue,
    Settings,
}

/// The main eframe application.
pub struct NobzApp {
    config: AppConfig,
    config_path: std::path::PathBuf,
    backend: BackendHandle,
    /// Held to keep the runtime alive.
    _runtime: Arc<tokio::runtime::Runtime>,
    wizard: Option<Wizard>,
    tab: Tab,
    /// Whether we've received the initial job list from the backend.
    /// Used to switch to Search tab if the queue is empty on first load.
    initial_jobs_received: bool,
    search: SearchState,
    queue: QueueState,
    settings: SettingsState,
    icons: Option<Icons>,
}

impl NobzApp {
    /// Create the app, loading config (or starting the wizard) and spawning
    /// the backend.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Apply Win95 theme + load icons.
        apply_theme(&cc.egui_ctx);
        let icons = Icons::load(&cc.egui_ctx);

        let dirs =
            directories::ProjectDirs::from("", ORG_NAME, APP_NAME).expect("resolve config dir");
        let config_path = AppConfig::config_path(&dirs);
        let config = AppConfig::load(&config_path).unwrap_or_else(|| AppConfig::defaults(&dirs));
        let wizard = if config.is_configured() {
            None
        } else {
            Some(Wizard::default())
        };

        let (backend, runtime) = Backend::spawn(config.clone(), cc.egui_ctx.clone());

        // Request an initial job list.
        backend.send(backend::BackendCmd::RefreshJobs);

        Self {
            config,
            config_path,
            backend,
            _runtime: Arc::new(runtime),
            wizard,
            tab: Tab::Queue,
            initial_jobs_received: false,
            search: SearchState::default(),
            queue: QueueState::default(),
            settings: SettingsState::default(),
            icons: Some(icons),
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::error!("Failed to save settings: {e}");
        }
        // Push updated config to the backend.
        self.backend.send(backend::BackendCmd::SetConfig(Box::new(
            self.config.clone(),
        )));
    }

    fn handle_events(&mut self) {
        let events = self.backend.drain();
        self.search.handle_events(&events);
        self.queue.handle_events(&events, &self.backend);
        self.settings.handle_events(&events);
        if let Some(ref mut wizard) = self.wizard {
            wizard.handle_events(&events);
        }
        for ev in &events {
            match ev {
                BackendEvent::Error(msg) => {
                    tracing::warn!("{msg}");
                }
                BackendEvent::PostProcessFailed { job_id, error } => {
                    tracing::warn!("Post-process failed (job {job_id:?}): {error}");
                }
                BackendEvent::JobsList(jobs) => {
                    // On first job list: switch to Search if queue is empty.
                    if !self.initial_jobs_received && jobs.is_empty() {
                        self.initial_jobs_received = true;
                        self.tab = Tab::Search;
                    } else if !self.initial_jobs_received {
                        self.initial_jobs_received = true;
                    }
                }
                _ => {}
            }
        }
    }

    /// Render the status bar at the bottom.
    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let speed_text = if self.queue.current_job_id.is_some() {
                format!("{}/s", format_speed(self.queue.current_speed as u64))
            } else {
                "Idle".to_string()
            };
            let speed_icon = if self.queue.current_job_id.is_some() {
                self.icons.as_ref().map(|i| i.tb_network.clone())
            } else {
                None
            };
            status_segment(ui, 200.0, 22.0, &speed_text, speed_icon);

            let job_count = self.queue.jobs.len();
            let jobs_text = format!("{job_count} job(s)");
            status_segment(ui, 120.0, 22.0, &jobs_text, None);
        });
    }
}

impl eframe::App for NobzApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll backend events every frame.
        self.handle_events();

        // Only request periodic repaints when there's an active download
        // or post-processing — avoids keeping the app awake when idle
        // (which causes OS "stalled" notifications when unfocused).
        let has_active = self.queue.current_job_id.is_some()
            || !self.queue.pp_in_progress.is_empty()
            || self
                .queue
                .jobs
                .iter()
                .any(|j| matches!(j.state, JobState::Downloading | JobState::Fetching));
        if has_active {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        // Wizard takes over the whole window if active.
        if let Some(ref mut wizard) = self.wizard {
            if wizard.done {
                self.wizard = None;
                self.save_config();
            } else {
                let config = &mut self.config;
                let backend = &self.backend;
                let result = egui::CentralPanel::default()
                    .show(ctx, |ui| wizard::ui(ui, wizard, config, backend));
                if result.inner {
                    self.save_config();
                    self.wizard = None;
                }
                return;
            }
        }

        // --- Tab bar ---
        egui::TopBottomPanel::top("tabs")
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(crate::theme::colors::BUTTON_FACE))
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    let search_icon = self.icons.as_ref().map(|i| i.tab_search.clone());
                    let queue_icon = self.icons.as_ref().map(|i| i.tab_download.clone());
                    let settings_icon = self.icons.as_ref().map(|i| i.tab_settings.clone());

                    if ui
                        .add(Win95TabButton::new(
                            search_icon,
                            "Search",
                            self.tab == Tab::Search,
                        ))
                        .clicked()
                    {
                        self.tab = Tab::Search;
                    }
                    if ui
                        .add(Win95TabButton::new(
                            queue_icon,
                            "Queue",
                            self.tab == Tab::Queue,
                        ))
                        .clicked()
                    {
                        self.tab = Tab::Queue;
                    }
                    if ui
                        .add(Win95TabButton::new(
                            settings_icon,
                            "Settings",
                            self.tab == Tab::Settings,
                        ))
                        .clicked()
                    {
                        self.tab = Tab::Settings;
                    }
                });
            });

        // --- Bottom: status bar ---
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(24.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(crate::theme::colors::BUTTON_FACE))
            .show(ctx, |ui| {
                self.status_bar(ui);
            });

        // --- Main content ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(crate::theme::colors::BUTTON_FACE))
            .show(ctx, |ui| match self.tab {
                Tab::Search => {
                    search_tab::ui(
                        ui,
                        &mut self.search,
                        &self.backend,
                        &self.config,
                        self.icons.as_ref(),
                    );
                }
                Tab::Queue => {
                    queue_tab::ui(ui, &mut self.queue, &self.backend, self.icons.as_ref());
                }
                Tab::Settings => {
                    if settings_tab::ui(ui, &mut self.settings, &mut self.config, &self.backend) {
                        self.save_config();
                    }
                }
            });
    }
}

/// Format a speed (bytes/sec) as a human-readable string.
fn format_speed(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return "0".into();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes_per_sec as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes_per_sec} B")
    }
}

/// Entry point — launches the eframe app.
pub fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("nobz GUI starting");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "nobz",
        native_options,
        Box::new(|cc| Ok(Box::new(NobzApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
