//! First-run wizard: shown when no config file exists. Guides the user
//! through adding at least one NNTP server and one indexer with inline
//! connection tests.

use turbonzb_core::nntp::ServerConfig;
use turbonzb_index::types::IndexerConfig;

use crate::backend::{BackendCmd, BackendEvent, BackendHandle};
use crate::settings::{AppConfig, ServerEntry};
use crate::win95_widgets::{Win95Button, Win95Checkbox, group};

/// Wizard state.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub step: WizardStep,
    /// Server being configured.
    pub server: ServerEntry,
    /// Indexer being configured.
    pub indexer: IndexerConfig,
    pub server_test_msg: Option<(String, bool)>,
    pub indexer_test_msg: Option<(String, bool)>,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Welcome,
    AddServer,
    TestServer,
    AddIndexer,
    TestIndexer,
    Finish,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: WizardStep::Welcome,
            server: ServerEntry {
                host: String::new(),
                port: 563,
                tls: true,
                user: None,
                password: None,
                max_connections: 8,
                priority: 0,
            },
            indexer: IndexerConfig {
                name: String::new(),
                url: String::new(),
                api_key: String::new(),
                max_concurrent: 1,
                timeout_s: 15,
                priority: 0,
            },
            server_test_msg: None,
            indexer_test_msg: None,
            done: false,
        }
    }
}

