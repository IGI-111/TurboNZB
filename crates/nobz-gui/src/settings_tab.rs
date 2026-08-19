//! Settings tab: manage NNTP servers, indexers, download directories,
//! category mappings, and post-processing defaults.

use std::path::PathBuf;

use nobz_core::nntp::ServerConfig;
use nobz_index::types::IndexerConfig;

use crate::backend::{BackendCmd, BackendEvent, BackendHandle};
use crate::settings::{AppConfig, CategoryMapping, PostProcessDefaults, ServerEntry};
use crate::win95_widgets::{Win95Button, Win95Checkbox, group};

/// State for the settings tab.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Inline test result messages.
    pub indexer_test_msg: Option<(String, bool)>,
    pub server_test_msg: Option<(String, bool)>,
    /// Editing buffers for new server.
    pub new_server: ServerEntry,
    /// Editing buffers for new indexer.
    pub new_indexer: IndexerConfig,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            indexer_test_msg: None,
            server_test_msg: None,
            new_server: ServerEntry::default(),
            new_indexer: IndexerConfig {
                name: String::new(),
                url: String::new(),
                api_key: String::new(),
                max_concurrent: 1,
                timeout_s: 15,
                priority: 0,
            },
        }
    }
}

impl SettingsState {
    pub fn handle_events(&mut self, events: &[BackendEvent]) {
        for ev in events {
            match ev {
                BackendEvent::IndexerTestResult { name, ok, message } => {
                    self.indexer_test_msg = Some((format!("{name}: {message}"), *ok));
                }
                BackendEvent::ServerTestResult { host, ok, message } => {
                    self.server_test_msg = Some((format!("{host}: {message}"), *ok));
                }
                _ => {}
            }
        }
    }
}

