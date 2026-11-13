//! Timeline surface: ruler, tracks, clips and the playhead.
//!
//! The widget owns no state. It takes the project's tracks plus a
//! `TimelineView`, draws them, and reports what the user did as a
//! `TimelineAction`. The app layer decides what those actions mean.

use crate::theme::{Metrics, Palette};
use crate::widgets::snap::{self, SnapKind, SnapResult};
use crate::widgets::timeline_view::{format_tick, visible_ticks, TimelineView, TRACK_HEAD_WIDTH};
use cf_core::domain::{ClipRef, TrackKind};
use cf_core::time::Time;
use egui::{pos2, vec2, Align2, FontId, Rect, Response, Sense, Stroke, StrokeKind, Ui};

const RULER_HEIGHT: f32 = 26.0;
const TRACK_GAP: f32 = 6.0;
const CLIP_RADIUS: u8 = 5;

/// Which part of a clip a drag grabbed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DragEdge {
    /// Drag the body: move the clip.
    Body,
    /// Drag the left handle: change the in-point.
    Start,
    /// Drag the right handle: change the out-point.
    End,
}

/// What the user did on the timeline this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineAction {
    /// Move the playhead to this position.
    Seek {
        time: Time,
        precise: bool,
    },
    /// Select (or clear, with `None`) a clip.
    Select(Option<cf_core::ClipId>),
    /// Scroll horizontally by a pixel delta.
    Scroll {
        dx_pixels: f32,
    },
    /// Zoom by a multiplicative factor around a pixel position in the track area.
    Zoom {
        anchor_x: f32,
        factor: f32,
    },
    /// Zoom to fit the whole project.
    Fit,
    /// A drag gesture started on a clip. `edge` distinguishes trimming from
    /// moving. The app records the pre-drag state so the commit is one undo step.
    DragStarted {
        clip: cf_core::ClipId,
        edge: DragEdge,
        grab_offset: Time,
    },
    /// The pointer moved during a drag. `at` is where the clipboard goes:
    /// body drags report the clip's start, trims report the moving edge.
    DragMoved {
        at: Time,
        snap: SnapResult,
    },
    /// The pointer was released; the app commits the edit.
    DragFinished,
    /// The drag was cancelled (Escape).
    DragCancelled,
    /// Split the selected clip at the playhead.
    Split,
    /// Delete the selected clip.
    Delete,
    /// Merge the selected clip with its neighbour.
    Merge,
    /// Undo / redo, mirrored here so the timeline owns its own edit controls.
    Undo,
    Redo,
}

/// Inputs the timeline needs from the app.
pub struct TimelineInput<'a> {
    pub view: &'a TimelineView,
    pub project: &'a cf_core::Project,
    pub playhead: Time,
    pub selected: Option<cf_core::ClipId>,
    pub content_end: Time,
    pub title: &'a str,
    pub empty_hint: &'a str,
    /// In-flight drag, owned by the app so a drag survives frames.
    pub drag: Option<&'a DragState>,
    /// True while a ruler scrub is in progress (owned by the app so it
    /// survives the pointer leaving the widget).
    pub scrubbing: bool,
    /// Which edit buttons are enabled.
    pub edit: crate::widgets::transport::EditAvailability,
    pub icons: Option<&'a crate::icons::IconSet>,
}

/// A drag in progress. The app holds this between frames; nothing is written
/// to the project until the pointer is released.
#[derive(Clone, Debug, PartialEq)]
pub struct DragState {
    pub clip: cf_core::ClipId,
    pub edge: DragEdge,
    /// Distance from the grabbed point to the clip's edge, so the clip does not
    /// jump on the first frame of the drag.
    pub grab_offset: Time,
    /// Previewed position, drawn instead of the committed one.
    pub preview_at: Time,
    pub snap: Option<SnapResult>,
}

