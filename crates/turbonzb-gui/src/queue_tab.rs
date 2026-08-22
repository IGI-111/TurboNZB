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
use turbonzb_core::PostProcessReport;
use turbonzb_core::engine::ProgressEvent;
use turbonzb_core::queue::{JobState, QueueJob, SegmentState};

use crate::backend::{BackendCmd, BackendEvent, BackendHandle, JobFileDetail, JobSegmentDetail};
use crate::theme::Icons;
use crate::win95_scroll::{Win95Table, vertical};
use crate::win95_widgets::{
    Win95Button, Win95IconButton, Win95ProgressBar, Win95TabButton, paint_sunken_bevel,
};

/// Which sub-tab is active in the queue's bottom pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailsTab {
    #[default]
    General,
    Files,
    Segments,
    Speed,
}

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
    /// Number of active NNTP connections (0 when idle).
    pub active_connections: usize,
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
    /// Latest PAR2 verification progress (done, total bytes) per job.
    pub pp_progress: std::collections::HashMap<i64, (u64, u64)>,
    /// Which sub-tab is active in the bottom details pane.
    pub details_tab: DetailsTab,
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
                    active_connections,
                } => {
                    self.current_job_id = *job_id;
                    self.current_speed = *bytes_per_sec;
                    self.speed_history = history.clone();
                    self.current_downloaded = *downloaded_bytes;
                    self.current_total = *total_bytes;
                    self.active_connections = *active_connections;
                }
                BackendEvent::Progress(ev) => match ev {
                    // Live segment progress — so the bar moves in real-time
                    // instead of waiting for the 100ms DB poll (which a very
                    // fast download can outrun entirely).
                    ProgressEvent::SegmentDone { status, bytes, .. }
                        if self.current_job_id.is_some() =>
                    {
                        let transferred =
                            matches!(status, SegmentState::Done | SegmentState::CrcMismatch);
                        if transferred {
                            self.current_downloaded =
                                self.current_downloaded.saturating_add(*bytes);
                            if let Some(job) = self
                                .jobs
                                .iter_mut()
                                .find(|j| Some(j.id) == self.current_job_id)
                            {
                                job.segments_done = job.segments_done.saturating_add(1);
                            }
                        }
                    }
                    // A file finished assembling — bump the file counter so
                    // "N / M files" reflects very fast completions too.
                    ProgressEvent::FileCompleted { .. } => {
                        if let Some(id) = self.current_job_id {
                            if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                                job.files_done = (job.files_done + 1).min(job.file_count);
                            }
                        }
                    }
                    _ => {}
                },
                BackendEvent::JobDetails { job_id, files } => {
                    if self.selected_job == Some(*job_id) {
                        self.job_details = files.clone();
                    }
                }
                BackendEvent::PostProcessProgress { job_id, done, total } => {
                    self.pp_progress.insert(*job_id, (*done, *total));
                }
                BackendEvent::PostProcessStarted { job_id } => {
                    self.pp_in_progress.insert(*job_id);
                    self.pp_progress.insert(*job_id, (0, 0));
                }
                BackendEvent::PostProcessDone { job_id, report } => {
                    if let Some(id) = job_id {
                        self.pp_in_progress.remove(id);
                        self.pp_progress.remove(id);
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
            // Top: toolbar with Open NZB, Play/Pause, and Clear completed.
            ui.horizontal(|ui| {
                // Open NZB file from disk.
                if let Some(icons) = icons {
                    if ui
                        .add(Win95IconButton::new(icons.tb_open.clone()).tooltip("Open NZB file"))
                        .clicked()
                    {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("NZB files", &["nzb"])
                            .pick_file()
                        {
                            backend.send(BackendCmd::OpenNzbFile { path });
                        }
                    }
                }

                // Global Play/Pause — always visible. Controls the download engine.
                if state.engine_paused {
                    if let Some(icons) = icons {
                        if ui
                            .add(
                                Win95IconButton::new(icons.tb_play.clone())
                                    .tooltip("Start downloads"),
                            )
                            .clicked()
                        {
                            backend.send(BackendCmd::ResumeEngine);
                        }
                    }
                } else {
                    if let Some(icons) = icons {
                        if ui
                            .add(
                                Win95IconButton::new(icons.tb_pause.clone())
                                    .tooltip("Pause downloads"),
                            )
                            .clicked()
                        {
                            backend.send(BackendCmd::PauseEngine);
                        }
                    }
                }

                // Clear completed — only enabled if there are completed/failed jobs.
                let has_completed = state
                    .jobs
                    .iter()
                    .any(|j| matches!(j.state, JobState::Complete | JobState::Failed));
                if let Some(icons) = icons {
                    if ui
                        .add(
                            Win95IconButton::new(icons.tb_delete.clone())
                                .enabled(has_completed)
                                .tooltip("Clear completed"),
                        )
                        .clicked()
                    {
                        backend.send(BackendCmd::ClearCompleted);
                    }
                }
            });

            crate::win95_widgets::etched_separator(ui, true);

            if state.jobs.is_empty() {
                ui.add_space(20.0);
                ui.label("Queue is empty. Search and download something!");
                return;
            }

            // Two-pane layout: job list on top, tabbed details below.
            let available = ui.available_height();
            let bottom_height = 220.0_f32.min(available * 0.45).max(140.0);
            let top_height = (available - bottom_height - 4.0).max(120.0);

            // --- Top pane: job list ---
            ui.push_id("job_list_pane", |ui| {
                ui.allocate_ui(egui::vec2(ui.available_width(), top_height), |ui| {
                    job_list_pane(ui, state, backend, icons);
                });
            });

            crate::win95_widgets::etched_separator(ui, true);

            // --- Bottom pane: tabbed details ---
            ui.push_id("details_pane", |ui| {
                ui.allocate_ui(egui::vec2(ui.available_width(), bottom_height), |ui| {
                    details_tabbed_pane(ui, state, icons);
                });
            });
        });
}

