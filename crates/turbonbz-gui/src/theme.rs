//! Windows 95-style theming for egui.
//!
//! Loads the Win95 color palette, flat widget styling (zero rounding),
//! and icon textures from the `icons/` directory.

use egui::{Context, TextureHandle};

/// Win95 color palette — exact values from COLOR_* system colors.
pub mod colors {
    use egui::Color32;
    pub const DESKTOP: Color32 = Color32::from_rgb(0, 128, 128); // teal
    pub const BUTTON_FACE: Color32 = Color32::from_rgb(192, 192, 192); // #c0c0c0 COLOR_BTNFACE
    pub const BUTTON_HIGHLIGHT: Color32 = Color32::from_rgb(255, 255, 255); // #ffffff COLOR_BTNHIGHLIGHT
    pub const BUTTON_LIGHT: Color32 = Color32::from_rgb(223, 223, 223); // #dfdfdf COLOR_3DLIGHT
    pub const BUTTON_SHADOW: Color32 = Color32::from_rgb(128, 128, 128); // #808080 COLOR_BTNSHADOW
    pub const BUTTON_DARK_SHADOW: Color32 = Color32::from_rgb(0, 0, 0); // #000000 COLOR_3DDKSHADOW
    pub const WINDOW: Color32 = Color32::from_rgb(255, 255, 255); // COLOR_WINDOW (white)
    pub const WINDOW_FRAME: Color32 = Color32::from_rgb(0, 0, 0); // COLOR_WINDOWFRAME
    pub const WINDOW_TEXT: Color32 = Color32::from_rgb(0, 0, 0);
    pub const TITLE_BAR_ACTIVE: Color32 = Color32::from_rgb(0, 14, 122); // #000e7a
    pub const TITLE_BAR_ACTIVE_GRADIENT: Color32 = Color32::from_rgb(16, 132, 208); // #1084d0
    pub const TITLE_BAR_ACTIVE_TEXT: Color32 = Color32::from_rgb(255, 255, 255);
    pub const TITLE_BAR_INACTIVE: Color32 = Color32::from_rgb(127, 120, 127); // #7f787f
    pub const TITLE_BAR_INACTIVE_TEXT: Color32 = Color32::from_rgb(198, 198, 198); // #c6c6c6
    /// Accent color — emerald green for fills (progress bars, graphs, dots).
    pub const ACCENT: Color32 = Color32::from_rgb(0, 128, 96); // #008060
    pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(0, 168, 128); // #00a880
    pub const SELECTION: Color32 = Color32::from_rgb(0, 0, 128);
    pub const SELECTION_TEXT: Color32 = Color32::from_rgb(255, 255, 255);
    /// Lighter selection for text editing — black text must remain readable.
    pub const TEXT_SELECTION: Color32 = Color32::from_rgb(180, 200, 255);
    pub const HIGHLIGHT: Color32 = Color32::from_rgb(0, 0, 128);
    pub const LINK: Color32 = Color32::from_rgb(0, 14, 122);
    pub const DISABLED_TEXT: Color32 = Color32::from_rgb(128, 128, 128); // #808080
    pub const DISABLED_TEXT_SHADOW: Color32 = Color32::from_rgb(255, 255, 255); // white emboss
}

/// Icon textures loaded at startup.
pub struct Icons {
    // --- React95 icons (legacy, larger 32x32 set) ---
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
    // --- Chicago95 16x16 icons for toolbar ---
    pub tb_play: TextureHandle,
    pub tb_pause: TextureHandle,
    pub tb_open: TextureHandle,
    pub tb_delete: TextureHandle,
    pub tb_search: TextureHandle,
    pub tb_download: TextureHandle,
    pub tb_up: TextureHandle,
    pub tb_stop: TextureHandle,
    pub tb_settings: TextureHandle,
    pub tb_info: TextureHandle,
    pub tb_warning: TextureHandle,
    pub tb_network: TextureHandle,
    pub tb_folder_open: TextureHandle,
    pub tb_computer: TextureHandle,
    pub tb_tick: TextureHandle,
    // --- Chicago95 32x32 icons for tab headers / title bar ---
    pub tab_search: TextureHandle,
    pub tab_download: TextureHandle,
    pub tab_settings: TextureHandle,
    pub tab_info: TextureHandle,
    pub tab_warning: TextureHandle,
    pub tab_network: TextureHandle,
    pub tab_computer: TextureHandle,
    pub tab_folder_open: TextureHandle,
    pub tab_play: TextureHandle,
    pub tab_pause: TextureHandle,
    pub tab_tick: TextureHandle,
    pub tab_stop: TextureHandle,
}

