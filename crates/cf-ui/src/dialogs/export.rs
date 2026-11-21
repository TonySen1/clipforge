//! Export dialog: pick a destination, see the real cut points, then watch or
//! cancel the job.
//!
//! The drift warning is the important part of this dialog. A lossless cut can
//! only land on keyframes, so the UI shows what will *actually* be cut before
//! the user commits, instead of silently producing a different clip.

use crate::i18n::tr;
use crate::theme::{Metrics, Palette};
use crate::widgets::controls::{primary_button, subtle_button};
use cf_core::media::export::{
    format_drift, CompatReport, EncoderSpeed, ExportBlocker, ExportMode, LosslessPlan, PrecisePlan,
};
use cf_core::time::Time;
use egui::{
    load::SizedTexture, Align, Color32, Context, Frame, Id, Image, Margin, Modal, RichText, Stroke,
    TextureHandle, Ui,
};

/// What the dialog asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ExportAction {
    None,
    Close,
    Browse,
    Start,
    Cancel,
    /// Switch between lossless and precise.
    SetMode(ExportMode),
    /// Switch to precise because lossless was blocked by incompatible clips.
    SwitchToPrecise,
    /// The user finished editing the output path text.
    OutputEdited,
    /// The user pressed the format shortcut next to the path.
    SetContainer(crate::dialogs::export::ContainerChoice),
    /// The user picked how much effort the encoder spends per frame.
    SetSpeed(EncoderSpeed),
}

/// Container picked from the extension buttons under the path field.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContainerChoice {
    Mp4,
    Mkv,
    Mov,
    Webm,
}

impl ContainerChoice {
    pub const ALL: [Self; 4] = [Self::Mp4, Self::Mkv, Self::Mov, Self::Webm];

    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mov => "mov",
            Self::Webm => "webm",
        }
    }

    /// True when `path` already ends with this container's extension.
    pub fn matches(self, path: &std::path::Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case(self.label()))
            .unwrap_or(false)
    }

    /// Replaces the extension of `path`, keeping the stem.
    pub fn apply(self, path: &std::path::Path) -> std::path::PathBuf {
        let mut out = path.to_path_buf();
        out.set_extension(self.label());
        out
    }
}

/// Everything the dialog renders.
pub struct ExportDialogState<'a> {
    pub mode: ExportMode,
    pub plan: Option<&'a LosslessPlan>,
    pub precise: Option<&'a PrecisePlan>,
    pub blocker: Option<ExportBlocker>,
    /// Set when lossless is blocked by incompatible segments; the dialog offers
    /// a one-click switch to precise mode.
    pub compat: Option<&'a CompatReport>,
    pub output: Option<&'a std::path::Path>,
    /// Editable text for the output path. Separate from `output` because the
    /// user can type an invalid path; the app re-parses it after editing.
    pub output_text: &'a mut String,
    /// Present while a job runs.
    pub running: Option<RunningState>,
    /// Set after a failure so the dialog can show why.
    pub error: Option<&'a str>,
    /// Last lines of FFmpeg output, shown in a collapsible section.
    pub log_tail: Option<&'a str>,
}

#[derive(Clone, Debug, Default)]
pub struct RunningState {
    pub percent: f32,
    pub done: Time,
    pub total: Time,
    pub speed: Option<u32>,
    pub cancelling: bool,
}

