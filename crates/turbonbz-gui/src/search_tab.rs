//! Search tab: unified search bar, filters, results table, and
//! "Send to queue" action.

use turbonbz_index::AggregatedResult;
use turbonbz_index::types::SearchQuery;

use crate::backend::{BackendCmd, BackendEvent, BackendHandle};
use crate::settings::AppConfig;
use crate::theme::Icons;
use crate::win95_scroll::Win95Table;
use crate::win95_widgets::Win95Button;

/// State for the search tab.
#[derive(Debug, Clone)]
pub struct SearchState {
    pub query: String,
    pub category: String,
    pub results: Vec<AggregatedResult>,
    pub status: SearchStatus,
    pub sort: SortState,
    /// Indices of results that have been sent to download (to disable
    /// their download button).
    pub downloaded: std::collections::HashSet<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Title,
    Sources,
    Size,
    Age,
    Category,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    pub col: SortColumn,
    pub asc: bool,
}

impl Default for SortState {
    fn default() -> Self {
        Self {
            col: SortColumn::Age,
            asc: false, // newest first
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    Idle,
    Searching,
    Results(usize),
    Error(String),
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            category: "All".into(),
            results: Vec::new(),
            status: SearchStatus::Idle,
            sort: SortState::default(),
            downloaded: std::collections::HashSet::new(),
        }
    }
}

impl SearchState {
    /// Process backend events relevant to search.
    pub fn handle_events(&mut self, events: &[BackendEvent]) {
        for ev in events {
            match ev {
                BackendEvent::SearchResults(results) => {
                    self.results = results.clone();
                    self.downloaded.clear();
                    self.status = SearchStatus::Results(results.len());
                    self.sort_results();
                }
                BackendEvent::SearchFailed(err) => {
                    self.status = SearchStatus::Error(err.clone());
                }
                _ => {}
            }
        }
    }

    /// Build the current search query from the UI state.
    pub fn build_query(&self) -> SearchQuery {
        let mut q = SearchQuery::text(self.query.trim());
        q.limit = 200;
        if self.category != "All" {
            q.categories = match self.category.as_str() {
                "Movies" => vec![2000],
                "TV" => vec![5000],
                "Audio" => vec![3000],
                "PC" => vec![4000],
                "Books" => vec![7000],
                "Console" => vec![1000],
                "XXX" => vec![6000],
                _ => vec![],
            };
        }
        q
    }

    /// Sort results by the current sort column/direction.
    fn sort_results(&mut self) {
        let asc = self.sort.asc;
        self.results.sort_by(|a, b| {
            let ord = match self.sort.col {
                SortColumn::Title => a.result.title.cmp(&b.result.title),
                SortColumn::Sources => a.sources.len().cmp(&b.sources.len()),
                SortColumn::Size => a.result.size.cmp(&b.result.size),
                SortColumn::Age => a.result.post_date.cmp(&b.result.post_date),
                SortColumn::Category => a.result.category.cmp(&b.result.category),
            };
            if asc { ord } else { ord.reverse() }
        });
    }
}

