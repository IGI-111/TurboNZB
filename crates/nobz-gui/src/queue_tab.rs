//! Queue tab: three-pane layout.
//!
//! Top pane: list of all jobs (active + completed) with progress and actions.
//! Rows are clickable to select a job.
//!
//! Middle pane: speed graph for the currently-downloading job (line chart
//! of recent speed samples). Shows "No active download" when idle.
//!
//! Bottom pane: details for the selected job — file list with per-file
//! segment progress, post-process status, output directory, and a
//! high-granularity segment dot grid.

use egui::Color32;
use egui_extras::{Column, TableBuilder};
use nobz_core::PostProcessReport;
use nobz_core::queue::{JobState, QueueJob};

use crate::backend::{BackendCmd, BackendEvent, BackendHandle, JobFileDetail};
use crate::theme::Icons;

/// State for the queue tab.
#[derive(Debug, Clone, Default)]
pub struct QueueState {
    pub jobs: Vec<QueueJob>,
    /// Speed of the currently-downloading job (bytes/sec), updated live.
    pub current_speed: f64,
    /// Speed history samples (most recent last), for the speed graph.
    pub speed_history: Vec<f64>,
    /// Downloaded bytes of the current download (live, from Speed events).
    pub current_downloaded: u64,
    /// Total bytes of the current download.
    pub current_total: u64,
    /// Job id of the currently-downloading job (None when idle).
    pub current_job_id: Option<i64>,
    /// Whether the download engine is paused (global Play/Pause).
    pub engine_paused: bool,
    /// Currently selected job (clicked row).
    pub selected_job: Option<i64>,
    /// Per-file details for the selected job.
    pub job_details: Vec<JobFileDetail>,
    /// Post-process reports keyed by job id.
    pub pp_reports: Vec<(Option<i64>, PostProcessReport)>,
    /// Jobs currently being post-processed (show "Verifying..." status).
    pub pp_in_progress: std::collections::HashSet<i64>,
}

impl QueueState {
    /// Process backend events relevant to the queue.
    pub fn handle_events(&mut self, events: &[BackendEvent], _backend: &BackendHandle) {
        for ev in events {
            match ev {
                BackendEvent::JobsList(jobs) => {
                    self.jobs = jobs.clone();
                    // Update current_job_id from the job list (the DB is
                    // the source of truth).
                    self.current_job_id = jobs
                        .iter()
                        .find(|j| j.state == JobState::Downloading)
                        .map(|j| j.id);
                }
                BackendEvent::JobStateChanged { job_id, state } => {
                    if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
                        job.state = *state;
                    }
                    if *state == JobState::Downloading {
                        self.current_job_id = Some(*job_id);
                    } else if self.current_job_id == Some(*job_id) {
                        self.current_job_id = None;
                    }
                }
                BackendEvent::JobAdded { .. } => {}
                BackendEvent::Speed {
                    job_id,
                    bytes_per_sec,
                    downloaded_bytes,
                    total_bytes,
                    history,
                } => {
                    self.current_job_id = *job_id;
                    self.current_speed = *bytes_per_sec;
                    self.speed_history = history.clone();
                    self.current_downloaded = *downloaded_bytes;
                    self.current_total = *total_bytes;
                }
                BackendEvent::JobDetails { job_id, files } => {
                    if self.selected_job == Some(*job_id) {
                        self.job_details = files.clone();
                    }
                }
                BackendEvent::PostProcessStarted { job_id } => {
                    self.pp_in_progress.insert(*job_id);
                }
                BackendEvent::PostProcessDone { job_id, report } => {
                    if let Some(id) = job_id {
                        self.pp_in_progress.remove(id);
                    }
                    self.pp_reports.push((*job_id, report.clone()));
                }
                BackendEvent::EnginePaused(paused) => {
                    self.engine_paused = *paused;
                }
                _ => {}
            }
        }
    }

    /// Select a job and request its details.
    pub fn select_job(&mut self, job_id: i64, backend: &BackendHandle) {
        self.selected_job = Some(job_id);
        self.job_details.clear();
        backend.send(BackendCmd::SetSelectedJob {
            job_id: Some(job_id),
        });
        backend.send(BackendCmd::GetJobDetails { job_id });
    }

    /// Clear selection.
    pub fn clear_selection(&mut self, backend: &BackendHandle) {
        self.selected_job = None;
        self.job_details.clear();
        backend.send(BackendCmd::SetSelectedJob { job_id: None });
    }
}

