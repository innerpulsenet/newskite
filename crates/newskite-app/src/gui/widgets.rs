//! Small reusable widgets built on the innerpulse components: toolbar and icon buttons,
//! the kit's buttons (one filled call to action per view, outline otherwise), status
//! chips with a glyph and words, notices, segment tracks, the article grid, cards,
//! eyebrows and the quiet credit. All colors come from the [`Palette`].

use eframe::egui::{
    self, text::LayoutJob, Align, Color32, CornerRadius, FontId, Pos2, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, TextFormat, Ui, Vec2,
};
use newskite_service::JobState;

use super::brand;
use super::fonts;
use super::metal::{self, KeyState, LampShape};
use super::theme::{Metal, Palette};

pub use super::icons::{paint_icon, Icon};

/// Letter spacing of mono eyebrows and readouts (≈ 0.12em at 11 px).
pub const EYEBROW_TRACKING: f32 = 1.2;

/// A single-line layout job with letter spacing (egui has no CSS `letter-spacing`).
pub fn spaced(text: &str, font: FontId, color: Color32, tracking: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font,
            color,
            extra_letter_spacing: tracking,
            ..Default::default()
        },
    );
    job.wrap.max_rows = 1;
    job.halign = Align::LEFT;
    job
}

/// How a toolbar button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// A working action.
    Normal,
    /// The action reflects the current view (e.g. the Queue tab is showing).
    Active,
    /// Visible for layout parity but not implemented yet; clicking is harmless.
    Unavailable,
    /// The header's queue control (Start / Pause / Resume): an accent outline, icon and
    /// label with no resting fill, so it reads first among the tools while the view's one
    /// filled button stays the ember one.
    Accent,
}

/// Paints the ember focus ring (the one place ember borders a control).
pub fn focus_ring(ui: &Ui, pal: &Palette, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius.saturating_add(2)),
        Stroke::new(2.0, pal.focus),
        StrokeKind::Inside,
    );
}

/// The platform's command key as shortcuts name it (`Ctrl+O`, `Cmd+O`).
pub const CMD_KEY: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

/// Seconds a copy button keeps saying "Copied".
const COPIED_SECS: f64 = 1.5;

/// A text button that copies `text()` to the clipboard and says "Copied" for a moment,
/// so the click is seen to have worked. `salt` tells the copy buttons on screen apart.
pub fn copy_button(ui: &mut Ui, pal: &Palette, salt: &str, label: &str, enabled: bool, text: impl FnOnce() -> String) -> Response {
    let id = egui::Id::new(("copied-at", salt));
    let now = ui.input(|i| i.time);
    let copied = ui.data(|d| d.get_temp::<f64>(id)).is_some_and(|at| now - at < COPIED_SECS);
    let resp = kit_button(ui, pal, ButtonKind::Text, None, if copied { "Copied" } else { label }, enabled);
    if resp.clicked() {
        ui.ctx().copy_text(text());
        ui.data_mut(|d| d.insert_temp(id, now));
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(COPIED_SECS));
    }
    resp
}

/// Height of a toolbar button on the flat themes: the kit button's, so the queue control
/// and Add NZB stand the same height side by side.
pub const TOOL_H: f32 = 30.0;
/// Height of a toolbar key on the material themes.
const TOOL_KEY_H: f32 = 30.0;
/// Letter spacing of a key's uppercase mono label.
const KEY_TRACKING: f32 = 0.5;

/// The font of a key's label on the material themes: the measuring mono, 600.
fn key_font(size: f32) -> FontId {
    FontId::new(size, fonts::mono_semibold())
}

/// How the key under `resp` stands this frame, its hover eased.
fn key_state(ui: &Ui, resp: &Response, enabled: bool, on: f32, color: Color32) -> KeyState {
    KeyState {
        hover: ui.ctx().animate_bool(resp.id.with("hover"), enabled && resp.hovered()),
        on,
        press: if enabled && resp.is_pointer_button_down_on() { 1.0 } else { 0.0 },
        color,
    }
}

