//! Application shell: window layout, state and action handling.
//!
//! P1 adds the import pipeline (FFmpeg discovery, probing, keyframe indexing)
//! and the media/metadata panels. Preview playback arrives in P2.

use crate::export::{ExportEvent, ExportJob, ExportWorker, JobSource};
use crate::import::{EngineInfo, ImportEvent, ImportWorker};
use crate::preview::{PreviewRequest, PreviewWorker};
use cf_core::media::export::{
    check_compat, only_segments, plan_precise, timeline_pieces, CompatReport, ExportMode,
    ExportRange, PreciseSettings,
};
use cf_core::{CoreError, History, MergeOutcome, Preferences, ThemeMode, TrimEdge};
use cf_ui::dialogs::export::{self, ExportAction, ExportDialogState, RunningState};
use cf_ui::dialogs::{about, settings};
use cf_ui::fonts::FontSource;
use cf_ui::i18n::{self, tr};
use cf_ui::theme::{self, Metrics, Palette};
use cf_ui::widgets::timeline::{DragEdge, DragState};
use cf_ui::widgets::transport::EditAvailability;
use cf_ui::widgets::{self, TransportAction};
use eframe::CreationContext;
use egui::{Align, CentralPanel, Context, Frame, Layout, Margin, Panel, Stroke, TextureHandle, Ui};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Status-bar content. Stores keys rather than translated text so a locale
/// switch re-renders the message in the new language.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StatusMessage {
    Ready,
    SettingsSaved,
    Importing(String),
    Imported(String),
    ImportFailed(String),
    EngineMissing,
    EditFailed(String),
    Undone(&'static str),
    Redone(&'static str),
    Exported(String),
    ExportFailed(String),
    ExportCancelled,
}

impl StatusMessage {
    fn text(&self) -> String {
        match self {
            Self::Ready => tr("status.ready"),
            Self::SettingsSaved => tr("settings.saved"),
            Self::Importing(name) => format!("{}: {name}", tr("status.importing")),
            Self::Imported(name) => format!("{}: {name}", tr("status.imported")),
            Self::ImportFailed(detail) => format!("{}: {detail}", tr("status.import_failed")),
            Self::EngineMissing => tr("status.engine_missing").to_string(),
            Self::EditFailed(detail) => format!("{}: {detail}", tr("status.edit_failed")),
            Self::Undone(label) => format!("{}: {}", tr("status.undone"), tr(label)),
            Self::Redone(label) => format!("{}: {}", tr("status.redone"), tr(label)),
            Self::Exported(path) => format!("{}: {path}", tr("status.exported")),
            Self::ExportFailed(detail) => {
                format!("{}: {detail}", tr("status.export_failed"))
            }
            Self::ExportCancelled => tr("status.export_cancelled").to_string(),
        }
    }

    /// Failures stay on screen until replaced; confirmations fade.
    fn sticky(&self) -> bool {
        matches!(
            self,
            Self::ImportFailed(_)
                | Self::EngineMissing
                | Self::EditFailed(_)
                | Self::ExportFailed(_)
        )
    }

    fn is_error(&self) -> bool {
        matches!(
            self,
            Self::ImportFailed(_)
                | Self::EngineMissing
                | Self::EditFailed(_)
                | Self::ExportFailed(_)
        )
    }
}

pub struct ClipForgeApp {
    prefs: Preferences,
    palette: Palette,
    font: FontSource,

    icons: cf_ui::icons::IconSet,

    logo_light: Option<TextureHandle>,
    logo_dark: Option<TextureHandle>,
    /// Transparent-background mark for the toolbar; works on both themes.
    mark: Option<TextureHandle>,

    settings_open: bool,
    about_open: bool,

    status: StatusMessage,
    status_until: Option<Instant>,

    muted: bool,

    system_dark: bool,
    last_theme_check: Instant,

    // --- P1: media ------------------------------------------------------
    importer: ImportWorker,
    engine: Option<EngineInfo>,
    engine_error: Option<Arc<CoreError>>,
    /// The editable document: sources, tracks, clips and the timeline playhead.
    project: cf_core::Project,
    /// Clip last clicked, drawn with a selection highlight.
    selected_clip: Option<cf_core::ClipId>,
    /// Which source the preview worker is currently pointed at.
    preview_source: Option<cf_core::SourceId>,
    /// Horizontal scroll and zoom of the timeline.
    timeline_view: cf_ui::widgets::timeline_view::TimelineView,
    /// Names of files currently being probed, shown as a loading hint.
    pending_imports: Vec<String>,

    // --- P2: preview ----------------------------------------------------
    preview: PreviewWorker,
    /// Current frame texture. Recreated when the frame size changes.
    frame_texture: Option<TextureHandle>,
    frame_aspect: f32,
    /// Bumped on every request so stale frames can be dropped.
    preview_generation: u64,
    /// Generation the UI is currently waiting for; `None` means idle.
    awaiting: Option<u64>,
    /// True while the transport is in playback (not scrubbing).
    playing_preview: bool,
    /// Timeline position playback started from, so incoming source-time
    /// frames can be mapped back onto the timeline.
    playback_timeline_origin: cf_core::Time,
    playback_source_origin: cf_core::Time,
    /// Auto-scroll the timeline so the playhead stays visible.
    follow_playhead: bool,
    /// Set when the timeline should refit on the next draw, once the real
    /// viewport width is known.
    fit_timeline_pending: bool,

    // --- P4: editing ----------------------------------------------------
    history: History,
    /// In-flight clip drag; nothing is written until the pointer is released.
    drag: Option<cf_ui::widgets::timeline::DragState>,
    /// Project snapshot taken when the drag started, for a single undo step.
    drag_before: Option<cf_core::Project>,
    /// True while the user is dragging the playhead on the ruler.
    scrubbing: bool,

    // --- audio ----------------------------------------------------------
    audio: crate::audio::AudioEngine,
    /// 0.0..=1.0, persisted in preferences.
    volume: f32,
    /// Timeline span the audio buffer covers, so a seek or a clip boundary can
    /// decide whether a re-decode is needed.
    audio_span: Option<(cf_core::Time, cf_core::Time)>,
    /// Set when playback was requested but the first video frame has not
    /// arrived yet. Audio starts only when that frame lands, otherwise sound
    /// runs 100-200 ms ahead of the picture after every seek.
    audio_pending_start: Option<cf_core::Time>,

    // --- update check ---------------------------------------------------
    /// Receiver from the one-shot GitHub release check.
    update_rx: std::sync::mpsc::Receiver<crate::update::UpdateEvent>,
    /// Newer tag found, shown in the status bar until clicked or dismissed.
    update_available: Option<String>,

    // --- P5: export -----------------------------------------------------
    export_open: bool,
    export_output: Option<PathBuf>,
    /// Mirror of `export_output` as text, so the dialog can edit it freely.
    export_output_text: String,
    /// Plan for the currently selected clip, refreshed whenever it changes.
    export_plan: Option<cf_core::media::export::LosslessPlan>,
    export_blocker: Option<cf_core::media::export::ExportBlocker>,
    export_running: Option<ExportWorker>,
    export_progress: RunningState,
    export_error: Option<String>,
    export_log: Option<String>,
    export_mode: ExportMode,
    export_range: ExportRange,
    /// Set when lossless is blocked by incompatible segments, to offer the
    /// "switch to precise" escape hatch in the dialog.
    export_compat: Option<CompatReport>,
    /// Plan for precise mode; recomputed with the mode.
    export_precise_plan: Option<cf_core::media::export::PrecisePlan>,
}

impl ClipForgeApp {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        let prefs = Preferences::load();
        i18n::set_locale(prefs.locale);

        let font = cf_ui::fonts::install(&cc.egui_ctx);
        let palette = theme::apply(&cc.egui_ctx, prefs.theme);

        let (logo_light, logo_dark, mark) = load_brand_textures(&cc.egui_ctx);

        let app = Self {
            prefs,
            palette,
            font,
            icons: cf_ui::icons::IconSet::load(&cc.egui_ctx),

            logo_light,
            logo_dark,
            mark,
            update_rx: crate::update::check_async(cc.egui_ctx.clone()),
            update_available: None,
            settings_open: false,
            about_open: false,
            status: StatusMessage::Ready,
            status_until: None,
            muted: true,
            system_dark: theme::resolve_dark(ThemeMode::System),
            last_theme_check: Instant::now(),

            importer: ImportWorker::spawn(cc.egui_ctx.clone()),
            engine: None,
            engine_error: None,
            project: cf_core::Project::empty(),
            selected_clip: None,
            preview_source: None,
            timeline_view: cf_ui::widgets::timeline_view::TimelineView::default(),
            pending_imports: Vec::new(),

            preview: PreviewWorker::spawn(cc.egui_ctx.clone()),
            frame_texture: None,
            frame_aspect: 16.0 / 9.0,
            preview_generation: 0,
            awaiting: None,
            playing_preview: false,
            playback_timeline_origin: cf_core::Time::ZERO,
            playback_source_origin: cf_core::Time::ZERO,
            follow_playhead: true,
            fit_timeline_pending: false,

            history: History::default(),
            drag: None,
            drag_before: None,
            scrubbing: false,

            audio: crate::audio::AudioEngine::new(),
            volume: 0.8,
            audio_span: None,
            audio_pending_start: None,

            export_open: false,
            export_output: None,
            export_output_text: String::new(),
            export_plan: None,
            export_blocker: None,
            export_running: None,
            export_progress: RunningState::default(),
            export_error: None,
            export_log: None,
            export_mode: ExportMode::default(),
            export_range: ExportRange::default(),
            export_compat: None,
            export_precise_plan: None,
        };

        app.importer.discover(app.prefs.ffmpeg_path.clone());
        app
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let p = self.palette;
        ui.horizontal(|ui| {
            widgets::brand::brand(
                ui,
                &p,
                self.mark.as_ref(),
                &tr("app.name"),
                &tr("app.tagline"),
            );

            ui.add_space(18.0);
            if widgets::controls::primary_button(ui, &p, tr("toolbar.import")).clicked() {
                self.pick_file();
            }

            let can_export = self.selected_clip.is_some() && self.export_running.is_none();
            ui.add_enabled_ui(can_export, |ui| {
                if widgets::controls::subtle_button(ui, &p, tr("toolbar.export")).clicked() {
                    self.open_export();
                }
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::controls::ghost_button(ui, &p, tr("toolbar.about")).clicked() {
                    self.about_open = true;
                }
                if widgets::controls::ghost_button(ui, &p, tr("toolbar.settings")).clicked() {
                    self.settings_open = true;
                }

                // Preview quality belongs with the other view settings on the
                // right side, not wedged against the import/export actions.
                ui.add_space(14.0);
                for quality in cf_core::PreviewQuality::ALL.into_iter().rev() {
                    let selected = self.prefs.preview_quality == quality;
                    if quality_chip(ui, &p, selected, quality.label()).clicked() && !selected {
                        self.prefs.preview_quality = quality;
                        self.persist_prefs();
                        self.change_preview_quality();
                    }
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(tr("quality.label"))
                        .size(11.0)
                        .color(p.text_muted),
                );
                ui.add_space(8.0);
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = self.palette;
        if let Some(until) = self.status_until {
            if Instant::now() >= until && !self.status.sticky() {
                self.status = StatusMessage::Ready;
                self.status_until = None;
            }
        }

        ui.horizontal(|ui| {
            let dot = if self.status.is_error() {
                p.error
            } else if self.status_until.is_some() {
                p.accent
            } else {
                p.success
            };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.0, dot);
            ui.label(
                egui::RichText::new(self.status.text())
                    .size(11.5)
                    .color(p.text_secondary),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(text) = self.audio_status_text() {
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new(text).size(11.0).color(p.warning));
                }
                ui.add_space(10.0);
                let engine_color = match &self.engine {
                    Some(_) => p.text_muted,
                    None => p.warning,
                };
                ui.label(
                    egui::RichText::new(self.engine_status_text())
                        .size(11.0)
                        .color(engine_color),
                );

                // Update notice sits left of the engine label and only appears
                // when a newer tag exists. Deliberately quiet: a check that
                // finds nothing leaves no trace at all.
                if let Some(tag) = self.update_available.clone() {
                    ui.add_space(10.0);
                    let label = tr("update.available").replace("{}", &tag);
                    let response = ui
                        .add(
                            egui::Label::new(egui::RichText::new(label).size(11.0).color(p.accent))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(tr("update.open_release"));
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(crate::update::RELEASES_PAGE));
                    }
                }
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let transport_h = Metrics::TRANSPORT_HEIGHT;
        let preview_h = (ui.available_height() - transport_h - 2.0).max(120.0);

        if let Some(error) = &self.engine_error {
            let detail = error.detail();
            ui.allocate_ui(egui::vec2(ui.available_width(), preview_h), |ui| {
                widgets::media_panel::engine_missing_card(ui, &p, Some(&detail));
            });
            return;
        }

        let has_clips = self.project.duration() > cf_core::Time::ZERO;
        let duration = self.duration();
        let timecode = self.project.playhead.to_string();

        ui.allocate_ui(egui::vec2(ui.available_width(), preview_h), |ui| {
            match (&self.frame_texture, has_clips) {
                (Some(texture), _) => widgets::preview::preview_frame(
                    ui,
                    &p,
                    texture,
                    self.frame_aspect,
                    Some(&timecode),
                ),
                _ if has_clips && self.awaiting.is_some() => widgets::preview::preview_empty(
                    ui,
                    &p,
                    &tr("preview.loading"),
                    &tr("preview.loading_hint"),
                ),
                _ => widgets::preview::preview_empty(
                    ui,
                    &p,
                    &tr("preview.empty"),
                    &tr("preview.hint"),
                ),
            }
        });

        ui.add_space(4.0);
        let enabled = has_clips;
        let action = widgets::transport::transport(
            ui,
            &p,
            self.project.playhead,
            duration,
            self.playing_preview,
            self.muted,
            self.volume,
            enabled,
            Some(&self.icons),
        );
        if let Some(action) = action {
            self.handle_transport(action);
        }
    }

    // --- preview plumbing -------------------------------------------------

    /// Re-decodes the current frame after the preview quality changed.
    ///
    /// The old texture is kept on screen until the new frame arrives: clearing
    /// it made the preview flash "no video imported" for a frame or two, which
    /// reads like the project was lost.
    fn change_preview_quality(&mut self) {
        // Force the worker to re-point at the source with the new size cap.
        self.preview_source = None;
        // Cached frames were decoded at the previous scale; drop them so the
        // new size is not silently mixed with old pixels.
        self.preview.send(PreviewRequest::ClearCache);
        self.request_seek(false);
    }

    /// Long-edge cap for preview decoding at the selected quality.
    fn preview_max_edge(&self) -> u32 {
        self.prefs
            .preview_quality
            .max_edge(self.prefs.preview_max_height)
    }

    fn duration(&self) -> cf_core::Time {
        self.project.duration()
    }

    /// Frame rate of whatever sits under the playhead, used for frame stepping.
    fn active_frame_rate(&self) -> cf_core::FrameRate {
        self.project
            .resolve(self.project.playhead)
            .map(|(source, _)| source.info.video.frame_rate)
            .unwrap_or_default()
    }

    /// Maps a timeline position to the source that should be shown there.
    fn media_at(&self, timeline: cf_core::Time) -> Option<(cf_core::SourceId, cf_core::Time)> {
        self.project
            .resolve(timeline)
            .map(|(source, source_time)| (source.id, source_time))
    }

    /// Requests the frame for the current timeline playhead.
    ///
    /// Crossing a clip boundary repoints the worker at a different source, so
    /// one function covers gaps, clip edges and source switches.
    fn request_seek(&mut self, precise: bool) {
        let Some(engine) = &self.engine else {
            return;
        };

        let Some((source_id, source_time)) = self.media_at(self.project.playhead) else {
            // The playhead is in a gap; nothing to decode.
            self.awaiting = None;
            return;
        };
        let Some(source) = self.project.source(source_id) else {
            return;
        };

        if self.preview_source != Some(source_id) {
            self.preview.send(PreviewRequest::Source {
                id: source.id,
                install: engine.install.clone(),
                path: source.path().clone(),
                info: source.info.clone(),
                max_edge: self.preview_max_edge(),
            });
            self.preview_source = Some(source_id);
            // The old texture stays on screen until the new frame arrives; it
            // is replaced wholesale when the size differs (see poll_preview),
            // and blanking it here made the preview flash its empty state.
        }

        self.preview_generation += 1;
        let generation = self.preview_generation;
        self.awaiting = Some(generation);
        self.preview.send(PreviewRequest::Seek {
            generation,
            time: source_time,
            precise,
        });
    }

    fn start_playback(&mut self) {
        let Some((source_id, source_time)) = self.media_at(self.project.playhead) else {
            return;
        };
        if self.project.playhead >= self.duration() {
            self.project.set_playhead(cf_core::Time::ZERO);
            return self.start_playback();
        }

        if self.preview_source != Some(source_id) {
            if let (Some(engine), Some(source)) = (&self.engine, self.project.source(source_id)) {
                self.preview.send(PreviewRequest::Source {
                    id: source.id,
                    install: engine.install.clone(),
                    path: source.path().clone(),
                    info: source.info.clone(),
                    max_edge: self.preview_max_edge(),
                });
                self.preview_source = Some(source_id);
            }
        }

        self.preview_generation += 1;
        let generation = self.preview_generation;
        self.awaiting = Some(generation);
        self.playing_preview = true;
        self.playback_timeline_origin = self.project.playhead;
        self.playback_source_origin = source_time;
        self.preview.send(PreviewRequest::Play {
            generation,
            from: source_time,
        });

        // Decode the audio, but do NOT start it: the first video frame is
        // still 100-200 ms away (the decoder has to seek), and starting now
        // would put sound ahead of the picture. `poll_preview` starts it the
        // moment the first matching frame is uploaded.
        self.prepare_audio_for_playback();
        self.audio.stop();
        self.audio_pending_start = Some(self.project.playhead);
    }

    /// Ensures the audio buffer covers the playhead, decoding if needed.
    ///
    /// Only the current segment is decoded: decoding a whole multi-clip
    /// timeline would waste time on parts the user may never reach, and the
    /// buffer has to be re-cut at every clip boundary anyway.
    fn prepare_audio_for_playback(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        // Resolve the clip under the playhead; gaps have no audio.
        let Some(entry) = self
            .project
            .video_track()
            .and_then(|track| track.clip_at(self.project.playhead))
            .cloned()
        else {
            self.audio.clear();
            self.audio_span = None;
            return;
        };
        let Some(source) = self.project.source(entry.source_id) else {
            return;
        };

        // Already covered by the loaded buffer? Nothing to do.
        if let Some((from, to)) = self.audio_span {
            if from <= self.project.playhead && self.project.playhead < to {
                return;
            }
        }

        let start = self.project.playhead;
        let remaining = entry.timeline_end().saturating_sub(start);

        match cf_core::media::audio::decode(
            &engine.install,
            source.path(),
            entry.source_time_at(start),
            remaining,
        ) {
            Ok(mut buffer) => {
                // Re-base the buffer onto the timeline so the cursor maps
                // straight onto playhead positions.
                buffer.start = start;
                if buffer.is_empty() {
                    log::debug!("no audio in {}", source.display_name());
                }
                self.audio.set_volume(self.volume);
                self.audio.set_muted(self.muted);
                self.audio.load(buffer);
                self.audio_span = Some((start, start.saturating_add(remaining)));
            }
            Err(err) => {
                log::warn!("audio decode failed: {}", err.detail());
                self.audio.clear();
                self.audio_span = None;
            }
        }
    }

    fn stop_playback(&mut self) {
        if !self.playing_preview {
            return;
        }
        self.playing_preview = false;
        self.audio_pending_start = None;
        self.audio.stop();
        self.preview.send(PreviewRequest::Stop);
        // Re-seek so the frame matches the playhead, which playback may have
        // overshot by up to one frame.
        self.request_seek(false);
    }

    /// Drains preview events and uploads the newest frame.
    fn poll_preview(&mut self, ctx: &egui::Context) {
        let drained = self.preview.drain();

        for (generation, detail) in drained.failures {
            if self.awaiting == Some(generation) {
                log::warn!("preview decode failed: {detail}");
                self.awaiting = None;
            }
        }

        if drained.finished {
            self.playing_preview = false;
            self.audio.stop();
            self.project.set_playhead(self.duration());
        }

        let Some((generation, frame, size, playing)) = drained.newest_frame else {
            return;
        };

        // Drop frames from superseded requests.
        match self.awaiting {
            Some(awaiting) if generation != awaiting => return,
            None if !playing => return,
            _ => {}
        }

        self.awaiting = None;

        if playing {
            // First frame of this playback run: the picture is now on screen,
            // so this is the correct instant to start the audio clock.
            if let Some(at) = self.audio_pending_start.take() {
                if !self.muted {
                    self.audio.play_from(at);
                }
            }

            // Audio is the master clock when it is available: a sound card
            // cannot drift, whereas `-re` is only accurate to a frame and the
            // two would slowly separate over a long clip. Fall back to the
            // decoder's own timestamps when there is no audio.
            let position = if self.muted {
                None
            } else {
                self.audio.position()
            };

            let next = match position {
                Some(audio_pos) if audio_pos > cf_core::Time::ZERO => audio_pos,
                _ => {
                    let elapsed = frame.time.saturating_sub(self.playback_source_origin);
                    self.playback_timeline_origin.saturating_add(elapsed)
                }
            };

            self.project.set_playhead(next);
            self.follow_playhead = true;

            // The buffer only covers one clip; re-arm at each boundary.
            if self
                .audio_span
                .is_some_and(|(_, end)| self.project.playhead >= end)
            {
                self.prepare_audio_for_playback();
                if !self.muted {
                    self.audio.play_from(self.project.playhead);
                }
            }
        }

        self.frame_aspect = if frame.height > 0 {
            frame.width as f32 / frame.height as f32
        } else {
            16.0 / 9.0
        };

        // Guard against a frame whose pixel buffer does not match the size
        // reported for it: uploading that would paint garbage.
        let expected = size.width as usize * size.height as usize * 4;
        if frame.rgba.len() != expected {
            log::warn!(
                "dropping frame with {} bytes, expected {} for {}x{}",
                frame.rgba.len(),
                expected,
                size.width,
                size.height
            );
            return;
        }

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [size.width as usize, size.height as usize],
            &frame.rgba,
        );
        let wanted = [size.width as usize, size.height as usize];

        match &mut self.frame_texture {
            Some(texture) if texture.size() == wanted => {
                texture.set(image, egui::TextureOptions::LINEAR);
            }
            _ => {
                self.frame_texture =
                    Some(ctx.load_texture("clipforge_frame", image, egui::TextureOptions::LINEAR));
            }
        }

        if playing {
            // `-re` paces frames; keep draining without waiting for input.
            ctx.request_repaint();
        }
    }

    // --- export ----------------------------------------------------------

    /// Recomputes the lossless plan for the selected clip.
    ///
    /// Called whenever the selection, edit state or output path changes, so
    /// the dialog always shows the cut points the current project would
    /// actually produce.
    /// The project the export actually encodes.
    ///
    /// When "remove gaps on export" is on, empty spans collapse so the output
    /// jumps straight from one clip to the next. This is a copy: the timeline
    /// the user sees keeps its layout, and the setting only changes the
    /// rendered result.
    fn export_project(&self) -> cf_core::Project {
        if self.prefs.remove_gaps_on_export {
            self.project.without_gaps()
        } else {
            self.project.clone()
        }
    }

    fn refresh_export_plan(&mut self) {
        self.export_plan = None;
        self.export_precise_plan = None;
        self.export_blocker = None;
        self.export_compat = None;

        let project = self.export_project();
        let Ok((start, end)) = self.export_range.resolve(&project) else {
            return;
        };

        let pieces = match timeline_pieces(&project, start, end) {
            Ok(pieces) => pieces,
            Err(blocker) => {
                self.export_blocker = Some(blocker);
                return;
            }
        };

        let real = only_segments(&pieces);
        if real.is_empty() {
            self.export_blocker = Some(cf_core::media::export::ExportBlocker::RangeEmpty);
            return;
        }

        let output = self
            .export_output
            .clone()
            .unwrap_or_else(|| default_export_path(&real_output_stem(&project, &real)));

        match self.export_mode {
            ExportMode::Precise => {
                // The container decides the codecs: VP9+Opus for webm, H.264+AAC
                // otherwise. Letting the user pick a webm path and then failing
                // at mux time would be a bad trade.
                let container = self
                    .export_output
                    .as_deref()
                    .and_then(cf_core::media::export::container_for)
                    .unwrap_or("mp4");
                let settings = match &self.engine {
                    Some(engine) => PreciseSettings::for_container(container, &engine.install),
                    None => PreciseSettings::default(),
                };
                let plan = plan_precise(&project, start, end, settings);
                self.export_blocker = plan.blocker.clone();
                self.export_precise_plan = Some(plan);
            }
            ExportMode::Lossless => {
                // Compatibility first: an incompatible set can never be
                // concatenated without re-encoding, and saying so up front is
                // more useful than a downstream muxer error.
                let source_ids: Vec<cf_core::SourceId> = real.iter().map(|s| s.source_id).collect();
                let unique: Vec<cf_core::SourceId> = {
                    let mut seen = Vec::new();
                    for id in source_ids {
                        if !seen.contains(&id) {
                            seen.push(id);
                        }
                    }
                    seen
                };

                let report = check_compat(&project, &unique);
                if !report.is_compatible() {
                    self.export_compat = Some(report.clone());
                    self.export_blocker = Some(
                        cf_core::media::export::ExportBlocker::IncompatibleSegments(report),
                    );
                    return;
                }

                // A stream copy cannot represent gaps.
                if pieces.iter().any(|piece| piece.is_gap()) {
                    self.export_blocker =
                        Some(cf_core::media::export::ExportBlocker::TimelineHasGaps);
                    return;
                }

                // Keyframes decide the real cut points; without them a lossless
                // export cannot be planned at all.
                if real.iter().any(|segment| {
                    project
                        .source(segment.source_id)
                        .map(|source| source.keyframes.is_degraded())
                        .unwrap_or(true)
                }) {
                    self.export_blocker =
                        Some(cf_core::media::export::ExportBlocker::KeyframesUnavailable);
                    return;
                }

                // Reuse the single-clip planner for the whole range so the
                // dialog keeps showing real cut points and drift.
                let Some(first) = real.first() else { return };
                let Some(source) = project.source(first.source_id) else {
                    self.export_blocker =
                        Some(cf_core::media::export::ExportBlocker::MissingSource);
                    return;
                };

                let clip = cf_core::Clip {
                    id: cf_core::ClipId(0),
                    source_id: first.source_id,
                    source_in: first.source_in,
                    source_out: first.source_out,
                    timeline_start: start,
                    group: None,
                    name: source.display_name(),
                };

                let plan = cf_core::media::export::plan_lossless(
                    &clip,
                    source,
                    &source.keyframes,
                    &output,
                );
                self.export_blocker = plan.blocker.clone();
                self.export_plan = Some(plan);
            }
        }
    }

    /// Opens the export dialog. The range follows the selection when there is
    /// one, otherwise the whole timeline.
    fn open_export(&mut self) {
        if self.project.duration() <= cf_core::Time::ZERO {
            self.notify(StatusMessage::EditFailed(
                tr("export.blocker.nothing").to_string(),
            ));
            return;
        }

        // Default to the whole timeline: that is the case users expect when
        // they hit Export, and it is also the one where compatibility checks
        // matter. A selection narrows it afterwards via the range control.
        self.export_range = ExportRange::Timeline;
        if self.export_output.is_none() {
            if let Some(entry) = self.selected_clip.and_then(|id| self.project.clip(id)) {
                if let Some(source) = self.project.source(entry.clip.source_id) {
                    self.set_export_output(default_export_path(&source.info.path));
                }
            }
        }
        self.export_error = None;
        self.export_log = None;
        self.refresh_export_plan();
        self.export_open = true;
    }

    /// Applies the dialog's edited text, if it parses as a usable path.
    fn commit_output_text(&mut self) {
        let trimmed = self.export_output_text.trim().to_string();
        if trimmed.is_empty() {
            self.export_output = None;
            self.refresh_export_plan();
            return;
        }
        self.set_export_output(PathBuf::from(trimmed));
    }

    /// Single place that updates both the path and its text mirror.
    fn set_export_output(&mut self, path: PathBuf) {
        self.export_output_text = path.display().to_string();
        self.export_output = Some(path);
        self.refresh_export_plan();
    }

    fn pick_export_output(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("Video", &["mp4", "mkv", "mov"]);
        if let Some(current) = &self.export_output {
            if let Some(parent) = current.parent() {
                dialog = dialog.set_directory(parent);
            }
            if let Some(name) = current.file_name() {
                dialog = dialog.set_file_name(name.to_string_lossy());
            }
        }

        if let Some(path) = dialog.save_file() {
            self.set_export_output(path);
        }
    }

    /// Starts the export described by the current plan.
    fn start_export(&mut self, ctx: &egui::Context) {
        if self.export_running.is_some() {
            return;
        }

        let Some(output) = self.export_output.clone() else {
            return;
        };
        if self.export_blocker.is_some() {
            return;
        }

        let Some(engine) = &self.engine else {
            self.export_error = Some(tr("export.blocker.engine_missing").to_string());
            return;
        };
        let install = engine.install.clone();

        // Refuse to overwrite an input file even if the user typed its path.
        if let Err(blocker) = cf_core::media::export::validate_output(&output, &self.project) {
            self.export_blocker = Some(blocker);
            return;
        }

        let job = match self.export_mode {
            ExportMode::Precise => {
                let Some(plan) = self.export_precise_plan.clone() else {
                    return;
                };
                let sources = self.job_sources();
                let label = self.export_label();
                ExportJob::Precise {
                    label,
                    install,
                    sources,
                    output,
                    plan,
                }
            }
            ExportMode::Lossless => {
                let Some(plan) = self.export_plan.clone() else {
                    return;
                };
                let sources = self.job_sources();

                let project = self.export_project();
                let Ok((range_start, range_end)) = self.export_range.resolve(&project) else {
                    return;
                };
                let pieces = timeline_pieces(&project, range_start, range_end).unwrap_or_default();
                let real = only_segments(&pieces);

                if real.len() <= 1 {
                    // Single segment: the plain stream copy, which keeps the
                    // source's own container nuances.
                    let Some(segment) = real.first() else { return };
                    let Some(source) = project.source(segment.source_id) else {
                        return;
                    };
                    ExportJob::LosslessSingle {
                        label: self.export_label(),
                        install,
                        source: source.path().clone(),
                        output,
                        plan,
                        has_audio: source.has_audio(),
                    }
                } else {
                    let total = real.iter().fold(cf_core::Time::ZERO, |acc, s| {
                        acc.saturating_add(s.duration())
                    });
                    ExportJob::LosslessConcat {
                        label: self.export_label(),
                        install,
                        sources,
                        segments: real,
                        output,
                        total,
                    }
                }
            }
        };

        self.export_error = None;
        self.export_log = None;
        self.export_progress = RunningState::default();
        self.export_running = Some(ExportWorker::start(job, ctx.clone()));
    }

    /// One entry per source the current export touches.
    fn job_sources(&self) -> Vec<JobSource> {
        let project = self.export_project();
        let Ok((start, end)) = self.export_range.resolve(&project) else {
            return Vec::new();
        };
        let Ok(pieces) = timeline_pieces(&project, start, end) else {
            return Vec::new();
        };

        let mut out: Vec<JobSource> = Vec::new();
        for segment in only_segments(&pieces) {
            if out.iter().any(|s| s.id == segment.source_id) {
                continue;
            }
            if let Some(source) = project.source(segment.source_id) {
                out.push(JobSource {
                    id: source.id,
                    path: source.path().clone(),
                    has_audio: source.has_audio(),
                });
            }
        }
        out
    }

    /// Human-readable job label for logs.
    fn export_label(&self) -> String {
        match &self.export_range {
            ExportRange::Clip(id) => self
                .project
                .clip(*id)
                .map(|entry| entry.clip.name.clone())
                .unwrap_or_else(|| "timeline".to_owned()),
            _ => "timeline".to_owned(),
        }
    }

    /// Drains export events into the dialog state.
    fn poll_export(&mut self) {
        let Some(worker) = &self.export_running else {
            return;
        };

        let drained = worker.drain();

        if let Some((done, total, percent, speed)) = drained.progress {
            self.export_progress.done = done;
            self.export_progress.total = total;
            self.export_progress.percent = percent;
            self.export_progress.speed = speed;
        }
        self.export_progress.cancelling = worker.is_cancelling();

        let Some(outcome) = drained.outcome else {
            return;
        };

        self.export_running = None;
        match outcome {
            ExportEvent::Succeeded { output, log_tail } => {
                self.export_progress.percent = 100.0;
                self.export_log = Some(log_tail);
                self.notify(StatusMessage::Exported(output.display().to_string()));
                self.export_open = false;
            }
            ExportEvent::Failed { detail, log_tail } => {
                self.export_error = Some(detail.clone());
                self.export_log = Some(log_tail);
                self.notify(StatusMessage::ExportFailed(detail));
            }
            ExportEvent::Cancelled => {
                self.export_progress = RunningState::default();
                self.notify(StatusMessage::ExportCancelled);
            }
            ExportEvent::Progress { .. } => {}
        }
    }

    fn export_dialog(&mut self, ctx: &egui::Context) {
        if !self.export_open {
            return;
        }

        let logo = if self.palette.dark {
            self.logo_dark.as_ref()
        } else {
            self.logo_light.as_ref()
        };

        let running = self
            .export_running
            .as_ref()
            .map(|_| self.export_progress.clone());

        let mut state = ExportDialogState {
            output_text: &mut self.export_output_text,
            mode: self.export_mode,
            plan: self.export_plan.as_ref(),
            precise: self.export_precise_plan.as_ref(),
            blocker: self.export_blocker.clone(),
            compat: self.export_compat.as_ref(),
            output: self.export_output.as_deref(),
            running,
            error: self.export_error.as_deref(),
            log_tail: self.export_log.as_deref(),
        };

        let action = export::show(
            ctx,
            &self.palette,
            logo,
            &mut state,
            env!("CARGO_PKG_VERSION"),
        );

        match action {
            ExportAction::Close => {
                // Closing while a job runs hides the dialog but keeps it going.
                self.export_open = false;
            }
            ExportAction::Browse => self.pick_export_output(),
            ExportAction::OutputEdited => self.commit_output_text(),
            ExportAction::SetContainer(choice) => {
                // Same blur race as `Start`: fold in whatever the user just
                // typed so the chip rewrites the path they can see.
                self.commit_output_text();
                if let Some(current) = self.export_output.clone() {
                    let next = choice.apply(&current);
                    self.set_export_output(next);
                }
            }
            ExportAction::SetMode(mode) => {
                self.export_mode = mode;
                self.refresh_export_plan();
            }
            ExportAction::SwitchToPrecise => {
                self.export_mode = ExportMode::Precise;
                self.refresh_export_plan();
            }
            ExportAction::Start => {
                // A click on the button blurs the path field in the same
                // frame, so `OutputEdited` would be overwritten by `Start`.
                // Commit here to keep the job pointed at the typed path.
                self.commit_output_text();
                self.start_export(ctx);
            }
            ExportAction::Cancel => {
                if let Some(worker) = &mut self.export_running {
                    worker.cancel();
                }
            }
            ExportAction::None => {}
        }
    }

    fn timeline_panel(&mut self, ui: &mut Ui) {
        use cf_ui::widgets::timeline::{timeline, TimelineAction, TimelineInput};

        let p = self.palette;
        let content_end = self.duration().max(cf_core::Time::from_secs(1.0));

        // The first draw after an import needs the real viewport width before
        // fitting; `timeline` overwrites `view.width` with it, so fit after
        // the first pass by remembering the width it reported.
        let mut view = self.timeline_view;
        // `timeline` reports the track-area width it actually used; fitting
        // needs that number, so the first frame after an import only records
        // the width and the next one fits.
        if self.fit_timeline_pending && view.width > 1.0 {
            view.fit(content_end);
            self.timeline_view = view;
            self.fit_timeline_pending = false;
        }

        // Keep the playhead visible while playing.
        if self.follow_playhead && content_end > view.visible_duration() {
            let playhead = self.project.playhead;
            if playhead < view.scroll {
                view.scroll = playhead;
            } else if playhead > view.scroll.saturating_add(view.visible_duration()) {
                view.scroll = playhead.saturating_sub(view.visible_duration());
            }
        }

        let input = TimelineInput {
            view: &view,
            project: &self.project,
            playhead: self.project.playhead,
            selected: self.selected_clip,
            content_end,
            title: &tr("timeline.title"),
            empty_hint: &tr("timeline.empty"),
            drag: self.drag.as_ref(),
            scrubbing: self.scrubbing,
            edit: EditAvailability {
                has_selection: self.selected_clip.is_some(),
                can_undo: self.history.can_undo(),
                can_redo: self.history.can_redo(),
            },
            icons: Some(&self.icons),
        };

        let (actions, drawn_width) = timeline(ui, &p, &input);

        // Mirror the geometry the widget actually used so next frame's zoom
        // anchoring and scroll clamping agree with what was drawn.
        if drawn_width > 1.0 {
            self.timeline_view.width = drawn_width;
        }

        for action in actions {
            match action {
                TimelineAction::Seek { time, precise } => {
                    self.stop_playback();
                    self.project.set_playhead(time);
                    // Invalidate the audio buffer: it starts at the old spot.
                    self.audio_span = None;
                    self.audio_pending_start = None;
                    self.audio.clear();
                    self.request_seek(precise);
                    // `precise` marks the release, which ends the scrub.
                    self.scrubbing = !precise;
                }
                TimelineAction::Select(clip) => {
                    if self.selected_clip != clip {
                        self.selected_clip = clip;
                        self.refresh_export_plan();
                    }
                }
                TimelineAction::Scroll { dx_pixels } => {
                    let delta = cf_core::Time::from_secs(
                        (dx_pixels / self.timeline_view.px_per_sec) as f64,
                    );
                    let mut view = self.timeline_view;
                    view.scroll_by(delta, content_end);
                    self.timeline_view = view;
                    self.follow_playhead = false;
                }
                TimelineAction::Zoom { anchor_x, factor } => {
                    let mut view = self.timeline_view;
                    view.zoom_at(anchor_x, factor, content_end);
                    self.timeline_view = view;
                }
                TimelineAction::Fit => {
                    let mut view = self.timeline_view;
                    view.fit(content_end);
                    self.timeline_view = view;
                    self.follow_playhead = false;
                }
                TimelineAction::DragStarted {
                    clip,
                    edge,
                    grab_offset,
                } => {
                    self.stop_playback();
                    self.selected_clip = Some(clip);
                    // Snapshot once: the whole drag becomes a single undo step.
                    self.drag_before = Some(self.project.clone());
                    // `preview_at` is always the clip's left edge. A body drag
                    // reports the raw pointer time; converting here once means
                    // every consumer (ghost drawing, commit) works in the same
                    // space.
                    let start = self
                        .project
                        .clip(clip)
                        .map(|entry| entry.clip.timeline_start)
                        .unwrap_or(cf_core::Time::ZERO);
                    self.drag = Some(DragState {
                        clip,
                        edge,
                        grab_offset,
                        preview_at: start,
                        snap: None,
                    });
                }
                TimelineAction::DragMoved { at, snap } => {
                    if let Some(drag) = &mut self.drag {
                        drag.preview_at = at;
                        drag.snap = snap.is_snapped().then_some(snap);
                    }
                }
                TimelineAction::Split => self.split_at_playhead(),
                TimelineAction::Merge => self.merge_selected(),
                TimelineAction::Delete => self.delete_selected(),
                TimelineAction::Undo => self.undo(),
                TimelineAction::Redo => self.redo(),
                TimelineAction::DragFinished => self.commit_drag(),
                TimelineAction::DragCancelled => {
                    // Restore the snapshot: the preview never touched the
                    // project, but a cancelled drag should also clear state.
                    if let Some(before) = self.drag_before.take() {
                        self.project = before;
                    }
                    self.drag = None;
                }
            }
        }
    }

    /// Applies the in-flight drag to the project as one undoable edit.
    fn commit_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let Some(before) = self.drag_before.take() else {
            return;
        };

        let target = drag.preview_at;
        let frame_rate = self.active_frame_rate();

        let mut after = before.clone();
        let result = match drag.edge {
            DragEdge::Body => after
                .move_group(drag.clip, target)
                .map(|()| EditOutcome::Moved),
            DragEdge::Start => after
                .trim_clip(drag.clip, TrimEdge::Start, target, frame_rate)
                .map(|()| EditOutcome::Trimmed),
            DragEdge::End => after
                .trim_clip(drag.clip, TrimEdge::End, target, frame_rate)
                .map(|()| EditOutcome::Trimmed),
        };

        match result {
            Ok(outcome) => {
                let label = outcome.label();
                self.project = after;
                self.history.push(label, before, self.project.clone());
                self.request_seek(false);
            }
            Err(err) => {
                // Leave the project exactly as it was and say why.
                self.notify(StatusMessage::EditFailed(err.to_string()));
            }
        }
    }

    // --- edit commands ---------------------------------------------------

    /// Splits the selected clip at the playhead.
    fn split_at_playhead(&mut self) {
        let Some(clip_id) = self.selected_clip else {
            self.notify(StatusMessage::EditFailed(
                tr("edit.select_first").to_string(),
            ));
            return;
        };

        let at = self.project.playhead;
        let rate = self.active_frame_rate();
        let result = self
            .history
            .commit(&mut self.project, "edit.split", |project| {
                project.split_clip(clip_id, at, rate)
            });

        match result {
            Ok(()) => self.after_edit(),
            Err(err) => self.notify(StatusMessage::EditFailed(err.to_string())),
        }
    }

    fn delete_selected(&mut self) {
        let Some(clip_id) = self.selected_clip else {
            self.notify(StatusMessage::EditFailed(
                tr("edit.select_first").to_string(),
            ));
            return;
        };

        if self
            .history
            .commit(&mut self.project, "edit.delete", |project| {
                project.delete_clip(clip_id)
            })
            .is_ok()
        {
            self.selected_clip = None;
            self.after_edit();
        }
    }

    /// Merges the selected clip with its neighbour across the nearest seam.
    fn merge_selected(&mut self) {
        let Some(clip_id) = self.selected_clip else {
            self.notify(StatusMessage::EditFailed(
                tr("edit.select_first").to_string(),
            ));
            return;
        };

        let Some((left, right)) = self.project.adjacent_pair_at(clip_id) else {
            self.notify(StatusMessage::EditFailed(
                tr("edit.no_neighbour").to_string(),
            ));
            return;
        };

        match self
            .history
            .commit(&mut self.project, "edit.merge", |project| {
                project.merge_clips(left, right)
            }) {
            Ok(MergeOutcome::Merged { kept, .. }) => {
                self.selected_clip = Some(kept);
                self.after_edit();
            }
            Ok(MergeOutcome::Grouped { .. }) => {
                // Both clips survive and are now grouped; keep the selection.
                self.selected_clip = Some(clip_id);
                self.after_edit();
            }
            Err(err) => self.notify(StatusMessage::EditFailed(err.to_string())),
        }
    }

    fn undo(&mut self) {
        match self.history.undo(&mut self.project) {
            Some(label) => {
                // The clip the user was looking at may be gone; drop the
                // selection rather than pointing at a stale id.
                if self
                    .selected_clip
                    .is_some_and(|id| self.project.clip(id).is_none())
                {
                    self.selected_clip = None;
                }
                self.notify(StatusMessage::Undone(label));
                self.after_edit();
            }
            None => self.notify(StatusMessage::EditFailed(
                tr("edit.nothing_to_undo").to_string(),
            )),
        }
    }

    fn redo(&mut self) {
        match self.history.redo(&mut self.project) {
            Some(label) => {
                self.notify(StatusMessage::Redone(label));
                self.after_edit();
            }
            None => self.notify(StatusMessage::EditFailed(
                tr("edit.nothing_to_redo").to_string(),
            )),
        }
    }

    /// Shared post-edit bookkeeping: clamp the playhead, refresh the frame,
    /// and recompute the export plan (the clip shape may have changed).
    fn after_edit(&mut self) {
        let duration = self.duration();
        let playhead = self.project.playhead;
        self.project.set_playhead(playhead.min(duration));
        self.request_seek(false);
        self.refresh_export_plan();
    }

    /// Keyboard shortcuts. Ignored while a modal is open or a drag is running.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.settings_open || self.about_open || self.drag.is_some() {
            return;
        }
        // Let text fields swallow keys first.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        use egui::{Key, Modifiers};

        // `consume_key` matches the exact modifier set, so Ctrl+Shift+Z cannot
        // be swallowed by the Ctrl+Z branch and vice versa.
        let (space, split, delete, undo, redo, merge, left, right, jump_start, jump_end) = ctx
            .input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::Space),
                    i.consume_key(Modifiers::NONE, Key::S),
                    i.consume_key(Modifiers::NONE, Key::Delete)
                        || i.consume_key(Modifiers::NONE, Key::Backspace),
                    i.consume_key(Modifiers::COMMAND, Key::Z),
                    i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z),
                    i.consume_key(Modifiers::NONE, Key::M),
                    // Shift+arrow must be matched separately: `consume_key(NONE, ..)`
                    // only fires when no modifier is held.
                    i.consume_key(Modifiers::NONE, Key::ArrowLeft)
                        || i.consume_key(Modifiers::SHIFT, Key::ArrowLeft),
                    i.consume_key(Modifiers::NONE, Key::ArrowRight)
                        || i.consume_key(Modifiers::SHIFT, Key::ArrowRight),
                    // Up/Down jump to the ends of the clip under the playhead,
                    // Home/End to the ends of the whole timeline.
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                )
            });

        if space {
            if self.playing_preview {
                self.stop_playback();
            } else {
                self.start_playback();
            }
        }
        if split {
            self.split_at_playhead();
        }
        if delete {
            self.delete_selected();
        }
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if merge {
            self.merge_selected();
        }

        // Arrow keys step one frame; with Shift they step one second.
        let step_secs = if ctx.input(|i| i.modifiers.shift) {
            1.0
        } else {
            0.0
        };
        if left || right {
            self.stop_playback();
            let delta = if step_secs > 0.0 {
                cf_core::Time::from_secs(step_secs)
            } else {
                self.active_frame_rate().frame_duration()
            };
            let signed = if left {
                cf_core::Time(-delta.micros())
            } else {
                delta
            };
            self.project.nudge_playhead(signed);
            self.request_seek(true);
        }

        if jump_start {
            self.jump_to_clip_edge(true);
        }
        if jump_end {
            self.jump_to_clip_edge(false);
        }
    }

    /// Up/Down move the playhead to the start/end of the clip it is inside.
    ///
    /// Falls back to the timeline ends when the playhead sits in a gap, so the
    /// keys always do something predictable.
    fn jump_to_clip_edge(&mut self, start: bool) {
        let Some(track) = self.project.video_track() else {
            return;
        };
        let target = track
            .clip_at(self.project.playhead)
            .map(|clip| {
                if start {
                    clip.timeline_start
                } else {
                    clip.timeline_end()
                }
            })
            .unwrap_or(if start {
                cf_core::Time::ZERO
            } else {
                self.project.duration()
            });

        self.stop_playback();
        self.project.set_playhead(target);
        self.audio_span = None;
        self.audio_pending_start = None;
        self.audio.clear();
        self.request_seek(true);
    }

    fn media_sidebar(&mut self, ui: &mut Ui) {
        let p = self.palette;

        ui.label(
            egui::RichText::new(tr("media.sources"))
                .size(12.0)
                .strong()
                .color(p.text_muted),
        );
        ui.add_space(4.0);

        let mut reveal = None;
        if let Some(index) = widgets::media_panel::source_list(
            ui,
            &p,
            &self.project.sources,
            None,
            &self.pending_imports,
        ) {
            reveal = self.project.sources.get(index).map(|s| s.id);
        }
        // Jump the playhead to the first clip that uses the clicked source.
        if let Some(source_id) = reveal {
            let start = self
                .project
                .clips()
                .find(|entry| entry.clip.source_id == source_id)
                .map(|entry| entry.clip.timeline_start);
            if let Some(start) = start {
                self.stop_playback();
                self.project.set_playhead(start);
                self.request_seek(false);
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(
            egui::RichText::new(tr("media.metadata"))
                .size(12.0)
                .strong()
                .color(p.text_muted),
        );
        ui.add_space(4.0);

        // Detail panel follows the selected clip's source.
        let selected = self
            .selected_clip
            .and_then(|id| self.project.clip(id))
            .and_then(|entry| self.project.source(entry.clip.source_id));
        egui::ScrollArea::vertical()
            .id_salt("clipforge_metadata")
            .show(ui, |ui| {
                widgets::media_panel::metadata(ui, &p, selected);
            });
    }

    fn engine_status_text(&self) -> String {
        match &self.engine {
            Some(info) => format!(
                "FFmpeg {} · {} · {}",
                info.version_label(),
                info.source.label(),
                if info.install.version.gpl {
                    "GPL"
                } else {
                    "LGPL"
                }
            ),
            None => tr("media.engine_missing").to_string(),
        }
    }

    /// Short audio-output indicator; only shown when audio is unavailable,
    /// because silence with no explanation is the worst possible state.
    fn audio_status_text(&self) -> Option<String> {
        if self.audio.is_available() {
            return None;
        }
        Some(format!(
            "{}: {}",
            tr("media.audio_unavailable"),
            self.audio.unavailable_reason().unwrap_or("unknown")
        ))
    }

    fn handle_transport(&mut self, action: TransportAction) {
        match action {
            TransportAction::PlayPause => {
                if self.playing_preview {
                    self.stop_playback();
                } else {
                    self.start_playback();
                }
            }
            TransportAction::MuteToggle => {
                self.muted = !self.muted;
                self.audio.set_muted(self.muted);
                if !self.muted && self.playing_preview {
                    // Resume from the current picture, not from the start.
                    self.audio.play_from(self.project.playhead);
                    self.audio_pending_start = None;
                } else if self.muted {
                    self.audio_pending_start = None;
                }
            }
            TransportAction::Volume(value) => {
                self.volume = value;
                self.audio.set_volume(value);
            }
            TransportAction::PrevFrame => {
                self.stop_playback();
                self.project.set_playhead(crate::preview::step_frames(
                    self.project.playhead,
                    self.active_frame_rate(),
                    -1,
                    self.duration(),
                ));
                self.request_seek(true);
            }
            TransportAction::NextFrame => {
                self.stop_playback();
                self.project.set_playhead(crate::preview::step_frames(
                    self.project.playhead,
                    self.active_frame_rate(),
                    1,
                    self.duration(),
                ));
                self.request_seek(true);
            }
        }
    }

    fn pick_file(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mkv", "mov", "webm", "avi", "m4v"]);
        if let Some(dir) = &self.prefs.last_import_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.prefs.last_import_dir = path.parent().map(|p| p.to_path_buf());
            self.persist_prefs();
            self.request_import(path);
        }
    }

    /// Public entry point for the startup arguments path.
    pub fn queue_import(&mut self, path: PathBuf) {
        self.request_import(path);
    }

    /// Kicks off a background import. Duplicate paths are ignored so dropping
    /// the same file twice does not produce two sources.
    fn request_import(&mut self, path: PathBuf) {
        // Re-importing an existing file selects it rather than adding a copy.
        if let Some(index) = self
            .project
            .sources
            .iter()
            .position(|source| source.path() == &path)
        {
            let source_id = self.project.sources[index].id;
            let start = self
                .project
                .clips()
                .find(|entry| entry.clip.source_id == source_id)
                .map(|entry| entry.clip.timeline_start);
            if let Some(start) = start {
                self.stop_playback();
                self.project.set_playhead(start);
                self.request_seek(false);
            }
            return;
        }

        if self.engine.is_none() && self.engine_error.is_some() {
            self.notify(StatusMessage::EngineMissing);
            return;
        }

        let id = self.project.alloc_source_id().0;
        self.pending_imports.push(
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        );
        self.importer.import(path, id);
    }

    /// Drains worker events. Called once per frame before drawing.
    /// Turns an arrived update check into a status-bar notice.
    ///
    /// A failure is logged only: being offline, behind a proxy, or rate-limited
    /// is not something to interrupt the user about.
    fn poll_update(&mut self) {
        let Some(event) = crate::update::poll(&self.update_rx) else {
            return;
        };
        match event {
            crate::update::UpdateEvent::Available { tag, .. } => {
                log::info!("a newer release is available: {tag}");
                self.update_available = Some(tag);
            }
            crate::update::UpdateEvent::UpToDate => log::debug!("update check: up to date"),
            crate::update::UpdateEvent::Failed(detail) => {
                log::debug!("update check failed: {detail}");
            }
        }
    }

    fn poll_imports(&mut self) {
        while let Some(event) = self.importer.try_recv() {
            match event {
                ImportEvent::Engine(result) => match result {
                    Ok(info) => {
                        log::info!(
                            "FFmpeg {} found via {}",
                            info.version_label(),
                            info.source.label()
                        );
                        self.engine = Some(info);
                        self.engine_error = None;
                    }
                    Err(error) => {
                        log::warn!("FFmpeg not found: {}", error.detail());
                        self.engine = None;
                        self.engine_error = Some(error);
                        self.notify(StatusMessage::EngineMissing);
                    }
                },
                ImportEvent::Started { path } => {
                    let name = file_label(&path);
                    self.notify(StatusMessage::Importing(name));
                }
                ImportEvent::Metadata {
                    source,
                    keyframe_warning,
                } => {
                    let name = source.display_name();
                    self.pending_imports.retain(|pending| pending != &name);

                    if let Some(warning) = keyframe_warning {
                        log::warn!("keyframe index for {name} degraded: {warning}");
                    }

                    // Place the source on the video track right away; P4 makes
                    // the resulting clip editable.
                    let clip_id = self.project.add_source(source);
                    let start = self
                        .project
                        .clip(clip_id)
                        .map(|entry| entry.clip.timeline_start)
                        .unwrap_or(cf_core::Time::ZERO);

                    self.selected_clip = Some(clip_id);
                    self.stop_playback();
                    self.project.set_playhead(start);
                    // Fit the view once, when the first clip arrives.
                    if self
                        .project
                        .video_track()
                        .is_some_and(|t| t.clips.len() == 1)
                    {
                        self.timeline_view.width = 0.0; // recomputed on draw
                        self.fit_timeline_pending = true;
                    }
                    self.request_seek(false);
                    self.notify(StatusMessage::Imported(name));
                }
                ImportEvent::Failed { path, error } => {
                    let name = file_label(&path);
                    self.pending_imports.retain(|pending| pending != &name);
                    log::warn!("import of {} failed: {}", path.display(), error.detail());
                    self.notify(StatusMessage::ImportFailed(error.detail()));
                }
            }
        }
    }

    fn notify(&mut self, message: StatusMessage) {
        self.status = message;
        self.status_until = Some(Instant::now() + Duration::from_secs(4));
    }

    fn persist_prefs(&self) {
        if let Err(err) = self.prefs.save() {
            log::warn!("failed to save preferences: {err}");
        }
    }

    /// Applies theme and locale. The status text is stored as a key so it is
    /// re-translated after the locale switch rather than frozen in the old one.
    fn apply_preferences(&mut self, ctx: &Context, message: Option<StatusMessage>) {
        i18n::set_locale(self.prefs.locale);
        self.palette = theme::apply(ctx, self.prefs.theme);
        self.system_dark = theme::resolve_dark(ThemeMode::System);
        self.persist_prefs();
        if let Some(message) = message {
            self.notify(message);
        }
        ctx.request_repaint();
    }

    fn maybe_follow_system_theme(&mut self, ctx: &Context) {
        if self.prefs.theme != ThemeMode::System {
            return;
        }
        if self.last_theme_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_theme_check = Instant::now();
        let dark = theme::resolve_dark(ThemeMode::System);
        if dark != self.system_dark {
            self.system_dark = dark;
            self.palette = theme::apply(ctx, ThemeMode::System);
            ctx.request_repaint();
        }
    }

    fn handle_dropped_files(&mut self, ctx: &Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            self.request_import(path);
        }
    }

    fn draw_drop_overlay(&self, ctx: &Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering && !self.settings_open && !self.about_open {
            widgets::drop_overlay::drop_overlay(
                ctx,
                &self.palette,
                &tr("drop.title"),
                &tr("drop.subtitle"),
            );
        }
    }

    fn dialogs(&mut self, ctx: &Context) {
        if self.settings_open {
            let p = self.palette;
            match settings::show(ctx, &p, &mut self.prefs) {
                settings::SettingsAction::Close => self.settings_open = false,
                settings::SettingsAction::Changed => {
                    self.apply_preferences(ctx, Some(StatusMessage::SettingsSaved));
                }
                settings::SettingsAction::None => {}
            }
        }

        if self.about_open {
            let logo = if self.palette.dark {
                self.logo_dark.as_ref()
            } else {
                self.logo_light.as_ref()
            };
            if about::show(
                ctx,
                &self.palette,
                logo,
                &self.icons,
                env!("CARGO_PKG_VERSION"),
                self.font,
            ) {
                self.about_open = false;
            }
        }
    }
}