/// Draws the timeline and returns everything the user did.
///
/// Also returns the width of the track area it actually drew, because the app
/// layer needs that number for zoom anchoring and scrolling.
pub fn timeline(ui: &mut Ui, p: &Palette, input: &TimelineInput<'_>) -> (Vec<TimelineAction>, f32) {
    let mut actions = Vec::new();

    header(ui, p, input, &mut actions);

    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click_and_drag());

    let painter = ui.painter();
    let body_left = rect.left() + TRACK_HEAD_WIDTH;
    let body_right = rect.right();
    let body_width = (body_right - body_left).max(1.0);

    // A local copy so drawing uses the same width the pointer math assumes.
    let mut view = *input.view;
    view.width = body_width;

    painter.rect(
        rect,
        Metrics::RADIUS_MD_C,
        p.bg_sunken,
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );

    let ruler = Rect::from_min_max(
        pos2(body_left, rect.top()),
        pos2(body_right, rect.top() + RULER_HEIGHT),
    );

    draw_track_heads(p, painter, rect, body_left);
    draw_ruler(p, painter, ruler, &view);
    draw_tracks(p, painter, rect, ruler, &view, input, &mut actions);
    draw_playhead(p, painter, rect, ruler, &view, input.playhead);
    draw_empty_hint(p, painter, rect, body_left, input);
    draw_snap_guide(p, painter, rect, ruler, &view, input.drag);
    draw_seams(p, painter, rect, ruler, &view, input.project);

    handle_pointer(ui, &response, rect, ruler, &view, input, &mut actions);
    handle_wheel(ui, &response, &view, &mut actions);

    (actions, view.width)
}

fn header(ui: &mut Ui, p: &Palette, input: &TimelineInput<'_>, actions: &mut Vec<TimelineAction>) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(input.title)
                .size(13.0)
                .strong()
                .color(p.text_primary),
        );

        // Edit controls sit with the timeline because they act on it; undo and
        // redo join them so all clip operations are in one place.
        ui.add_space(6.0);
        ui.separator();

        if edit_button(
            ui,
            p,
            input.icons,
            crate::icons::Icon::Undo,
            &crate::i18n::tr("toolbar.undo"),
            input.edit.can_undo,
        )
        .clicked()
        {
            actions.push(TimelineAction::Undo);
        }
        if edit_button(
            ui,
            p,
            input.icons,
            crate::icons::Icon::Redo,
            &crate::i18n::tr("toolbar.redo"),
            input.edit.can_redo,
        )
        .clicked()
        {
            actions.push(TimelineAction::Redo);
        }

        ui.separator();

        if edit_button(
            ui,
            p,
            input.icons,
            crate::icons::Icon::Split,
            &crate::i18n::tr("edit.split_hint"),
            input.edit.has_selection,
        )
        .clicked()
        {
            actions.push(TimelineAction::Split);
        }
        if edit_button(
            ui,
            p,
            input.icons,
            crate::icons::Icon::Magnet,
            &crate::i18n::tr("edit.merge_hint"),
            input.edit.has_selection,
        )
        .clicked()
        {
            actions.push(TimelineAction::Merge);
        }
        if edit_button(
            ui,
            p,
            input.icons,
            crate::icons::Icon::Trash,
            &crate::i18n::tr("toolbar.delete"),
            input.edit.has_selection,
        )
        .clicked()
        {
            actions.push(TimelineAction::Delete);
        }

        ui.separator();

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(crate::i18n::tr("timeline.fit"))
                            .size(11.0)
                            .color(p.text_muted),
                    )
                    .frame(false),
                )
                .on_hover_text(crate::i18n::tr("timeline.fit_hint"))
                .clicked()
            {
                actions.push(TimelineAction::Fit);
            }

            if ui
                .add(
                    egui::Button::new(egui::RichText::new("+").size(14.0).color(p.text_muted))
                        .frame(false),
                )
                .on_hover_text(crate::i18n::tr("timeline.zoom_in"))
                .clicked()
            {
                actions.push(TimelineAction::Zoom {
                    anchor_x: input.view.width * 0.5,
                    factor: 1.5,
                });
            }

            if ui
                .add(
                    egui::Button::new(egui::RichText::new("−").size(14.0).color(p.text_muted))
                        .frame(false),
                )
                .on_hover_text(crate::i18n::tr("timeline.zoom_out"))
                .clicked()
            {
                actions.push(TimelineAction::Zoom {
                    anchor_x: input.view.width * 0.5,
                    factor: 1.0 / 1.5,
                });
            }

            ui.label(
                egui::RichText::new(crate::i18n::tr("timeline.zoom"))
                    .size(11.0)
                    .color(p.text_muted),
            );
        });
    });
}