/// A toolbar button as a key of smoked glass: the current view's key is lit, the queue
/// control glows faintly so it reads first, an unavailable one is dark.
fn tool_key(ui: &mut Ui, pal: &Palette, m: &Metal, icon: Icon, label: &str, show_label: bool, state: ToolState) -> Response {
    let galley = show_label
        .then(|| ui.painter().layout_job(spaced(&label.to_uppercase(), key_font(11.0), pal.text, KEY_TRACKING)));
    let size = match &galley {
        Some(g) => Vec2::new(11.0 + 16.0 + 7.0 + g.size().x + 12.0, TOOL_KEY_H),
        None => Vec2::splat(TOOL_KEY_H),
    };
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let live = state != ToolState::Unavailable;
        let on = match state {
            ToolState::Active => 1.0,
            ToolState::Accent => 0.4,
            _ => 0.0,
        };
        let k = key_state(ui, &resp, live, on, pal.accent);
        let painter = ui.painter();
        metal::key_body(painter, m, rect, 4, k);
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 4);
        }
        let fg = match state {
            ToolState::Accent => pal.accent_fg,
            _ => metal::key_text(pal, k, live),
        };
        let drop = Vec2::new(0.0, k.press);
        let icon_center = match &galley {
            Some(_) => Pos2::new(rect.left() + 11.0 + 8.0, rect.center().y),
            None => rect.center(),
        };
        paint_icon(painter, Rect::from_center_size(icon_center + drop, Vec2::splat(16.0)), icon, fg);
        if let Some(g) = galley {
            painter.galley_with_override_text_color(
                Pos2::new(rect.left() + 11.0 + 16.0 + 7.0, rect.center().y - g.size().y / 2.0) + drop,
                g,
                fg,
            );
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, state != ToolState::Unavailable, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A toolbar button: an 18 px icon and its label side by side in a rounded slot (the
/// icon alone on narrow windows). Icons are text-2 at rest, text when hovered or
/// current, text-3 when unavailable; never ember. The hover wash fades in and out.
pub fn tool_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    show_label: bool,
    state: ToolState,
) -> Response {
    if let Some(m) = pal.metal {
        return tool_key(ui, pal, &m, icon, label, show_label, state);
    }
    // Set like the kit button beside it: uppercase semibold 12 px, 0.06em tracking, a
    // 16 px icon and the kit's corner (the accessible label keeps the sentence case).
    // The label keeps the text color beside the accent outline and icon; an unavailable
    // control's dims with its icon.
    let label_color = if state == ToolState::Unavailable { pal.faint } else { pal.text };
    let galley = show_label.then(|| ui.painter().layout_job(spaced(&label.to_uppercase(), fonts::semibold_id(12.0), label_color, 0.72)));
    let size = match &galley {
        Some(g) => Vec2::new(14.0 + 16.0 + 7.0 + g.size().x + 14.0, TOOL_H),
        None => Vec2::splat(TOOL_H),
    };
    // An unavailable button is not a click target: no hand cursor, no keyboard stop.
    let sense = if state == ToolState::Unavailable { Sense::hover() } else { Sense::click() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let live = state != ToolState::Unavailable;
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), live && resp.hovered());
        let pressed = live && resp.is_pointer_button_down_on();
        let painter = ui.painter();
        let radius = CornerRadius::same(pal.radius(6));
        match state {
            ToolState::Accent => {
                // The accent wash only under the pointer: at rest the slot is outlined, not
                // filled.
                let fill = if pressed { pal.accent_soft } else { Palette::fade(pal.accent_soft, hover) };
                painter.rect(rect, radius, fill, Stroke::new(1.0, Palette::fade(pal.accent, 0.5)), StrokeKind::Inside);
            }
            _ if pressed => {
                painter.rect_filled(rect, radius, pal.sunken);
            }
            ToolState::Active => {
                painter.rect_filled(rect, radius, pal.hover);
            }
            _ if hover > 0.0 => {
                painter.rect_filled(rect, radius, Palette::fade(pal.hover, hover));
            }
            _ => {}
        }
        if resp.has_focus() {
            focus_ring(ui, pal, rect, pal.radius(6));
        }
        let fg = match state {
            ToolState::Unavailable => pal.faint,
            ToolState::Accent => pal.accent_fg,
            ToolState::Active => pal.text,
            ToolState::Normal => pal.muted.lerp_to_gamma(pal.text, hover),
        };
        let icon_center = match &galley {
            Some(_) => Pos2::new(rect.left() + 14.0 + 8.0, rect.center().y),
            None => rect.center(),
        };
        paint_icon(painter, Rect::from_center_size(icon_center, Vec2::splat(16.0)), icon, fg);
        if let Some(g) = galley {
            painter.galley(
                Pos2::new(rect.left() + 14.0 + 16.0 + 7.0, rect.center().y - g.size().y / 2.0),
                g,
                label_color,
            );
        }
        if state == ToolState::Active {
            // The current view: a short accent rule along the bottom of the slot.
            let r = Rect::from_center_size(Pos2::new(rect.center().x, rect.bottom() - 2.0), Vec2::new(18.0, 2.5));
            painter.rect_filled(r, CornerRadius::same(1), pal.accent);
        }
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            state != ToolState::Unavailable,
            label,
        )
    });
    if state == ToolState::Unavailable {
        return resp;
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Side of an icon button: at least the 24 px minimum target, with a 16 px glyph.
pub const ICON_BUTTON: f32 = 26.0;

/// A small square icon button (row `×`, dock close, etc.). `color` is the resting tint;
/// hover brightens it to the text color.
pub fn icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    color: Color32,
    tooltip: &str,
) -> Response {
    icon_button_enabled(ui, pal, icon, color, tooltip, true)
}