/// Render the queue tab (three-pane layout).
pub fn ui(
    ui: &mut egui::Ui,
    state: &mut QueueState,
    backend: &BackendHandle,
    _icons: Option<&Icons>,
) {
    // Top: toolbar with Open NZB, Play/Pause, and Clear completed.
    ui.horizontal(|ui| {
        // Open NZB file from disk.
        if icon_button(ui, IconKind::Open, true, "Open NZB file").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("NZB files", &["nzb"])
                .pick_file()
            {
                backend.send(BackendCmd::OpenNzbFile { path });
            }
        }

        // Global Play/Pause — always visible. Controls the download engine.
        if state.engine_paused {
            if icon_button(ui, IconKind::Play, true, "Start downloads").clicked() {
                backend.send(BackendCmd::ResumeEngine);
            }
        } else {
            if icon_button(ui, IconKind::Pause, true, "Pause downloads").clicked() {
                backend.send(BackendCmd::PauseEngine);
            }
        }

        // Clear completed — only enabled if there are completed/failed jobs.
        let has_completed = state
            .jobs
            .iter()
            .any(|j| matches!(j.state, JobState::Complete | JobState::Failed));
        if icon_button(ui, IconKind::Clear, has_completed, "Clear completed").clicked() {
            backend.send(BackendCmd::ClearCompleted);
        }
    });

    ui.separator();

    if state.jobs.is_empty() {
        ui.label("Queue is empty. Search and download something!");
        return;
    }

    // Split the remaining space: three panes.
    let available = ui.available_height();
    let graph_height = 100.0;
    let top_height = ((available - graph_height) * 0.50).max(120.0);
    let bottom_height = (available - top_height - graph_height).max(120.0);

    // --- Top pane: job list ---
    ui.push_id("job_list_pane", |ui| {
        ui.allocate_ui(egui::vec2(ui.available_width(), top_height), |ui| {
            job_list_pane(ui, state, backend);
        });
    });

    ui.separator();

    // --- Middle pane: speed graph ---
    ui.push_id("speed_graph_pane", |ui| {
        ui.allocate_ui(egui::vec2(ui.available_width(), graph_height), |ui| {
            speed_graph_pane(ui, state);
        });
    });

    ui.separator();

    // --- Bottom pane: job details ---
    ui.push_id("details_pane", |ui| {
        ui.allocate_ui(egui::vec2(ui.available_width(), bottom_height), |ui| {
            details_pane(ui, state);
        });
    });
}