/// Toolbar-style icon button: icon when available, first label char otherwise.
///
/// The tooltip always carries the full label so the control is never
/// icon-only for meaning.
fn edit_button(
    ui: &mut Ui,
    p: &Palette,
    icons: Option<&crate::icons::IconSet>,
    icon: crate::icons::Icon,
    label: &str,
    enabled: bool,
) -> Response {
    let size = vec2(30.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let response = response.on_hover_text(label);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();

    if hovered || pressed {
        let fill = if pressed { p.accent_soft } else { p.bg_hover };
        ui.painter().rect_filled(rect, Metrics::RADIUS_SM_C, fill);
    }

    let color = if !enabled {
        p.text_muted
    } else if pressed {
        p.accent
    } else if hovered {
        p.text_primary
    } else {
        p.text_secondary
    };

    let mut painted = false;
    if let Some(icons) = icons {
        if let Some(texture) = icons.get(icon) {
            let side = 20.0;
            let image_rect = Rect::from_center_size(rect.center(), vec2(side, side));
            let mut mesh = egui::Mesh::with_texture(texture.id());
            mesh.add_rect_with_uv(
                image_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                color,
            );
            ui.painter().add(egui::Shape::mesh(mesh));
            painted = true;
        }
    }

    if !painted {
        let glyph: String = label.chars().take(1).collect();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional(14.0),
            color,
        );
    }

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn draw_track_heads(p: &Palette, painter: &egui::Painter, rect: Rect, body_left: f32) {
    let head = Rect::from_min_max(rect.min, pos2(body_left, rect.bottom()));
    painter.rect_filled(head, Metrics::RADIUS_MD_C, p.bg_panel);
    painter.line_segment(
        [head.right_top(), head.right_bottom()],
        Stroke::new(1.0, p.border),
    );
}

fn draw_ruler(p: &Palette, painter: &egui::Painter, ruler: Rect, view: &TimelineView) {
    painter.rect_filled(ruler, 0.0, p.bg_panel);
    painter.line_segment(
        [ruler.left_bottom(), ruler.right_bottom()],
        Stroke::new(1.0, p.border),
    );

    let (major_step, _) = crate::widgets::timeline_view::tick_steps(view.px_per_sec);
    let clipped = painter.with_clip_rect(ruler);
    for tick in visible_ticks(view, 400) {
        let x = ruler.left() + tick.x;
        let height = if tick.major { 9.0 } else { 5.0 };
        clipped.line_segment(
            [pos2(x, ruler.bottom() - height), pos2(x, ruler.bottom())],
            Stroke::new(1.0, p.border_strong),
        );
        if tick.major {
            clipped.text(
                pos2(x + 4.0, ruler.center().y - 2.0),
                Align2::LEFT_CENTER,
                format_tick(tick.time, major_step),
                FontId::monospace(10.0),
                p.text_muted,
            );
        }
    }
}

/// Vertical layout of one track row.
fn track_row(rect: Rect, ruler: Rect, index: usize, count: usize) -> (Rect, Rect) {
    let top = ruler.bottom() + 2.0;
    let available = (rect.bottom() - 6.0 - top).max(40.0);
    let count = count.max(1) as f32;
    let track_height = ((available - TRACK_GAP * (count - 1.0)) / count).max(28.0);

    let y = top + index as f32 * (track_height + TRACK_GAP);
    let row = Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), track_height));
    let body = Rect::from_min_max(
        pos2(rect.left() + TRACK_HEAD_WIDTH, row.top()),
        pos2(rect.right(), row.bottom()),
    );
    (row, body)
}

