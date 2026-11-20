//! Source list and metadata inspector.
//!
//! Shows what the import pipeline learned about each file. The panel never
//! touches the filesystem: everything it renders comes from `MediaSource`.

use crate::i18n::tr;
use crate::theme::{Metrics, Palette};
use cf_core::domain::MediaSource;
use cf_core::time::Time;
use egui::{RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui};

/// What the user did to the source list.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SourceAction {
    /// Clicked a row; it becomes the selected source.
    Select(usize),
    /// Clicked the row's delete button.
    Delete(usize),
}

/// Draws the source list. Returns the action the user triggered, if any.
pub fn source_list(
    ui: &mut Ui,
    p: &Palette,
    sources: &[MediaSource],
    selected: Option<usize>,
    pending: &[String],
    icons: Option<&crate::icons::IconSet>,
) -> Option<SourceAction> {
    let mut action = None;

    ScrollArea::vertical()
        .id_salt("clipforge_sources")
        .max_height(220.0)
        .show(ui, |ui| {
            if sources.is_empty() && pending.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(tr("media.no_sources"))
                        .size(12.0)
                        .color(p.text_muted),
                );
                return;
            }

            for (index, source) in sources.iter().enumerate() {
                match source_row(ui, p, icons, source, selected == Some(index)) {
                    Some(RowHit::Select) => action = Some(SourceAction::Select(index)),
                    Some(RowHit::Delete) => action = Some(SourceAction::Delete(index)),
                    None => {}
                }
            }

            for name in pending {
                pending_row(ui, p, name);
            }
        });

    action
}

/// Which part of a row was clicked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RowHit {
    Select,
    Delete,
}

/// Draws one row and reports which part of it was clicked.
///
/// The delete button is drawn *after* the row response so it can claim the
/// click first; otherwise the row would select and the button would never fire.
fn source_row(
    ui: &mut Ui,
    p: &Palette,
    icons: Option<&crate::icons::IconSet>,
    source: &MediaSource,
    selected: bool,
) -> Option<RowHit> {
    let width = ui.available_width();
    let height = 46.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());

    let bg = if selected {
        p.accent_soft
    } else if response.hovered() {
        p.bg_hover
    } else {
        p.bg_sunken
    };
    let border = if selected { p.accent } else { p.divider };

    let painter = ui.painter();
    painter.rect(
        rect,
        Metrics::RADIUS_SM_C,
        bg,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );

    // Accent rail marks the selected row.
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height())),
            Metrics::RADIUS_XS_C,
            p.accent,
        );
    }

    let text_left = rect.left() + 12.0;
    // Leave room for the delete button, and truncate by measured width rather
    // than by character count: a CJK glyph is roughly twice as wide as a Latin
    // one, so a fixed character budget let Chinese names run under the button.
    let name_width = (rect.width() - 52.0).max(24.0);
    let mut job = egui::text::LayoutJob::single_section(
        source.display_name(),
        egui::TextFormat {
            font_id: egui::FontId::proportional(12.5),
            color: if selected { p.accent } else { p.text_primary },
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(name_width);
    painter.galley(
        egui::pos2(text_left, rect.top() + 11.0),
        painter.layout_job(job),
        egui::Color32::WHITE,
    );

    let (w, h) = source.info.display_size();
    let subtitle = format!(
        "{} · {}×{} · {} fps",
        source.duration,
        w,
        h,
        trim_fps(source.info.video.frame_rate.fps())
    );
    painter.text(
        egui::pos2(text_left, rect.top() + 27.0),
        egui::Align2::LEFT_TOP,
        subtitle,
        egui::FontId::proportional(11.0),
        p.text_muted,
    );

    // Delete button, right-aligned. It sits on top of the row rect, so its own
    // interact() call has to come first in pointer priority — egui gives the
    // topmost widget the click, and this is registered after the row.
    let button_size = 22.0;
    let button_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 6.0 - button_size * 0.5, rect.center().y),
        egui::vec2(button_size, button_size),
    );
    let delete = ui.interact(
        button_rect,
        ui.scope_id().with(("media_delete", source.id.0)),
        Sense::click(),
    );
    if delete.hovered() {
        ui.painter()
            .rect_filled(button_rect, Metrics::RADIUS_XS_C, p.bg_hover);
    }
    if let Some(texture) = icons.and_then(|set| set.get(crate::icons::Icon::Trash)) {
        let side = 15.0;
        let image_rect = egui::Rect::from_center_size(button_rect.center(), egui::vec2(side, side));
        let mut mesh = egui::Mesh::with_texture(texture.id());
        mesh.add_rect_with_uv(
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            // Secondary rather than muted: the button has to be findable on
            // a dark row without hovering first.
            if delete.hovered() {
                p.error
            } else {
                p.text_secondary
            },
        );
        ui.painter().add(egui::Shape::mesh(mesh));
    }
    let delete = delete
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tr("media.remove"));

    if delete.clicked() {
        return Some(RowHit::Delete);
    }

    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        return Some(RowHit::Select);
    }

    None
}

fn pending_row(ui: &mut Ui, p: &Palette, name: &str) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 40.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        Metrics::RADIUS_SM_C,
        p.bg_sunken,
        Stroke::new(1.0, p.divider),
        StrokeKind::Inside,
    );
    // Same measured truncation as the real row: character counts misjudge CJK.
    let mut job = egui::text::LayoutJob::single_section(
        format!("{name} · {}", tr("media.loading")),
        egui::TextFormat {
            font_id: egui::FontId::proportional(11.5),
            color: p.text_muted,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - 24.0).max(24.0));
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y * 0.5),
        galley,
        egui::Color32::WHITE,
    );
}