/// Top pane: table of all jobs with clickable rows.
fn job_list_pane(ui: &mut egui::Ui, state: &mut QueueState, backend: &BackendHandle) {
    // Name width = available - (State + Progress + Actions) - padding
    let state_w = 80.0;
    let progress_w = 220.0;
    let actions_w = 78.0; // 3 icon buttons: up, down, delete
    let name_width = (ui.available_width() - state_w - progress_w - actions_w - 16.0).max(100.0);

    let avail_h = ui.available_height();
    let table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .vscroll(true)
        .min_scrolled_height(avail_h)
        .column(Column::exact(name_width).clip(true)) // Name (stretches)
        .column(Column::exact(state_w)) // State
        .column(Column::exact(progress_w)) // Progress
        .column(Column::exact(actions_w)); // Actions

    table
        .header(20.0, |mut header| {
            for label in ["Name", "State", "Progress", ""] {
                header.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|mut body| {
            let jobs = state.jobs.clone();
            let selected = state.selected_job;
            let current_job_id = state.current_job_id;
            let job_count = jobs.len();
            for (row_idx, job) in jobs.iter().enumerate() {
                let is_selected = selected == Some(job.id);
                let row_height = 28.0;
                body.row(row_height, |mut row| {
                    row.col(|ui| {
                        if ui.selectable_label(is_selected, &job.name).clicked() {
                            state.select_job(job.id, backend);
                        }
                    });
                    row.col(|ui| {
                        if state.pp_in_progress.contains(&job.id) {
                            ui.colored_label(Color32::from_rgb(80, 180, 80), "Verifying...");
                        } else {
                            let (color, text) = state_color(&job.state);
                            ui.colored_label(color, text);
                        }
                    });
                    row.col(|ui| {
                        // Use live downloaded bytes for the active job,
                        // fall back to DB-stored values for others.
                        let downloaded = if current_job_id == Some(job.id) {
                            state.current_downloaded
                        } else {
                            job.downloaded_bytes
                        };

                        let (pct, bar_text) = if job.total_bytes > 0 && downloaded > 0 {
                            let p = (downloaded as f64 / job.total_bytes as f64).min(1.0);
                            (
                                p as f32,
                                format!(
                                    "{} / {} ({}%)",
                                    format_size(downloaded),
                                    format_size(job.total_bytes),
                                    (p * 100.0) as u32
                                ),
                            )
                        } else if job.total_segments > 0 {
                            let p = job.segments_done as f32 / job.total_segments as f32;
                            (
                                p,
                                format!(
                                    "{} / {} seg  ({} / {} files)",
                                    job.segments_done,
                                    job.total_segments,
                                    job.files_done,
                                    job.file_count
                                ),
                            )
                        } else {
                            (0.0, "—".into())
                        };
                        ui.add(egui::ProgressBar::new(pct).text(bar_text));
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            // Up button — disabled for first row.
                            if icon_button(ui, IconKind::Up, row_idx > 0, "Move up").clicked() {
                                backend.send(BackendCmd::MoveJobUp { job_id: job.id });
                            }
                            // Down button — disabled for last row.
                            if icon_button(ui, IconKind::Down, row_idx < job_count - 1, "Move down")
                                .clicked()
                            {
                                backend.send(BackendCmd::MoveJobDown { job_id: job.id });
                            }
                            // Delete button.
                            if icon_button(ui, IconKind::Delete, true, "Delete").clicked() {
                                backend.send(BackendCmd::DeleteJob { job_id: job.id });
                                if selected == Some(job.id) {
                                    state.clear_selection(backend);
                                }
                            }
                        });
                    });
                });
            }
        });
}

/// Middle pane: speed graph for the currently-downloading job.
fn speed_graph_pane(ui: &mut egui::Ui, state: &QueueState) {
    let available = ui.available_size();
    let (rect, _) = ui.allocate_at_least(available, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let bg = Color32::from_rgb(30, 30, 40);
    let grid = Color32::from_rgb(50, 50, 60);
    let line = Color32::from_rgb(80, 220, 80);
    let text = Color32::from_rgb(200, 200, 200);

    painter.rect_filled(rect, 0.0, bg);

    if state.current_job_id.is_none() || state.speed_history.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No active download",
            egui::FontId::proportional(14.0),
            text,
        );
        return;
    }

    // Draw horizontal grid lines (4 lines).
    for i in 0..=4 {
        let y = rect.top() + (rect.height() * i as f32 / 4.0);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(1.0_f32, grid),
        );
    }

    let history = &state.speed_history;
    let max_bps = history.iter().cloned().fold(0.0f64, f64::max).max(1.0);
    let n = history.len();
    let padding = 8.0;
    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + padding, rect.top() + padding),
        egui::pos2(rect.right() - padding, rect.bottom() - padding),
    );

    // Draw the speed line.
    let mut points = Vec::with_capacity(n);
    for (i, &bps) in history.iter().enumerate() {
        let x = if n > 1 {
            plot_rect.left() + (plot_rect.width() * i as f32 / (n - 1) as f32)
        } else {
            plot_rect.left()
        };
        let y = plot_rect.bottom() - (plot_rect.height() * (bps as f32 / max_bps as f32));
        points.push(egui::pos2(x, y));
    }

    if points.len() >= 2 {
        // Draw the speed line only — no fill (fill creates visible edges
        // from the baseline to the first/last data points).
        painter.add(egui::Shape::line(points, egui::Stroke::new(2.0_f32, line)));
    } else if points.len() == 1 {
        painter.circle_filled(points[0], 3.0, line);
    }

    // Draw current speed label.
    let speed_label = format!("{}/s", format_size(state.current_speed as u64));
    painter.text(
        egui::pos2(plot_rect.left(), rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        &speed_label,
        egui::FontId::proportional(13.0),
        text,
    );

    // Draw max label.
    let max_label = format!("max: {}/s", format_size(max_bps as u64));
    painter.text(
        egui::pos2(plot_rect.right(), rect.top() + 4.0),
        egui::Align2::RIGHT_TOP,
        &max_label,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(140, 140, 140),
    );

    // Draw downloaded / total.
    if state.current_total > 0 {
        let progress_label = format!(
            "{} / {} ({}%)",
            format_size(state.current_downloaded),
            format_size(state.current_total),
            (state.current_downloaded as f64 / state.current_total as f64 * 100.0) as u32
        );
        painter.text(
            egui::pos2(plot_rect.right(), rect.bottom() - 4.0),
            egui::Align2::RIGHT_BOTTOM,
            &progress_label,
            egui::FontId::proportional(11.0),
            Color32::from_rgb(140, 140, 140),
        );
    }
}