fn draw_tracks(
    p: &Palette,
    painter: &egui::Painter,
    rect: Rect,
    ruler: Rect,
    view: &TimelineView,
    input: &TimelineInput<'_>,
    actions: &mut Vec<TimelineAction>,
) {
    let count = input.project.tracks.len();
    let _ = actions;
    for (index, track) in input.project.tracks.iter().enumerate() {
        let (row, body) = track_row(rect, ruler, index, count);
        if row.bottom() > rect.bottom() {
            break;
        }

        let head = Rect::from_min_max(row.min, pos2(rect.left() + TRACK_HEAD_WIDTH, row.bottom()));
        painter.text(
            head.center(),
            Align2::CENTER_CENTER,
            track.label(),
            FontId::monospace(11.0),
            match track.kind {
                TrackKind::Video => p.text_secondary,
                TrackKind::Audio => p.text_muted,
            },
        );

        painter.rect(
            body,
            Metrics::RADIUS_XS_C,
            p.track_bg,
            Stroke::new(1.0, p.divider),
            StrokeKind::Inside,
        );

        let clipped = painter.with_clip_rect(body);
        draw_grid(&clipped, p, body, view);

        for clip in &track.clips {
            // While dragging, draw the clip at its previewed position instead
            // of its committed one.
            let preview = input
                .drag
                .as_ref()
                .filter(|drag| drag.clip == clip.id)
                .map(|drag| drag.preview_at);

            draw_clip(
                &clipped,
                p,
                body,
                view,
                ClipRef { track, clip },
                input.selected == Some(clip.id),
                track.kind,
                preview,
                input.drag.as_ref().map(|drag| drag.edge),
            );
        }
    }
}

