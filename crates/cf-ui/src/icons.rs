//! UI icon set.
//!
//! Icons are author-time SVGs (`assets/icons/ui/*.svg`) rasterised by
//! `cargo xtask ui-icons` into white-on-transparent PNGs. Rendering them white
//! and tinting at draw time means one asset serves both themes.
//!
//! Everything is `include_bytes!`, so the binaries ship inside the executable
//! and a packaged build needs no asset directory.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};

/// One icon in the set.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Split,
    Razor,
    Scissors,
    Trash,
    Undo,
    Redo,
    Select,
    Crop,
    Magnet,
    Speed,
    Volume,
    VolumeMuted,
    Github,
}

impl Icon {
    pub const ALL: [Self; 13] = [
        Self::Split,
        Self::Razor,
        Self::Scissors,
        Self::Trash,
        Self::Undo,
        Self::Redo,
        Self::Select,
        Self::Crop,
        Self::Magnet,
        Self::Speed,
        Self::Volume,
        Self::VolumeMuted,
        Self::Github,
    ];

    fn stem(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Razor => "razor",
            Self::Scissors => "scissors",
            Self::Trash => "trash",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Select => "select",
            Self::Crop => "crop",
            Self::Magnet => "magnet",
            Self::Speed => "speed",
            Self::Volume => "volume",
            Self::VolumeMuted => "volume-muted",
            Self::Github => "github",
        }
    }

    /// 32 px PNG source. Icons are drawn at roughly their native size, so
    /// rasterising larger only added downscale blur; 32 keeps strokes crisp at
    /// the 20-24 px slots the UI actually uses.
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Split => include_bytes!("../../../assets/icons/ui-png/split-32.png"),
            Self::Razor => include_bytes!("../../../assets/icons/ui-png/razor-32.png"),
            Self::Scissors => {
                include_bytes!("../../../assets/icons/ui-png/scissors-32.png")
            }
            Self::Trash => include_bytes!("../../../assets/icons/ui-png/trash-32.png"),
            Self::Undo => include_bytes!("../../../assets/icons/ui-png/undo-32.png"),
            Self::Redo => include_bytes!("../../../assets/icons/ui-png/redo-32.png"),
            Self::Select => include_bytes!("../../../assets/icons/ui-png/select-32.png"),
            Self::Crop => include_bytes!("../../../assets/icons/ui-png/crop-32.png"),
            Self::Magnet => include_bytes!("../../../assets/icons/ui-png/magnet-32.png"),
            Self::Speed => include_bytes!("../../../assets/icons/ui-png/speed-32.png"),
            Self::Volume => include_bytes!("../../../assets/icons/ui-png/volume-32.png"),
            Self::VolumeMuted => {
                include_bytes!("../../../assets/icons/ui-png/volume-muted-32.png")
            }
            // Brand mark: drawn at 20 px, so rasterise the 64 px source. The
            // 32 px one visibly stair-steps at this size.
            Self::Github => include_bytes!("../../../assets/icons/ui-png/github-64.png"),
        }
    }
}

/// Handles for every icon, created once at startup.
pub struct IconSet {
    textures: Vec<(Icon, TextureHandle)>,
}

impl IconSet {
    pub fn load(ctx: &Context) -> Self {
        let textures = Icon::ALL
            .into_iter()
            .filter_map(|icon| load(ctx, icon).map(|handle| (icon, handle)))
            .collect();
        Self { textures }
    }

    pub fn get(&self, icon: Icon) -> Option<&TextureHandle> {
        self.textures
            .iter()
            .find(|(candidate, _)| *candidate == icon)
            .map(|(_, handle)| handle)
    }
}

fn load(ctx: &Context, icon: Icon) -> Option<TextureHandle> {
    let image = image::load_from_memory(icon.bytes()).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(
        format!("clipforge_icon_{}", icon.stem()),
        color,
        TextureOptions::LINEAR,
    ))
}

/// Draws an icon tinted to `color`, centered in a `size` × `size` box.
///
/// Returns `false` when the texture failed to load, so callers can fall back to
/// a text label instead of leaving a hole.
pub fn draw(ui: &mut egui::Ui, icons: &IconSet, icon: Icon, size: f32, color: Color32) -> bool {
    let Some(texture) = icons.get(icon) else {
        return false;
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let mut mesh = egui::Mesh::with_texture(texture.id());
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    mesh.add_rect_with_uv(rect, uv, color);
    ui.painter().add(egui::Shape::mesh(mesh));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_embedded_bytes() {
        for icon in Icon::ALL {
            assert!(!icon.bytes().is_empty(), "{:?} has no data", icon);
        }
    }

    #[test]
    fn stems_are_unique() {
        let mut stems: Vec<&str> = Icon::ALL.iter().map(|icon| icon.stem()).collect();
        stems.sort_unstable();
        let count = stems.len();
        stems.dedup();
        assert_eq!(stems.len(), count);
    }
}