impl Icons {
    /// Load all icons from embedded PNG data.
    pub fn load(ctx: &Context) -> Self {
        Self {
            // React95 legacy icons
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
            // Chicago95 16x16 toolbar icons
            tb_play: load_icon(ctx, "win95_play_16.png"),
            tb_pause: load_icon(ctx, "win95_pause_16.png"),
            tb_open: load_icon(ctx, "win95_open_16.png"),
            tb_delete: load_icon(ctx, "win95_delete_16.png"),
            tb_search: load_icon(ctx, "win95_search_16.png"),
            tb_download: load_icon(ctx, "win95_download_16.png"),
            tb_up: load_icon(ctx, "win95_up_16.png"),
            tb_stop: load_icon(ctx, "win95_stop_16.png"),
            tb_settings: load_icon(ctx, "win95_settings_16.png"),
            tb_info: load_icon(ctx, "win95_info_16.png"),
            tb_warning: load_icon(ctx, "win95_warning_16.png"),
            tb_network: load_icon(ctx, "win95_network_16.png"),
            tb_folder_open: load_icon(ctx, "win95_folder_open_16.png"),
            tb_computer: load_icon(ctx, "win95_computer_16.png"),
            tb_tick: load_icon(ctx, "Tick_16x16_4.png"),
            // Chicago95 32x32 tab icons
            tab_search: load_icon(ctx, "win95_search_32.png"),
            tab_download: load_icon(ctx, "win95_download_32.png"),
            tab_settings: load_icon(ctx, "win95_settings_32.png"),
            tab_info: load_icon(ctx, "win95_info_32.png"),
            tab_warning: load_icon(ctx, "win95_warning_32.png"),
            tab_network: load_icon(ctx, "win95_network_32.png"),
            tab_computer: load_icon(ctx, "win95_computer_32.png"),
            tab_folder_open: load_icon(ctx, "win95_folder_open_32.png"),
            tab_play: load_icon(ctx, "win95_play_32.png"),
            tab_pause: load_icon(ctx, "win95_pause_32.png"),
            tab_tick: load_icon(ctx, "win95_tick_32.png"),
            tab_stop: load_icon(ctx, "win95_stop_32.png"),
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
    match filename {
        // React95 legacy icons
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
        // Chicago95 16x16 toolbar icons
        "win95_play_16.png" => include_bytes!("../icons/win95_play_16.png"),
        "win95_pause_16.png" => include_bytes!("../icons/win95_pause_16.png"),
        "win95_open_16.png" => include_bytes!("../icons/win95_open_16.png"),
        "win95_delete_16.png" => include_bytes!("../icons/win95_delete_16.png"),
        "win95_search_16.png" => include_bytes!("../icons/win95_search_16.png"),
        "win95_download_16.png" => include_bytes!("../icons/win95_download_16.png"),
        "win95_up_16.png" => include_bytes!("../icons/win95_up_16.png"),
        "win95_stop_16.png" => include_bytes!("../icons/win95_stop_16.png"),
        "win95_settings_16.png" => include_bytes!("../icons/win95_settings_16.png"),
        "win95_info_16.png" => include_bytes!("../icons/win95_info_16.png"),
        "win95_warning_16.png" => include_bytes!("../icons/win95_warning_16.png"),
        "win95_network_16.png" => include_bytes!("../icons/win95_network_16.png"),
        "win95_folder_open_16.png" => include_bytes!("../icons/win95_folder_open_16.png"),
        "win95_computer_16.png" => include_bytes!("../icons/win95_computer_16.png"),
        // Chicago95 32x32 tab icons
        "win95_search_32.png" => include_bytes!("../icons/win95_search_32.png"),
        "win95_download_32.png" => include_bytes!("../icons/win95_download_32.png"),
        "win95_settings_32.png" => include_bytes!("../icons/win95_settings_32.png"),
        "win95_info_32.png" => include_bytes!("../icons/win95_info_32.png"),
        "win95_warning_32.png" => include_bytes!("../icons/win95_warning_32.png"),
        "win95_network_32.png" => include_bytes!("../icons/win95_network_32.png"),
        "win95_computer_32.png" => include_bytes!("../icons/win95_computer_32.png"),
        "win95_folder_open_32.png" => include_bytes!("../icons/win95_folder_open_32.png"),
        "win95_play_32.png" => include_bytes!("../icons/win95_play_32.png"),
        "win95_pause_32.png" => include_bytes!("../icons/win95_pause_32.png"),
        "win95_tick_32.png" => include_bytes!("../icons/win95_tick_32.png"),
        "win95_stop_32.png" => include_bytes!("../icons/win95_stop_32.png"),
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
    // NOTE: egui's bg_stroke is only a single 1px stroke. The authentic Win95
    // 2-layer bevel (outer highlight/darkest + inner light/shadow) is painted
    // by our custom win95_widgets. These visuals serve as the fallback for
    // any stock egui widgets we don't override.
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = colors::BUTTON_FACE;
    widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_SHADOW);

    widgets.inactive.bg_fill = colors::BUTTON_FACE;
    widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, colors::WINDOW_TEXT);
    widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, colors::BUTTON_SHADOW);

    // Win95 had NO hover state — buttons looked identical whether hovered or not.
    widgets.hovered = widgets.inactive;

    // Pressed/sunken state: invert the bevel (dark top-left, light bottom-right).
    widgets.active.bg_fill = colors::BUTTON_FACE;
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

    // Zero window margin — Win95 apps have no padding around the main
    // window content. This prevents the grey border on the sides.
    style.spacing.window_margin = egui::Margin::same(0.0);

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
