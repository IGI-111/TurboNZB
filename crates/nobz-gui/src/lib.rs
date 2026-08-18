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
use crate::wizard::Wizard;

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
    search: SearchState,
    queue: QueueState,
    settings: SettingsState,
    icons: Option<Icons>,
    /// Transient error/status messages shown at the bottom.
    toasts: Vec<String>,
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
            tab: Tab::Search,
            search: SearchState::default(),
            queue: QueueState::default(),
            settings: SettingsState::default(),
            icons: Some(icons),
            toasts: Vec::new(),
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            self.toasts.push(format!("Failed to save settings: {e}"));
        } else {
            self.toasts.push("Settings saved.".into());
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
                    self.toasts.push(msg.clone());
                }
                BackendEvent::PostProcessFailed { job_id, error } => {
                    self.toasts
                        .push(format!("Post-process failed (job {job_id:?}): {error}"));
                }
                _ => {}
            }
        }
    }

    /// Render a tab button with an icon + label.
    fn tab_button(
        ui: &mut egui::Ui,
        icon: Option<&egui::TextureHandle>,
        label: &str,
        is_selected: bool,
    ) -> bool {
        let icon_img =
            icon.map(|t| egui::Image::from_texture(t).fit_to_exact_size(egui::vec2(24.0, 24.0)));
        let btn = egui::Button::opt_image_and_text(icon_img, Some(label.into()));
        if is_selected {
            // Subtle: slightly lighter background + thin bottom border,
            // like a Win95 pressed tab.
            ui.add(
                btn.fill(egui::Color32::from_rgb(223, 223, 223))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        egui::Color32::from_rgb(128, 128, 128),
                    )),
            )
            .clicked()
        } else {
            ui.add(btn).clicked()
        }
    }
}

impl eframe::App for NobzApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll backend events every frame.
        self.handle_events();

        // Auto-refresh jobs every 2 seconds while there's an active download.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

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

        // Top tab bar with icons.
        let search_icon = self.icons.as_ref().map(|i| &i.search);
        let folder_icon = self.icons.as_ref().map(|i| &i.folder_open);
        let settings_icon = self.icons.as_ref().map(|i| &i.settings);
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if Self::tab_button(ui, search_icon, "Search", self.tab == Tab::Search) {
                    self.tab = Tab::Search;
                }
                if Self::tab_button(ui, folder_icon, "Queue", self.tab == Tab::Queue) {
                    self.tab = Tab::Queue;
                }
                if Self::tab_button(ui, settings_icon, "Settings", self.tab == Tab::Settings) {
                    self.tab = Tab::Settings;
                }
            });
        });

        // Bottom status bar for toasts.
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.toasts.is_empty() {
                    ui.label("Ready.");
                } else {
                    // Show the most recent toast.
                    ui.label(&self.toasts[self.toasts.len() - 1]);
                }
            });
            // Decay toasts.
            if !self.toasts.is_empty() {
                self.toasts.remove(0);
            }
        });

        // Main content.
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Search => {
                search_tab::ui(ui, &mut self.search, &self.backend, &self.config);
            }
            Tab::Queue => {
                queue_tab::ui(ui, &mut self.queue, &self.backend);
            }
            Tab::Settings => {
                if settings_tab::ui(ui, &mut self.settings, &mut self.config, &self.backend) {
                    self.save_config();
                }
            }
        });
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
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([700.0, 400.0]),
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
