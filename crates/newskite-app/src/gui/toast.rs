//! Toasts: a short card in the lower right when a job finishes or fails, so the outcome
//! is seen without watching the History tab. "Show" opens the job in the inspector.
//! Completions leave after [`OK_SECS`], failures stay for [`FAILED_SECS`]; at most
//! [`MAX_SHOWN`] are on screen (the oldest go first).

use eframe::egui::{self, Align, Align2, CornerRadius, Layout, Pos2, RichText, Sense, Stroke, Vec2};

use super::app::Action;
use super::fonts;
use super::icons::{paint_icon, Icon};
use super::theme::Palette;
use super::widgets::icon_button;

/// Seconds a completion toast stays.
pub const OK_SECS: f64 = 6.0;
/// Seconds a failure toast stays.
pub const FAILED_SECS: f64 = 15.0;
/// Toasts on screen at once.
pub const MAX_SHOWN: usize = 3;
const WIDTH: f32 = 320.0;
/// Seconds a toast takes to slide in, and to fade out at the end of its life.
const ENTER_SECS: f64 = 0.22;
const EXIT_SECS: f64 = 0.3;

/// One announcement.
#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub job_id: i64,
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// egui time it was first drawn (set on its first frame).
    pub shown_at: Option<f64>,
}

impl Toast {
    fn life(&self) -> f64 {
        if self.ok { OK_SECS } else { FAILED_SECS }
    }

    /// Whether it should leave at egui time `now`.
    pub fn expired(&self, now: f64) -> bool {
        self.shown_at.is_some_and(|t| now - t >= self.life())
    }
}

/// Drops expired toasts and keeps the newest [`MAX_SHOWN`]. Pure, for tests.
pub fn prune(toasts: &mut Vec<Toast>, now: f64) {
    toasts.retain(|t| !t.expired(now));
    let extra = toasts.len().saturating_sub(MAX_SHOWN);
    toasts.drain(..extra);
}

/// Draws the toasts above the status bar; `bottom` is the y they stack up from.
pub fn show(ctx: &egui::Context, pal: &Palette, toasts: &mut Vec<Toast>, bottom: f32, reduced: bool, actions: &mut Vec<Action>) {
    let (now, dt) = ctx.input(|i| (i.time, f64::from(i.stable_dt).min(0.1)));
    for t in toasts.iter_mut() {
        t.shown_at.get_or_insert(now);
        // A toast under the pointer waits: its clock stops while it is being read.
        if ctx.data(|d| d.get_temp::<bool>(hover_id(t.id))).unwrap_or(false) {
            if let Some(s) = t.shown_at.as_mut() {
                *s += dt;
            }
        }
    }
    prune(toasts, now);
    if toasts.is_empty() {
        return;
    }
    if reduced {
        // Wake up for the next expiry only.
        let next = toasts.iter().filter_map(|t| t.shown_at.map(|s| s + t.life() - now)).fold(f64::MAX, f64::min);
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(next.clamp(0.05, 60.0)));
    } else {
        // The countdown line moves continuously.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
    let right = ctx.content_rect().right() - 12.0;
    let mut y = bottom - 12.0;
    for t in toasts.iter().rev() {
        let shown_at = t.shown_at.unwrap_or(now);
        let age = now - shown_at;
        let left = (shown_at + t.life() - now).max(0.0);
        let (enter, exit) = if reduced {
            (1.0, 1.0)
        } else {
            (
                super::motion::ease_out((age / ENTER_SECS).clamp(0.0, 1.0) as f32),
                (left / EXIT_SECS).clamp(0.0, 1.0) as f32,
            )
        };
        let slide = (1.0 - enter) * 28.0 + (1.0 - exit) * 16.0;
        let color = if t.ok { pal.positive } else { pal.negative };
        let area = egui::Area::new(egui::Id::new(("toast", t.id)))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .fixed_pos(Pos2::new(right + slide, y))
            .interactable(true);
        let resp = area.show(ctx, |ui| {
            ui.set_opacity(enter.min(exit));
            egui::Frame::new()
                .fill(pal.raised)
                .stroke(Stroke::new(1.0, pal.border_strong))
                .corner_radius(CornerRadius::same(10))
                .shadow(egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: pal.shadow })
                .inner_margin(egui::Margin { left: 14, right: 8, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(WIDTH);
                    ui.horizontal_top(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        paint_icon(ui.painter(), r, if t.ok { Icon::StatusOk } else { Icon::Alert }, color);
                        ui.vertical(|ui| {
                            ui.set_width(WIDTH - 86.0);
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(
                                RichText::new(if t.ok { "Download complete" } else { "Download failed" })
                                    .font(fonts::semibold_id(12.5))
                                    .color(pal.text),
                            );
                            ui.add(egui::Label::new(RichText::new(&t.name).small().color(pal.text)).truncate());
                            ui.add(
                                egui::Label::new(RichText::new(&t.detail).small().color(if t.ok { pal.muted } else { pal.negative }))
                                    .truncate(),
                            );
                        });
                        ui.with_layout(Layout::top_down(Align::Max), |ui| {
                            if icon_button(ui, pal, Icon::Close, pal.muted, "Dismiss").clicked() {
                                actions.push(Action::DismissToast(t.id));
                            }
                            let show = ui
                                .add(egui::Label::new(RichText::new("Show").small().color(pal.accent_fg)).extend().sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text("Open this job in the inspector");
                            if show.clicked() {
                                actions.push(Action::ShowJob(t.job_id));
                                actions.push(Action::DismissToast(t.id));
                            }
                        });
                    });
                })
                .response
        });
        let rect = resp.response.rect;
        let hovered = ctx.rect_contains_pointer(resp.response.layer_id, rect);
        ctx.data_mut(|d| d.insert_temp(hover_id(t.id), hovered));
        let painter = ctx.layer_painter(resp.response.layer_id);
        let fade = enter.min(exit);
        // The status rule down the left edge, like a notice.
        painter.line_segment(
            [Pos2::new(rect.left() + 1.0, rect.top() + 6.0), Pos2::new(rect.left() + 1.0, rect.bottom() - 6.0)],
            Stroke::new(2.0, Palette::fade(color, fade)),
        );
        if !reduced {
            // Time left, as a hairline shrinking along the bottom edge.
            let f = (left / t.life()).clamp(0.0, 1.0) as f32;
            let x0 = rect.left() + 10.0;
            let x1 = x0 + (rect.width() - 20.0) * f;
            painter.line_segment(
                [Pos2::new(x0, rect.bottom() - 2.5), Pos2::new(x1, rect.bottom() - 2.5)],
                Stroke::new(1.5, Palette::fade(color, 0.55 * fade)),
            );
        }
        y = rect.top() - 10.0;
    }
}

/// Temp-data key: whether the pointer was over toast `id` last frame.
fn hover_id(id: u64) -> egui::Id {
    egui::Id::new(("toast-hover", id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(id: u64, ok: bool, at: Option<f64>) -> Toast {
        Toast { id, job_id: id as i64, name: format!("j{id}"), ok, detail: String::new(), shown_at: at }
    }

    #[test]
    fn failures_stay_longer_and_only_three_show() {
        let mut v = vec![toast(1, true, Some(0.0)), toast(2, false, Some(0.0)), toast(3, true, None)];
        prune(&mut v, OK_SECS + 0.1);
        assert_eq!(v.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2, 3], "unshown toasts wait");
        let mut v: Vec<Toast> = (1..=5).map(|i| toast(i, true, Some(10.0))).collect();
        prune(&mut v, 10.0);
        assert_eq!(v.iter().map(|t| t.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    }
}