pub fn show(
    ctx: &Context,
    p: &Palette,
    logo: Option<&TextureHandle>,
    state: &mut ExportDialogState<'_>,
    version: &str,
) -> ExportAction {
    let mut action = ExportAction::None;

    let frame = Frame::new()
        .fill(p.bg_elevated)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(Metrics::RADIUS_LG_C)
        .inner_margin(Margin::same(22))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 28,
            spread: 0,
            color: p.shadow,
        });

    let response = Modal::new(Id::unique("clipforge_export"))
        .frame(frame)
        .backdrop_color(super::about::backdrop_color(p))
        .show(ctx, |ui| {
            let running = state.running.clone();

            // While a job runs the dialog only needs to show progress, so it
            // drops to a narrow card instead of a mostly-empty settings page.
            ui.set_width(if running.is_some() { 380.0 } else { 460.0 });

            ui.horizontal(|ui| {
                if let Some(tex) = logo {
                    let size = if running.is_some() {
                        egui::vec2(22.0, 22.0)
                    } else {
                        egui::vec2(30.0, 30.0)
                    };
                    ui.add(Image::from_texture(SizedTexture::new(tex.id(), size)));
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if running.is_some() {
                        ui.label(
                            RichText::new(
                                if state.running.as_ref().is_some_and(|r| r.cancelling) {
                                    tr("export.cancelling").to_string()
                                } else {
                                    tr("export.exporting").to_string()
                                },
                            )
                            .size(14.0)
                            .strong()
                            .color(p.text_primary),
                        );
                    } else {
                        ui.label(
                            RichText::new(tr("export.title"))
                                .size(18.0)
                                .strong()
                                .color(p.text_primary),
                        );
                        ui.label(
                            RichText::new(format!("{} {version}", tr("about.version")))
                                .size(10.5)
                                .color(p.text_muted),
                        );
                    }
                });
            });

            ui.add_space(if running.is_some() { 10.0 } else { 14.0 });
            if running.is_none() {
                divider(ui, p);
                ui.add_space(12.0);
            }

            if let Some(running) = &running {
                running_body(ui, p, running, &mut action);
            } else {
                setup_body(ui, p, state, &mut action);
            }

            ui.add_space(if running.is_some() { 10.0 } else { 16.0 });
            if running.is_none() {
                divider(ui, p);
                ui.add_space(10.0);
            }

            footer(ui, p, state, &mut action);
        });

    if response.should_close() && state.running.is_none() {
        action = ExportAction::Close;
    }

    action
}

