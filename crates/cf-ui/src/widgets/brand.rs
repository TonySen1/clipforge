//! Brand lockup shown in the toolbar and about dialog.

use crate::theme::Palette;
use egui::{load::SizedTexture, Image, RichText, TextureHandle, Ui};

pub fn brand(ui: &mut Ui, p: &Palette, mark: Option<&TextureHandle>, title: &str, tagline: &str) {
    ui.horizontal(|ui| {
        if let Some(tex) = mark {
            // 36 px reads clearly at the toolbar height; 28 was too small to
            // make out the film plate and the cut beam.
            let size = egui::vec2(36.0, 36.0);
            ui.add(Image::from_texture(SizedTexture::new(tex.id(), size)));
        }
        ui.add_space(2.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(
                RichText::new(title)
                    .size(17.0)
                    .strong()
                    .color(p.text_primary),
            );
            ui.label(RichText::new(tagline).size(11.5).color(p.text_muted));
        });
    });
}