/// [`icon_button`] that can be disabled: then it is faded, has no hover wash or hand
/// cursor, ignores clicks and its tooltip says why.
pub fn icon_button_enabled(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    color: Color32,
    tooltip: &str,
    enabled: bool,
) -> Response {
    if !enabled {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(ICON_BUTTON), Sense::hover());
        if ui.is_rect_visible(rect) {
            paint_icon(ui.painter(), rect.shrink(5.0), icon, Palette::fade(color, 0.4));
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, false, tooltip));
        return resp.on_hover_text(tooltip);
    }
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(ICON_BUTTON), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        if let Some(m) = pal.metal {
            // A key that rises out of the surface under the pointer.
            if hover > 0.0 {
                let mut p = ui.painter().clone();
                p.multiply_opacity(hover);
                let press = if resp.is_pointer_button_down_on() { 1.0 } else { 0.0 };
                metal::key_body(&p, &m, rect.shrink(1.0), 4, KeyState { hover: 1.0, on: 0.0, press, color: pal.accent });
            }
        } else if resp.is_pointer_button_down_on() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(6), pal.sunken);
        } else if hover > 0.0 {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(6), Palette::fade(pal.hover, hover));
        }
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 6);
        }
        let fg = color.lerp_to_gamma(pal.text, hover);
        paint_icon(ui.painter(), rect.shrink(5.0), icon, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    resp.on_hover_text(tooltip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Kinds of kit button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one filled call to action of the view (the action ember, white text).
    Primary,
    /// The quiet outline: everything else.
    Outline,
    /// Bare: dismiss, back, less.
    Text,
    /// The outline in the error color: the "yes" of a destructive confirmation.
    Danger,
}

/// The kit's button: uppercase Instrument Sans 600 at 12 px with 0.06em tracking, an
/// optional 16 px icon before the label, 30 px tall. The accessible label keeps the
/// sentence-case `text`. A disabled primary drops to the outline (no ember on a control
/// that cannot act).
pub fn kit_button(
    ui: &mut Ui,
    pal: &Palette,
    kind: ButtonKind,
    icon: Option<Icon>,
    text: &str,
    enabled: bool,
) -> Response {
    kit_button_sized(ui, pal, kind, icon, text, enabled, false)
}

/// [`kit_button`] at 24 px with 11 px type, for inline confirmations and field-side
/// actions inside table rows and forms.
pub fn compact_button(ui: &mut Ui, pal: &Palette, kind: ButtonKind, text: &str, enabled: bool) -> Response {
    kit_button_sized(ui, pal, kind, None, text, enabled, true)
}

fn kit_button_sized(
    ui: &mut Ui,
    pal: &Palette,
    kind: ButtonKind,
    icon: Option<Icon>,
    text: &str,
    enabled: bool,
    compact: bool,
) -> Response {
    let kind = if kind == ButtonKind::Primary && !enabled {
        ButtonKind::Outline
    } else {
        kind
    };
    // On the material themes every button but the bare one is a key, labeled in the mono.
    let key = pal.metal.filter(|_| kind != ButtonKind::Text);
    let (font, tracking) = match key {
        Some(_) => (key_font(if compact { 10.5 } else { 11.0 }), KEY_TRACKING),
        None => (fonts::semibold_id(if compact { 11.0 } else { 12.0 }), 0.72),
    };
    let probe = ui
        .painter()
        .layout_job(spaced(&text.to_uppercase(), font.clone(), pal.text, tracking));
    let icon_w = if icon.is_some() { 16.0 + 7.0 } else { 0.0 };
    let pad = match (kind, compact) {
        (ButtonKind::Text, true) => 5.0,
        (ButtonKind::Text, false) => 6.0,
        (_, true) => 9.0,
        (_, false) => 14.0,
    };
    let size = Vec2::new(probe.size().x + icon_w + 2.0 * pad, if compact { 24.0 } else { 30.0 });
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let radius = CornerRadius::same(pal.radius(6));
        let (fill, stroke, mut fg) = match kind {
            ButtonKind::Primary => {
                let f = if hovered || pressed {
                    pal.action_hover
                } else {
                    pal.action
                };
                (f, Stroke::new(1.0, f), pal.action_text)
            }
            ButtonKind::Danger => {
                let fill = if pressed {
                    pal.negative_soft
                } else if hovered {
                    Palette::fade(pal.negative, 0.12)
                } else {
                    pal.transparent
                };
                let fg = if enabled { pal.negative } else { Palette::fade(pal.negative, 0.45) };
                (fill, Stroke::new(1.0, Palette::fade(pal.negative, if hovered { 0.9 } else { 0.6 })), fg)
            }
            ButtonKind::Outline => {
                let fill = if pressed { pal.sunken } else { pal.transparent };
                let border = if hovered {
                    pal.border_strong
                } else {
                    pal.border
                };
                let fg = if enabled {
                    pal.text
                } else {
                    Palette::fade(pal.text, 0.45)
                };
                (fill, Stroke::new(1.0, border), fg)
            }
            ButtonKind::Text => {
                let fg = if !enabled {
                    Palette::fade(pal.muted, 0.45)
                } else if hovered {
                    pal.text
                } else {
                    pal.muted
                };
                (pal.transparent, Stroke::NONE, fg)
            }
        };
        let painter = ui.painter();
        let mut drop = 0.0;
        match (key, kind) {
            (Some(_), ButtonKind::Primary) => {
                // The one filled key: ember, standing proud of the plate until pressed.
                let press = if pressed { 1.0 } else { 0.0 };
                painter.rect(rect, radius, fill, Stroke::new(1.0, Metal::shade(215)), StrokeKind::Outside);
                metal::key_face(painter, rect, pal.radius(6), press);
                drop = press;
            }
            (Some(m), _) => {
                let danger = kind == ButtonKind::Danger;
                let k = key_state(ui, &resp, enabled, 0.0, if danger { pal.negative } else { pal.accent });
                // A destructive key warms to its color under the pointer.
                let k = KeyState { on: if danger { 0.5 * k.hover } else { 0.0 }, ..k };
                metal::key_body(painter, &m, rect, pal.radius(6), k);
                if !danger {
                    fg = metal::key_text(pal, k, enabled);
                }
                drop = k.press;
            }
            (None, _) => {
                if kind == ButtonKind::Primary {
                    // The call to action sits a little above the surface.
                    painter.add(egui::epaint::Shadow { offset: [0, 2], blur: 8, spread: 0, color: Palette::fade(pal.shadow, 0.5) }.as_shape(rect, radius));
                }
                painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
            }
        }
        let rect = rect.translate(Vec2::new(0.0, drop));
        if resp.has_focus() {
            focus_ring(ui, pal, rect, pal.radius(6));
        }
        let mut x = rect.left() + pad;
        if let Some(icon) = icon {
            paint_icon(
                painter,
                Rect::from_center_size(Pos2::new(x + 8.0, rect.center().y), Vec2::splat(16.0)),
                icon,
                fg,
            );
            x += icon_w;
        }
        let galley = painter.layout_job(spaced(&text.to_uppercase(), font, fg, tracking));
        painter.galley(
            Pos2::new(x, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
        if kind == ButtonKind::Text && hovered {
            let y = rect.bottom() - 6.0;
            painter.line_segment(
                [
                    Pos2::new(rect.left() + pad, y),
                    Pos2::new(rect.right() - pad, y),
                ],
                Stroke::new(1.0, pal.border_strong),
            );
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Status of a chip or notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Something finished well.
    Ok,
    /// Needs attention.
    Warn,
    /// Failed.
    Err,
    /// Waiting / not started.
    Idle,
    /// Running now (the product accent; ember stays the mark's).
    Live,
}

impl Status {
    /// Glyph, if the status has one (live has a dot instead).
    pub fn glyph(self) -> Option<Icon> {
        match self {
            Status::Ok => Some(Icon::StatusOk),
            Status::Warn => Some(Icon::StatusWarn),
            Status::Err => Some(Icon::StatusErr),
            Status::Idle => Some(Icon::StatusIdle),
            Status::Live => None,
        }
    }

    /// Color of the glyph, words and outline.
    pub fn color(self, pal: &Palette) -> Color32 {
        match self {
            Status::Ok => pal.positive,
            Status::Warn => pal.warning,
            Status::Err => pal.negative,
            Status::Idle => pal.muted,
            Status::Live => pal.accent_fg,
        }
    }
}

/// Font of a status chip's words.
fn chip_font() -> FontId {
    FontId::proportional(12.0)
}

/// Horizontal room a chip needs around its words: padding, glyph and gap.
const CHIP_CHROME: f32 = 10.0 + 11.0 + 6.0 + 11.0;

/// Width a [`status_chip`] with `text` takes.
pub fn status_chip_width(ui: &Ui, text: &str) -> f32 {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), chip_font(), ui.visuals().text_color());
    galley.size().x + CHIP_CHROME
}

/// A status chip: a soft pill tinted with the status color (a faint wash and a hairline
/// of it), its 11 px glyph and the words in that color. Reads without color: the glyph
/// carries the state.
pub fn status_chip(ui: &mut Ui, pal: &Palette, status: Status, text: &str) -> Response {
    status_chip_live(ui, pal, status, text, None)
}

/// [`status_chip`] whose live dot breathes: `breath` is the beat's 0…1 breath while
/// bytes move (`None` rests). Only a `Live` chip has a dot, so other statuses ignore it.
pub fn status_chip_live(
    ui: &mut Ui,
    pal: &Palette,
    status: Status,
    text: &str,
    breath: Option<f32>,
) -> Response {
    let color = status.color(pal);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), chip_font(), color);
    let size = Vec2::new(galley.size().x + CHIP_CHROME, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let wash = if pal.dark { 0.16 } else { 0.11 };
        painter.rect(
            rect,
            CornerRadius::same(pal.radius(11)),
            Palette::fade(color, wash),
            Stroke::new(1.0, Palette::fade(color, 0.34)),
            StrokeKind::Inside,
        );
        let glyph_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 10.0 + 5.5, rect.center().y),
            Vec2::splat(11.0),
        );
        match status.glyph() {
            // With materials the state is a lamp: its shape carries it as the glyph did,
            // and the live lamp breathes.
            _ if pal.metal.is_some() => {
                let (shape, lit) = match status {
                    Status::Ok => (LampShape::Round, 1.0),
                    Status::Warn => (LampShape::Triangle, 1.0),
                    Status::Err => (LampShape::Square, 1.0),
                    Status::Idle => (LampShape::Round, 0.0),
                    Status::Live => (LampShape::Round, breath.map_or(1.0, |b| 0.55 + 0.45 * b.clamp(0.0, 1.0))),
                };
                metal::lamp(painter, pal, glyph_rect.center(), 3.6, color, lit, shape);
            }
            Some(g) => paint_icon(painter, glyph_rect, g, color),
            None => {
                let c = glyph_rect.center();
                match breath {
                    Some(b) => {
                        // A soft ring that grows as the dot dims, then the dot.
                        let b = b.clamp(0.0, 1.0);
                        painter.circle_stroke(
                            c,
                            3.5 + 3.0 * (1.0 - b),
                            Stroke::new(1.0, Palette::fade(color, 0.45 * b)),
                        );
                        painter.circle_filled(c, 3.5, Palette::fade(color, 0.6 + 0.4 * b));
                    }
                    None => {
                        painter.circle_filled(c, 3.5, color);
                    }
                }
            }
        }
        painter.galley(
            Pos2::new(rect.left() + 10.0 + 11.0 + 6.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    resp
}

/// The chip status of a job state: queued idle, downloading live, done ok, failed err,
/// paused warn.
pub fn job_status(state: JobState) -> Status {
    match state {
        JobState::Queued => Status::Idle,
        JobState::Downloading => Status::Live,
        JobState::Done => Status::Ok,
        JobState::Failed => Status::Err,
        JobState::Paused => Status::Warn,
    }
}

/// Job-state chip (`queued`, `downloading`, ...).
pub fn state_badge(ui: &mut Ui, pal: &Palette, state: JobState) -> Response {
    status_chip(ui, pal, job_status(state), state.as_str())
}

/// A notice: a hairline box on surface 1 with a 2 px rule in the status color down its
/// left edge, the glyph in that color and the words in the text color. `lead` is set in
/// the sans 600 before `text`.
pub fn notice(ui: &mut Ui, pal: &Palette, status: Status, lead: &str, text: &str) -> Response {
    let color = status.color(pal);
    let icon = match status {
        Status::Ok => Icon::StatusOk,
        Status::Warn | Status::Err => Icon::Alert,
        Status::Idle | Status::Live => Icon::Info,
    };
    let inner = egui::Frame::new()
        .fill(pal.raised)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius {
            nw: 0,
            sw: 0,
            ne: 4,
            se: 4,
        })
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                paint_icon(ui.painter(), r, icon, color);
                ui.add_space(2.0);
                let mut job = LayoutJob::default();
                let body = FontId::proportional(12.5);
                if !lead.is_empty() {
                    job.append(
                        lead,
                        0.0,
                        TextFormat::simple(fonts::semibold_id(12.5), pal.text),
                    );
                    job.append(" ", 0.0, TextFormat::simple(body.clone(), pal.text));
                }
                job.append(text, 0.0, TextFormat::simple(body, pal.text));
                job.wrap.max_width = ui.available_width();
                ui.add(egui::Label::new(job).wrap());
            });
        });
    let r = inner.response.rect;
    ui.painter().line_segment(
        [
            Pos2::new(r.left() + 1.0, r.top()),
            Pos2::new(r.left() + 1.0, r.bottom()),
        ],
        Stroke::new(2.0, color),
    );
    inner.response
}

