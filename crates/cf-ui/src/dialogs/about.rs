//! About dialog: identity, license and engine information.

use crate::fonts::FontSource;
use crate::i18n::tr;
use crate::icons::{Icon, IconSet};
use crate::theme::{Metrics, Palette};
use crate::widgets::controls::subtle_button;
use egui::{
    load::SizedTexture, Align, Color32, Context, Frame, Id, Image, Margin, Modal, OpenUrl,
    RichText, Stroke, TextureHandle, Ui,
};

/// Where the repository button points. Kept next to the dialog so the link and
/// the About copy cannot drift apart.
pub const REPOSITORY_URL: &str = "https://github.com/TonySen1/clipforge";

pub fn show(
    ctx: &Context,
    p: &Palette,
    logo: Option<&TextureHandle>,
    icons: &IconSet,
    version: &str,
    font: FontSource,
) -> bool {
    let mut close = false;

    let frame = Frame::new()
        .fill(p.bg_elevated)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(Metrics::RADIUS_LG_C)
        .inner_margin(Margin::same(24))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 28,
            spread: 0,
            color: p.shadow,
        });

    let response = Modal::new(Id::unique("clipforge_about"))
        .frame(frame)
        .backdrop_color(backdrop_color(p))
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.vertical_centered(|ui| {
                if let Some(tex) = logo {
                    let size = egui::vec2(96.0, 96.0);
                    ui.add(Image::from_texture(SizedTexture::new(tex.id(), size)));
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new("ClipForge")
                        .size(22.0)
                        .strong()
                        .color(p.text_primary),
                );
                ui.label(
                    RichText::new(tr("about.description"))
                        .size(12.5)
                        .color(p.text_secondary),
                );
            });

            ui.add_space(18.0);
            ui.separator();
            ui.add_space(10.0);

            version_row(ui, p, icons, version);
            row(ui, p, &tr("about.license"), &tr("about.license_value"));
            row(ui, p, &tr("about.ffmpeg"), &tr("about.ffmpeg_value"));
            row(ui, p, &tr("about.font"), font.label());

            ui.add_space(18.0);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if subtle_button(ui, p, tr("about.close")).clicked() {
                    close = true;
                }
                ui.add_space(8.0);
                if repo_button(ui, p, icons, 20.0) {
                    ui.ctx().open_url(OpenUrl::new_tab(REPOSITORY_URL));
                }
            });
        });

    close || response.should_close()
}

/// Version line plus the cat. Both open the repository, so the version number
/// itself is a link rather than a dead label.
fn version_row(ui: &mut Ui, p: &Palette, icons: &IconSet, version: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [110.0, 20.0],
            egui::Label::new(
                RichText::new(tr("about.version"))
                    .size(12.5)
                    .color(p.text_muted),
            ),
        );

        let label = ui.add(
            egui::Label::new(RichText::new(version).size(12.5).color(p.accent))
                .sense(egui::Sense::click()),
        );
        if label.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            label.clone().on_hover_text(REPOSITORY_URL);
        }
        if label.clicked() {
            ui.ctx().open_url(OpenUrl::new_tab(REPOSITORY_URL));
        }

        if repo_button(ui, p, icons, 20.0) {
            ui.ctx().open_url(OpenUrl::new_tab(REPOSITORY_URL));
        }
    });
    ui.add_space(2.0);
}

/// Draws the GitHub mark tinted to the palette and reports a click.
fn repo_button(ui: &mut Ui, p: &Palette, icons: &IconSet, size: f32) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let color = if response.hovered() {
        p.text_primary
    } else {
        p.text_muted
    };

    if let Some(texture) = icons.get(Icon::Github) {
        let mut mesh = egui::Mesh::with_texture(texture.id());
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        mesh.add_rect_with_uv(rect, uv, color);
        ui.painter().add(egui::Shape::mesh(mesh));
    }

    response.clicked()
}

/// Light themes need a much lighter scrim; a black-90% veil over a light
/// surface reads as a broken theme.
pub(crate) fn backdrop_color(p: &Palette) -> Color32 {
    p.scrim_modal
}

fn row(ui: &mut egui::Ui, p: &Palette, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [110.0, 20.0],
            egui::Label::new(RichText::new(label).size(12.5).color(p.text_muted)),
        );
        ui.label(RichText::new(value).size(12.5).color(p.text_secondary));
    });
    ui.add_space(2.0);
}
