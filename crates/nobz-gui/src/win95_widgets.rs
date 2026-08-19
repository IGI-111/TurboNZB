//! Windows 95 custom widgets — pixel-accurate bevels, buttons, and frames.
//!
//! These widgets bypass egui's default single-stroke border system and paint
//! the authentic 2-layer Win95 3D bevels using individual edge lines.
//!
//! ## Bevel anatomy
//!
//! A raised Win95 button has 2px of border per side, drawn as two nested 1px
//! bevels:
//!
//! ```text
//!   Top/Left (light):    Bottom/Right (dark):
//!   outer #FFFFFF         outer #000000
//!   inner #DFDFDF         inner #808080
//! ```
//!
//! Sunken (pressed/text fields) inverts the scheme. Disabled text gets a white
//! +1,+1 emboss shadow. Focused widgets show a 1px dotted black rect inset 4px.

use egui::{
    Color32, FontId, Pos2, Response, Sense, Shape, Stroke, TextWrapMode, Ui, Vec2, Widget,
    WidgetInfo, WidgetText, WidgetType,
};

use crate::theme::colors;

// Re-export egui's NumExt so we can use .at_least() / .at_most() on f32.
use egui::NumExt;

// ─── Bevel painting helpers ──────────────────────────────────────────────

/// Paint a raised 2-layer Win95 bevel around `rect`.
pub fn paint_raised_bevel(painter: &egui::Painter, rect: egui::Rect) {
    let outer = rect;
    let inner = rect.shrink(1.0);

    // Outer bevel (1px): top/left = highlight, bottom/right = darkest.
    painter.hline(
        outer.x_range(),
        outer.top(),
        Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
    );
    painter.vline(
        outer.left(),
        outer.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
    );
    painter.hline(
        outer.x_range(),
        outer.bottom() - 1.0,
        Stroke::new(1.0_f32, colors::BUTTON_DARK_SHADOW),
    );
    painter.vline(
        outer.right() - 1.0,
        outer.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_DARK_SHADOW),
    );

    // Inner bevel (1px): top/left = light, bottom/right = shadow.
    painter.hline(
        inner.x_range(),
        inner.top(),
        Stroke::new(1.0_f32, colors::BUTTON_LIGHT),
    );
    painter.vline(
        inner.left(),
        inner.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_LIGHT),
    );
    painter.hline(
        inner.x_range(),
        inner.bottom() - 1.0,
        Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
    );
    painter.vline(
        inner.right() - 1.0,
        inner.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
    );
}

/// Paint a sunken 2-layer Win95 bevel around `rect` (for text fields, list
/// views, status bar wells).
pub fn paint_sunken_bevel(painter: &egui::Painter, rect: egui::Rect) {
    let outer = rect;
    let inner = rect.shrink(1.0);

    // Outer bevel (1px): top/left = darkest, bottom/right = highlight.
    painter.hline(
        outer.x_range(),
        outer.top(),
        Stroke::new(1.0_f32, colors::BUTTON_DARK_SHADOW),
    );
    painter.vline(
        outer.left(),
        outer.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_DARK_SHADOW),
    );
    painter.hline(
        outer.x_range(),
        outer.bottom() - 1.0,
        Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
    );
    painter.vline(
        outer.right() - 1.0,
        outer.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
    );

    // Inner bevel (1px): top/left = shadow, bottom/right = light.
    painter.hline(
        inner.x_range(),
        inner.top(),
        Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
    );
    painter.vline(
        inner.left(),
        inner.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
    );
    painter.hline(
        inner.x_range(),
        inner.bottom() - 1.0,
        Stroke::new(1.0_f32, colors::BUTTON_LIGHT),
    );
    painter.vline(
        inner.right() - 1.0,
        inner.y_range(),
        Stroke::new(1.0_f32, colors::BUTTON_LIGHT),
    );
}

/// Paint a "field" style bevel (sunken with black outer) for text inputs.
pub fn paint_field_bevel(painter: &egui::Painter, rect: egui::Rect) {
    paint_sunken_bevel(painter, rect);
}

/// Paint a dotted focus rectangle inset 4px from `rect`.
pub fn paint_focus_rect(painter: &egui::Painter, rect: egui::Rect) {
    let focus = rect.shrink(4.0);
    let lt = focus.left_top();
    let rt = focus.right_top();
    let rb = focus.right_bottom();
    let lb = focus.left_bottom();
    let points = [lt, rt, rb, lb, lt];
    let shapes = Shape::dotted_line(&points, Color32::BLACK, 0.0, 1.0);
    painter.extend(shapes);
}

