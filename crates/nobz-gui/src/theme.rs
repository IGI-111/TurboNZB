//! Windows 95-style theming for egui.
//!
//! Loads the Win95 color palette, flat widget styling (zero rounding),
//! and icon textures from the `icons/` directory.

use egui::{Context, TextureHandle};

/// Win95 color palette.
pub mod colors {
    use egui::Color32;
    pub const DESKTOP: Color32 = Color32::from_rgb(0, 128, 128); // teal
    pub const BUTTON_FACE: Color32 = Color32::from_rgb(192, 192, 192); // #c0c0c0
    pub const BUTTON_HIGHLIGHT: Color32 = Color32::from_rgb(255, 255, 255); // #ffffff
    pub const BUTTON_LIGHT: Color32 = Color32::from_rgb(223, 223, 223);
    pub const BUTTON_SHADOW: Color32 = Color32::from_rgb(128, 128, 128); // #808080
    pub const BUTTON_DARK_SHADOW: Color32 = Color32::from_rgb(64, 64, 64);
    pub const WINDOW: Color32 = Color32::from_rgb(192, 192, 192);
    pub const WINDOW_TEXT: Color32 = Color32::from_rgb(0, 0, 0);
    pub const TITLE_BAR_ACTIVE: Color32 = Color32::from_rgb(0, 0, 128); // navy
    pub const TITLE_BAR_ACTIVE_TEXT: Color32 = Color32::from_rgb(255, 255, 255);
    pub const TITLE_BAR_INACTIVE: Color32 = Color32::from_rgb(128, 128, 128);
    pub const TITLE_BAR_INACTIVE_TEXT: Color32 = Color32::from_rgb(192, 192, 192);
    pub const SELECTION: Color32 = Color32::from_rgb(0, 0, 128);
    pub const SELECTION_TEXT: Color32 = Color32::from_rgb(255, 255, 255);
    /// Lighter selection for text editing — black text must remain readable.
    pub const TEXT_SELECTION: Color32 = Color32::from_rgb(180, 200, 255);
    pub const HIGHLIGHT: Color32 = Color32::from_rgb(0, 0, 128);
    pub const LINK: Color32 = Color32::from_rgb(0, 0, 255);
}

/// Icon textures loaded at startup.
pub struct Icons {
    pub search: TextureHandle,
    pub download: TextureHandle,
    pub folder_open: TextureHandle,
    pub info: TextureHandle,
    pub settings: TextureHandle,
    pub delete: TextureHandle,
    pub warning: TextureHandle,
    pub tick: TextureHandle,
    pub computer: TextureHandle,
    pub network: TextureHandle,
    pub file_transfer: TextureHandle,
}

impl Icons {
    /// Load all icons from embedded PNG data.
    pub fn load(ctx: &Context) -> Self {
        Self {
            search: load_icon(ctx, "FileFind_32x32_4.png"),
            download: load_icon(ctx, "Download_16x16_4.png"),
            folder_open: load_icon(ctx, "FolderOpen_16x16_4.png"),
            info: load_icon(ctx, "InfoBubble_32x32_4.png"),
            settings: load_icon(ctx, "Settings_16x16_4.png"),
            delete: load_icon(ctx, "Delete_16x16_4.png"),
            warning: load_icon(ctx, "Warning_32x32_4.png"),
            tick: load_icon(ctx, "Tick_16x16_4.png"),
            computer: load_icon(ctx, "Computer_16x16_4.png"),
            network: load_icon(ctx, "Network_32x32_4.png"),
            file_transfer: load_icon(ctx, "FileTransfer_32x32_4.png"),
        }
    }
}

fn load_icon(ctx: &Context, filename: &str) -> TextureHandle {
    let bytes = include_bytes_concat(filename);
    let image = image::load_from_memory(bytes)
        .expect("icon load")
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let pixels = image.into_raw();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
    ctx.load_texture(filename, color_image, egui::TextureOptions::NEAREST)
}