/// Bottom pane: details for the selected job.
fn details_pane(ui: &mut egui::Ui, state: &QueueState) {
    let Some(job_id) = state.selected_job else {
        ui.label("Select a job to see details.");
        return;
    };

    let Some(job) = state.jobs.iter().find(|j| j.id == job_id) else {
        ui.label("Job not found.");
        return;
    };

    // Job header
    ui.horizontal(|ui| {
        ui.heading(&job.name);
        let (color, text) = state_color(&job.state);
        ui.colored_label(color, text);
    });

    ui.horizontal(|ui| {
        ui.label(format!("ID: {}", job.id));
        ui.label(format!("Output: {}", job.output_dir.display()));
    });

    // Progress summary
    let downloaded = if state.current_job_id == Some(job.id) {
        state.current_downloaded
    } else {
        job.downloaded_bytes
    };
    if job.total_bytes > 0 {
        let p = (downloaded as f64 / job.total_bytes as f64).min(1.0);
        ui.add(egui::ProgressBar::new(p as f32).text(format!(
            "{} / {} ({}%)",
            format_size(downloaded),
            format_size(job.total_bytes),
            (p * 100.0) as u32
        )));
    }

    // Post-process status: show "Verifying..." if in progress, otherwise
    // show the report summary.
    if state.pp_in_progress.contains(&job.id) {
        ui.horizontal(|ui| {
            ui.label("Post-process:");
            ui.colored_label(Color32::from_rgb(80, 180, 80), "Verifying + unpacking...");
            ui.spinner();
        });
    } else if let Some((_, report)) = state
        .pp_reports
        .iter()
        .rev()
        .find(|(id, _)| *id == Some(job.id))
    {
        ui.horizontal(|ui| {
            ui.label("Post-process:");
            ui.label(pp_summary(report));
        });
    }

    ui.separator();

    // File list table with per-file segment dot grid
    if state.job_details.is_empty() {
        ui.label("No file details available.");
        return;
    }

    // File list table — Filename stretches with the window.
    let segs_w = 80.0;
    let size_w = 140.0;
    let grid_w = 170.0;
    let file_width = (ui.available_width() - segs_w - size_w - grid_w - 16.0).max(100.0);

    let avail_h = ui.available_height();
    let table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .vscroll(true)
        .min_scrolled_height(avail_h)
        .column(Column::exact(file_width).clip(true)) // Filename (stretches)
        .column(Column::exact(segs_w)) // Segs
        .column(Column::exact(size_w)) // Size
        .column(Column::exact(grid_w)); // Segment grid

    table
        .header(20.0, |mut header| {
            for label in ["File", "Segs", "Size", "Segments"] {
                header.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|mut body| {
            for file in &state.job_details {
                body.row(24.0, |mut row| {
                    row.col(|ui| {
                        ui.label(&file.filename);
                    });
                    row.col(|ui| {
                        ui.label(format!("{} / {}", file.segments_done, file.segment_count));
                    });
                    row.col(|ui| {
                        ui.label(format!(
                            "{} / {}",
                            format_size(file.downloaded_bytes),
                            format_size(file.total_bytes)
                        ));
                    });
                    row.col(|ui| {
                        file_segment_grid(ui, file);
                    });
                });
            }
        });
}

/// High-granularity per-file segment dot grid.
/// Shows one dot per segment (up to 200), colored by status.
fn file_segment_grid(ui: &mut egui::Ui, file: &JobFileDetail) {
    let total = file.segment_count.min(200) as usize;
    if total == 0 {
        return;
    }
    let done = (file.segments_done as usize * total) / file.segment_count as usize;
    let missing = (file.segments_missing as usize * total) / file.segment_count as usize;

    let dot_size = 4.0;
    let gap = 1.0;
    let dots_per_row = 28usize;
    let rows = total.div_ceil(dots_per_row);
    let width = dots_per_row as f32 * (dot_size + gap);
    let height = rows as f32 * (dot_size + gap);

    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    for i in 0..total {
        let col = i % dots_per_row;
        let row = i / dots_per_row;
        let x = rect.left() + col as f32 * (dot_size + gap);
        let y = rect.top() + row as f32 * (dot_size + gap);
        let color = if i < done {
            Color32::from_rgb(80, 180, 80) // green = done
        } else if i < done + missing {
            Color32::from_rgb(200, 60, 60) // red = missing
        } else {
            Color32::from_rgb(160, 160, 160) // gray = pending
        };
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(dot_size, dot_size)),
            0.0,
            color,
        );
    }
}