/// Fill the face color inside the bevel (shrink 2px from each edge).
pub fn fill_face(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect.shrink(2.0), 0.0, colors::BUTTON_FACE);
}

// ─── Win95Button — text beveled button ──────────────────────────────────

/// A Windows 95-style beveled text button.
pub struct Win95Button {
    text: WidgetText,
    enabled: bool,
    min_size: Vec2,
    selected: bool,
}

impl Win95Button {
    pub fn new(text: impl Into<WidgetText>) -> Self {
        Self {
            text: text.into(),
            enabled: true,
            min_size: Vec2::ZERO,
            selected: false,
        }
    }

    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }

    pub fn min_size(mut self, s: Vec2) -> Self {
        self.min_size = s;
        self
    }

    pub fn selected(mut self, s: bool) -> Self {
        self.selected = s;
        self
    }
}

impl Widget for Win95Button {
    fn ui(self, ui: &mut Ui) -> Response {
        let padding = ui.spacing().button_padding;
        let wrap_width = ui.available_width() - 2.0 * padding.x;
        let galley = self.text.into_galley(
            ui,
            Some(TextWrapMode::Extend),
            wrap_width,
            egui::TextStyle::Button,
        );

        // Minimum 75x23 like a standard Win95 button, plus bevel margin.
        let mut desired = galley.size() + 2.0 * padding + Vec2::new(8.0, 6.0);
        desired.y = desired.y.at_least(23.0);
        desired.x = desired.x.at_least(75.0);
        desired = desired.at_least(self.min_size);

        let (rect, mut response) = ui.allocate_at_least(desired, Sense::click());
        if !self.enabled {
            response = response.on_hover_cursor(egui::CursorIcon::NotAllowed);
        }
        response
            .widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, galley.text()));

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            painter.rect_filled(rect, 0.0, colors::BUTTON_FACE);

            let pressed = self.enabled && (response.is_pointer_button_down_on() || self.selected);
            if pressed {
                paint_sunken_bevel(painter, rect);
            } else {
                paint_raised_bevel(painter, rect);
            }

            // Focus rect
            if response.has_focus() && !pressed {
                paint_focus_rect(painter, rect);
            }

            // Content offset: 1px down-right when pressed.
            let off = if pressed {
                Vec2::new(1.0, 1.0)
            } else {
                Vec2::ZERO
            };

            let text_pos = ui
                .layout()
                .align_size_within_rect(galley.size(), rect.shrink2(padding))
                .min
                + off;

            if self.enabled {
                painter.galley(text_pos, galley, colors::WINDOW_TEXT);
            } else {
                // Embossed disabled text: white shadow at +1,+1 then gray.
                painter.galley(
                    text_pos + Vec2::new(1.0, 1.0),
                    galley.clone(),
                    colors::DISABLED_TEXT_SHADOW,
                );
                painter.galley(text_pos, galley, colors::DISABLED_TEXT);
            }
        }

        response
    }
}

// ─── Win95IconButton — small toolbar icon button ───────────────────────

/// A small Win95-style toolbar button with just an icon (no text).
pub struct Win95IconButton {
    icon: egui::TextureHandle,
    icon_size: Vec2,
    enabled: bool,
    tooltip: &'static str,
    selected: bool,
}

impl Win95IconButton {
    pub fn new(icon: egui::TextureHandle) -> Self {
        Self {
            icon,
            icon_size: Vec2::new(16.0, 16.0),
            enabled: true,
            tooltip: "",
            selected: false,
        }
    }

    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }

    pub fn tooltip(mut self, t: &'static str) -> Self {
        self.tooltip = t;
        self
    }

    pub fn icon_size(mut self, s: Vec2) -> Self {
        self.icon_size = s;
        self
    }

    pub fn selected(mut self, s: bool) -> Self {
        self.selected = s;
        self
    }
}

