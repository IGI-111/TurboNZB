//! Search tab: unified search bar, filters, results table, and
//! "Send to queue" action.

use egui_extras::{Column, TableBuilder};
use nobz_index::AggregatedResult;
use nobz_index::types::SearchQuery;

use crate::backend::{BackendCmd, BackendEvent, BackendHandle};
use crate::settings::AppConfig;

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
pub fn ui(ui: &mut egui::Ui, state: &mut SearchState, backend: &BackendHandle, config: &AppConfig) {
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

        // Trigger search on Enter key while focused in the text field.
        let enter_pressed = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

        if enter_pressed
            || ui
                .add_enabled(!searching, egui::Button::new("Search"))
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

    ui.separator();

    // Status line
    match &state.status {
        SearchStatus::Idle => {
            ui.label("Enter a query and click Search.");
        }
        SearchStatus::Searching => {
            ui.label("Searching…");
        }
        SearchStatus::Results(n) => {
            ui.label(format!("{n} results"));
        }
        SearchStatus::Error(e) => {
            ui.colored_label(egui::Color32::RED, format!("Error: {e}"));
        }
    }

    ui.separator();

    // Results table — Title width is computed from available space so it
    // stretches with the window while other columns stay fixed.
    let other_cols: f32 = 100.0 + 80.0 + 50.0 + 80.0 + 50.0 + 80.0; // sources+size+parts+cat+age+action
    let title_width = (ui.available_width() - other_cols - 16.0).max(150.0);

    let table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .column(Column::exact(title_width).clip(true)) // title (stretches with window)
        .column(Column::exact(100.0).clip(true)) // sources
        .column(Column::exact(80.0)) // size
        .column(Column::exact(50.0)) // parts
        .column(Column::exact(80.0).clip(true)) // cat
        .column(Column::exact(50.0)) // age
        .column(Column::exact(80.0)); // action

    table
        .header(24.0, |mut header| {
            header.col(|ui| {
                sort_button(ui, &mut state.sort, SortColumn::Title, "Title");
            });
            header.col(|ui| {
                sort_button(ui, &mut state.sort, SortColumn::Sources, "Sources");
            });
            header.col(|ui| {
                sort_button(ui, &mut state.sort, SortColumn::Size, "Size");
            });
            header.col(|ui| {
                ui.strong("Parts");
            });
            header.col(|ui| {
                sort_button(ui, &mut state.sort, SortColumn::Category, "Cat");
            });
            header.col(|ui| {
                sort_button(ui, &mut state.sort, SortColumn::Age, "Age");
            });
            header.col(|ui| {
                ui.strong("");
            });
        })
        .body(|mut body| {
            let downloaded = state.downloaded.clone();
            for (i, result) in state.results.iter().enumerate() {
                let already_downloaded = downloaded.contains(&i);
                let row_height = 22.0;
                body.row(row_height, |mut row| {
                    row.col(|ui| {
                        ui.label(&result.result.title);
                    });
                    row.col(|ui| {
                        ui.label(result.sources.join(", "));
                    });
                    row.col(|ui| {
                        ui.label(format_size(result.result.size));
                    });
                    row.col(|ui| {
                        if result.result.files > 0 {
                            ui.label(result.result.files.to_string());
                        } else {
                            ui.label("?");
                        }
                    });
                    row.col(|ui| {
                        ui.label(&result.result.category_name);
                    });
                    row.col(|ui| {
                        ui.label(format_age(result.result.post_date));
                    });
                    row.col(|ui| {
                        let btn = if already_downloaded {
                            egui::Button::new("Queued")
                        } else {
                            egui::Button::new("Download")
                        };
                        if ui.add_enabled(!already_downloaded, btn).clicked() {
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
                    });
                });
            }
        });

    // No bulk action anymore — single download per row.
}

/// Render a sortable column header. Clicking toggles sort direction, or
/// switches to that column. Uses a plain label (not a Button) to avoid
/// layout issues in table headers.
fn sort_button(ui: &mut egui::Ui, sort: &mut SortState, col: SortColumn, label: &str) {
    let is_active = sort.col == col;
    let arrow = if is_active {
        if sort.asc {
            " \u{25B2}" // ▲
        } else {
            " \u{25BC}" // ▼
        }
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