fn draw_grid(painter: &egui::Painter, p: &Palette, body: Rect, view: &TimelineView) {
    for tick in visible_ticks(view, 400) {
        let x = body.left() + tick.x;
        painter.line_segment(
            [pos2(x, body.top()), pos2(x, body.bottom())],
            Stroke::new(
                1.0,
                if tick.major {
                    p.divider
                } else {
                    p.divider.gamma_multiply(0.5)
                },
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_clip(
    painter: &egui::Painter,
    p: &Palette,
    body: Rect,
    view: &TimelineView,
    entry: ClipRef<'_>,
    selected: bool,
    kind: TrackKind,
    preview_start: Option<Time>,
    drag_edge: Option<DragEdge>,
) {
    // A body drag moves both edges; an edge drag moves only that side.
    let (timeline_start, timeline_end) = match (preview_start, drag_edge) {
        (Some(at), Some(DragEdge::Body)) => {
            let duration = entry.clip.duration();
            (at, at.saturating_add(duration))
        }
        (Some(at), Some(DragEdge::Start)) => (at, entry.clip.timeline_end()),
        (Some(at), Some(DragEdge::End)) => (entry.clip.timeline_start, at),
        _ => (entry.clip.timeline_start, entry.clip.timeline_end()),
    };
    if timeline_end <= timeline_start {
        return;
    }

    let start_x = body.left() + view.time_to_x(timeline_start);
    let end_x = body.left() + view.time_to_x(timeline_end);
    if end_x <= body.left() || start_x >= body.right() {
        return;
    }

    // Clamp to the track body so a long clip does not paint past the edge.
    let left = start_x.max(body.left());
    let right = end_x.min(body.right());
    if right - left < 1.0 {
        return;
    }

    let clip_rect = Rect::from_min_max(
        pos2(left, body.top() + 3.0),
        pos2(right, body.bottom() - 3.0),
    );

    let fill = match kind {
        TrackKind::Video => p.clip_video,
        TrackKind::Audio => p.clip_audio,
    };
    let fill = if selected {
        fill.gamma_multiply(1.15)
    } else {
        fill
    };
    let stroke = if selected {
        Stroke::new(2.0, p.clip_selected)
    } else {
        Stroke::new(1.0, p.border_strong.gamma_multiply(0.7))
    };

    painter.rect(clip_rect, CLIP_RADIUS, fill, stroke, StrokeKind::Inside);

    // Selection rail, so selection survives at very small clip widths.
    if selected {
        painter.rect_filled(
            Rect::from_min_size(clip_rect.min, vec2(3.0, clip_rect.height())),
            CLIP_RADIUS,
            p.clip_selected,
        );
    }

    // Label, only when there is room for it.
    if clip_rect.width() > 42.0 {
        let text_left = clip_rect.left() + if selected { 9.0 } else { 6.0 };
        let text = painter.with_clip_rect(clip_rect.shrink(4.0));
        text.text(
            pos2(text_left, clip_rect.center().y),
            Align2::LEFT_CENTER,
            ellipsize(
                &entry.clip.name,
                ((clip_rect.width() - 12.0) / 6.5) as usize,
            ),
            FontId::proportional(11.0),
            if kind == TrackKind::Video {
                p.on_clip
            } else {
                p.text_secondary
            },
        );
    }
}

fn draw_playhead(
    p: &Palette,
    painter: &egui::Painter,
    rect: Rect,
    ruler: Rect,
    view: &TimelineView,
    playhead: Time,
) {
    let body_left = rect.left() + TRACK_HEAD_WIDTH;
    let x = body_left + view.time_to_x(playhead);
    if x < body_left || x > rect.right() {
        return;
    }

    painter.line_segment(
        [pos2(x, ruler.bottom()), pos2(x, rect.bottom() - 4.0)],
        Stroke::new(1.5, p.playhead),
    );
    // Handle in the ruler.
    let head = vec![
        pos2(x - 5.0, ruler.bottom() - 11.0),
        pos2(x + 5.0, ruler.bottom() - 11.0),
        pos2(x, ruler.bottom()),
    ];
    painter.add(egui::Shape::convex_polygon(head, p.playhead, Stroke::NONE));
}

fn draw_empty_hint(
    p: &Palette,
    painter: &egui::Painter,
    rect: Rect,
    body_left: f32,
    input: &TimelineInput<'_>,
) {
    let has_clips = input
        .project
        .tracks
        .iter()
        .any(|track| !track.clips.is_empty());
    if has_clips {
        return;
    }

    let center = pos2(
        (body_left + rect.right()) * 0.5,
        (rect.top() + RULER_HEIGHT + rect.bottom()) * 0.5,
    );
    painter.text(
        center,
        Align2::CENTER_CENTER,
        input.empty_hint,
        FontId::proportional(12.5),
        p.text_muted,
    );
}

fn handle_pointer(
    ui: &Ui,
    response: &Response,
    rect: Rect,
    ruler: Rect,
    view: &TimelineView,
    input: &TimelineInput<'_>,
    actions: &mut Vec<TimelineAction>,
) {
    // Escape cancels an in-flight drag.
    if input.drag.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(TimelineAction::DragCancelled);
        return;
    }
    let body_left = rect.left() + TRACK_HEAD_WIDTH;

    // Clicking or dragging in the ruler scrubs; dragging keeps seeking with
    // fast (keyframe) seeks and the release does the precise seek.
    let ruler_rect = Rect::from_min_max(
        pos2(body_left, rect.top()),
        pos2(rect.right(), ruler.bottom()),
    );

    // Scrubbing follows the raw pointer, not `response.dragged()`: egui stops
    // reporting a drag as soon as the pointer leaves the widget rect, which
    // made the playhead stick whenever the user dragged up or down out of the
    // ruler. `scrubbing` latches on press-inside and releases anywhere.
    let (pressed_pos, released, pointer_down, live_pos) = ui.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.any_released(),
            i.pointer.primary_down(),
            i.pointer.latest_pos(),
        )
    });

    let started_in_ruler =
        input.scrubbing || pressed_pos.is_some_and(|pos| ruler_rect.contains(pos));

    if started_in_ruler && pointer_down {
        if let Some(pos) = live_pos {
            // Clamp vertically away: only x matters while scrubbing.
            let x = (pos.x - body_left).max(0.0);
            let time = scrub_time(view, input, x);
            actions.push(TimelineAction::Seek {
                time,
                precise: false,
            });
        }
    } else if input.scrubbing && released {
        if let Some(pos) = live_pos {
            let x = (pos.x - body_left).max(0.0);
            let time = scrub_time(view, input, x);
            actions.push(TimelineAction::Seek {
                time,
                precise: true,
            });
        }
    }

    let pointer = live_pos;
    let in_ruler = pointer.is_some_and(|pos| ruler_rect.contains(pos));

    // Double-click in the track area fits the project.
    if response.double_clicked() && !in_ruler {
        actions.push(TimelineAction::Fit);
        return;
    }

    // --- clip drag lifecycle -------------------------------------------
    // Start a drag on the clip under the press point.
    //
    // Uses the raw press event rather than `response.drag_started()` for the
    // same reason as scrubbing: once the pointer leaves the widget rect egui
    // stops calling it a drag, and the clip would freeze mid-gesture.
    let press_in_tracks = input.drag.is_none()
        && pressed_pos
            .is_some_and(|pos| pos.x >= body_left && rect.contains(pos) && pos.y > ruler.bottom());

    if press_in_tracks {
        if let Some(pos) = pressed_pos {
            let x = pos.x - body_left;
            let time = view.x_to_time(x.max(0.0));
            if let Some(hit) = hit_test(input, pos, time, body_left) {
                actions.push(TimelineAction::Select(Some(hit.clip)));
                actions.push(TimelineAction::DragStarted {
                    clip: hit.clip,
                    edge: hit.edge,
                    grab_offset: hit.grab_offset,
                });
            }
        }
    }

    // Report movement while a drag is active.
    if let Some(drag) = input.drag {
        if pointer_down || released {
            if let Some(pos) = pointer {
                let x = (pos.x - body_left).max(0.0);
                let pointer_time = view.x_to_time(x).max(Time::ZERO);

                // `raw` is the value the drag edits: for a body move that is
                // the clip's start, for a trim it is the edge itself. `snap`
                // returns a corrected version of the same value, so the app can
                // commit it without further conversion.
                let raw = match drag.edge {
                    DragEdge::Body => pointer_time.saturating_sub(drag.grab_offset),
                    DragEdge::Start | DragEdge::End => pointer_time,
                }
                .max(Time::ZERO);

                let edges = edges_for(input.project, drag, raw);
                let candidates = snap::candidates(input.project, Some(drag.clip), input.playhead);
                let mut result = snap::snap(raw, &edges, &candidates, view, snap::SNAP_PX);

                if !result.is_snapped() {
                    if let Some(rate) = clip_frame_rate(input.project, drag.clip) {
                        let frame = snap::snap_to_frame(result.time, rate, view, snap::SNAP_PX);
                        if frame.is_snapped() {
                            result = frame;
                        }
                    }
                }

                actions.push(TimelineAction::DragMoved {
                    at: result.time.max(Time::ZERO),
                    snap: result,
                });
            }
        }

        // The release is tracked globally, so a drag that ends outside the
        // timeline still commits instead of leaving the clip in limbo.
        if released {
            actions.push(TimelineAction::DragFinished);
        }
    }

    // A plain click (no drag) selects, or clears the selection.
    if response.clicked() && !in_ruler && input.drag.is_none() {
        if let Some(pos) = pointer {
            let x = pos.x - body_left;
            let time = view.x_to_time(x.max(0.0));
            actions.push(TimelineAction::Select(
                hit_test(input, pos, time, body_left).map(|hit| hit.clip),
            ));
        }
    }

    let _ = ui;
}