fn state_color(state: &JobState) -> (Color32, &'static str) {
    match state {
        JobState::Fetching => (Color32::from_rgb(100, 150, 220), "Fetching..."),
        JobState::Queued => (Color32::from_rgb(120, 120, 120), "Queued"),
        JobState::Downloading => (Color32::from_rgb(80, 180, 80), "Downloading"),
        JobState::Complete => (Color32::from_rgb(60, 160, 60), "Complete"),
        JobState::Failed => (Color32::from_rgb(200, 60, 60), "Failed"),
    }
}

fn pp_summary(report: &PostProcessReport) -> String {
    let verify = if let Some(vr) = &report.verify {
        format!(
            "PAR2: {}ok/{}damaged/{}missing",
            vr.healthy, vr.damaged, vr.missing
        )
    } else {
        "PAR2: skipped".into()
    };
    let unpack = if let Some(ur) = &report.unpack {
        format!("Unpacked: {} files", ur.extracted_files.len())
    } else {
        "Unpacked: —".into()
    };
    format!("{verify} | {unpack} | {:?}", report.status)
}

/// Format a byte count as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0".into();
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

// --- Vector-drawn icon buttons (no font dependency) ---

/// Which icon to draw on a button.
#[derive(Hash)]
enum IconKind {
    Play,
    Pause,
    Delete,
    Up,
    Down,
    Clear,
    Open,
}