impl Widget for Win95IconButton {
    fn ui(self, ui: &mut Ui) -> Response {
        let size = Vec2::new(self.icon_size.x + 10.0, self.icon_size.y + 4.0);
        let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
        if !self.enabled {
            response = response.on_hover_cursor(egui::CursorIcon::NotAllowed);
        }
        if !self.tooltip.is_empty() {
            response = response.on_hover_text(self.tooltip);
        }
        response
            .widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, self.tooltip));

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            painter.rect_filled(rect, 0.0, colors::BUTTON_FACE);

            let pressed = self.enabled && (response.is_pointer_button_down_on() || self.selected);
            if pressed {
                paint_sunken_bevel(painter, rect);
            } else {
                paint_raised_bevel(painter, rect);
            }

            if response.has_focus() && !pressed {
                paint_focus_rect(painter, rect);
            }

            let off = if pressed {
                Vec2::new(1.0, 1.0)
            } else {
                Vec2::ZERO
            };

            let icon_rect = egui::Rect::from_center_size(rect.center() + off, self.icon_size);
            let tint = if self.enabled {
                Color32::WHITE
            } else {
                Color32::from_rgb(128, 128, 128)
            };
            painter.image(
                self.icon.id(),
                icon_rect,
                egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                tint,
            );
        }

        response
    }
}

// ─── Win95Group — raised beveled group box ──────────────────────────────

/// Render a Win95-style group box with a raised beveled frame and optional
/// label in the top-left border. The inner content is rendered by the
/// provided closure inside a child UI with proper padding. The group box
/// stretches to fill the full available width.
pub fn group<R>(ui: &mut Ui, label: Option<&str>, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    // Claim the full available width so the group box spans the panel.
    let full_width = ui.available_width();
    ui.vertical(|ui| {
        ui.set_min_width(full_width);

        // Reserve space for the label on the top border.
        let label_galley = label.map(|l| {
            ui.painter().layout_no_wrap(
                l.to_string(),
                FontId::proportional(16.0),
                colors::WINDOW_TEXT,
            )
        });

        let mut content_rect: Option<egui::Rect> = None;

        // Generous padding: 14px left/right, 16px top (room for label),
        // 12px bottom.
        let label_h = label_galley.as_ref().map_or(0.0, |g| g.size().y);
        let frame = egui::Frame::none()
            .fill(colors::BUTTON_FACE)
            .inner_margin(egui::Margin {
                left: 14.0,
                right: 14.0,
                top: 16.0 + label_h,
                bottom: 12.0,
            });

        let inner = frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let before = ui.min_rect();
            let result = add_contents(ui);
            let after = ui.min_rect();
            content_rect = Some(egui::Rect::from_min_max(
                before.min - Vec2::new(14.0, 16.0 + label_h),
                after.max + Vec2::new(14.0, 12.0),
            ));
            result
        });

        // Draw bevel around the content area.
        if let Some(r) = content_rect {
            let painter = ui.painter_at(r);
            paint_raised_bevel(&painter, r);

            // Draw label background on the top border, vertically centered
            // on the top bevel line.
            if let Some(g) = &label_galley {
                let label_x = r.left() + 12.0;
                let label_y = r.top() + 2.0;
                let label_w = g.size().x + 8.0;
                let label_h = g.size().y;
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        Pos2::new(label_x, label_y),
                        Vec2::new(label_w, label_h),
                    ),
                    0.0,
                    colors::BUTTON_FACE,
                );
                painter.galley(
                    Pos2::new(label_x + 4.0, label_y),
                    g.clone(),
                    colors::WINDOW_TEXT,
                );
            }
        }

        inner.inner
    })
    .inner
}

// ─── Win95StatusBar — sunken well segments ─────────────────────────────

/// Paint a sunken status bar well of a given size, returning the inner rect
/// for text placement.
pub fn status_segment(
    ui: &mut Ui,
    width: f32,
    height: f32,
    text: &str,
    icon: Option<egui::TextureHandle>,
) -> Response {
    let size = Vec2::new(width, height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 0.0, colors::BUTTON_FACE);
    paint_sunken_bevel(&painter, rect);

    let inner = rect.shrink(3.0);
    let mut text_x = inner.left() + 2.0;
    if let Some(tex) = icon {
        let icon_rect = egui::Rect::from_min_size(
            Pos2::new(text_x, inner.center().y - 7.0),
            Vec2::new(14.0, 14.0),
        );
        painter.image(
            tex.id(),
            icon_rect,
            egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        text_x += 18.0;
    }
    painter.text(
        Pos2::new(text_x, inner.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(14.0),
        colors::WINDOW_TEXT,
    );

    response
}

/// Paint a raised separator line (etched groove).
pub fn etched_separator(ui: &mut Ui, horizontal: bool) {
    let size = if horizontal {
        Vec2::new(ui.available_width(), 2.0)
    } else {
        Vec2::new(2.0, ui.available_height())
    };
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    if horizontal {
        painter.hline(
            rect.x_range(),
            rect.top(),
            Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
        );
        painter.hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
        );
    } else {
        painter.vline(
            rect.left(),
            rect.y_range(),
            Stroke::new(1.0_f32, colors::BUTTON_SHADOW),
        );
        painter.vline(
            rect.right() - 1.0,
            rect.y_range(),
            Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT),
        );
    }
}