/// Render the search tab.
pub fn ui(
    ui: &mut egui::Ui,
    state: &mut SearchState,
    backend: &BackendHandle,
    config: &AppConfig,
    icons: Option<&Icons>,
) {
    // Use a frame with horizontal margins so content isn't flush against
    // the window edges, but still gets the full vertical space.
    egui::Frame::none()
        .inner_margin(egui::Margin {
            left: 8.0,
            right: 8.0,
            top: 4.0,
            bottom: 4.0,
        })
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width() - 8.0);
            // Search bar
            ui.horizontal(|ui| {
                ui.label("Search:");
                let response = ui.text_edit_singleline(&mut state.query);
                ui.label("Category:");
                egui::ComboBox::from_label("")
                    .selected_text(&state.category)
                    .show_ui(ui, |ui| {
                        for cat in [
                            "All", "Movies", "TV", "Audio", "PC", "Books", "Console", "XXX",
                        ] {
                            ui.selectable_value(&mut state.category, cat.to_string(), cat);
                        }
                    });
                let searching = state.status == SearchStatus::Searching;

                let enter_pressed =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if enter_pressed
                    || ui
                        .add_enabled(!searching, Win95Button::new("Search"))
                        .clicked()
                {
                    if state.query.trim().is_empty() {
                        state.status = SearchStatus::Error("Enter a search query".into());
                    } else {
                        state.status = SearchStatus::Searching;
                        backend.send(BackendCmd::Search {
                            query: state.build_query(),
                        });
                    }
                }
            });

            crate::win95_widgets::etched_separator(ui, true);

            // Status line
            match &state.status {
                SearchStatus::Idle => {
                    ui.label("Enter a query and click Search.");
                }
                SearchStatus::Searching => {
                    ui.label("Searching...");
                }
                SearchStatus::Results(n) => {
                    ui.label(format!("{n} results"));
                }
                SearchStatus::Error(e) => {
                    ui.colored_label(egui::Color32::RED, format!("Error: {e}"));
                }
            }

            crate::win95_widgets::etched_separator(ui, true);

            // Extract sort state into a local so the header closure can
            // modify it without borrowing state mutably.
            let mut sort = state.sort;
            // Pre-sort results for the body closure.
            let mut results = state.results.clone();
            {
                let asc = sort.asc;
                results.sort_by(|a, b| {
                    let ord = match sort.col {
                        SortColumn::Title => a.result.title.cmp(&b.result.title),
                        SortColumn::Sources => a.sources.len().cmp(&b.sources.len()),
                        SortColumn::Size => a.result.size.cmp(&b.result.size),
                        SortColumn::Age => a.result.post_date.cmp(&b.result.post_date),
                        SortColumn::Category => a.result.category.cmp(&b.result.category),
                    };
                    if asc { ord } else { ord.reverse() }
                });
            }

            // Results table
            Win95Table::new()
                .striped(true)
                .id_salt("search_results")
                .min_scrolled_height(ui.available_height())
                .column_remainder()
                .column(100.0)
                .column(80.0)
                .column(50.0)
                .column(80.0)
                .column(50.0)
                .column(36.0)
                .header_body(
                    ui,
                    24.0,
                    |row, ui| {
                        row.col(ui, |ui| {
                            sort_button(ui, &mut sort, SortColumn::Title, "Title");
                        });
                        row.col(ui, |ui| {
                            sort_button(ui, &mut sort, SortColumn::Sources, "Sources");
                        });
                        row.col(ui, |ui| {
                            sort_button(ui, &mut sort, SortColumn::Size, "Size");
                        });
                        row.col(ui, |ui| {
                            ui.strong("Parts");
                        });
                        row.col(ui, |ui| {
                            sort_button(ui, &mut sort, SortColumn::Category, "Cat");
                        });
                        row.col(ui, |ui| {
                            sort_button(ui, &mut sort, SortColumn::Age, "Age");
                        });
                        row.col(ui, |ui| {
                            ui.strong("");
                        });
                    },
                    |body| {
                        let downloaded = state.downloaded.clone();
                        for (i, result) in results.iter().enumerate() {
                            let already_downloaded = downloaded.contains(&i);
                            let row_height = 22.0;
                            body.row(row_height, |row, ui| {
                                row.col(ui, |ui| {
                                    ui.label(&result.result.title);
                                });
                                row.col(ui, |ui| {
                                    ui.label(result.sources.join(", "));
                                });
                                row.col(ui, |ui| {
                                    ui.label(format_size(result.result.size));
                                });
                                row.col(ui, |ui| {
                                    if result.result.files > 0 {
                                        ui.label(result.result.files.to_string());
                                    } else {
                                        ui.label("?");
                                    }
                                });
                                row.col(ui, |ui| {
                                    ui.label(&result.result.category_name);
                                });
                                row.col(ui, |ui| {
                                    ui.label(format_age(result.result.post_date));
                                });
                                row.col(ui, |ui| {
                                    if let Some(icons) = icons {
                                        if already_downloaded {
                                            ui.add(
                                                crate::win95_widgets::Win95IconButton::new(
                                                    icons.tb_tick.clone(),
                                                )
                                                .enabled(false)
                                                .tooltip("Added to queue"),
                                            );
                                        } else {
                                            if ui
                                                .add(
                                                    crate::win95_widgets::Win95IconButton::new(
                                                        icons.tb_download.clone(),
                                                    )
                                                    .tooltip("Download"),
                                                )
                                                .clicked()
                                            {
                                                let url = result.result.nzb_url.clone();
                                                let title = result.result.title.clone();
                                                let category = if state.category == "All" {
                                                    None
                                                } else {
                                                    Some(state.category.clone())
                                                };
                                                let download_dir = config.download_dir.clone();
                                                backend.send(BackendCmd::DownloadFromUrl {
                                                    url,
                                                    title,
                                                    download_dir,
                                                    category,
                                                });
                                                state.downloaded.insert(i);
                                            }
                                        }
                                    } else {
                                        if ui
                                            .add_enabled(
                                                !already_downloaded,
                                                Win95Button::new("Download"),
                                            )
                                            .clicked()
                                        {
                                            let url = result.result.nzb_url.clone();
                                            let title = result.result.title.clone();
                                            let category = if state.category == "All" {
                                                None
                                            } else {
                                                Some(state.category.clone())
                                            };
                                            let download_dir = config.download_dir.clone();
                                            backend.send(BackendCmd::DownloadFromUrl {
                                                url,
                                                title,
                                                download_dir,
                                                category,
                                            });
                                            state.downloaded.insert(i);
                                        }
                                    }
                                });
                            });
                        }
                    },
                );

            // Write back sort state and re-sort state.results so the next
            // frame reflects the new ordering.
            if state.sort != sort {
                state.sort = sort;
                state.sort_results();
            }
        });
}

/// Render a sortable column header. Clicking toggles sort direction, or
/// switches to that column.
fn sort_button(ui: &mut egui::Ui, sort: &mut SortState, col: SortColumn, label: &str) {
    let is_active = sort.col == col;
    let arrow = if is_active {
        if sort.asc { " ^" } else { " v" }
    } else {
        ""
    };
    let text = format!("{label}{arrow}");
    let text_color = if is_active {
        egui::Color32::BLACK
    } else {
        egui::Color32::from_rgb(60, 60, 60)
    };
    let painter = ui.painter().clone();
    let galley = painter.layout_no_wrap(text.clone(), egui::FontId::proportional(16.0), text_color);
    let text_size = galley.size();
    let (rect, resp) =
        ui.allocate_exact_size(text_size + egui::vec2(8.0, 4.0), egui::Sense::click());
    let painter = ui.painter();
    if is_active {
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(200, 200, 200));
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        &text,
        egui::FontId::proportional(16.0),
        text_color,
    );
    if resp.clicked() {
        if is_active {
            sort.asc = !sort.asc;
        } else {
            sort.col = col;
            sort.asc = false;
        }
    }
}

/// Format a byte count as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "?".into();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Format a Unix timestamp as a relative age (e.g. "3d", "2w", "1mo").
fn format_age(post_date: u64) -> String {
    if post_date == 0 {
        return "?".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if post_date > now {
        return "?".into();
    }
    let delta = now - post_date;
    let days = delta / 86400;
    if days == 0 {
        format!("{}h", delta / 3600)
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}
