//! Queue tab: uTorrent-style two-pane layout.
//!
//! Top pane: list of all jobs (active + completed) with progress, speed,
//! and actions. Rows are clickable to select a job.
//!
//! Bottom pane: details for the selected job — file list with per-file
//! segment progress, post-process status, output directory, and a
//! high-granularity segment dot grid.

use std::collections::HashMap;

use egui::Color32;
use egui_extras::{Column, TableBuilder};
use nobz_core::PostProcessReport;
use nobz_core::queue::{JobState, QueueJob};

use crate::backend::{BackendCmd, BackendEvent, BackendHandle, JobFileDetail};

/// State for the queue tab.
#[derive(Debug, Clone, Default)]
pub struct QueueState {
    pub jobs: Vec<QueueJob>,
    /// Speed per job (bytes/sec), updated live from Speed events.
    pub speeds: HashMap<i64, f64>,
    /// Downloaded bytes per job (from Speed events, for live display).
    pub live_downloaded: HashMap<i64, u64>,
    /// Currently selected job (clicked row).
    pub selected_job: Option<i64>,
    /// Per-file details for the selected job.
    pub job_details: Vec<JobFileDetail>,
    /// Post-process reports keyed by job id (absorbed from HistoryState).
    pub pp_reports: Vec<(Option<i64>, PostProcessReport)>,
}