fn setup_body(
    ui: &mut Ui,
    p: &Palette,
    state: &mut ExportDialogState<'_>,
    action: &mut ExportAction,
) {
    // --- destination ----------------------------------------------------
    ui.label(
        RichText::new(tr("export.output"))
            .size(11.0)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(4.0);

    // Editable path: users often want to rename the file or retarget the
    // folder without opening a native dialog.
    let mut edited = false;
    ui.horizontal(|ui| {
        let response = ui.add_sized(
            [320.0, 26.0],
            egui::TextEdit::singleline(state.output_text)
                .font(egui::TextStyle::Monospace)
                .text_color(p.text_secondary)
                .hint_text(tr("export.no_output"))
                .margin(Margin::symmetric(8, 5)),
        );

        // Commit on blur or Enter so every keystroke does not re-plan.
        let committed = response.lost_focus()
            || (response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if committed {
            edited = true;
        }

        if subtle_button(ui, p, tr("export.browse")).clicked() {
            *action = ExportAction::Browse;
        }
    });

    if edited {
        *action = ExportAction::OutputEdited;
    }

    // Container shortcuts: rewriting the extension is the common case, and
    // picking it here also keeps the encoder choice in sync.
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("export.container"))
                .size(10.5)
                .color(p.text_muted),
        );
        for choice in ContainerChoice::ALL {
            let selected = state
                .output
                .map(|path| choice.matches(path))
                .unwrap_or(false);
            if container_chip(ui, p, selected, choice.label()).clicked() && !selected {
                *action = ExportAction::SetContainer(choice);
            }
        }
    });

    ui.add_space(14.0);

    // --- mode -----------------------------------------------------------
    ui.label(
        RichText::new(tr("export.mode"))
            .size(11.0)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        for mode in ExportMode::ALL {
            let selected = state.mode == mode;
            if mode_chip(ui, p, selected, &tr(mode.i18n_key())).clicked() && !selected {
                *action = ExportAction::SetMode(mode);
            }
        }
    });

    ui.add_space(6.0);
    Frame::new()
        .fill(p.accent_soft)
        .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.5)))
        .corner_radius(Metrics::RADIUS_SM_C)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(tr(state.mode.hint_key()))
                    .size(10.5)
                    .color(p.text_secondary),
            );
        });

    ui.add_space(14.0);

    // --- plan details ---------------------------------------------------
    match state.mode {
        ExportMode::Lossless => match state.plan {
            Some(plan) => plan_body(ui, p, plan),
            None => {
                ui.label(
                    RichText::new(tr("export.no_selection"))
                        .size(12.0)
                        .color(p.text_muted),
                );
            }
        },
        ExportMode::Precise => match state.precise {
            Some(plan) => precise_body(ui, p, plan, action),
            None => {
                ui.label(
                    RichText::new(tr("export.no_selection"))
                        .size(12.0)
                        .color(p.text_muted),
                );
            }
        },
    }

    if let Some(blocker) = &state.blocker {
        ui.add_space(10.0);
        Frame::new()
            .fill(p.bg_sunken)
            .stroke(Stroke::new(1.0, p.error))
            .corner_radius(Metrics::RADIUS_SM_C)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(tr("export.cannot_export"))
                        .size(11.5)
                        .strong()
                        .color(p.error),
                );
                ui.label(
                    RichText::new(tr(blocker.i18n_key()))
                        .size(11.0)
                        .color(p.text_secondary),
                );

                let details = blocker.details();
                if !details.is_empty() {
                    ui.add_space(4.0);
                    for line in details.iter().take(6) {
                        ui.label(
                            RichText::new(line)
                                .size(10.0)
                                .monospace()
                                .color(p.text_muted),
                        );
                    }
                    if details.len() > 6 {
                        ui.label(
                            RichText::new(format!("+{}", details.len() - 6))
                                .size(10.0)
                                .color(p.text_muted),
                        );
                    }
                }

                // The incompatibility card is the one place where a different
                // mode is a genuine fix, so offer it inline.
                if matches!(blocker, ExportBlocker::IncompatibleSegments(_))
                    && state.mode == ExportMode::Lossless
                {
                    ui.add_space(8.0);
                    if subtle_button(ui, p, tr("export.switch_precise")).clicked() {
                        *action = ExportAction::SwitchToPrecise;
                    }
                }

                if matches!(blocker, ExportBlocker::TimelineHasGaps)
                    && state.mode == ExportMode::Lossless
                {
                    ui.add_space(8.0);
                    if subtle_button(ui, p, tr("export.switch_precise")).clicked() {
                        *action = ExportAction::SwitchToPrecise;
                    }
                }
            });
    }

    if let Some(error) = state.error {
        ui.add_space(10.0);
        Frame::new()
            .fill(p.bg_sunken)
            .stroke(Stroke::new(1.0, p.error))
            .corner_radius(Metrics::RADIUS_SM_C)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(tr("export.failed"))
                        .size(11.5)
                        .strong()
                        .color(p.error),
                );
                ui.label(
                    RichText::new(error)
                        .size(10.5)
                        .monospace()
                        .color(p.text_secondary),
                );
            });
    }

    if let Some(log) = state.log_tail {
        if !log.is_empty() {
            ui.add_space(8.0);
            egui::CollapsingHeader::new(
                RichText::new(tr("export.details"))
                    .size(11.0)
                    .color(p.text_muted),
            )
            .id_salt("export_log")
            .show(ui, |ui| {
                Frame::new()
                    .fill(p.bg_sunken)
                    .corner_radius(Metrics::RADIUS_XS_C)
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("export_log_scroll")
                            .max_height(110.0)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(log)
                                        .size(10.0)
                                        .monospace()
                                        .color(p.text_muted),
                                );
                            });
                    });
            });
        }
    }
}

