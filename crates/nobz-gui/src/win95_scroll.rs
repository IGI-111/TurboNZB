//! Win95-style scroll area with elevator scrollbar and custom table.
//!
//! Replaces egui's default ScrollArea and egui_extras::TableBuilder with
//! pixel-accurate Win95 scrollbars: 17px wide, arrow buttons at top/bottom,
//! dithered checkerboard track, raised beveled thumb (elevator).

use std::hash::Hash;

use egui::{Color32, Id, Layout, Painter, Pos2, Rect, Sense, Shape, Stroke, Ui, UiBuilder, Vec2};

use crate::theme::colors;
use crate::win95_widgets::{paint_raised_bevel, paint_sunken_bevel};

const SCROLLBAR_WIDTH: f32 = 17.0;
const ARROW_SIZE: f32 = 17.0;
const MIN_THUMB: f32 = 12.0;
const SCROLL_LINE: f32 = 16.0;

// ─── Scroll area ─────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct ScrollState {
    offset: f32,
}

/// A vertical scroll area with a Win95 elevator scrollbar.
pub fn vertical<R>(ui: &mut Ui, id_salt: impl Hash, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let id = ui.id().with(id_salt);
    let available = ui.available_rect_before_wrap();
    let total_w = available.width();
    let content_w = (total_w - SCROLLBAR_WIDTH).max(0.0);
    let height = available.height();

    let content_rect = Rect::from_min_size(available.min, Vec2::new(content_w, height));
    let bar_rect = Rect::from_min_size(
        Pos2::new(content_rect.right(), available.top()),
        Vec2::new(SCROLLBAR_WIDTH, height),
    );

    let mut state = ui.data_mut(|d| d.get_temp::<ScrollState>(id).unwrap_or_default());

    // Paint content background so there are no white bands.
    ui.painter()
        .rect_filled(content_rect, 0.0, colors::BUTTON_FACE);

    // Create child UI for content, shifted by scroll offset.
    let content_max_rect = Rect::from_min_size(
        Pos2::new(content_rect.left(), content_rect.top() - state.offset),
        Vec2::new(content_w, f32::INFINITY),
    );
    let mut content_ui = ui.new_child(UiBuilder::new().max_rect(content_max_rect));
    // Intersect clip rect with content rect so text is clipped when scrolled.
    content_ui.set_clip_rect(content_rect.intersect(ui.clip_rect()));

    let result = add_contents(&mut content_ui);

    // Measure content.
    let content_h = content_ui.min_rect().height().max(content_rect.height());
    let max_offset = (content_h - content_rect.height()).max(0.0);
    state.offset = state.offset.min(max_offset).max(0.0);

    // Scroll wheel.
    if ui.rect_contains_pointer(content_rect) {
        let delta = ui.input(|i| i.smooth_scroll_delta[0] + i.smooth_scroll_delta[1]);
        if delta != 0.0 {
            state.offset = (state.offset - delta).max(0.0).min(max_offset);
        }
    }

    // ── Draw scrollbar ──
    let painter = ui.painter_at(bar_rect);
    painter.rect_filled(bar_rect, 0.0, colors::BUTTON_FACE);

    // Up arrow button.
    let up_rect = Rect::from_min_size(bar_rect.min, Vec2::new(SCROLLBAR_WIDTH, ARROW_SIZE));
    let up_id = id.with("up");
    let up_resp = ui.interact(up_rect, up_id, Sense::click());
    let up_pressed = up_resp.is_pointer_button_down_on();
    painter.rect_filled(up_rect, 0.0, colors::BUTTON_FACE);
    if up_pressed {
        paint_sunken_bevel(&painter, up_rect);
    } else {
        paint_raised_bevel(&painter, up_rect);
    }
    draw_arrow(&painter, up_rect, true, up_pressed);
    if up_resp.clicked() {
        state.offset = (state.offset - SCROLL_LINE).max(0.0);
    }

    // Down arrow button.
    let down_rect = Rect::from_min_size(
        Pos2::new(bar_rect.left(), bar_rect.bottom() - ARROW_SIZE),
        Vec2::new(SCROLLBAR_WIDTH, ARROW_SIZE),
    );
    let down_id = id.with("down");
    let down_resp = ui.interact(down_rect, down_id, Sense::click());
    let down_pressed = down_resp.is_pointer_button_down_on();
    painter.rect_filled(down_rect, 0.0, colors::BUTTON_FACE);
    if down_pressed {
        paint_sunken_bevel(&painter, down_rect);
    } else {
        paint_raised_bevel(&painter, down_rect);
    }
    draw_arrow(&painter, down_rect, false, down_pressed);
    if down_resp.clicked() {
        state.offset = (state.offset + SCROLL_LINE).min(max_offset);
    }

    // Track between arrows.
    let track_rect = Rect::from_min_max(
        Pos2::new(bar_rect.left(), up_rect.bottom()),
        Pos2::new(bar_rect.right(), down_rect.top()),
    );

    if max_offset > 0.0 {
        paint_dithered_track(&painter, track_rect);

        // Thumb (elevator).
        let track_h = track_rect.height();
        let thumb_h = (content_rect.height() / content_h * track_h).clamp(MIN_THUMB, track_h);
        let thumb_y = track_rect.top() + (state.offset / max_offset) * (track_h - thumb_h);
        let thumb_rect = Rect::from_min_size(
            Pos2::new(track_rect.left(), thumb_y),
            Vec2::new(SCROLLBAR_WIDTH, thumb_h),
        );

        // Thumb drag.
        let thumb_id = id.with("thumb");
        let thumb_resp = ui.interact(thumb_rect, thumb_id, Sense::drag());
        if thumb_resp.dragged() {
            if let Some(pos) = thumb_resp.interact_pointer_pos() {
                let new_top = pos.y - thumb_h / 2.0;
                let ratio = ((new_top - track_rect.top()) / (track_h - thumb_h)).clamp(0.0, 1.0);
                state.offset = ratio * max_offset;
            }
        }

        // Track click = page up/down.
        let track_id = id.with("track");
        let track_resp = ui.interact(track_rect, track_id, Sense::click());
        if track_resp.clicked() {
            if let Some(pos) = track_resp.interact_pointer_pos() {
                if pos.y < thumb_rect.top() {
                    state.offset = (state.offset - content_rect.height()).max(0.0);
                } else {
                    state.offset = (state.offset + content_rect.height()).min(max_offset);
                }
            }
        }

        // Draw thumb.
        painter.rect_filled(thumb_rect, 0.0, colors::BUTTON_FACE);
        paint_raised_bevel(&painter, thumb_rect);
    } else {
        // No scrolling needed — fill track with face color.
        painter.rect_filled(track_rect, 0.0, colors::BUTTON_FACE);
    }

    // Consume the full rect.
    ui.advance_cursor_after_rect(available);

    // Store state.
    ui.data_mut(|d| d.insert_temp(id, state));

    result
}