/// Result of hit testing a pointer against the clips.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ClipHit {
    pub clip: cf_core::ClipId,
    pub edge: DragEdge,
    /// Distance from the grabbed point to the clip's start edge.
    pub grab_offset: Time,
}

/// Pixels either side of a clip edge that grab the trim handle.
pub const TRIM_HANDLE_PX: f32 = 6.0;

/// Finds the clip edge under `pos`, using the same row layout as drawing.
///
/// Handles win over the body, so a clip only 12 px wide can still be trimmed
/// from either side.
fn hit_test(
    input: &TimelineInput<'_>,
    pos: egui::Pos2,
    time: Time,
    body_left: f32,
) -> Option<ClipHit> {
    let view = input.view;

    for track in &input.project.tracks {
        let Some(clip) = track.clip_at(time) else {
            continue;
        };

        let start_x = view.time_to_x(clip.timeline_start);
        let end_x = view.time_to_x(clip.timeline_end());
        let local = pos.x - body_left;

        let near_start = (local - start_x).abs() <= TRIM_HANDLE_PX;
        let near_end = (local - end_x).abs() <= TRIM_HANDLE_PX;

        // A very narrow clip cannot offer two handles; prefer the start.
        let edge = if near_start && !near_end {
            DragEdge::Start
        } else if near_end && !near_start {
            DragEdge::End
        } else if near_start && near_end {
            DragEdge::Start
        } else {
            DragEdge::Body
        };

        let grab_offset = match edge {
            DragEdge::End => clip.duration(),
            _ => time.saturating_sub(clip.timeline_start),
        };

        return Some(ClipHit {
            clip: clip.id,
            edge,
            grab_offset,
        });
    }

    None
}