/// An error line: the `ip-s-err` glyph and the words, both in the error color. On paper
/// the error red, the action fill and ember text are all deep reds; the glyph is what
/// marks the line as an error (components.md, Fields), so every error line uses this.
pub fn error_line(ui: &mut Ui, pal: &Palette, text: &str) -> Response {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 16.0), Sense::hover());
        paint_icon(
            ui.painter(),
            Rect::from_center_size(r.center(), Vec2::splat(12.0)),
            Icon::StatusErr,
            pal.negative,
        );
        ui.add(egui::Label::new(RichText::new(text).color(pal.negative)).wrap());
    })
    .response
}

/// Height of a segment track.
pub const TRACK_H: f32 = 6.0;
/// Gap between the cells of a segment track.
pub const TRACK_GAP: f32 = 3.0;
/// Preferred width of one cell: short capsules, so a track reads as a calm row of
/// segments rather than a dotted line.
pub const TRACK_CELL_W: f32 = 14.0;
/// Most cells a segment track draws.
pub const TRACK_MAX_CELLS: usize = 24;

/// Number of cells a segment track of `width` draws: as many capsules of about
/// [`TRACK_CELL_W`] as fit, 1 to [`TRACK_MAX_CELLS`].
pub fn track_cells(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return 1;
    }
    (((width + TRACK_GAP) / (TRACK_CELL_W + TRACK_GAP)).floor() as usize).clamp(1, TRACK_MAX_CELLS)
}