fn draw_arrow(painter: &Painter, rect: Rect, up: bool, pressed: bool) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let s = 4.0_f32;
    let off = if pressed {
        Vec2::new(1.0, 1.0)
    } else {
        Vec2::ZERO
    };
    let color = colors::WINDOW_TEXT;
    if up {
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(cx - s + off.x, cy + s * 0.7 + off.y),
                Pos2::new(cx + s + off.x, cy + s * 0.7 + off.y),
                Pos2::new(cx + off.x, cy - s + off.y),
            ],
            color,
            Stroke::NONE,
        ));
    } else {
        painter.add(Shape::convex_polygon(
            vec![
                Pos2::new(cx - s + off.x, cy - s * 0.7 + off.y),
                Pos2::new(cx + s + off.x, cy - s * 0.7 + off.y),
                Pos2::new(cx + off.x, cy + s + off.y),
            ],
            color,
            Stroke::NONE,
        ));
    }
}

fn paint_dithered_track(painter: &Painter, rect: Rect) {
    let checker = 2.0_f32;
    let cols = (rect.width() / checker).ceil() as i32;
    let rows = (rect.height() / checker).ceil() as i32;
    for row in 0..rows {
        for col in 0..cols {
            let color = if (row + col) % 2 == 0 {
                colors::BUTTON_FACE
            } else {
                colors::BUTTON_LIGHT
            };
            let x = rect.left() + col as f32 * checker;
            let y = rect.top() + row as f32 * checker;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, y), Vec2::new(checker, checker)),
                0.0,
                color,
            );
        }
    }
}

// ─── Table ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ColSpec {
    Exact(f32),
    Remainder,
}

pub struct Win95Table {
    columns: Vec<ColSpec>,
    striped: bool,
    id_salt: Id,
    min_height: f32,
}

impl Default for Win95Table {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            striped: false,
            id_salt: Id::new("__win95_table"),
            min_height: 200.0,
        }
    }
}

impl Win95Table {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn column(mut self, width: f32) -> Self {
        self.columns.push(ColSpec::Exact(width));
        self
    }

    pub fn column_remainder(mut self) -> Self {
        self.columns.push(ColSpec::Remainder);
        self
    }

    pub fn striped(mut self, striped: bool) -> Self {
        self.striped = striped;
        self
    }