/// Top pane: table of all jobs with clickable rows.
fn job_list_pane(
    ui: &mut egui::Ui,
    state: &mut QueueState,
    backend: &BackendHandle,
    icons: Option<&Icons>,
) {
    let state_w = 100.0;
    let progress_w = 220.0;
    let actions_w = 90.0;

    Win95Table::new()
        .striped(true)
        .id_salt("job_list")
        .min_scrolled_height(ui.available_height())
        .column_remainder()
        .column(state_w)
        .column(progress_w)
        .column(actions_w)
        .header_body(
            ui,
            20.0,
            |row, ui| {
                for label in ["Name", "State", "Progress", "Actions"] {
                    row.col(ui, |ui| {
                        ui.strong(label);
                    });
                }
            },
            |body| {
                let jobs = state.jobs.clone();
                let selected = state.selected_job;
                let current_job_id = state.current_job_id;
                let job_count = jobs.len();
                for (row_idx, job) in jobs.iter().enumerate() {
                    let is_selected = selected == Some(job.id);
                    let row_height = 28.0;
                    body.row(row_height, |row, ui| {
                        row.col(ui, |ui| {
                            if ui.selectable_label(is_selected, &job.name).clicked() {
                                state.select_job(job.id, backend);
                            }
                        });
                        row.col(ui, |ui| {
                            // While verifying, show a real PAR2 progress bar.
                            if state.pp_in_progress.contains(&job.id) {
                                let (done, total) = state
                                    .pp_progress
                                    .get(&job.id)
                                    .copied()
                                    .unwrap_or((0, 0));
                                let p = if total > 0 {
                                    (done as f64 / total as f64).min(1.0) as f32
                                } else {
                                    0.0
                                };
                                ui.add(
                                    Win95ProgressBar::new(p).text(if total > 0 {
                                        format!("Verifying… {} / {}", format_size(done), format_size(total))
                                    } else {
                                        "Verifying…".into()
                                    }),
                                );
                            } else if let Some(err) = &job.error {
                                // Salient error state — red "Error" with the
                                // reason on hover.
                                ui.colored_label(Color32::from_rgb(200, 60, 60), "Error")
                                    .on_hover_text(err.as_str());
                            } else {
                                let (color, text) = state_color(&job.state);
                                ui.colored_label(color, text);
                            }
                        });
                        row.col(ui, |ui| {
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
                            ui.add(Win95ProgressBar::new(pct).text(bar_text));
                        });
                        row.col(ui, |ui| {
                            ui.horizontal(|ui| {
                                if let Some(icons) = icons {
                                    if ui
                                        .add(
                                            Win95IconButton::new(icons.tb_up.clone())
                                                .enabled(row_idx > 0)
                                                .tooltip("Move up"),
                                        )
                                        .clicked()
                                    {
                                        backend.send(BackendCmd::MoveJobUp { job_id: job.id });
                                    }
                                    if ui
                                        .add(
                                            Win95IconButton::new(icons.tb_download.clone())
                                                .enabled(row_idx < job_count - 1)
                                                .tooltip("Move down"),
                                        )
                                        .clicked()
                                    {
                                        backend.send(BackendCmd::MoveJobDown { job_id: job.id });
                                    }
                                    if ui
                                        .add(
                                            Win95IconButton::new(icons.tb_delete.clone())
                                                .tooltip("Delete"),
                                        )
                                        .clicked()
                                    {
                                        backend.send(BackendCmd::DeleteJob { job_id: job.id });
                                        if selected == Some(job.id) {
                                            state.clear_selection(backend);
                                        }
                                    }
                                } else {
                                    if ui
                                        .add_enabled(row_idx > 0, Win95Button::new("Up"))
                                        .clicked()
                                    {
                                        backend.send(BackendCmd::MoveJobUp { job_id: job.id });
                                    }
                                    if ui
                                        .add_enabled(
                                            row_idx < job_count - 1,
                                            Win95Button::new("Down"),
                                        )
                                        .clicked()
                                    {
                                        backend.send(BackendCmd::MoveJobDown { job_id: job.id });
                                    }
                                    if ui.add(Win95Button::new("Delete")).clicked() {
                                        backend.send(BackendCmd::DeleteJob { job_id: job.id });
                                        if selected == Some(job.id) {
                                            state.clear_selection(backend);
                                        }
                                    }
                                }
                            });
                        });
                    });
                }
            },
        );
}