fn plan_body(ui: &mut Ui, p: &Palette, plan: &LosslessPlan) {
    ui.label(
        RichText::new(tr("export.cut_points"))
            .size(11.0)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(4.0);

    kv(ui, p, &tr("export.real_in"), &plan.real_start.to_string());
    kv(ui, p, &tr("export.real_out"), &plan.real_end.to_string());
    kv(
        ui,
        p,
        &tr("export.real_duration"),
        &plan.real_duration.to_string(),
    );

    let drifted = plan.start_drift.micros() != 0 || plan.end_drift.micros() != 0;
    if drifted {
        ui.add_space(8.0);
        Frame::new()
            .fill(p.bg_sunken)
            .stroke(Stroke::new(1.0, p.warning.gamma_multiply(0.7)))
            .corner_radius(Metrics::RADIUS_SM_C)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(tr("export.drift_warning"))
                        .size(11.0)
                        .strong()
                        .color(p.warning),
                );
                ui.label(
                    RichText::new(format!(
                        "{} {}   {} {}",
                        tr("export.drift_in"),
                        format_drift(plan.start_drift),
                        tr("export.drift_out"),
                        format_drift(plan.end_drift),
                    ))
                    .size(10.5)
                    .monospace()
                    .color(p.text_secondary),
                );
            });
    }
}

/// Small button used for the container shortcuts under the path field.
fn container_chip(ui: &mut Ui, p: &Palette, selected: bool, label: &str) -> egui::Response {
    let fill = if selected {
        p.accent_soft
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected {
        Stroke::new(1.0, p.accent)
    } else {
        Stroke::new(1.0, p.border)
    };
    let text = if selected { p.accent } else { p.text_muted };

    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = fill;
        w.inactive.bg_fill = fill;
        w.inactive.bg_stroke = stroke;
        w.inactive.fg_stroke = Stroke::new(1.0, text);
        w.hovered.weak_bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_stroke = stroke;
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_secondary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.accent);

        ui.add(
            egui::Button::new(RichText::new(label).size(10.5).monospace().color(text))
                .frame(true)
                .min_size(egui::vec2(38.0, 20.0)),
        )
    })
    .inner
}

fn mode_chip(ui: &mut Ui, p: &Palette, selected: bool, label: &str) -> egui::Response {
    let fill = if selected {
        p.accent_soft
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected {
        Stroke::new(1.0, p.accent)
    } else {
        Stroke::new(1.0, p.border)
    };
    let text = if selected { p.accent } else { p.text_secondary };

    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = fill;
        w.inactive.bg_fill = fill;
        w.inactive.bg_stroke = stroke;
        w.inactive.fg_stroke = Stroke::new(1.0, text);
        w.hovered.weak_bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_fill = if selected { fill } else { p.bg_hover };
        w.hovered.bg_stroke = stroke;
        w.hovered.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.active.weak_bg_fill = p.accent_soft;
        w.active.bg_fill = p.accent_soft;
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.accent);

        ui.add(
            egui::Button::new(RichText::new(label).size(12.0))
                .frame(true)
                .min_size(egui::vec2(0.0, 30.0)),
        )
    })
    .inner
}