impl QueueState {
    /// Process backend events relevant to the queue.
    pub fn handle_events(&mut self, events: &[BackendEvent], _backend: &BackendHandle) {
        for ev in events {
            match ev {
                BackendEvent::JobsList(jobs) => {
                    self.jobs = jobs.clone();
                    let active_ids: std::collections::HashSet<i64> =
                        jobs.iter().map(|j| j.id).collect();
                    self.speeds.retain(|id, _| active_ids.contains(id));
                    self.live_downloaded.retain(|id, _| active_ids.contains(id));
                }
                BackendEvent::JobStateChanged { job_id, state } => {
                    if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
                        job.state = *state;
                    }
                    if *state == JobState::Complete || *state == JobState::Failed {
                        self.speeds.remove(job_id);
                        self.live_downloaded.remove(job_id);
                    }
                }
                BackendEvent::JobAdded { .. } => {}
                BackendEvent::Speed {
                    job_id,
                    bytes_per_sec,
                    downloaded_bytes,
                    ..
                } => {
                    self.speeds.insert(*job_id, *bytes_per_sec);
                    self.live_downloaded.insert(*job_id, *downloaded_bytes);
                }
                BackendEvent::JobDetails { job_id, files } => {
                    if self.selected_job == Some(*job_id) {
                        self.job_details = files.clone();
                    }
                }
                BackendEvent::PostProcessDone { job_id, report } => {
                    self.pp_reports.push((*job_id, report.clone()));
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

/// Render the queue tab (two-pane layout).
pub fn ui(ui: &mut egui::Ui, state: &mut QueueState, backend: &BackendHandle) {
    // Top: toolbar
    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            backend.send(BackendCmd::RefreshJobs);
        }
        ui.label(format!("{} jobs", state.jobs.len()));
    });

    ui.separator();

    if state.jobs.is_empty() {
        ui.label("Queue is empty. Search and download something!");
        return;
    }

    // Split the remaining space: top ~55% for job list, bottom ~45% for details.
    let available = ui.available_height();
    let top_height = (available * 0.55).max(120.0);

    // --- Top pane: job list ---
    ui.allocate_ui(egui::vec2(ui.available_width(), top_height), |ui| {
        job_list_pane(ui, state, backend);
    });

    ui.separator();

    // --- Bottom pane: job details ---
    details_pane(ui, state);
}

/// Top pane: table of all jobs with clickable rows.
fn job_list_pane(ui: &mut egui::Ui, state: &mut QueueState, backend: &BackendHandle) {
    // Name width = available - (ID + State + Progress + Speed + Actions) - padding
    let id_w = 30.0;
    let state_w = 80.0;
    let progress_w = 220.0;
    let speed_w = 80.0;
    let actions_w = 120.0;
    let name_width =
        (ui.available_width() - id_w - state_w - progress_w - speed_w - actions_w - 16.0)
            .max(100.0);

    let table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .column(Column::exact(id_w)) // ID
        .column(Column::exact(name_width).clip(true)) // Name (stretches)
        .column(Column::exact(state_w)) // State
        .column(Column::exact(progress_w)) // Progress
        .column(Column::exact(speed_w)) // Speed
        .column(Column::exact(actions_w)); // Actions

    table
        .header(20.0, |mut header| {
            for label in ["ID", "Name", "State", "Progress", "Speed", "Actions"] {
                header.col(|ui| {
                    ui.strong(label);
                });
            }
        })
        .body(|mut body| {
            let jobs = state.jobs.clone();
            let speeds = state.speeds.clone();
            let live_downloaded = state.live_downloaded.clone();
            let selected = state.selected_job;
            for job in &jobs {
                let is_selected = selected == Some(job.id);
                let row_height = 28.0;
                body.row(row_height, |mut row| {
                    row.col(|ui| {
                        ui.label(job.id.to_string());
                    });
                    row.col(|ui| {
                        if ui.selectable_label(is_selected, &job.name).clicked() {
                            state.select_job(job.id, backend);
                        }
                    });
                    row.col(|ui| {
                        let (color, text) = state_color(&job.state);
                        ui.colored_label(color, text);
                    });
                    row.col(|ui| {
                        let downloaded = live_downloaded
                            .get(&job.id)
                            .copied()
                            .unwrap_or(job.downloaded_bytes);

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
                        if let Some(bps) = speeds.get(&job.id) {
                            if *bps > 0.0 {
                                ui.colored_label(
                                    Color32::from_rgb(80, 180, 80),
                                    format!("{}/s", format_size(*bps as u64)),
                                );
                            } else {
                                ui.label("—");
                            }
                        } else {
                            ui.label("—");
                        }
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            let can_resume =
                                matches!(job.state, JobState::Paused | JobState::Failed);
                            let can_pause =
                                matches!(job.state, JobState::Queued | JobState::Downloading);
                            if ui
                                .add_enabled(can_resume, egui::Button::new("Resume"))
                                .clicked()
                            {
                                backend.send(BackendCmd::ResumeJob { job_id: job.id });
                            }
                            if ui
                                .add_enabled(can_pause, egui::Button::new("Pause"))
                                .clicked()
                            {
                                backend.send(BackendCmd::PauseJob { job_id: job.id });
                            }
                            if ui.button("Delete").clicked() {
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
        if let Some(bps) = state.speeds.get(&job.id) {
            if *bps > 0.0 {
                ui.colored_label(
                    Color32::from_rgb(80, 180, 80),
                    format!("{}/s", format_size(*bps as u64)),
                );
            }
        }
    });

    ui.horizontal(|ui| {
        ui.label(format!("ID: {}", job.id));
        ui.label(format!("Output: {}", job.output_dir.display()));
    });

    // Progress summary
    let downloaded = state
        .live_downloaded
        .get(&job.id)
        .copied()
        .unwrap_or(job.downloaded_bytes);
    if job.total_bytes > 0 {
        let p = (downloaded as f64 / job.total_bytes as f64).min(1.0);
        ui.add(egui::ProgressBar::new(p as f32).text(format!(
            "{} / {} ({}%)",
            format_size(downloaded),
            format_size(job.total_bytes),
            (p * 100.0) as u32
        )));
    }

    // Post-process status
    if let Some((_, report)) = state
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
    let size_w = 80.0;
    let status_w = 100.0;
    let grid_w = 140.0;
    let file_width = (ui.available_width() - segs_w - size_w - status_w - grid_w - 16.0).max(100.0);

    let table = TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .column(Column::exact(file_width).clip(true)) // Filename (stretches)
        .column(Column::exact(segs_w)) // Segs
        .column(Column::exact(size_w)) // Size
        .column(Column::exact(status_w)) // Status
        .column(Column::exact(grid_w)); // Segment grid

    table
        .header(20.0, |mut header| {
            for label in ["File", "Segs", "Size", "Status", "Segments"] {
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
                        let (color, text) = file_status(file);
                        ui.colored_label(color, text);
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
    let dots_per_row = 40usize;
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

fn file_status(file: &JobFileDetail) -> (Color32, &'static str) {
    if file.segments_missing > 0 {
        (Color32::from_rgb(200, 60, 60), "Missing segments")
    } else if file.segments_done >= file.segment_count {
        (Color32::from_rgb(60, 160, 60), "Done")
    } else if file.segments_done > 0 {
        (Color32::from_rgb(200, 160, 40), "Downloading")
    } else {
        (Color32::from_rgb(120, 120, 120), "Pending")
    }
}

fn state_color(state: &JobState) -> (Color32, &'static str) {
    match state {
        JobState::Queued => (Color32::from_rgb(120, 120, 120), "Queued"),
        JobState::Downloading => (Color32::from_rgb(80, 180, 80), "Downloading"),
        JobState::Paused => (Color32::from_rgb(200, 160, 40), "Paused"),
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