impl eframe::App for ClipForgeApp {
    fn on_exit(&mut self) {
        self.preview.send(PreviewRequest::Shutdown);
        self.importer.shutdown();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_imports();
        self.poll_preview(&ctx);
        self.poll_export();
        self.poll_update();
        self.handle_shortcuts(&ctx);
        self.maybe_follow_system_theme(&ctx);
        self.handle_dropped_files(&ctx);

        let p = self.palette;

        Panel::top("clipforge_toolbar")
            .exact_size(Metrics::TOOLBAR_HEIGHT)
            .frame(
                Frame::new()
                    .fill(p.bg_panel)
                    .stroke(Stroke::new(1.0, p.divider))
                    .inner_margin(Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| self.toolbar(ui));

        Panel::bottom("clipforge_status")
            .exact_size(Metrics::STATUS_HEIGHT)
            .frame(
                Frame::new()
                    .fill(p.bg_panel)
                    .stroke(Stroke::new(1.0, p.divider))
                    .inner_margin(Margin::symmetric(14, 4)),
            )
            .show(ui, |ui| self.status_bar(ui));

        Panel::bottom("clipforge_timeline")
            .resizable(true)
            .default_size(Metrics::TIMELINE_DEFAULT_HEIGHT)
            .min_size(160.0)
            .max_size(440.0)
            .frame(
                Frame::new()
                    .fill(p.bg_panel)
                    .stroke(Stroke::new(1.0, p.divider))
                    .inner_margin(Margin::same(10)),
            )
            .show(ui, |ui| self.timeline_panel(ui));

        if self.project.is_empty() && self.pending_imports.is_empty() {
            CentralPanel::default()
                .frame(Frame::new().fill(p.bg_base).inner_margin(Margin::same(10)))
                .show(ui, |ui| self.preview_panel(ui));
        } else {
            Panel::right("clipforge_media")
                .resizable(true)
                .default_size(272.0)
                .min_size(220.0)
                .max_size(420.0)
                .frame(
                    Frame::new()
                        .fill(p.bg_panel)
                        .stroke(Stroke::new(1.0, p.divider))
                        .inner_margin(Margin::same(12)),
                )
                .show(ui, |ui| self.media_sidebar(ui));

            CentralPanel::default()
                .frame(Frame::new().fill(p.bg_base).inner_margin(Margin::same(10)))
                .show(ui, |ui| self.preview_panel(ui));
        }

        self.dialogs(&ctx);
        self.export_dialog(&ctx);
        self.draw_drop_overlay(&ctx);
    }
}

/// Which edit a drag commit produced, used for the undo label.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EditOutcome {
    Moved,
    Trimmed,
}