/// Render the settings tab. Returns `true` if config was changed and should
/// be saved.
pub fn ui(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    config: &mut AppConfig,
    backend: &BackendHandle,
) -> bool {
    let mut changed = false;

    crate::win95_scroll::vertical(ui, "settings_scroll", |ui| {
        egui::Frame::none()
            .inner_margin(egui::Margin {
                left: 8.0,
                right: 8.0,
                top: 4.0,
                bottom: 4.0,
            })
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - 8.0);
                // --- NNTP Servers ---
                group(ui, Some("NNTP Servers"), |ui| {
                    ui.label("Servers are tried in priority order (lower = first).");
                    ui.add_space(4.0);

                    let mut to_remove: Option<usize> = None;
                    for (i, server) in config.servers.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label("Host:");
                            ui.add(
                                egui::TextEdit::singleline(&mut server.host).desired_width(160.0),
                            );
                            ui.label("Port:");
                            ui.add(egui::DragValue::new(&mut server.port).range(1..=65535));
                            ui.add(Win95Checkbox::new(&mut server.tls, "TLS"));
                        });
                        ui.horizontal(|ui| {
                            let mut user = server.user.clone().unwrap_or_default();
                            ui.label("User:");
                            ui.add(egui::TextEdit::singleline(&mut user).desired_width(120.0));
                            server.user = if user.is_empty() { None } else { Some(user) };
                            let mut pass = server.password.clone().unwrap_or_default();
                            ui.label("Pass:");
                            ui.add(egui::TextEdit::singleline(&mut pass).desired_width(120.0));
                            server.password = if pass.is_empty() { None } else { Some(pass) };
                        });
                        ui.horizontal(|ui| {
                            ui.label("Connections:");
                            ui.add(
                                egui::DragValue::new(&mut server.max_connections).range(1..=100),
                            );
                            ui.label("Priority:");
                            ui.add(egui::DragValue::new(&mut server.priority).range(0..=100));
                            let cfg: ServerConfig = (server as &ServerEntry).into();
                            if ui.add(Win95Button::new("Test")).clicked() {
                                backend.send(BackendCmd::TestServer { config: cfg });
                            }
                            if ui.add(Win95Button::new("Remove")).clicked() {
                                to_remove = Some(i);
                            }
                        });
                        ui.add_space(4.0);
                    }
                    if let Some(i) = to_remove {
                        config.servers.remove(i);
                        changed = true;
                    }

                    if let Some((msg, ok)) = &state.server_test_msg {
                        let color = if *ok {
                            egui::Color32::from_rgb(0, 128, 0)
                        } else {
                            egui::Color32::RED
                        };
                        ui.colored_label(color, msg);
                    }
                });

                ui.add_space(8.0);

                // Add new server
                group(ui, Some("Add Server"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Host:");
                        ui.text_edit_singleline(&mut state.new_server.host);
                        ui.label("Port:");
                        ui.add(egui::DragValue::new(&mut state.new_server.port).range(1..=65535));
                        ui.add(Win95Checkbox::new(&mut state.new_server.tls, "TLS"));
                    });
                    ui.horizontal(|ui| {
                        let mut user = state.new_server.user.clone().unwrap_or_default();
                        ui.label("User:");
                        ui.text_edit_singleline(&mut user);
                        state.new_server.user = if user.is_empty() { None } else { Some(user) };
                        let mut pass = state.new_server.password.clone().unwrap_or_default();
                        ui.label("Pass:");
                        ui.text_edit_singleline(&mut pass);
                        state.new_server.password = if pass.is_empty() { None } else { Some(pass) };
                    });
                    ui.horizontal(|ui| {
                        ui.label("Connections:");
                        ui.add(
                            egui::DragValue::new(&mut state.new_server.max_connections)
                                .range(1..=100),
                        );
                        ui.label("Priority:");
                        ui.add(egui::DragValue::new(&mut state.new_server.priority).range(0..=100));
                    });
                    ui.horizontal(|ui| {
                        let cfg: ServerConfig = (&state.new_server).into();
                        if ui.add(Win95Button::new("Test")).clicked() {
                            backend.send(BackendCmd::TestServer { config: cfg });
                        }
                        if ui.add(Win95Button::new("Add")).clicked() {
                            config.servers.push(state.new_server.clone());
                            state.new_server = ServerEntry {
                                host: String::new(),
                                port: 563,
                                tls: true,
                                user: None,
                                password: None,
                                max_connections: 8,
                                priority: config.servers.len() as u32,
                            };
                            changed = true;
                        }
                    });
                });

                ui.add_space(8.0);

                // --- Indexers ---
                group(ui, Some("Newznab Indexers"), |ui| {
                    let mut to_remove_idx: Option<usize> = None;
                    for (i, indexer) in config.indexers.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            ui.add(
                                egui::TextEdit::singleline(&mut indexer.name).desired_width(120.0),
                            );
                            ui.label("Priority:");
                            ui.add(egui::DragValue::new(&mut indexer.priority).range(0..=100));
                        });
                        ui.horizontal(|ui| {
                            ui.label("URL:");
                            ui.add(
                                egui::TextEdit::singleline(&mut indexer.url).desired_width(240.0),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("API Key:");
                            ui.add(
                                egui::TextEdit::singleline(&mut indexer.api_key)
                                    .desired_width(200.0),
                            );
                            if ui.add(Win95Button::new("Test")).clicked() {
                                backend.send(BackendCmd::TestIndexer {
                                    config: indexer.clone(),
                                });
                            }
                            if ui.add(Win95Button::new("Remove")).clicked() {
                                to_remove_idx = Some(i);
                            }
                        });
                        ui.add_space(4.0);
                    }
                    if let Some(i) = to_remove_idx {
                        config.indexers.remove(i);
                        changed = true;
                    }

                    if let Some((msg, ok)) = &state.indexer_test_msg {
                        let color = if *ok {
                            egui::Color32::from_rgb(0, 128, 0)
                        } else {
                            egui::Color32::RED
                        };
                        ui.colored_label(color, msg);
                    }
                });

                ui.add_space(8.0);

                // Add new indexer
                group(ui, Some("Add Indexer"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.text_edit_singleline(&mut state.new_indexer.name);
                        ui.label("URL:");
                        ui.text_edit_singleline(&mut state.new_indexer.url);
                    });
                    ui.horizontal(|ui| {
                        ui.label("API Key:");
                        ui.text_edit_singleline(&mut state.new_indexer.api_key);
                        ui.label("Priority:");
                        ui.add(
                            egui::DragValue::new(&mut state.new_indexer.priority).range(0..=100),
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui.add(Win95Button::new("Test")).clicked() {
                            backend.send(BackendCmd::TestIndexer {
                                config: state.new_indexer.clone(),
                            });
                        }
                        if ui.add(Win95Button::new("Add")).clicked() {
                            config.indexers.push(state.new_indexer.clone());
                            state.new_indexer = IndexerConfig {
                                name: String::new(),
                                url: String::new(),
                                api_key: String::new(),
                                max_concurrent: 1,
                                timeout_s: 15,
                                priority: config.indexers.len() as u32,
                            };
                            changed = true;
                        }
                    });
                });

                ui.add_space(8.0);

                // --- Directories ---
                group(ui, Some("Directories"), |ui| {
                    let mut dl_dir = config.download_dir.to_string_lossy().to_string();
                    ui.horizontal(|ui| {
                        ui.label("Download dir:");
                        ui.text_edit_singleline(&mut dl_dir);
                    });
                    if dl_dir != config.download_dir.to_string_lossy() {
                        config.download_dir = PathBuf::from(dl_dir);
                        changed = true;
                    }
                    let mut comp_dir = config.completed_dir.to_string_lossy().to_string();
                    ui.horizontal(|ui| {
                        ui.label("Completed dir:");
                        ui.text_edit_singleline(&mut comp_dir);
                    });
                    if comp_dir != config.completed_dir.to_string_lossy() {
                        config.completed_dir = PathBuf::from(comp_dir);
                        changed = true;
                    }
                    ui.horizontal(|ui| {
                        ui.label("DB path:");
                        ui.label(config.db_path.display().to_string());
                    });
                    ui.horizontal(|ui| {
                        ui.label("Max connections (0 = use server totals):");
                        ui.add(egui::DragValue::new(&mut config.max_connections).range(0..=100));
                    });
                });

                ui.add_space(8.0);

                // --- Categories ---
                group(ui, Some("Category -> Subfolder Mapping"), |ui| {
                    let mut cat_remove: Option<usize> = None;
                    for (i, cat) in config.categories.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            ui.text_edit_singleline(&mut cat.name);
                            ui.label("Subfolder:");
                            ui.text_edit_singleline(&mut cat.subfolder);
                            if ui.add(Win95Button::new("Remove")).clicked() {
                                cat_remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = cat_remove {
                        config.categories.remove(i);
                        changed = true;
                    }
                    if ui.add(Win95Button::new("Add category")).clicked() {
                        config.categories.push(CategoryMapping {
                            name: String::new(),
                            subfolder: String::new(),
                        });
                        changed = true;
                    }
                });

                ui.add_space(8.0);

                // --- Post-processing defaults ---
                group(ui, Some("Post-processing Defaults"), |ui| {
                    let pp = &mut config.post_process;
                    if ui
                        .add(Win95Checkbox::new(
                            &mut pp.auto_post_process,
                            "Auto post-process after download",
                        ))
                        .changed()
                    {
                        changed = true;
                    }
                    if ui
                        .add(Win95Checkbox::new(
                            &mut pp.skip_verify,
                            "Skip PAR2 verification",
                        ))
                        .changed()
                    {
                        changed = true;
                    }
                    if ui
                        .add(Win95Checkbox::new(
                            &mut pp.cleanup_archives,
                            "Delete archives after unpack",
                        ))
                        .changed()
                    {
                        changed = true;
                    }
                });

                ui.add_space(12.0);

                // Save button
                if ui.add(Win95Button::new("Save settings")).clicked() {
                    changed = true;
                }
            });
    });

    changed
}

// Keep `PostProcessDefaults` referenced so we don't get an unused import
// when the fields are accessed through `config.post_process`.
#[allow(unused)]
fn _type_check(_pp: &PostProcessDefaults) {}