impl Wizard {
    pub fn handle_events(&mut self, events: &[BackendEvent]) {
        for ev in events {
            match ev {
                BackendEvent::ServerTestResult { host, ok, message } => {
                    self.server_test_msg = Some((format!("{host}: {message}"), *ok));
                    if *ok && self.step == WizardStep::TestServer {
                        self.step = WizardStep::AddIndexer;
                    }
                }
                BackendEvent::IndexerTestResult { name, ok, message } => {
                    self.indexer_test_msg = Some((format!("{name}: {message}"), *ok));
                    if *ok && self.step == WizardStep::TestIndexer {
                        self.step = WizardStep::Finish;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Render the wizard. Returns `true` when the wizard is finished and the
/// config should be saved.
pub fn ui(
    ui: &mut egui::Ui,
    wizard: &mut Wizard,
    config: &mut AppConfig,
    backend: &BackendHandle,
) -> bool {
    let mut finished = false;
    egui::CentralPanel::default().show_inside(ui, |ui| {
        // Center the wizard content.
        let available = ui.available_width();
        let wizard_w = (available * 0.6).min(600.0);
        let wizard_h = ui.available_height() * 0.8;

        ui.vertical_centered(|ui| {
            ui.add_space(20.0);

            group(ui, Some("TurboNZB Setup Wizard"), |ui| {
                ui.set_min_width(wizard_w);
                ui.set_min_height(wizard_h);
                ui.add_space(10.0);

                ui.vertical_centered(|ui| {
                    ui.heading("Welcome to TurboNZB");
                    ui.label("Usenet search & downloader for your desktop.");

                    match wizard.step {
                        WizardStep::Welcome => {
                            ui.add_space(20.0);
                            ui.label("Let's set up your NNTP server and a search indexer.");
                            ui.add_space(10.0);
                            if ui.add(Win95Button::new("Get started")).clicked() {
                                wizard.step = WizardStep::AddServer;
                            }
                        }
                        WizardStep::AddServer => {
                            ui.add_space(10.0);
                            ui.heading("Step 1: NNTP Server");
                            server_form(ui, wizard);
                            ui.add_space(8.0);
                            if ui.add(Win95Button::new("Test connection")).clicked() {
                                let cfg: ServerConfig = (&wizard.server).into();
                                backend.send(BackendCmd::TestServer { config: cfg });
                                wizard.step = WizardStep::TestServer;
                                wizard.server_test_msg = None;
                            }
                        }
                        WizardStep::TestServer => {
                            ui.add_space(10.0);
                            ui.heading("Step 1: Testing NNTP Server...");
                            server_form(ui, wizard);
                            ui.add_space(8.0);
                            if ui.add(Win95Button::new("Retry")).clicked() {
                                let cfg: ServerConfig = (&wizard.server).into();
                                backend.send(BackendCmd::TestServer { config: cfg });
                            }
                            if let Some((msg, ok)) = &wizard.server_test_msg {
                                let color = if *ok {
                                    egui::Color32::from_rgb(0, 128, 0)
                                } else {
                                    egui::Color32::RED
                                };
                                ui.colored_label(color, msg);
                            }
                        }
                        WizardStep::AddIndexer => {
                            ui.add_space(10.0);
                            ui.heading("Step 2: Newznab Indexer");
                            indexer_form(ui, wizard);
                            ui.add_space(8.0);
                            if ui.add(Win95Button::new("Test indexer")).clicked() {
                                backend.send(BackendCmd::TestIndexer {
                                    config: wizard.indexer.clone(),
                                });
                                wizard.step = WizardStep::TestIndexer;
                                wizard.indexer_test_msg = None;
                            }
                            if ui.add(Win95Button::new("Skip (add later)")).clicked() {
                                wizard.step = WizardStep::Finish;
                            }
                        }
                        WizardStep::TestIndexer => {
                            ui.add_space(10.0);
                            ui.heading("Step 2: Testing Indexer...");
                            indexer_form(ui, wizard);
                            ui.add_space(8.0);
                            if ui.add(Win95Button::new("Retry")).clicked() {
                                backend.send(BackendCmd::TestIndexer {
                                    config: wizard.indexer.clone(),
                                });
                            }
                            if ui.add(Win95Button::new("Skip")).clicked() {
                                wizard.step = WizardStep::Finish;
                            }
                            if let Some((msg, ok)) = &wizard.indexer_test_msg {
                                let color = if *ok {
                                    egui::Color32::from_rgb(0, 128, 0)
                                } else {
                                    egui::Color32::RED
                                };
                                ui.colored_label(color, msg);
                            }
                        }
                        WizardStep::Finish => {
                            ui.add_space(10.0);
                            ui.heading("Setup complete!");
                            ui.label("Your settings will be saved. You can change them anytime in the Settings tab.");
                            ui.add_space(8.0);
                            if ui.add(Win95Button::new("Start using TurboNZB")).clicked() {
                                if !wizard.server.host.is_empty() {
                                    config.servers.push(wizard.server.clone());
                                }
                                if !wizard.indexer.url.is_empty() {
                                    config.indexers.push(wizard.indexer.clone());
                                }
                                wizard.done = true;
                                finished = true;
                            }
                        }
                    }
                });
            });
        });
    });
    finished
}

fn server_form(ui: &mut egui::Ui, wizard: &mut Wizard) {
    ui.horizontal(|ui| {
        ui.label("Host:");
        ui.text_edit_singleline(&mut wizard.server.host);
        ui.label("Port:");
        ui.add(egui::DragValue::new(&mut wizard.server.port).range(1..=65535));
        ui.add(Win95Checkbox::new(&mut wizard.server.tls, "TLS"));
    });
    ui.horizontal(|ui| {
        let mut user = wizard.server.user.clone().unwrap_or_default();
        ui.label("User:");
        ui.text_edit_singleline(&mut user);
        wizard.server.user = if user.is_empty() { None } else { Some(user) };
        let mut pass = wizard.server.password.clone().unwrap_or_default();
        ui.label("Pass:");
        ui.text_edit_singleline(&mut pass);
        wizard.server.password = if pass.is_empty() { None } else { Some(pass) };
    });
    ui.horizontal(|ui| {
        ui.label("Connections:");
        ui.add(egui::DragValue::new(&mut wizard.server.max_connections).range(1..=100));
    });
}

fn indexer_form(ui: &mut egui::Ui, wizard: &mut Wizard) {
    ui.horizontal(|ui| {
        ui.label("Name:");
        ui.text_edit_singleline(&mut wizard.indexer.name);
    });
    ui.horizontal(|ui| {
        ui.label("URL:");
        ui.text_edit_singleline(&mut wizard.indexer.url);
        ui.label("(e.g. https://api.example.com/api)");
    });
    ui.horizontal(|ui| {
        ui.label("API Key:");
        ui.text_edit_singleline(&mut wizard.indexer.api_key);
    });
}