/// Edges of the dragged clip at the candidate position.
///
/// A body drag exposes *both* ends so the clip sticks to a neighbour whichever
/// side reaches it first. Trimming only moves one edge, so only that edge can
/// snap. Snapping the grab point instead (as a body drag used to) meant a clip
/// grabbed in the middle never lined up with anything.
/// Maps a ruler x to a playhead time, snapping to clip edges and the origin.
///
/// Without the snap a scrub can never land exactly on 0 or on a clip edge at
/// normal zoom levels, which makes "put the playhead at the start" impossible
/// by mouse alone.
fn scrub_time(view: &TimelineView, input: &TimelineInput<'_>, x: f32) -> Time {
    let raw = view.x_to_time(x).min(input.content_end);
    // Exclude the playhead itself: it is the thing being dragged, so snapping
    // to its own position would pin the scrub in place.
    let candidates: Vec<_> = snap::candidates(input.project, None, input.playhead)
        .into_iter()
        .filter(|(_, kind)| *kind != SnapKind::Playhead)
        .collect();
    let snapped = snap::snap(raw, &[raw], &candidates, view, snap::SNAP_PX);
    snapped.time.min(input.content_end).max(Time::ZERO)
}

fn edges_for(project: &cf_core::Project, drag: &DragState, at: Time) -> Vec<Time> {
    let Some(entry) = project.clip(drag.clip) else {
        return vec![at];
    };
    match drag.edge {
        DragEdge::Body => {
            let end = at.saturating_add(entry.clip.duration());
            vec![at, end]
        }
        DragEdge::Start | DragEdge::End => vec![at],
    }
}

fn clip_frame_rate(
    project: &cf_core::Project,
    clip: cf_core::ClipId,
) -> Option<cf_core::FrameRate> {
    let entry = project.clip(clip)?;
    project
        .source(entry.clip.source_id)
        .map(|source| source.info.video.frame_rate)
}