/// Middle pane: speed graph for the currently-downloading job.
fn speed_graph_pane(ui: &mut egui::Ui, state: &QueueState) {
    let available = ui.available_size();
    let (rect, _) = ui.allocate_at_least(available, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Win95 sunken bevel + white background.
    painter.rect_filled(rect, 0.0, crate::theme::colors::BUTTON_FACE);
    paint_sunken_bevel(&painter, rect);
    let inner = rect.shrink(2.0);
    painter.rect_filled(inner, 0.0, crate::theme::colors::WINDOW);

    let bg = Color32::from_rgb(255, 255, 255);
    let grid = Color32::from_rgb(200, 200, 200);
    let line = crate::theme::colors::ACCENT;
    let text = Color32::from_rgb(0, 0, 0);

    painter.rect_filled(inner, 0.0, bg);

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
        let y = inner.top() + (inner.height() * i as f32 / 4.0);
        painter.line_segment(
            [egui::pos2(inner.left(), y), egui::pos2(inner.right(), y)],
            egui::Stroke::new(1.0_f32, grid),
        );
    }

    let history = &state.speed_history;
    let max_bps = history.iter().cloned().fold(0.0f64, f64::max).max(1.0);
    let n = history.len();
    let padding = 8.0;
    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(inner.left() + padding, inner.top() + padding),
        egui::pos2(inner.right() - padding, inner.bottom() - padding),
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
        Color32::from_rgb(100, 100, 100),
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
            Color32::from_rgb(100, 100, 100),
        );
    }
}

/// Bottom pane: tabbed details (General / Files / Speed).
fn details_tabbed_pane(ui: &mut egui::Ui, state: &mut QueueState, icons: Option<&Icons>) {
    let has_selection = state.selected_job.is_some();

    // Default to Speed tab when no job is selected.
    let active_tab = if has_selection {
        state.details_tab
    } else {
        DetailsTab::Speed
    };

    // Tab bar at the top of the bottom pane.
    ui.horizontal(|ui| {
        if ui
            .add(
                Win95TabButton::new(None, "General", active_tab == DetailsTab::General)
                    .enabled(has_selection),
            )
            .clicked()
        {
            state.details_tab = DetailsTab::General;
        }
        if ui
            .add(
                Win95TabButton::new(None, "Files", active_tab == DetailsTab::Files)
                    .enabled(has_selection),
            )
            .clicked()
        {
            state.details_tab = DetailsTab::Files;
        }
        if ui
            .add(
                Win95TabButton::new(None, "Segments", active_tab == DetailsTab::Segments)
                    .enabled(has_selection),
            )
            .clicked()
        {
            state.details_tab = DetailsTab::Segments;
        }
        if ui
            .add(Win95TabButton::new(
                None,
                "Speed",
                active_tab == DetailsTab::Speed,
            ))
            .clicked()
        {
            state.details_tab = DetailsTab::Speed;
        }
    });

    let _ = icons;

    match active_tab {
        DetailsTab::General => {
            general_pane(ui, state);
        }
        DetailsTab::Files => {
            files_pane(ui, state);
        }
        DetailsTab::Segments => {
            segments_pane(ui, state);
        }
        DetailsTab::Speed => {
            speed_graph_pane(ui, state);
        }
    }
}

