//! Full-window overlay shown while files hover over the application.

use crate::theme::{Metrics, Palette};
use egui::{pos2, Align2, Context, FontId, Id, LayerId, Order, Stroke, StrokeKind};

pub fn drop_overlay(ctx: &Context, p: &Palette, title: &str, subtitle: &str) {
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(LayerId::new(
        Order::Foreground,
        Id::unique("clipforge_drop_overlay"),
    ));

    painter.rect_filled(screen, 0.0, p.scrim_overlay);

    let inner = screen.shrink(26.0);
    painter.rect(
        inner,
        Metrics::RADIUS_XL,
        p.accent_soft,
        Stroke::new(2.0, p.accent),
        StrokeKind::Inside,
    );

    let center = screen.center();
    painter.text(
        pos2(center.x, center.y - 16.0),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(22.0),
        p.text_primary,
    );
    painter.text(
        pos2(center.x, center.y + 18.0),
        Align2::CENTER_CENTER,
        subtitle,
        FontId::proportional(13.0),
        p.text_secondary,
    );
}