/// Draws the snap guide line while dragging.
fn draw_snap_guide(
    p: &Palette,
    painter: &egui::Painter,
    rect: Rect,
    ruler: Rect,
    view: &TimelineView,
    drag: Option<&DragState>,
) {
    let Some(drag) = drag else {
        return;
    };
    let Some(snap) = drag.snap else {
        return;
    };
    if !snap.is_snapped() {
        return;
    }

    let x = rect.left() + TRACK_HEAD_WIDTH + view.time_to_x(snap.time);
    painter.line_segment(
        [pos2(x, ruler.bottom()), pos2(x, rect.bottom() - 4.0)],
        Stroke::new(1.0, p.accent.gamma_multiply(0.7)),
    );
    painter.circle_filled(pos2(x, ruler.bottom()), 3.0, p.accent);
}

/// Marks seams between grouped clips so "merged but not really merged" reads
/// as intentional rather than as a rendering glitch.
fn draw_seams(
    p: &Palette,
    painter: &egui::Painter,
    rect: Rect,
    ruler: Rect,
    view: &TimelineView,
    project: &cf_core::Project,
) {
    let count = project.tracks.len();
    for (index, track) in project.tracks.iter().enumerate() {
        let (row, body) = track_row(rect, ruler, index, count);
        if row.bottom() > rect.bottom() {
            break;
        }
        let clipped = painter.with_clip_rect(body);

        for window in track.clips.windows(2) {
            let (left, right) = (&window[0], &window[1]);
            let grouped = match (left.group, right.group) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !grouped || left.timeline_end() != right.timeline_start {
                continue;
            }

            let x = body.left() + view.time_to_x(left.timeline_end());
            clipped.line_segment(
                [pos2(x, body.top() + 3.0), pos2(x, body.bottom() - 3.0)],
                Stroke::new(1.5, p.accent.gamma_multiply(0.85)),
            );
        }
    }
}

fn handle_wheel(
    ui: &Ui,
    response: &Response,
    view: &TimelineView,
    actions: &mut Vec<TimelineAction>,
) {
    if !response.hovered() {
        return;
    }

    let (scroll, zoom, shift) =
        ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers.shift));

    let anchor_x = response
        .hover_pos()
        .map(|pos| (pos.x - response.rect.left() - TRACK_HEAD_WIDTH).max(0.0))
        .unwrap_or(view.width * 0.5);

    // Vertical wheel = zoom, anchored at the pointer. That is what people
    // reach for on a timeline, and it needs no modifier.
    let mut consumed_vertical = false;
    if scroll.y.abs() > f32::EPSILON && !shift {
        // One notch (~50 px) maps to a comfortable ~1.18x step.
        let factor = (scroll.y * 0.004).exp();
        actions.push(TimelineAction::Zoom { anchor_x, factor });
        consumed_vertical = true;
    }

    // Pinch gestures report through `zoom_delta`.
    if (zoom - 1.0).abs() > 0.01 {
        actions.push(TimelineAction::Zoom {
            anchor_x,
            factor: zoom,
        });
    }

    // Horizontal scrolling: trackpad horizontal delta, plus Shift+wheel.
    let dx = if scroll.x.abs() > f32::EPSILON {
        scroll.x
    } else if shift && !consumed_vertical {
        scroll.y
    } else {
        0.0
    };

    if dx.abs() > f32::EPSILON {
        let _ = view;
        actions.push(TimelineAction::Scroll { dx_pixels: dx });
    }
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if max_chars < 4 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars - 1).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsize_respects_the_budget() {
        assert_eq!(ellipsize("short", 10), "short");
        let long = ellipsize("a rather long clip name", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn ellipsize_hides_text_when_there_is_no_room() {
        assert_eq!(ellipsize("anything", 2), "");
    }
}