fn precise_body(ui: &mut Ui, p: &Palette, plan: &PrecisePlan, action: &mut ExportAction) {
    ui.label(
        RichText::new(tr("export.precise_summary"))
            .size(11.0)
            .strong()
            .color(p.text_muted),
    );
    ui.add_space(4.0);

    kv(
        ui,
        p,
        &tr("export.real_duration"),
        &plan.duration.to_string(),
    );
    kv(
        ui,
        p,
        &tr("media.resolution"),
        &format!("{}×{}", plan.width, plan.height),
    );
    kv(
        ui,
        p,
        &tr("media.frame_rate"),
        &format!("{} fps", trim_fps(plan.frame_rate.fps())),
    );
    kv(
        ui,
        p,
        &tr("export.segments"),
        &plan.pieces.len().to_string(),
    );

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("export.speed_label"))
                .size(10.5)
                .color(p.text_muted),
        );
        for speed in EncoderSpeed::ALL {
            let selected = plan.settings.speed == speed;
            if container_chip(ui, p, selected, &tr(speed.i18n_key())).clicked() && !selected {
                *action = ExportAction::SetSpeed(speed);
            }
        }
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new(tr(plan.settings.speed.hint_key()))
            .size(10.0)
            .color(p.text_muted),
    );

    let gaps = plan.pieces.iter().filter(|piece| piece.is_gap()).count();
    if gaps > 0 {
        ui.add_space(6.0);
        Frame::new()
            .fill(p.bg_sunken)
            .stroke(Stroke::new(1.0, p.warning.gamma_multiply(0.7)))
            .corner_radius(Metrics::RADIUS_SM_C)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(tr("export.gaps_filled"))
                        .size(10.5)
                        .color(p.warning),
                );
            });
    }
}

fn trim_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.3}")
    }
}

fn running_body(ui: &mut Ui, p: &Palette, running: &RunningState, action: &mut ExportAction) {
    crate::widgets::progress::striped_progress(ui, p, running.percent / 100.0, 14.0);

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} / {}", running.done, running.total))
                .size(11.0)
                .monospace()
                .color(p.text_secondary),
        );

        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if let Some(speed) = running.speed {
                ui.label(
                    RichText::new(format!("{speed}x"))
                        .size(11.0)
                        .monospace()
                        .color(p.text_muted),
                );
                ui.add_space(8.0);
            }
            ui.label(
                RichText::new(format!("{:.0}%", running.percent))
                    .size(11.0)
                    .monospace()
                    .color(p.accent),
            );
        });
    });

    if !running.cancelling {
        ui.add_space(12.0);
        // Cancel sits on the same row as Hide: two stacked buttons would add
        // a band of empty space to an otherwise single-purpose dialog.
        ui.horizontal(|ui| {
            if subtle_button(ui, p, tr("export.cancel")).clicked() {
                *action = ExportAction::Cancel;
            }
        });
    }
}

fn footer(ui: &mut Ui, p: &Palette, state: &ExportDialogState<'_>, action: &mut ExportAction) {
    let running = state.running.is_some();
    // A zero-height row keeps the buttons at their natural height. A plain
    // `ui.with_layout` would take all the space left in the parent, which
    // matters after switching from the tall setup page to the short running
    // page: the modal would keep the old height and show a large blank gap.
    let row = egui::vec2(ui.available_width(), 0.0);
    ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(Align::Center), |ui| {
        if !running {
            // Each mode has its own plan; the lossless plan is absent in
            // precise mode, so checking only `plan` would keep the button
            // disabled forever.
            let has_plan = match state.mode {
                ExportMode::Lossless => state.plan.is_some(),
                ExportMode::Precise => state.precise.is_some(),
            };
            let can_start = has_plan && state.blocker.is_none() && state.output.is_some();
            ui.add_enabled_ui(can_start, |ui| {
                if primary_button(ui, p, tr("export.start")).clicked() {
                    *action = ExportAction::Start;
                }
            });
        }

        let close_label = if running {
            tr("export.hide")
        } else {
            tr("export.close")
        };
        if subtle_button(ui, p, close_label).clicked() {
            *action = ExportAction::Close;
        }
    });
}

fn kv(ui: &mut Ui, p: &Palette, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [118.0, 18.0],
            egui::Label::new(RichText::new(label).size(11.0).color(p.text_muted)),
        );
        ui.label(
            RichText::new(value)
                .size(11.0)
                .monospace()
                .color(p.text_secondary),
        );
    });
    ui.add_space(1.0);
}

fn divider(ui: &mut Ui, p: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.divider);
}