impl EditOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Moved => "edit.move",
            Self::Trimmed => "edit.trim",
        }
    }
}

/// Compact segmented-control chip used for the preview quality selector.
fn quality_chip(
    ui: &mut egui::Ui,
    p: &cf_ui::theme::Palette,
    selected: bool,
    label: &str,
) -> egui::Response {
    use egui::{Color32, Stroke};
    let fill = if selected {
        p.accent_soft
    } else {
        Color32::TRANSPARENT
    };
    let border = if selected { p.accent } else { p.border };
    let text = if selected { p.accent } else { p.text_secondary };

    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = fill;
        w.inactive.bg_fill = fill;
        w.inactive.bg_stroke = Stroke::new(1.0, border);
        w.inactive.fg_stroke = Stroke::new(1.0, text);
        w.hovered.weak_bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_stroke = Stroke::new(1.0, border);
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.accent);

        ui.add(
            egui::Button::new(egui::RichText::new(label).size(11.0).color(text))
                .frame(true)
                .min_size(egui::vec2(30.0, 24.0)),
        )
    })
    .inner
}

/// Source path used to derive a default output name for a segment list.
fn real_output_stem(
    project: &cf_core::Project,
    segments: &[cf_core::media::export::Segment],
) -> std::path::PathBuf {
    segments
        .first()
        .and_then(|segment| project.source(segment.source_id))
        .map(|source| source.path().clone())
        .unwrap_or_else(|| std::path::PathBuf::from("output.mp4"))
}

/// `clip.mp4` next to the source becomes `clip-edited.mp4`.
fn default_export_path(source: &std::path::Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_owned());
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_owned());

    let name = format!("{stem}-edited.{ext}");
    source
        .parent()
        .map(|parent| parent.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn load_brand_textures(
    ctx: &Context,
) -> (
    Option<TextureHandle>,
    Option<TextureHandle>,
    Option<TextureHandle>,
) {
    (
        // The 3D mark carries its own dark tile, so it works unchanged on a
        // light or dark dialog. The old light/dark pair is kept in the repo but
        // no longer loaded, otherwise About would show a different logo than
        // the taskbar and Explorer.
        load_png(
            ctx,
            "clipforge_logo_light",
            include_bytes!("../../../assets/icons/logo-3d-256.png"),
        ),
        load_png(
            ctx,
            "clipforge_logo_dark",
            include_bytes!("../../../assets/icons/logo-3d-256.png"),
        ),
        load_png(
            ctx,
            "clipforge_mark",
            include_bytes!("../../../assets/icons/mark-neutral-64.png"),
        ),
    )
}

fn load_png(ctx: &Context, name: &str, bytes: &[u8]) -> Option<TextureHandle> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}