/// How full each of `cells` cells is for `fraction` (clamped; NaN is 0): each cell is an
/// equal slice of the job, so the cells fill left to right and the boundary cell fills
/// partly.
pub fn track_fill(fraction: f32, cells: usize) -> Vec<f32> {
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = f * cells as f32;
    (0..cells)
        .map(|i| (filled - i as f32).clamp(0.0, 1.0))
        .collect()
}

/// A segment track: a row of small square cells, each a proportional slice of the job's
/// articles, filled to `fraction` in `color`; empty cells are faint wells.
pub fn segment_track(
    ui: &mut Ui,
    pal: &Palette,
    fraction: f32,
    color: Color32,
    width: f32,
) -> Response {
    segment_track_live(ui, pal, fraction, color, width, None)
}

/// Opacity of filled cell `i` of `n` under a sweep at `phase` (0…1): a soft highlight
/// two cells wide travels left to right across the filled cells once per cycle, the
/// rest sit at 0.78. Pure, for tests.
pub fn sweep_alpha(i: usize, n: usize, phase: f32) -> f32 {
    if n == 0 {
        return 1.0;
    }
    0.78 + 0.22 * sweep_shine(i, n, phase)
}

/// How strongly the sweep lights filled cell `i` of `n` at `phase`: 1 under its head,
/// easing to 0 two cells away. Pure, for tests.
pub fn sweep_shine(i: usize, n: usize, phase: f32) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let head = phase.clamp(0.0, 1.0) * (n as f32 + 2.0) - 1.0;
    let d = (i as f32 - head).abs();
    let glow = (1.0 - d / 2.0).clamp(0.0, 1.0);
    glow * glow * (3.0 - 2.0 * glow)
}

/// [`segment_track`] with a highlight sweeping along the filled cells while the job
/// downloads (`sweep` is the beat phase; `None` rests).
pub fn segment_track_live(
    ui: &mut Ui,
    pal: &Palette,
    fraction: f32,
    color: Color32,
    width: f32,
    sweep: Option<f32>,
) -> Response {
    let width = if width.is_finite() {
        width.max(10.0)
    } else {
        10.0
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, TRACK_H), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_segments(ui.painter(), pal, rect, fraction, color, sweep);
    }
    resp
}

/// Paints a segment track into `rect` (see [`segment_track_live`]): capsule cells, the
/// filled ones in `color` and lit in turn by the sweep, the boundary cell filled partly,
/// the rest quiet wells.
pub fn paint_segments(
    painter: &egui::Painter,
    pal: &Palette,
    rect: Rect,
    fraction: f32,
    color: Color32,
    sweep: Option<f32>,
) {
    let cells = track_cells(rect.width());
    let fill = track_fill(fraction, cells);
    let cell_w = (rect.width() - TRACK_GAP * (cells as f32 - 1.0)) / cells as f32;
    let h = rect.height();
    let r = CornerRadius::same((h / 2.0).floor().clamp(1.0, 255.0) as u8);
    let filled = fill.iter().filter(|a| **a > 0.0).count();
    // The sweep brightens toward the text on dark grounds and toward the paper on light
    // ones, so it always reads as light passing over the cells.
    let shine_to = if pal.dark { pal.text } else { pal.raised };
    for (i, amount) in fill.iter().enumerate() {
        let x = rect.left() + i as f32 * (cell_w + TRACK_GAP);
        let cell = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(cell_w, h));
        let lit = match sweep {
            Some(phase) => color.lerp_to_gamma(shine_to, 0.38 * sweep_shine(i, filled, phase)),
            None => color,
        };
        if *amount >= 1.0 {
            painter.rect_filled(cell, r, lit);
        } else {
            painter.rect_filled(cell, r, pal.well);
            if *amount > 0.0 {
                let mut part = cell;
                part.set_width((cell_w * amount).max(h.min(cell_w)));
                painter.rect_filled(part, r, lit);
            }
        }
    }
}

/// Cells of the empty queue's article grid: `(columns, rows)`.
pub const GRID_SIZE: (usize, usize) = (28, 4);

/// Whether cell `(col, row)` of the article grid is lit. Never: the grid is drawn only
/// while the queue is empty, and there are no articles to show then (a lit cell would
/// read as a count that does not exist).
pub fn grid_lit(_col: usize, _row: usize) -> bool {
    false
}

/// Brightness (0…1) of well `(col, row)` of the article grid under an ambient wave at
/// `phase`: a soft diagonal band drifts across the empty wells. Decorative only: the
/// wells stay wells (no cell is ever lit in the accent, which would read as a count).
pub fn grid_wave(col: usize, row: usize, phase: f32) -> f32 {
    let (cols, rows) = GRID_SIZE;
    let along = (col as f32 + row as f32 * 1.5) / (cols as f32 + rows as f32 * 1.5);
    let d = (along - phase.clamp(0.0, 1.0)).rem_euclid(1.0);
    let d = d.min(1.0 - d) * 2.0;
    (1.0 - d * 2.5).clamp(0.0, 1.0)
}

/// The empty queue's article grid: a few rows of quiet, unlit wells, where articles
/// will land once an NZB is queued. No baseline and no lit cells, so it reads as an
/// empty matrix, not as a chart. With `wave` (the ambient beat phase) a faint band
/// drifts across the wells.
pub fn article_grid(ui: &mut Ui, pal: &Palette, wave: Option<f32>) {
    article_grid_lit(ui, pal, wave, 0);
}