    pub fn min_scrolled_height(mut self, h: f32) -> Self {
        self.min_height = h;
        self
    }

    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        self.id_salt = Id::new(salt);
        self
    }

    fn compute_widths(&self, available: f32) -> Vec<f32> {
        compute_widths(&self.columns, available)
    }

    /// Render header + body in one call.
    pub fn header_body(
        self,
        ui: &mut Ui,
        header_height: f32,
        add_header: impl FnOnce(&mut TableRow, &mut Ui),
        add_body: impl FnOnce(&mut TableBody<'_>),
    ) {
        let total_width = ui.available_width();
        let content_width = (total_width - SCROLLBAR_WIDTH).max(0.0);
        let widths = self.compute_widths(content_width);

        // Header background (full width including scrollbar column).
        let header_rect =
            Rect::from_min_size(ui.cursor().min, Vec2::new(total_width, header_height));
        let painter = ui.painter_at(header_rect);
        painter.rect_filled(header_rect, 0.0, colors::BUTTON_FACE);
        paint_raised_bevel(&painter, header_rect);

        // Create child UI for header content.
        let mut header_ui = ui.new_child(UiBuilder::new().max_rect(header_rect));
        header_ui.set_clip_rect(header_rect.intersect(ui.clip_rect()));

        let mut row = TableRow::header_row(
            header_rect.left(),
            header_rect.top(),
            header_height,
            widths.clone(),
        );
        add_header(&mut row, &mut header_ui);

        // Advance cursor past header.
        ui.advance_cursor_after_rect(header_rect);

        // Body.
        let striped = self.striped;
        let id_salt = self.id_salt;
        let widths_inner = widths;
        vertical(ui, id_salt, |ui| {
            let mut body = TableBody {
                ui,
                widths: widths_inner,
                striped,
                row_index: 0,
                content_width,
            };
            add_body(&mut body);
        });
    }
}

fn compute_widths(columns: &[ColSpec], available: f32) -> Vec<f32> {
    let mut widths = Vec::with_capacity(columns.len());
    let mut remainder_count = 0usize;
    let mut used = 0.0_f32;
    for col in columns {
        match col {
            ColSpec::Exact(w) => {
                widths.push(*w);
                used += *w;
            }
            ColSpec::Remainder => {
                widths.push(0.0);
                remainder_count += 1;
            }
        }
    }
    if remainder_count > 0 {
        let rem = ((available - used) / remainder_count as f32).max(0.0);
        for (i, col) in columns.iter().enumerate() {
            if matches!(col, ColSpec::Remainder) {
                widths[i] = rem;
            }
        }
    }
    widths
}

// ─── Table body (returned by header(), has body() method) ────────────────

// ─── Table row (used by header and body) ─────────────────────────────────

pub struct TableRow {
    left: f32,
    current_x: f32,
    top: f32,
    height: f32,
    widths: Vec<f32>,
    col_index: usize,
    is_header: bool,
}

impl TableRow {
    pub fn new(top: f32, height: f32, widths: Vec<f32>) -> Self {
        Self {
            left: 0.0,
            current_x: 0.0,
            top,
            height,
            widths,
            col_index: 0,
            is_header: false,
        }
    }

    pub fn with_left(left: f32, top: f32, height: f32, widths: Vec<f32>) -> Self {
        Self {
            left,
            current_x: 0.0,
            top,
            height,
            widths,
            col_index: 0,
            is_header: false,
        }
    }

    pub fn header_row(left: f32, top: f32, height: f32, widths: Vec<f32>) -> Self {
        Self {
            left,
            current_x: 0.0,
            top,
            height,
            widths,
            col_index: 0,
            is_header: true,
        }
    }

    pub fn col<R>(&mut self, ui: &mut Ui, add_content: impl FnOnce(&mut Ui) -> R) -> R {
        let width = self.widths.get(self.col_index).copied().unwrap_or(0.0);
        let col_rect = Rect::from_min_size(
            Pos2::new(self.left + self.current_x, self.top),
            Vec2::new(width, self.height),
        );
        self.current_x += width;
        self.col_index += 1;

        // Header cells: centered+justified (short labels, fine to fill).
        // Body cells: left-to-right with vertical center, no justify (clips
        // instead of wrapping to multiple lines).
        let layout = if self.is_header {
            Layout::centered_and_justified(egui::Direction::TopDown)
        } else {
            Layout::left_to_right(egui::Align::Center)
        };

        let mut col_ui = ui.new_child(UiBuilder::new().max_rect(col_rect).layout(layout));
        // Intersect with parent clip rect so content is clipped properly.
        col_ui.set_clip_rect(col_rect.intersect(ui.clip_rect()));
        add_content(&mut col_ui)
    }
}

// ─── Table body (inside scroll area, renders rows) ───────────────────────

pub struct TableBody<'a> {
    ui: &'a mut Ui,
    widths: Vec<f32>,
    striped: bool,
    row_index: usize,
    content_width: f32,
}

impl<'a> TableBody<'a> {
    pub fn row<R>(&mut self, height: f32, add_row: impl FnOnce(&mut TableRow, &mut Ui) -> R) -> R {
        let (row_rect, _) = self
            .ui
            .allocate_exact_size(Vec2::new(self.content_width, height), Sense::hover());

        // Striped background — very subtle, barely visible.
        if self.striped && self.row_index % 2 == 1 {
            self.ui
                .painter()
                .rect_filled(row_rect, 0.0, Color32::from_rgb(223, 223, 223));
        }

        let mut row =
            TableRow::with_left(row_rect.left(), row_rect.top(), height, self.widths.clone());
        let result = add_row(&mut row, self.ui);
        self.row_index += 1;
        result
    }
}
