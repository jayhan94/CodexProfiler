use eframe::egui::{self, IconData, TextureHandle, TextureOptions, Vec2};
use std::sync::{Arc, OnceLock};

const PNG: &[u8] = include_bytes!("../assets/branding/app.png");

/// Decode the bundled mark once; windows and interface textures share the same source artwork.
pub fn icon() -> Arc<IconData> {
    static ICON: OnceLock<Arc<IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        Arc::new(eframe::icon_data::from_png_bytes(PNG).expect("valid bundled application icon"))
    }))
}

/// Keep one small GPU texture per GUI context instead of decoding the logo on every repaint.
pub fn show(ui: &mut egui::Ui, size: f32) {
    let key = egui::Id::new("codex-profiler-brand-texture");
    let texture = ui
        .ctx()
        .data_mut(|data| data.get_temp::<TextureHandle>(key))
        .unwrap_or_else(|| {
            let icon = icon();
            let pixels = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            let texture =
                ui.ctx()
                    .load_texture("CodexProfiler logo", pixels, TextureOptions::LINEAR);
            ui.ctx()
                .data_mut(|data| data.insert_temp(key, texture.clone()));
            texture
        });
    ui.add(egui::Image::new((texture.id(), Vec2::splat(size))).alt_text("CodexProfiler"));
}

#[cfg(test)]
mod tests {
    use super::*;

    // The native icon must have valid dimensions and RGBA pixels before a GUI is launched.
    #[test]
    fn bundled_icon_is_valid_and_shared() {
        let a = icon();
        let b = icon();
        assert_eq!((a.width, a.height), (256, 256));
        assert_eq!(a.rgba.len(), 256 * 256 * 4);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.rgba[3], 0);
    }

    // Repainting the header keeps the same texture rather than uploading a fresh logo each frame.
    #[test]
    fn logo_texture_is_reused_across_frames() {
        let context = egui::Context::default();
        let key = egui::Id::new("codex-profiler-brand-texture");
        let mut textures = Vec::new();
        for _ in 0..2 {
            let _ = context.run(egui::RawInput::default(), |context| {
                egui::CentralPanel::default().show(context, |ui| show(ui, 39.0));
            });
            textures.push(
                context
                    .data_mut(|data| data.get_temp::<TextureHandle>(key))
                    .unwrap()
                    .id(),
            );
        }
        assert_eq!(textures[0], textures[1]);
    }
}