/// [`article_grid`] with the first `lit` cells (row by row, capped at the grid) lit in
/// the accent: for a real count, such as the NZB files being dragged over the window.
pub fn article_grid_lit(ui: &mut Ui, pal: &Palette, wave: Option<f32>, lit: usize) {
    let (cols, rows) = GRID_SIZE;
    let (cell, gap) = (7.0, 3.0);
    let w = cols as f32 * cell + (cols as f32 - 1.0) * gap;
    let h = rows as f32 * cell + (rows as f32 - 1.0) * gap;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    for row in 0..rows {
        for col in 0..cols {
            let min = Pos2::new(
                rect.left() + col as f32 * (cell + gap),
                rect.top() + row as f32 * (cell + gap),
            );
            if row * cols + col < lit {
                painter.rect_filled(
                    Rect::from_min_size(min, Vec2::splat(cell)),
                    CornerRadius::same(1),
                    pal.accent,
                );
                continue;
            }
            let fill = match wave {
                Some(phase) => {
                    let w = grid_wave(col, row, phase);
                    if w > 0.0 {
                        // The well brightened toward the strong hairline, never the accent.
                        let lift = Palette::fade(pal.border_strong, 0.55 * w);
                        painter.rect_filled(
                            Rect::from_min_size(min, Vec2::splat(cell)),
                            CornerRadius::same(1),
                            pal.well,
                        );
                        lift
                    } else {
                        pal.well
                    }
                }
                None => pal.well,
            };
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::splat(cell)),
                CornerRadius::same(1),
                fill,
            );
        }
    }
}

/// A raised card frame used for docks and settings sections: surface 1, a hairline,
/// a 10 px radius and a soft, short shadow that lifts it off the ground.
///
/// With materials the card is dark glass let into the plate: no hairline and no shadow
/// (show it with [`sunk`], which draws its relief).
pub fn card(pal: &Palette) -> egui::Frame {
    let frame = egui::Frame::new().fill(pal.raised).corner_radius(CornerRadius::same(pal.radius(10))).inner_margin(egui::Margin::same(12));
    if pal.metal.is_some() {
        return frame;
    }
    frame
        .stroke(Stroke::new(1.0, pal.border))
        .shadow(egui::Shadow { offset: [0, 2], blur: 10, spread: 0, color: Palette::fade(pal.shadow, 0.4) })
}

/// Shows `frame` with `add` inside it. With materials the frame is a well sunk into the
/// plate: its collar, the shadow under its top wall and the lit lip below its foot are
/// drawn over its fill and under its contents. On the flat look it is the frame alone.
pub fn sunk<R>(ui: &mut Ui, pal: &Palette, frame: egui::Frame, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    if pal.metal.is_none() {
        return frame.show(ui, add);
    }
    let radius = frame.corner_radius.nw;
    let mut relief = None;
    let shown = frame.show(ui, |ui| {
        relief = Some(ui.painter().add(egui::Shape::Noop));
        add(ui)
    });
    if let Some(idx) = relief {
        ui.painter().set(idx, metal::well(shown.response.rect, radius));
    }
    shown
}

/// Shows `frame` with `add` inside it as something that floats over the window. With
/// materials it is frosted glass: smoked and a little translucent, with a sheen and a lit
/// rim drawn over its fill and under its contents. On the flat look it is the frame alone.
pub fn frosted<R>(ui: &mut Ui, pal: &Palette, frame: egui::Frame, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let Some(m) = pal.metal else {
        return frame.show(ui, add);
    };
    let radius = pal.radius(frame.corner_radius.nw);
    let frame = frame.fill(m.frost).stroke(Stroke::new(1.0, Metal::shade(200))).corner_radius(CornerRadius::same(radius));
    let mut finish = None;
    let shown = frame.show(ui, |ui| {
        finish = Some(ui.painter().add(egui::Shape::Noop));
        add(ui)
    });
    if let Some(idx) = finish {
        ui.painter().set(idx, metal::frost(&m, shown.response.rect, radius));
    }
    shown
}

/// The sheen of a frosted sheet ([`dialog_frame`]), painted first thing inside it: `margin`
/// is the frame's inner margin, so the finish covers the sheet edge to edge.
pub fn frost_sheen(ui: &Ui, pal: &Palette, margin: f32) {
    if let Some(m) = pal.metal {
        let r = ui.max_rect().expand(margin);
        let r = Rect::from_min_max(r.min, Pos2::new(r.right(), r.top() + 120.0));
        let egui::Shape::Vec(parts) = metal::frost(&m, r, pal.radius(14) + 1) else { return };
        // The haze and the sheen only: the sheet's own frame is its rim.
        ui.painter().extend(parts.into_iter().take(2));
    }
}

/// A circular progress ring: a quiet full track and an arc in `color` from twelve
/// o'clock clockwise for `fraction` (clamped; NaN is 0), with round ends.
pub fn progress_ring(painter: &egui::Painter, pal: &Palette, center: Pos2, radius: f32, width: f32, fraction: f32, color: Color32) {
    let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    painter.circle_stroke(center, radius, Stroke::new(width, pal.well));
    if f <= 0.0 {
        return;
    }
    let n = ((96.0 * f).ceil() as usize).max(2);
    let start = -std::f32::consts::FRAC_PI_2;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = start + std::f32::consts::TAU * f * i as f32 / n as f32;
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect();
    if f < 1.0 {
        painter.circle_filled(pts[0], width / 2.0, color);
        painter.circle_filled(pts[n], width / 2.0, color);
    }
    painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
}

/// The frame of a modal sheet: the window ground, a hairline, a 14 px radius and a deep,
/// soft shadow. Sheets lay out their own header, body and footer padding.
///
/// With materials the sheet is frosted glass (call [`frost_sheen`] first thing inside it).
pub fn dialog_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.metal.map_or(pal.surface, |m| m.frost))
        .stroke(Stroke::new(1.0, pal.border_strong))
        .corner_radius(CornerRadius::same(pal.radius(14) + if pal.metal.is_some() { 1 } else { 0 }))
        .shadow(egui::Shadow { offset: [0, 18], blur: 48, spread: 0, color: pal.shadow })
        .inner_margin(egui::Margin::ZERO)
}