/// Concatenate the icon path prefix.
fn include_bytes_concat(filename: &str) -> &'static [u8] {
    // This is a macro-free approach using match. Since include_bytes!
    // requires a literal, we use a match statement for each icon.
    match filename {
        "FileFind_32x32_4.png" => include_bytes!("../icons/FileFind_32x32_4.png"),
        "Download_16x16_4.png" => include_bytes!("../icons/Download_16x16_4.png"),
        "FolderOpen_16x16_4.png" => include_bytes!("../icons/FolderOpen_16x16_4.png"),
        "InfoBubble_32x32_4.png" => include_bytes!("../icons/InfoBubble_32x32_4.png"),
        "Settings_16x16_4.png" => include_bytes!("../icons/Settings_16x16_4.png"),
        "Delete_16x16_4.png" => include_bytes!("../icons/Delete_16x16_4.png"),
        "Warning_32x32_4.png" => include_bytes!("../icons/Warning_32x32_4.png"),
        "Tick_16x16_4.png" => include_bytes!("../icons/Tick_16x16_4.png"),
        "Computer_16x16_4.png" => include_bytes!("../icons/Computer_16x16_4.png"),
        "Network_32x32_4.png" => include_bytes!("../icons/Network_32x32_4.png"),
        "FileTransfer_32x32_4.png" => include_bytes!("../icons/FileTransfer_32x32_4.png"),
        _ => panic!("unknown icon: {filename}"),
    }
}

/// Apply the Win95 theme to the egui context.
pub fn apply_theme(ctx: &Context) {
    // --- Font setup: W95FA as the primary font ---
    // W95FA is a free re-creation of MS Sans Serif (the Win95 system font).
    // We install it as the Proportional and Button family, keeping the
    // default fonts as fallbacks for glyphs W95FA doesn't cover (emoji,
    // CJK, etc.).
    {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "W95FA".to_owned(),
            egui::FontData::from_static(include_bytes!("../fonts/W95FA.otf")),
        );
        // Put W95FA first in the Proportional family so it takes priority,
        // with Ubuntu-Light as fallback for missing glyphs.
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "W95FA".to_owned());
        // Use W95FA for monospace too (Win95 didn't distinguish, but
        // keep Hack as fallback for code blocks).
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "W95FA".to_owned());
        ctx.set_fonts(fonts);
    }

    // Start from egui's built-in light visuals, then customize.
    let mut visuals = egui::Visuals::light();

    // Zero rounding for that flat Win95 look.
    visuals.window_rounding = 0.0.into();
    visuals.menu_rounding = 0.0.into();
    visuals.widgets.noninteractive.rounding = 0.0.into();
    visuals.widgets.inactive.rounding = 0.0.into();
    visuals.widgets.hovered.rounding = 0.0.into();
    visuals.widgets.active.rounding = 0.0.into();
    visuals.widgets.open.rounding = 0.0.into();

    // Win95 background colors.
    visuals.panel_fill = colors::BUTTON_FACE;
    visuals.window_fill = colors::BUTTON_FACE;
    visuals.extreme_bg_color = colors::WINDOW;
    visuals.faint_bg_color = colors::BUTTON_FACE;

    // Widget colors: Win95 beveled button look.
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = colors::BUTTON_FACE;
    widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_SHADOW);

    widgets.inactive.bg_fill = colors::BUTTON_FACE;
    widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_SHADOW);

    widgets.hovered.bg_fill = colors::BUTTON_LIGHT;
    widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_HIGHLIGHT);

    widgets.active.bg_fill = colors::BUTTON_SHADOW;
    widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_DARK_SHADOW);

    widgets.open.bg_fill = colors::BUTTON_FACE;
    widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_SHADOW);

    // Selection — light blue background so black text stays readable when
    // highlighted in text boxes. Win95 used navy with white text, but egui
    // doesn't swap text color on selection, so we use a lighter blue.
    visuals.selection.bg_fill = colors::TEXT_SELECTION;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);

    // Hyperlinks
    visuals.hyperlink_color = colors::LINK;

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();

    // Scale up everything — the default egui sizes are too tiny on most
    // displays. 1.3 gives comfortably readable text and controls.
    ctx.set_pixels_per_point(1.3);

    // Slightly larger spacing for readability.
    style.spacing.item_spacing = egui::vec2(4.0, 3.0);
    style.spacing.button_padding = egui::vec2(6.0, 4.0);

    // Larger font sizes for readability.
    style.text_styles = {
        let mut styles = std::collections::BTreeMap::new();
        styles.insert(egui::TextStyle::Small, egui::FontId::proportional(14.0));
        styles.insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
        styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(15.0));
        styles.insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
        styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
        styles.insert(
            egui::TextStyle::Name("Title".into()),
            egui::FontId::proportional(22.0),
        );
        styles
    };

    ctx.set_style(style);
}