// ─── Win95TabButton — tab strip button ──────────────────────────────────

/// A Win95-style tab button (for the main tab bar). Selected tab looks
/// raised/pressed; unselected tabs look raised.
pub struct Win95TabButton {
    icon: Option<egui::TextureHandle>,
    label: &'static str,
    selected: bool,
    icon_size: Vec2,
}

impl Win95TabButton {
    pub fn new(icon: Option<egui::TextureHandle>, label: &'static str, selected: bool) -> Self {
        Self {
            icon,
            label,
            selected,
            icon_size: Vec2::new(16.0, 16.0),
        }
    }

    pub fn icon_size(mut self, s: Vec2) -> Self {
        self.icon_size = s;
        self
    }
}

impl Widget for Win95TabButton {
    fn ui(self, ui: &mut Ui) -> Response {
        let padding = Vec2::new(8.0, 4.0);
        let icon_w = self.icon.as_ref().map_or(0.0, |_| self.icon_size.x + 4.0);
        let label_galley = ui.painter().layout_no_wrap(
            self.label.to_string(),
            FontId::proportional(16.0),
            colors::WINDOW_TEXT,
        );
        let text_w = label_galley.size().x;
        let h = 28.0_f32;
        let w = icon_w + text_w + 2.0 * padding.x;

        let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, self.label));

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            painter.rect_filled(rect, 0.0, colors::BUTTON_FACE);

            let pressed = response.is_pointer_button_down_on();

            if self.selected {
                // Selected tab: raised outer, but the bottom border is the
                // face color (looks connected to the content below).
                paint_raised_bevel(painter, rect);
                // Overwrite bottom border with face color (no bottom line).
                painter.hline(
                    rect.x_range(),
                    rect.bottom() - 1.0,
                    Stroke::new(2.0_f32, colors::BUTTON_FACE),
                );
                painter.hline(
                    rect.x_range(),
                    rect.bottom() - 2.0,
                    Stroke::new(1.0_f32, colors::BUTTON_FACE),
                );
            } else if pressed {
                // Pressed (clicking an unselected tab): sunken bevel.
                paint_sunken_bevel(painter, rect);
            } else {
                // Unselected tab: plain raised bevel.
                paint_raised_bevel(painter, rect);
            }

            // Content.
            let mut x = rect.left() + padding.x;
            let cy = rect.center().y;
            if let Some(tex) = self.icon {
                let ir = egui::Rect::from_center_size(
                    Pos2::new(x + self.icon_size.x / 2.0, cy),
                    self.icon_size,
                );
                painter.image(
                    tex.id(),
                    ir,
                    egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                x += self.icon_size.x + 4.0;
            }
            painter.galley(
                Pos2::new(x, cy - label_galley.size().y / 2.0),
                label_galley,
                colors::WINDOW_TEXT,
            );
        }

        response
    }
}

// ─── Win95Checkbox — beveled checkbox ──────────────────────────────────

/// A Win95-style checkbox with sunken check square.
pub struct Win95Checkbox<'a> {
    checked: &'a mut bool,
    text: &'a str,
    enabled: bool,
}

impl<'a> Win95Checkbox<'a> {
    pub fn new(checked: &'a mut bool, text: &'a str) -> Self {
        Self {
            checked,
            text,
            enabled: true,
        }
    }

    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }
}