/// An on/off switch with its label: a 32×18 capsule whose knob slides across as it
/// turns on (the track fills with the accent). Assistive technology reads it as a
/// checkbox with that label.
pub fn toggle(ui: &mut Ui, pal: &Palette, on: &mut bool, label: &str) -> Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, pal.text);
    let track = Vec2::new(32.0, 18.0);
    let height = galley.size().y.max(ui.spacing().interact_size.y);
    let (rect, mut resp) =
        ui.allocate_exact_size(Vec2::new(track.x + 10.0 + galley.size().x, height), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id.with("knob"), *on);
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        let painter = ui.painter();
        let tr = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - track.y / 2.0), track);
        let x = egui::lerp((tr.left() + 9.0)..=(tr.right() - 9.0), t);
        let knob = Pos2::new(x, tr.center().y);
        if let Some(m) = pal.metal {
            // A slot cut into the plate, backlit in the accent when on, with a steel cap.
            painter.rect_filled(tr, CornerRadius::same(9), pal.sunken.lerp_to_gamma(pal.accent, 0.45 * t));
            painter.add(metal::well(tr, 9));
            painter.rect_stroke(tr, CornerRadius::same(9), Stroke::new(1.0, Palette::fade(pal.accent, 0.55 * t)), StrokeKind::Inside);
            metal::cap(painter, pal, &m, knob, 6.5, t.max(hover * 0.5));
        } else {
            let fill = pal.sunken.lerp_to_gamma(pal.accent, t);
            let edge = pal.border_strong.lerp_to_gamma(pal.accent, t);
            painter.rect(tr, CornerRadius::same(9), fill, Stroke::new(1.0, edge), StrokeKind::Inside);
            if hover > 0.0 {
                painter.circle_filled(knob, 11.0, Palette::fade(pal.accent, 0.16 * hover));
            }
            // The knob is light when on in every theme: the paper ground on light schemes,
            // the text color (near white) on dark ones.
            let lit = if pal.dark { pal.text } else { pal.surface };
            painter.circle_filled(knob, 6.5, pal.muted.lerp_to_gamma(lit, t));
        }
        if resp.has_focus() {
            focus_ring(ui, pal, tr, 9);
        }
        painter.galley(
            Pos2::new(tr.right() + 10.0, rect.center().y - galley.size().y / 2.0),
            galley,
            pal.text,
        );
    }
    let enabled = ui.is_enabled();
    let state = *on;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, state, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A segmented control: the options side by side in one sunken slot, the chosen one on
/// a raised pill that glides between them. Returns the option clicked this frame.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    pal: &Palette,
    id: egui::Id,
    options: &[(T, &str)],
    current: T,
) -> Option<T> {
    let font = fonts::mono_id(10.5);
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, l)| ui.painter().layout_no_wrap((*l).to_owned(), font.clone(), pal.text))
        .collect();
    let pad = 10.0;
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 2.0 * pad).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 24.0), Sense::hover());
    let mut picked = None;
    if !ui.is_rect_visible(rect) {
        return None;
    }
    ui.painter().rect_filled(rect, CornerRadius::same(pal.radius(7)), pal.sunken);
    if pal.metal.is_some() {
        ui.painter().add(metal::well(rect, pal.radius(7)));
    }
    let mut x = rect.left() + 2.0;
    let mut slots = Vec::with_capacity(options.len());
    for w in &widths {
        slots.push(Rect::from_min_size(Pos2::new(x, rect.top() + 2.0), Vec2::new(*w, rect.height() - 4.0)));
        x += w;
    }
    if let Some(sel) = options.iter().position(|(v, _)| *v == current) {
        let ctx = ui.ctx();
        let secs = ui.style().animation_time;
        let left = ctx.animate_value_with_time(id.with("pill-l"), slots[sel].left(), secs);
        let right = ctx.animate_value_with_time(id.with("pill-r"), slots[sel].right(), secs);
        let pill = Rect::from_x_y_ranges(left..=right, slots[sel].y_range());
        let painter = ui.painter();
        if let Some(m) = pal.metal {
            // The backlight slides to the chosen key.
            metal::key_body(painter, &m, pill.shrink(1.0), 3, KeyState { hover: 0.0, on: 1.0, press: 0.0, color: pal.accent });
        } else {
            painter.add(egui::epaint::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Palette::fade(pal.shadow, 0.5) }.as_shape(pill, CornerRadius::same(5)));
            painter.rect(pill, CornerRadius::same(5), pal.raised, Stroke::new(1.0, pal.border), StrokeKind::Inside);
        }
    }
    for (i, ((value, label), galley)) in options.iter().zip(galleys).enumerate() {
        let slot = slots[i];
        let on = *value == current;
        let resp = ui.interact(slot, id.with(i), Sense::click());
        let fg = if on {
            pal.text
        } else if resp.hovered() {
            pal.muted
        } else {
            pal.faint
        };
        ui.painter().galley(slot.center() - galley.size() / 2.0, galley, fg);
        if resp.has_focus() {
            focus_ring(ui, pal, slot, 5);
        }
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, *label));
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            picked = Some(*value);
        }
    }
    picked
}

/// A soft shadow cast downward from the line `y` across `x`, `depth` px deep: a
/// gradient that fades quickly, then trails off.
pub fn shade_below(painter: &egui::Painter, pal: &Palette, x: egui::Rangef, y: f32, depth: f32) {
    let stops = [(0.0, 0.5), (0.35, 0.18), (1.0, 0.0)];
    let mut mesh = egui::epaint::Mesh::default();
    for (i, w) in stops.windows(2).enumerate() {
        let ((t0, a0), (t1, a1)) = (w[0], w[1]);
        let (c0, c1) = (Palette::fade(pal.shadow, a0), Palette::fade(pal.shadow, a1));
        let base = (i * 4) as u32;
        mesh.colored_vertex(Pos2::new(x.min, y + depth * t0), c0);
        mesh.colored_vertex(Pos2::new(x.max, y + depth * t0), c0);
        mesh.colored_vertex(Pos2::new(x.max, y + depth * t1), c1);
        mesh.colored_vertex(Pos2::new(x.min, y + depth * t1), c1);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// A key cap (`Ctrl`, `O`): mono text in a small raised box with a heavier bottom edge,
/// for keyboard hints.
pub fn keycap(ui: &mut Ui, pal: &Palette, key: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(key.to_owned(), fonts::mono_id(10.5), pal.muted);
    let size = Vec2::new((galley.size().x + 10.0).max(20.0), 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if let Some(m) = pal.metal {
            metal::key_body(painter, &m, rect, 3, KeyState { hover: 0.0, on: 0.0, press: 0.0, color: pal.accent });
        } else {
            painter.rect_filled(rect.translate(Vec2::new(0.0, 1.5)), CornerRadius::same(4), pal.border);
            painter.rect(rect, CornerRadius::same(4), pal.raised, Stroke::new(1.0, pal.border), StrokeKind::Inside);
        }
        painter.galley(rect.center() - galley.size() / 2.0, galley, pal.muted);
    }
    resp
}

/// A mono uppercase eyebrow (11 px, 0.12em, text-3): section and dock titles.
///
/// With materials the title is engraved in the plate, a step brighter so the relief reads.
pub fn section_title(ui: &mut Ui, pal: &Palette, text: &str) {
    if let Some(m) = pal.metal {
        let job = spaced(&text.to_uppercase(), fonts::mono_id(11.0), pal.muted, EYEBROW_TRACKING);
        let galley = ui.painter().layout_job(job);
        let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::hover());
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text.to_uppercase()));
        if ui.is_rect_visible(rect) {
            metal::engraved(ui.painter(), &m, rect.min, galley, pal.muted);
        }
        return;
    }
    let job = spaced(
        &text.to_uppercase(),
        fonts::mono_id(11.0),
        pal.faint,
        EYEBROW_TRACKING,
    );
    ui.add(egui::Label::new(job).selectable(false));
}

