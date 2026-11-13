use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};

pub const BG: Color32 = Color32::from_rgb(246, 241, 235);
pub const SURFACE: Color32 = Color32::WHITE;
pub const INK: Color32 = Color32::from_rgb(54, 49, 49);
pub const MUTED: Color32 = Color32::from_rgb(108, 99, 99);
pub const PINK: Color32 = Color32::from_rgb(165, 74, 107);
pub const PINK_SOFT: Color32 = Color32::from_rgb(242, 210, 221);
pub const YELLOW: Color32 = Color32::from_rgb(242, 207, 70);
pub const YELLOW_SOFT: Color32 = Color32::from_rgb(250, 232, 156);
pub const BORDER: Color32 = Color32::from_rgb(70, 64, 64);

/// Shared colors, spacing and typography keep the interface quiet without animated redraws.
pub fn install(context: &egui::Context) {
    fonts(context);
    let mut style = (*context.style()).clone();
    style.visuals = egui::Visuals::light();
    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(INK);
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.window_corner_radius = egui::CornerRadius::same(2);
    visuals.menu_corner_radius = egui::CornerRadius::same(2);
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.faint_bg_color = BG;
    visuals.extreme_bg_color = SURFACE;
    visuals.code_bg_color = BG;
    visuals.selection.bg_fill = PINK_SOFT;
    visuals.selection.stroke = Stroke::new(1.0_f32, PINK);
    visuals.hyperlink_color = PINK;
    visuals.warn_fg_color = Color32::from_rgb(137, 102, 35);
    visuals.error_fg_color = Color32::from_rgb(165, 57, 77);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(2);
        widget.bg_stroke = Stroke::new(1.0_f32, BORDER);
        widget.fg_stroke = Stroke::new(1.0_f32, INK);
    }
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.weak_bg_fill = SURFACE;
    visuals.widgets.hovered.bg_fill = PINK_SOFT;
    visuals.widgets.hovered.weak_bg_fill = PINK_SOFT;
    visuals.widgets.active.bg_fill = PINK_SOFT;
    visuals.widgets.active.weak_bg_fill = PINK_SOFT;
    visuals.widgets.open.bg_fill = PINK_SOFT;
    style.spacing.item_spacing = egui::vec2(10.0, 9.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 28.0;
    style.animation_time = 0.0;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.5));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(12.5));
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(22.0, FontFamily::Name("display".into())),
    );
    context.set_style(style);
}

/// JetBrains Mono ships with the app; a local font supplies Chinese glyphs it lacks.
fn fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "jetbrains".into(),
        FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")).into(),
    );
    fonts.font_data.insert(
        "jetbrains-medium".into(),
        FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf")).into(),
    );
    let mut fallback = Vec::new();
    for path in ["C:/Windows/Fonts/msyh.ttc", "C:/Windows/Fonts/simhei.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), FontData::from_owned(bytes).into());
            fallback.push("cjk".to_owned());
            break;
        }
    }
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let names = fonts.families.entry(family).or_default();
        names.insert(0, "jetbrains".into());
        names.extend(fallback.clone());
    }
    let mut heading = vec!["jetbrains-medium".into()];
    heading.extend(fallback);
    fonts
        .families
        .insert(FontFamily::Name("display".into()), heading);
    context.set_fonts(fonts);
}

pub fn card(fill: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(2)
        .inner_margin(14)
}