impl<'a> Widget for Win95Checkbox<'a> {
    fn ui(self, ui: &mut Ui) -> Response {
        let box_size = 13.0_f32;
        let _padding = Vec2::new(4.0, 2.0);
        let galley = ui.painter().layout_no_wrap(
            self.text.to_string(),
            FontId::proportional(16.0),
            if self.enabled {
                colors::WINDOW_TEXT
            } else {
                colors::DISABLED_TEXT
            },
        );
        let h = box_size.max(galley.size().y + 2.0);
        let w = box_size + 4.0 + galley.size().x;

        let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Checkbox, self.enabled, self.text));

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            let box_rect = egui::Rect::from_min_size(
                Pos2::new(rect.left(), rect.center().y - box_size / 2.0),
                Vec2::new(box_size, box_size),
            );
            painter.rect_filled(box_rect, 0.0, colors::BUTTON_FACE);
            paint_field_bevel(painter, box_rect);

            if *self.checked {
                // Draw an X or checkmark inside the box.
                let p = box_rect.shrink(2.0);
                let stroke = Stroke::new(
                    1.5_f32,
                    if self.enabled {
                        colors::WINDOW_TEXT
                    } else {
                        colors::DISABLED_TEXT
                    },
                );
                // Diagonal checkmark.
                painter.line_segment(
                    [
                        p.left_bottom() - Vec2::new(0.0, 1.0),
                        Pos2::new(p.left() + p.width() * 0.35, p.center().y),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        Pos2::new(p.left() + p.width() * 0.35, p.center().y),
                        p.right_top() + Vec2::new(0.0, 1.0),
                    ],
                    stroke,
                );
            }

            let text_pos = Pos2::new(
                box_rect.right() + 4.0,
                rect.center().y - galley.size().y / 2.0,
            );
            if self.enabled {
                painter.galley(text_pos, galley, colors::WINDOW_TEXT);
            } else {
                painter.galley(
                    text_pos + Vec2::new(1.0, 1.0),
                    galley.clone(),
                    colors::DISABLED_TEXT_SHADOW,
                );
                painter.galley(text_pos, galley, colors::DISABLED_TEXT);
            }

            if response.has_focus() {
                paint_focus_rect(painter, rect);
            }
        }

        if response.clicked() && self.enabled {
            *self.checked = !*self.checked;
        }

        response
    }
}

// ─── ProgressBar — Win95-style chunky blue progress ────────────────────

/// A Win95-style progress bar: sunken white well with blue segment fill.
pub struct Win95ProgressBar {
    progress: f32,
    text: Option<String>,
}

impl Win95ProgressBar {
    pub fn new(progress: f32) -> Self {
        Self {
            progress: progress.clamp(0.0, 1.0),
            text: None,
        }
    }

    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.text = Some(t.into());
        self
    }
}

impl Widget for Win95ProgressBar {
    fn ui(self, ui: &mut Ui) -> Response {
        let height = 22.0_f32;
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter_at(rect);

        // Sunken bevel + white background.
        painter.rect_filled(rect, 0.0, colors::BUTTON_FACE);
        paint_sunken_bevel(&painter, rect);
        let inner = rect.shrink(2.0);
        painter.rect_filled(inner, 0.0, colors::WINDOW);

        // Blue fill.
        let fill_w = if self.progress > 0.0 {
            let w = inner.width() * self.progress;
            let fill_rect =
                egui::Rect::from_min_size(inner.left_top(), Vec2::new(w, inner.height()));
            painter.rect_filled(fill_rect, 0.0, colors::TITLE_BAR_ACTIVE);
            w
        } else {
            0.0
        };

        // Optional text overlay — Win95 style: each character is white if
        // it's over the blue fill, black if it's over the white background.
        if let Some(t) = &self.text {
            let galley =
                painter.layout_no_wrap(t.clone(), FontId::proportional(14.0), colors::WINDOW_TEXT);
            let pos = ui.layout().align_size_within_rect(galley.size(), inner).min;
            let fill_right = inner.left() + fill_w;

            // Paint each character individually with the right color.
            for row in &galley.rows {
                for glyph in &row.glyphs {
                    let glyph_center_x = pos.x + glyph.pos.x + glyph.advance_width / 2.0;
                    let color = if glyph_center_x <= fill_right && fill_w > 0.0 {
                        Color32::WHITE
                    } else {
                        colors::WINDOW_TEXT
                    };
                    painter.text(
                        egui::pos2(pos.x + glyph.pos.x, pos.y + row.rect.top()),
                        egui::Align2::LEFT_TOP,
                        glyph.chr.to_string(),
                        FontId::proportional(14.0),
                        color,
                    );
                }
            }
        }

        response
    }
}