/// General tab: job header, progress bar, post-process status.
fn general_pane(ui: &mut egui::Ui, state: &QueueState) {
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
        if let Some(err) = &job.error {
            ui.colored_label(color, "Error").on_hover_text(err.as_str());
        } else {
            ui.colored_label(color, text);
        }
    });

    // Salient error box for failed jobs (download or post-process).
    if let Some(err) = &job.error {
        ui.add_space(4.0);
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(6.0))
            .show(ui, |ui| {
                ui.colored_label(Color32::from_rgb(200, 60, 60), format!("Error: {err}"));
            });
    }

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
        ui.add(Win95ProgressBar::new(p as f32).text(format!(
            "{} / {} ({}%)",
            format_size(downloaded),
            format_size(job.total_bytes),
            (p * 100.0) as u32
        )));
    } else if job.total_segments > 0 {
        let p = job.segments_done as f32 / job.total_segments as f32;
        ui.add(Win95ProgressBar::new(p).text(format!(
            "{} / {} segments ({} / {} files)",
            job.segments_done, job.total_segments, job.files_done, job.file_count
        )));
    }

    // Post-process status
    if state.pp_in_progress.contains(&job.id) {
        ui.horizontal(|ui| {
            ui.label("Post-process:");
            ui.colored_label(Color32::from_rgb(80, 180, 80), "Verifying + unpacking...");
            ui.spinner();
        });
        if let Some((done, total)) = state.pp_progress.get(&job.id).copied() {
            if total > 0 {
                let p = (done as f64 / total as f64).min(1.0) as f32;
                ui.add(Win95ProgressBar::new(p).text(format!(
                    "PAR2 verify: {} / {} ({}%)",
                    format_size(done),
                    format_size(total),
                    (p * 100.0) as u32
                )));
            }
        }
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

    // Segment stats
    if !state.job_details.is_empty() {
        let total_segs: u64 = state
            .job_details
            .iter()
            .map(|f| f.segment_count as u64)
            .sum();
        let done_segs: u64 = state
            .job_details
            .iter()
            .map(|f| f.segments_done as u64)
            .sum();
        let missing_segs: u64 = state
            .job_details
            .iter()
            .map(|f| f.segments_missing as u64)
            .sum();
        ui.add_space(8.0);
        ui.label(format!(
            "Segments: {} done / {} total / {} missing across {} files",
            done_segs,
            total_segs,
            missing_segs,
            state.job_details.len()
        ));
    }
}

/// Files tab: file list table with per-file segment dot grid.
fn files_pane(ui: &mut egui::Ui, state: &QueueState) {
    let Some(_job_id) = state.selected_job else {
        ui.label("Select a job to see file details.");
        return;
    };

    if state.job_details.is_empty() {
        ui.label("No file details available.");
        return;
    }

    // File list table — Filename stretches with the window.
    let segs_w = 80.0;
    let size_w = 140.0;

    Win95Table::new()
        .striped(true)
        .id_salt("file_list")
        .min_scrolled_height(ui.available_height())
        .column_remainder()
        .column(segs_w)
        .column(size_w)
        .header_body(
            ui,
            20.0,
            |row, ui| {
                for label in ["File", "Segs", "Size"] {
                    row.col(ui, |ui| {
                        ui.strong(label);
                    });
                }
            },
            |body| {
                for file in &state.job_details {
                    body.row(24.0, |row, ui| {
                        row.col(ui, |ui| {
                            ui.label(&file.filename);
                        });
                        row.col(ui, |ui| {
                            ui.label(format!("{} / {}", file.segments_done, file.segment_count));
                        });
                        row.col(ui, |ui| {
                            ui.label(format!(
                                "{} / {}",
                                format_size(file.downloaded_bytes),
                                format_size(file.total_bytes)
                            ));
                        });
                    });
                }
            },
        );
}