/// Draws the metadata inspector for the selected source.
pub fn metadata(ui: &mut Ui, p: &Palette, source: Option<&MediaSource>) {
    let Some(source) = source else {
        ui.label(
            RichText::new(tr("media.select_hint"))
                .size(12.0)
                .color(p.text_muted),
        );
        return;
    };

    let info = &source.info;

    row(ui, p, &tr("media.duration"), &source.duration.to_string());

    let (w, h) = info.display_size();
    let resolution = if info.video.rotation != 0 {
        format!("{w}×{h}  ({}°)", info.video.rotation)
    } else {
        format!("{w}×{h}")
    };
    row(ui, p, &tr("media.resolution"), &resolution);

    row(
        ui,
        p,
        &tr("media.frame_rate"),
        &format!(
            "{} {}",
            trim_fps(info.video.frame_rate.fps()),
            tr("media.fps")
        ),
    );

    row(ui, p, &tr("media.video_codec"), &video_codec(info));

    let audio = match &info.audio {
        Some(audio) => {
            let channels = channel_label(audio.channels, audio.channel_layout.as_deref());
            // `bit_rate` is per stream; the container figure below covers total.
            let bitrate = audio
                .bit_rate
                .map(|rate| format!(" · {}", fmt_rate(rate)))
                .unwrap_or_default();
            format!(
                "{} · {} · {}{}",
                audio.codec,
                channels,
                fmt_hz(audio.sample_rate),
                bitrate
            )
        }
        None => tr("media.no_audio").to_string(),
    };
    row(ui, p, &tr("media.audio_codec"), &audio);

    row(ui, p, &tr("media.container"), &info.container.format_name);

    if let Some(bit_rate) = info.container.bit_rate {
        row(
            ui,
            p,
            &tr("media.bit_rate"),
            &format!("{}/s", fmt_rate(bit_rate)),
        );
    }

    if let Some(size) = info.container.size {
        row(ui, p, &tr("media.size"), &fmt_bytes(size));
    }

    let keyframes = if source.keyframes.is_degraded() {
        tr("media.degraded_keyframes").to_string()
    } else {
        source.keyframes.len().to_string()
    };
    row(ui, p, &tr("media.keyframes"), &keyframes);

    if source.keyframes.is_degraded() {
        ui.add_space(6.0);
        egui::Frame::new()
            .fill(p.accent_soft)
            .stroke(Stroke::new(1.0, p.warning))
            .corner_radius(Metrics::RADIUS_XS_C)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(tr("media.degraded_keyframes"))
                        .size(11.0)
                        .color(p.warning),
                );
            });
    }
}

fn row(ui: &mut Ui, p: &Palette, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [92.0, 18.0],
            egui::Label::new(RichText::new(label).size(11.5).color(p.text_muted)),
        );
        ui.label(RichText::new(value).size(11.5).color(p.text_secondary));
    });
    ui.add_space(1.0);
}

fn video_codec(info: &cf_core::media::MediaInfo) -> String {
    let mut text = info.video.codec.clone();
    if let Some(profile) = &info.video.profile {
        text.push_str(" · ");
        text.push_str(profile);
    }
    if let Some(pix_fmt) = &info.video.pix_fmt {
        text.push_str(" · ");
        text.push_str(pix_fmt);
    }
    text
}

fn channel_label(channels: u32, layout: Option<&str>) -> String {
    match channels {
        1 => tr("media.channels_mono").to_string(),
        2 => tr("media.channels_stereo").to_string(),
        n => layout
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{n} ch")),
    }
}

/// `29.97` rather than `29.970029`, and `30` rather than `30.000`.
fn trim_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.3}")
    }
}

fn fmt_hz(hz: u32) -> String {
    if hz >= 1000 {
        format!("{:.1} kHz", hz as f64 / 1000.0)
    } else {
        format!("{hz} Hz")
    }
}

fn fmt_rate(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1} Mb/s", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.0} kb/s", value as f64 / 1_000.0)
    } else {
        format!("{value} b/s")
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The "no FFmpeg" card. Shown instead of the import hint when the engine is
/// missing, because nothing else can work.
pub fn engine_missing_card(ui: &mut Ui, p: &Palette, detail: Option<&str>) {
    egui::Frame::new()
        .fill(p.bg_elevated)
        .stroke(Stroke::new(1.0, p.warning))
        .corner_radius(Metrics::RADIUS_MD_C)
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.label(
                RichText::new(tr("media.engine_missing"))
                    .size(14.0)
                    .strong()
                    .color(p.warning),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(tr("media.engine_missing_hint"))
                    .size(12.0)
                    .color(p.text_secondary),
            );
            if let Some(detail) = detail {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(detail)
                        .size(10.5)
                        .monospace()
                        .color(p.text_muted),
                );
            }
        });
}

/// Formats a time for the transport bar; kept next to the panel so both use
/// the same precision.
pub fn timecode(t: Time) -> String {
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_whole_frame_rates() {
        assert_eq!(trim_fps(30.0), "30");
        assert_eq!(trim_fps(60.0), "60");
    }

    #[test]
    fn keeps_ntsc_precision() {
        assert_eq!(trim_fps(29.970_029), "29.970");
        assert_eq!(trim_fps(23.976_024), "23.976");
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn formats_sample_rates() {
        assert_eq!(fmt_hz(44_100), "44.1 kHz");
        assert_eq!(fmt_hz(48_000), "48.0 kHz");
        assert_eq!(fmt_hz(800), "800 Hz");
    }

    #[test]
    fn formats_bit_rates() {
        assert_eq!(fmt_rate(900), "900 b/s");
        assert_eq!(fmt_rate(128_000), "128 kb/s");
        assert_eq!(fmt_rate(3_369_540), "3.4 Mb/s");
    }
}