/// Centered, readable empty-state text.
pub fn empty_state(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).color(pal.muted));
    });
    ui.add_space(8.0);
}

/// A link-styled label (accent text, underline on hover).
pub fn link_label(ui: &mut Ui, pal: &Palette, text: &str) -> Response {
    let resp =
        ui.add(egui::Label::new(RichText::new(text).color(pal.accent_fg)).sense(Sense::click()));
    if resp.hovered() {
        let r = resp.rect;
        ui.painter().line_segment(
            [r.left_bottom(), r.right_bottom()],
            Stroke::new(1.0, pal.accent_fg),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A key/value row inside a two-column grid; the value is a reading, set in mono.
pub fn kv(ui: &mut Ui, pal: &Palette, key: &str, value: impl Into<String>) {
    kv_colored(ui, pal, key, value, pal.text);
}

/// [`kv`] with a value color (e.g. a failure count).
pub fn kv_colored(ui: &mut Ui, pal: &Palette, key: &str, value: impl Into<String>, color: Color32) {
    let value: String = value.into();
    ui.label(RichText::new(key).color(pal.muted));
    ui.label(RichText::new(value).font(fonts::mono_id(12.0)).color(color));
    ui.end_row();
}

/// The innerpulse site the credit links to.
pub const CREDIT_URL: &str = "https://www.innerpulse.net/";

/// The quiet credit: a ~12 px static mark and the word `innerpulse` in muted mono; no
/// pill, no border, no motion. Hover brightens the word; a click is the caller's to
/// handle (it opens [`CREDIT_URL`]).
pub fn credit(ui: &mut Ui, pal: &Palette) -> Response {
    let font = fonts::mono_id(11.5);
    let probe = ui
        .painter()
        .layout_no_wrap("innerpulse".to_owned(), font.clone(), pal.muted);
    let size = Vec2::new(12.0 + 6.0 + probe.size().x, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fg = if resp.hovered() { pal.text } else { pal.muted };
        let mark = Rect::from_center_size(
            Pos2::new(rect.left() + 6.0, rect.center().y),
            Vec2::splat(12.0),
        );
        brand::paint_mark(ui.painter(), mark, &brand::rest_heights(), pal.mark);
        let galley = ui
            .painter()
            .layout_no_wrap("innerpulse".to_owned(), font, fg);
        ui.painter().galley(
            Pos2::new(rect.left() + 18.0, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 2);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, "innerpulse"));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("An innerpulse project · www.innerpulse.net")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_track_cells_and_fill() {
        assert_eq!(track_cells(222.0), 13);
        assert_eq!(track_cells(14.0), 1);
        assert_eq!(track_cells(10_000.0), TRACK_MAX_CELLS);
        assert_eq!(track_cells(0.0), 1);
        assert_eq!(track_cells(f32::NAN), 1);
        assert_eq!(track_fill(0.0, 4), vec![0.0; 4]);
        assert_eq!(track_fill(1.0, 4), vec![1.0; 4]);
        assert_eq!(track_fill(0.5, 4), vec![1.0, 1.0, 0.0, 0.0]);
        let f = track_fill(0.3, 10);
        assert_eq!(f[0..2], [1.0, 1.0]);
        assert!((f[2] - 1.0).abs() < 1e-5 || f[2] < 1.0);
        assert!(f[3..].iter().all(|v| *v < 1e-5));
        assert_eq!(track_fill(f32::NAN, 3), vec![0.0; 3]);
        assert_eq!(track_fill(7.0, 2), vec![1.0; 2]);
        // A tiny fraction still shows in the first cell.
        assert!(track_fill(0.01, 28)[0] > 0.0);
    }

    #[test]
    fn sweep_and_wave_stay_in_range() {
        for k in 0..=20 {
            let phase = k as f32 / 20.0;
            for i in 0..10 {
                let a = sweep_alpha(i, 10, phase);
                assert!((0.78..=1.0).contains(&a), "{a}");
            }
            for row in 0..GRID_SIZE.1 {
                for col in 0..GRID_SIZE.0 {
                    let w = grid_wave(col, row, phase);
                    assert!((0.0..=1.0).contains(&w));
                }
            }
        }
        assert_eq!(sweep_alpha(3, 0, 0.5), 1.0);
        // The head of the sweep is brightest.
        assert!(sweep_alpha(4, 10, 5.0 / 12.0) > sweep_alpha(0, 10, 5.0 / 12.0));
    }

    #[test]
    fn job_states_map_to_chips() {
        assert_eq!(job_status(JobState::Queued), Status::Idle);
        assert_eq!(job_status(JobState::Downloading), Status::Live);
        assert_eq!(job_status(JobState::Done), Status::Ok);
        assert_eq!(job_status(JobState::Failed), Status::Err);
        assert_eq!(job_status(JobState::Paused), Status::Warn);
        for s in [Status::Ok, Status::Warn, Status::Err, Status::Idle] {
            assert!(s.glyph().is_some(), "every status but live carries a glyph");
        }
    }

    #[test]
    fn empty_article_grid_shows_no_data() {
        let (cols, rows) = GRID_SIZE;
        let lit = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (c, r)))
            .filter(|&(c, r)| grid_lit(c, r))
            .count();
        assert_eq!(lit, 0, "an empty queue has no articles to light");
    }
}