/// Color + short label for a segment's state.
fn segment_style(seg: &JobSegmentDetail, downloading: bool) -> (Color32, &'static str) {
    match seg.state {
        SegmentState::Done => (Color32::from_rgb(0, 128, 96), "done"),
        SegmentState::CrcMismatch => (Color32::from_rgb(190, 130, 30), "bad CRC"),
        SegmentState::Missing => (Color32::from_rgb(200, 60, 60), "missing"),
        SegmentState::Failed => (Color32::from_rgb(150, 30, 30), "failed"),
        SegmentState::Pending => {
            if downloading {
                // The current job is downloading — pending segments are
                // queued to be fetched very soon.
                (Color32::from_rgb(60, 110, 190), "fetching")
            } else {
                (Color32::from_rgb(140, 140, 140), "pending")
            }
        }
    }
}

/// Segments tab: full-job block map in the style of the classic disk
/// defragmenter. Every segment is a colored tile; watch the download
/// fill the map live. Scrollable when the map is taller than the pane.
fn segments_pane(ui: &mut egui::Ui, state: &QueueState) {
    let Some(job_id) = state.selected_job else {
        ui.label("Select a job to see its segments.");
        return;
    };
    if state.job_details.is_empty() {
        ui.label("No segment data available.");
        return;
    }
    let downloading = state.current_job_id == Some(job_id);

    let total: usize = state.job_details.iter().map(|f| f.segments.len()).sum();
    if total == 0 {
        ui.label("No segments yet.");
        return;
    }

    // Padded frame so the map isn't cramped against the pane edges.
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .show(ui, |ui| {
            vertical(ui, "seg_block_map", |ui| {
                let avail_w = ui.available_width();
                let tile = 10.0;
                let gap = 1.0;
                let margin = 8.0;
                // Tiles per row: fill the width, but never stretch a tiny
                // job across the whole window.
                let per_row = ((avail_w - margin * 2.0) / (tile + gap)) as usize;
                let per_row = per_row.max(1).min(total);
                let rows = total.div_ceil(per_row);
                let w = margin * 2.0 + per_row as f32 * (tile + gap);
                let h = margin * 2.0 + rows as f32 * (tile + gap);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());

                let painter = ui.painter_at(rect);
                // Dark background like the defragmenter view.
                painter.rect_filled(rect, 2.0, Color32::from_rgb(24, 24, 28));

                let mut hover_idx: Option<usize> = None;
                let mut i = 0usize;
                'outer: for file in &state.job_details {
                    for seg in &file.segments {
                        if i >= total {
                            break 'outer;
                        }
                        let (color, _) = segment_style(seg, downloading);
                        let col = i % per_row;
                        let row = i / per_row;
                        let x = rect.left() + margin + col as f32 * (tile + gap);
                        let y = rect.top() + margin + row as f32 * (tile + gap);
                        let t = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(tile, tile));
                        painter.rect_filled(t, 1.0, color);
                        if hover_idx.is_none() && ui.rect_contains_pointer(t) {
                            hover_idx = Some(i);
                        }
                        i += 1;
                    }
                }

                // Tooltip: file + segment number + status.
                if let Some(idx) = hover_idx {
                    let mut cursor = 0usize;
                    let hit: Option<(&str, &JobSegmentDetail)> = 'find: {
                        for file in &state.job_details {
                            for seg in &file.segments {
                                if cursor == idx {
                                    break 'find Some((file.filename.as_str(), seg));
                                }
                                cursor += 1;
                            }
                        }
                        None
                    };
                    if let Some((name, seg)) = hit {
                        let (_, status) = segment_style(seg, downloading);
                        let _ = resp.clone();
                        egui::show_tooltip_at_pointer(
                            ui.ctx(),
                            resp.layer_id,
                            ui.id().with("seg_tooltip"),
                            |ui| {
                                ui.label(format!("{name} · segment #{} — {status}", seg.number));
                                ui.small(format!("size: {}", format_size(seg.bytes)));
                            },
                        );
                    }
                }
            });
        });
}

fn state_color(state: &JobState) -> (Color32, &'static str) {
    match state {
        JobState::Fetching => (Color32::from_rgb(100, 150, 220), "Fetching..."),
        JobState::Queued => (Color32::from_rgb(120, 120, 120), "Queued"),
        JobState::Downloading => (Color32::from_rgb(0, 128, 0), "Downloading"),
        JobState::Complete => (Color32::from_rgb(0, 128, 0), "Complete"),
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