/// A small button with a vector-drawn icon. Uses `ui.interact` for proper
/// enabled/disabled/hover/click state handling.
fn icon_button(ui: &mut egui::Ui, kind: IconKind, enabled: bool, tooltip: &str) -> egui::Response {
    let size = egui::vec2(24.0, 20.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let id = ui.id().with(("icon_btn", &kind));
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let response = ui.interact(rect, id, sense).on_hover_text(tooltip);

    let painter = ui.painter_at(rect);
    let visuals = ui.style().interact(&response);

    // Button background.
    painter.rect_filled(rect, 0.0, visuals.bg_fill);
    painter.rect_stroke(rect, 0.0, visuals.bg_stroke);

    // Icon color: black when enabled, grey when disabled.
    let icon_color = if enabled {
        Color32::from_rgb(0, 0, 0)
    } else {
        Color32::from_rgb(128, 128, 128)
    };

    let cx = rect.center().x;
    let cy = rect.center().y;
    let s = 5.0; // half-size of the icon

    match kind {
        IconKind::Play => {
            let p1 = egui::pos2(cx - s * 0.6, cy - s);
            let p2 = egui::pos2(cx - s * 0.6, cy + s);
            let p3 = egui::pos2(cx + s, cy);
            painter.add(egui::Shape::convex_polygon(
                vec![p1, p2, p3],
                icon_color,
                egui::Stroke::NONE,
            ));
        }
        IconKind::Pause => {
            let bar_w = 2.5_f32;
            let bar_h = s * 2.0;
            let gap = 2.5_f32;
            let total_w = bar_w * 2.0 + gap;
            let left_x = cx - total_w / 2.0;
            let right_x = left_x + bar_w + gap;
            let top_y = cy - bar_h / 2.0;
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(left_x, top_y), egui::vec2(bar_w, bar_h)),
                0.0,
                icon_color,
            );
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(right_x, top_y), egui::vec2(bar_w, bar_h)),
                0.0,
                icon_color,
            );
        }
        IconKind::Delete => {
            let stroke = egui::Stroke::new(2.0_f32, icon_color);
            painter.line_segment(
                [egui::pos2(cx - s, cy - s), egui::pos2(cx + s, cy + s)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(cx + s, cy - s), egui::pos2(cx - s, cy + s)],
                stroke,
            );
        }
        IconKind::Up => {
            // Up arrow: filled triangle pointing up.
            let p1 = egui::pos2(cx, cy - s);
            let p2 = egui::pos2(cx - s, cy + s * 0.7);
            let p3 = egui::pos2(cx + s, cy + s * 0.7);
            painter.add(egui::Shape::convex_polygon(
                vec![p1, p2, p3],
                icon_color,
                egui::Stroke::NONE,
            ));
        }
        IconKind::Down => {
            // Down arrow: filled triangle pointing down.
            let p1 = egui::pos2(cx, cy + s);
            let p2 = egui::pos2(cx - s, cy - s * 0.7);
            let p3 = egui::pos2(cx + s, cy - s * 0.7);
            painter.add(egui::Shape::convex_polygon(
                vec![p1, p2, p3],
                icon_color,
                egui::Stroke::NONE,
            ));
        }
        IconKind::Clear => {
            // Trash can: lid + handle + trapezoid body.
            let stroke = egui::Stroke::new(2.0_f32, icon_color);
            // Handle: small line on top.
            painter.line_segment(
                [
                    egui::pos2(cx - s * 0.3, cy - s),
                    egui::pos2(cx + s * 0.3, cy - s),
                ],
                stroke,
            );
            // Lid: horizontal line.
            painter.line_segment(
                [
                    egui::pos2(cx - s, cy - s * 0.6),
                    egui::pos2(cx + s, cy - s * 0.6),
                ],
                stroke,
            );
            // Body: left side.
            painter.line_segment(
                [
                    egui::pos2(cx - s * 0.7, cy - s * 0.6),
                    egui::pos2(cx - s * 0.4, cy + s),
                ],
                stroke,
            );
            // Body: right side.
            painter.line_segment(
                [
                    egui::pos2(cx + s * 0.7, cy - s * 0.6),
                    egui::pos2(cx + s * 0.4, cy + s),
                ],
                stroke,
            );
            // Body: bottom.
            painter.line_segment(
                [
                    egui::pos2(cx - s * 0.4, cy + s),
                    egui::pos2(cx + s * 0.4, cy + s),
                ],
                stroke,
            );
        }
        IconKind::Open => {
            // Folder icon: tab on top-left + body rectangle.
            let stroke = egui::Stroke::new(2.0_f32, icon_color);
            // Tab (the little notch on the folder flap).
            painter.line_segment(
                [
                    egui::pos2(cx - s, cy - s * 0.5),
                    egui::pos2(cx - s * 0.3, cy - s * 0.5),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(cx - s * 0.3, cy - s * 0.5),
                    egui::pos2(cx - s * 0.1, cy - s * 0.8),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(cx - s * 0.1, cy - s * 0.8),
                    egui::pos2(cx + s, cy - s * 0.8),
                ],
                stroke,
            );
            // Top edge.
            painter.line_segment(
                [
                    egui::pos2(cx + s, cy - s * 0.8),
                    egui::pos2(cx + s, cy - s * 0.5),
                ],
                stroke,
            );
            // Right side.
            painter.line_segment(
                [egui::pos2(cx + s, cy - s * 0.5), egui::pos2(cx + s, cy + s)],
                stroke,
            );
            // Bottom.
            painter.line_segment(
                [egui::pos2(cx - s, cy + s), egui::pos2(cx + s, cy + s)],
                stroke,
            );
            // Left side.
            painter.line_segment(
                [egui::pos2(cx - s, cy - s * 0.5), egui::pos2(cx - s, cy + s)],
                stroke,
            );
        }
    }

    response
}
