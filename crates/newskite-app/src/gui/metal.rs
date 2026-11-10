//! The materials of the innerpulse ink grounds, shared with the studio's other desktop
//! apps: one plate the whole window is cut from (brushed on carbon), grooves cut into it,
//! wells sunk into it, keys of smoked glass set in dark collars, lamps, engraved labels
//! and the steel of the wordmark. Everything is lit from above: top edges catch a lip of
//! light, feet fall into shade.
//!
//! Only the brand ink themes carry materials ([`Palette::metal`]); desktop themes and
//! paper stay flat, and callers branch on that. Colors come from the [`Palette`], its
//! [`Metal`] and the light and shade [`Metal`] hands out; this module holds none itself.

use eframe::egui::{
    self, pos2, vec2, Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
};

use super::theme::{Metal, Palette};

/// A vertical gradient over `r`, `top` to `bottom`.
fn vgrad(r: Rect, top: Color32, bottom: Color32) -> Shape {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(2, 1, 3);
    Shape::mesh(m)
}

/// Size of the brushed tile.
const TILE: (usize, usize) = (512, 64);

/// Brushed metal: fine horizontal grain, each line its own shade and drifting slowly
/// along its length, as a brushed plate catches the light. One tile, repeated; the same
/// plate every run (a fixed seed).
fn brushed(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("brushed-metal");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return t;
    }
    let (w, h) = TILE;
    let mut seed = 0x2545_f491_u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    let mut px = vec![Metal::light(0); w * h];
    for y in 0..h {
        let line = rnd();
        // Knots along the line, smoothed between: the streak's slow drift.
        let knots: Vec<f32> = (0..=16).map(|_| rnd()).collect();
        for x in 0..w {
            let f = x as f32 / w as f32 * 16.0;
            let k = f.floor() as usize;
            let t = f - k as f32;
            let t = t * t * (3.0 - 2.0 * t);
            let drift = knots[k] + (knots[(k + 1) % 16] - knots[k]) * t;
            let grain = rnd();
            let v = 0.45 * line + 0.35 * drift + 0.20 * grain;
            px[y * w + x] = Metal::light((v * 30.0) as u8);
        }
    }
    let tex = ctx.load_texture(
        "brushed-metal",
        egui::ColorImage::new([w, h], px),
        egui::TextureOptions {
            wrap_mode: egui::TextureWrapMode::Repeat,
            ..egui::TextureOptions::LINEAR
        },
    );
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// The window's one plate over `r`: its fill, the grain tied to the screen so no panel
/// edge shows a seam, light falling from the top and gone by the foot.
pub fn plate(painter: &Painter, m: &Metal, r: Rect) {
    painter.rect_filled(r, CornerRadius::ZERO, m.plate);
    if m.brushed {
        let t = brushed(painter.ctx());
        let (w, h) = (TILE.0 as f32, TILE.1 as f32);
        let uv = Rect::from_min_max(pos2(r.left() / w, r.top() / h), pos2(r.right() / w, r.bottom() / h));
        painter.image(t.id(), r, uv, Metal::white());
    }
    let top = Rect::from_min_max(r.min, pos2(r.right(), r.top() + r.height() * 0.22));
    painter.add(vgrad(top, Metal::light(13), Metal::light(0)));
    let foot = Rect::from_min_max(pos2(r.left(), r.bottom() - r.height() * 0.28), r.max);
    painter.add(vgrad(foot, Metal::shade(0), Metal::shade(56)));
}

/// A groove cut across the plate at `y`: a dark line and the lit lip under it.
pub fn groove_h(painter: &Painter, x0: f32, x1: f32, y: f32) {
    let y = y.round();
    painter.line_segment([pos2(x0, y + 0.5), pos2(x1, y + 0.5)], Stroke::new(1.0, Metal::shade(158)));
    painter.line_segment([pos2(x0, y + 1.5), pos2(x1, y + 1.5)], Stroke::new(1.0, Metal::light(Metal::LIP)));
}

/// A groove cut down the plate at `x`.
pub fn groove_v(painter: &Painter, x: f32, y0: f32, y1: f32) {
    let x = x.round();
    painter.line_segment([pos2(x + 0.5, y0), pos2(x + 0.5, y1)], Stroke::new(1.0, Metal::shade(158)));
    painter.line_segment([pos2(x + 1.5, y0), pos2(x + 1.5, y1)], Stroke::new(1.0, Metal::light(Metal::LIP)));
}

/// The relief of a well sunk into the plate, drawn over its fill and under its contents:
/// a dark collar, the shadow its top wall casts inside it and the lit lip under its foot.
pub fn well(r: Rect, radius: u8) -> Shape {
    let cr = CornerRadius::same(radius);
    let rr = f32::from(radius);
    let shade = Rect::from_min_size(r.min, vec2(r.width(), (r.height() * 0.4).min(8.0)));
    Shape::Vec(vec![
        vgrad(shade.shrink2(vec2(rr * 0.4, 0.0)), Metal::shade(90), Metal::shade(0)),
        Shape::rect_stroke(r, cr, Stroke::new(1.0, Metal::shade(190)), StrokeKind::Outside),
        Shape::line_segment(
            [r.left_bottom() + vec2(rr, 1.5), r.right_bottom() + vec2(-rr, 1.5)],
            Stroke::new(1.0, Metal::light(Metal::LIP)),
        ),
    ])
}

/// Glass, lit from above: a sheen through the top edge and a one-pixel highlight lip.
/// Painted over a card's fill.
pub fn glass_top(painter: &Painter, m: &Metal, r: Rect, radius: u8) {
    let strength = if m.brushed { 1.0 } else { 0.6 };
    let rr = f32::from(radius);
    let sheen = Rect::from_min_size(r.min, vec2(r.width(), (r.height() * 0.35).min(28.0)));
    painter.add(vgrad(sheen.shrink2(vec2(rr * 0.4, 0.0)), Metal::light((10.0 * strength) as u8), Metal::light(0)));
    painter.line_segment(
        [r.left_top() + vec2(rr, 0.5), r.right_top() + vec2(-rr, 0.5)],
        Stroke::new(1.0, Metal::light((f32::from(Metal::LIP) * strength) as u8)),
    );
}

/// How a key stands this frame: `hover`, `on` and `press` each run 0 to 1.
#[derive(Clone, Copy, Debug)]
pub struct KeyState {
    pub hover: f32,
    /// Lit from behind: latched, current, or the control that leads.
    pub on: f32,
    pub press: f32,
    /// What lights it.
    pub color: Color32,
}

/// A fan from `center` to `outline`, `inner` at the center and `outer` at the edge: a
/// radial gradient in any convex shape.
fn radial_fan(painter: &Painter, center: Pos2, outline: &[Pos2], inner: Color32, outer: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, inner);
    for p in outline {
        mesh.colored_vertex(*p, outer);
    }
    let n = outline.len() as u32;
    for k in 0..n {
        mesh.add_triangle(0, 1 + k, 1 + (k + 1) % n);
    }
    painter.add(Shape::mesh(mesh));
}

/// A key: a cap of smoked glass set in a dark collar, lit from behind when `on`. A held
/// key goes darker; a lit one takes the backlight's color through the glass, brightest at
/// its middle.
pub fn key_body(painter: &Painter, m: &Metal, r: Rect, radius: u8, k: KeyState) {
    let [rest_top, rest_foot, held_top, held_foot] = m.key;
    let on = k.on.clamp(0.0, 1.0);
    let press = k.press.clamp(0.0, 1.0);
    let white = Metal::white();
    let lift = 0.06 * k.hover * (1.0 - on.max(press));
    let top = Metal::lerp(
        Metal::lerp(Metal::lerp(rest_top, held_top, press), Metal::lerp(rest_top, k.color, 0.07), on),
        white,
        lift,
    );
    let foot = Metal::lerp(
        Metal::lerp(Metal::lerp(rest_foot, held_foot, press), Metal::lerp(held_top, k.color, 0.05), on),
        white,
        lift,
    );
    let cr = CornerRadius::same(radius);
    let inset = f32::from(radius).max(2.0);
    // The cap and its collar as one shape, the collar's lit lip under it.
    painter.rect(r, cr, Metal::lerp(top, foot, 0.5), Stroke::new(1.0, Metal::shade(215)), StrokeKind::Outside);
    painter.line_segment(
        [r.left_bottom() + vec2(inset, 1.5), r.right_bottom() + vec2(-inset, 1.5)],
        Stroke::new(1.0, Metal::light(Metal::LIP)),
    );
    // The glass in one mesh, a sheen across its upper half.
    let glass = r.shrink(1.0);
    let mid = glass.center().y;
    let mut mesh = Mesh::default();
    for (y, c) in [
        (glass.top(), Metal::lerp(top, white, 0.035)),
        (mid, Metal::lerp(top, foot, 0.5)),
        (glass.bottom(), foot),
    ] {
        mesh.colored_vertex(pos2(glass.left(), y), c);
        mesh.colored_vertex(pos2(glass.right(), y), c);
    }
    for i in [0, 2] {
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 2, i + 1, i + 3);
    }
    painter.add(Shape::mesh(mesh));
    if on > 0.0 {
        let (c, x0, x1, y0, y1) = (glass.center(), glass.left(), glass.right(), glass.top(), glass.bottom());
        let edge = [
            pos2(x0, y0),
            pos2(c.x, y0),
            pos2(x1, y0),
            pos2(x1, c.y),
            pos2(x1, y1),
            pos2(c.x, y1),
            pos2(x0, y1),
            pos2(x0, c.y),
        ];
        radial_fan(painter, c, &edge, Metal::alpha(k.color, 0.34 * on), Metal::alpha(k.color, 0.03 * on));
        painter.rect_stroke(r, cr, Stroke::new(1.0, Metal::alpha(k.color, 0.42 * on)), StrokeKind::Inside);
    }
    painter.line_segment(
        [r.left_top() + vec2(inset, 0.5), r.right_top() + vec2(-inset, 0.5)],
        Stroke::new(1.0, Metal::light(Metal::LIP_HARD)),
    );
}

/// A key's label color, in step with its [`KeyState`].
pub fn key_text(pal: &Palette, k: KeyState, enabled: bool) -> Color32 {
    if !enabled {
        return pal.faint;
    }
    let off = Metal::lerp(pal.muted, pal.text, k.hover);
    Metal::lerp(off, Metal::lerp(k.color, Metal::white(), 0.6), k.on)
}

/// The relief of a filled key over its face, `press` 0 up to 1 down. Up, it stands proud
/// of the plate: a lit top edge and a dark foot under it. Down, it sits in the plate:
/// shade falls into it from the top edge and the foot is gone.
pub fn key_face(painter: &Painter, r: Rect, radius: u8, press: f32) {
    let up = 1.0 - press.clamp(0.0, 1.0);
    let rr = f32::from(radius);
    if up > 0.0 {
        painter.line_segment(
            [r.left_top() + vec2(rr, 1.5), r.right_top() + vec2(-rr, 1.5)],
            Stroke::new(1.0, Metal::light((f32::from(Metal::LIP_HARD) * up) as u8)),
        );
        painter.line_segment(
            [r.left_bottom() + vec2(rr, 0.5), r.right_bottom() + vec2(-rr, 0.5)],
            Stroke::new(1.0, Metal::shade((110.0 * up) as u8)),
        );
    }
    if press > 0.0 {
        let shade = Rect::from_min_size(r.min + vec2(1.0, 1.0), vec2(r.width() - 2.0, (r.height() * 0.45).min(7.0)));
        painter.add(vgrad(shade, Metal::shade((120.0 * press) as u8), Metal::shade(0)));
    }
}

/// Paints the registered SVG `uri` (a wordmark) into `rect` in brushed steel: bright
/// above the crease, dark through it and a lighter band below, in five cuts, over a
/// one-pixel drop shadow. Without steel (the warm ground) it is the text color.
pub fn steel_svg(painter: &Painter, pal: &Palette, m: &Metal, uri: &'static str, rect: Rect) {
    let Some(steel) = m.steel else {
        super::icons::paint_svg(painter, uri, rect, pal.text);
        return;
    };
    super::icons::paint_svg(painter, uri, rect.translate(vec2(0.0, 1.0)), Metal::shade(200));
    let cuts = [(0.0, 0.38), (0.38, 0.54), (0.54, 0.62), (0.62, 0.84), (0.84, 1.0)];
    let clip = painter.clip_rect();
    for ((a, b), color) in cuts.into_iter().zip(steel) {
        let band = Rect::from_min_max(
            pos2(rect.left() - 1.0, rect.top() + rect.height() * a),
            pos2(rect.right() + 1.0, rect.top() + rect.height() * b),
        );
        super::icons::paint_svg(&painter.with_clip_rect(band.intersect(clip)), uri, rect, color);
    }
}

/// Text stamped into the plate, as if lit from above: the letters' upper walls cast a
/// hairline of shadow and their lower lips catch the light.
pub fn engraved(painter: &Painter, m: &Metal, pos: Pos2, galley: std::sync::Arc<egui::Galley>, color: Color32) {
    let lip = if m.brushed { 30 } else { 20 };
    painter.galley_with_override_text_color(pos + vec2(0.0, 1.0), galley.clone(), Metal::light(lip));
    painter.galley_with_override_text_color(pos + vec2(0.0, -0.7), galley.clone(), Metal::shade(150));
    painter.galley_with_override_text_color(pos, galley, color);
}

/// A lamp's outline: what shape it lights in, so a state reads by shape as well as color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LampShape {
    Round,
    Triangle,
    Square,
}

fn lamp_outline(shape: LampShape, c: Pos2, r: f32) -> Vec<Pos2> {
    match shape {
        LampShape::Round => (0..24)
            .map(|k| {
                let a = k as f32 / 24.0 * std::f32::consts::TAU;
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect(),
        LampShape::Triangle => {
            let r = r * 1.18;
            vec![c + vec2(0.0, -r), c + vec2(r * 0.95, r * 0.7), c + vec2(-r * 0.95, r * 0.7)]
        }
        LampShape::Square => {
            let r = r * 0.88;
            vec![c + vec2(-r, -r), c + vec2(r, -r), c + vec2(r, r), c + vec2(-r, r)]
        }
    }
}

/// An indicator lamp of radius `r`: lit (`lit` 1), a glass bulb glowing in `color` with a
/// halo and a hot core; dark (0), the same bulb unlit, smoked glass with only its
/// specular highlight, so an off state still reads as a lamp that is off.
pub fn lamp(painter: &Painter, pal: &Palette, center: Pos2, r: f32, color: Color32, lit: f32, shape: LampShape) {
    let lit = lit.clamp(0.0, 1.0);
    if lit > 0.01 {
        let halo = lamp_outline(LampShape::Round, center, r * 2.7);
        radial_fan(painter, center, &halo, Metal::alpha(color, 0.32 * lit), Metal::alpha(color, 0.0));
    }
    let outline = lamp_outline(shape, center, r);
    let dark_core = Metal::lerp(pal.hover, color, 0.22);
    let dark_edge = Metal::lerp(pal.sunken, color, 0.10);
    let core = Metal::lerp(dark_core, Metal::lerp(color, Metal::white(), 0.6), lit);
    let edge = Metal::lerp(dark_edge, color, lit);
    // The hot center sits a little above middle, where the light falls.
    radial_fan(painter, center + vec2(0.0, -r * 0.15), &outline, core, edge);
    let mut closed = outline.clone();
    closed.push(outline[0]);
    painter.add(Shape::line(closed, Stroke::new(1.0, Metal::shade(140))));
    let spec = center + vec2(-r * 0.32, -r * 0.38);
    painter.circle_filled(spec, (r * 0.32).max(0.9), Metal::light((70.0 + 60.0 * (1.0 - lit)) as u8));
}

/// A light along the top edge of `r` in `color`: a hairline brightest at `travel` (0 the
/// left end, 1 the right) and fading away from it, with a soft wash falling from it into
/// the plate. `level` (0…1) dims the whole light.
pub fn edge_light(painter: &Painter, r: Rect, color: Color32, travel: f32, level: f32) {
    let w = r.width();
    let peak = r.left() + w * travel.clamp(0.0, 1.0);
    let at = |x: f32| -> f32 {
        if x <= peak {
            (1.0 - (peak - x) / (w * 0.12)).clamp(0.0, 1.0)
        } else {
            (1.0 - (x - peak) / (w * 0.5)).clamp(0.0, 1.0)
        }
    };
    let mut cols: Vec<f32> = [r.left(), peak - w * 0.12, peak, peak + w * 0.5, r.right()]
        .iter()
        .map(|x| x.clamp(r.left(), r.right()))
        .collect();
    cols.sort_by(f32::total_cmp);
    cols.dedup();
    for (h, a) in [(1.0, 0.85), (10.0, 0.16)] {
        let mut mesh = Mesh::default();
        for x in &cols {
            let i = mesh.vertices.len() as u32;
            let c = Metal::alpha(color, a * level * at(*x));
            mesh.colored_vertex(pos2(*x, r.top()), c);
            mesh.colored_vertex(pos2(*x, r.top() + h), if h > 1.0 { Metal::alpha(color, 0.0) } else { c });
            if i >= 2 {
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i + 1, i);
            }
        }
        painter.add(Shape::mesh(mesh));
    }
}

/// The finish of frosted glass over its fill and under its contents: light caught in its
/// upper part, a slanted sheen across its left, and a lit rim all around.
pub fn frost(m: &Metal, r: Rect, radius: u8) -> Shape {
    let strength = if m.brushed { 1.0 } else { 0.7 };
    let a = |v: f32| Metal::light((v * strength) as u8);
    let rr = f32::from(radius);
    let inner = r.shrink(1.0);
    let haze = Rect::from_min_max(inner.min + vec2(rr * 0.4, 0.0), pos2(inner.right() - rr * 0.4, inner.top() + (inner.height() * 0.5).min(64.0)));
    let w = inner.width();
    let mut sheen = Mesh::default();
    sheen.colored_vertex(pos2(inner.left() + w * 0.06, inner.top()), a(10.0));
    sheen.colored_vertex(pos2(inner.left() + w * 0.24, inner.top()), a(10.0));
    sheen.colored_vertex(pos2(inner.left() + w * 0.12, inner.bottom()), a(0.0));
    sheen.colored_vertex(pos2(inner.left() + rr, inner.bottom()), a(0.0));
    sheen.add_triangle(0, 1, 2);
    sheen.add_triangle(0, 2, 3);
    Shape::Vec(vec![
        vgrad(haze, a(14.0), a(0.0)),
        Shape::mesh(sheen),
        Shape::rect_stroke(r, CornerRadius::same(radius), Stroke::new(1.0, a(f32::from(Metal::LIP))), StrokeKind::Inside),
        Shape::line_segment([r.left_top() + vec2(rr, 0.5), r.right_top() + vec2(-rr, 0.5)], Stroke::new(1.0, a(f32::from(Metal::LIP_HARD)))),
    ])
}

/// Corner radius of a display's glass.
const LCD_R: u8 = 5;

/// The display behind its readings: a bezel cut into the plate, and a panel of black
/// glass lit from behind in the display's color, brightest at its middle as a backlight
/// falls off toward the frame. `level` (0…1) is how brightly the backlight is on.
pub fn lcd_back(painter: &Painter, m: &Metal, r: Rect, level: f32) {
    let bezel = r.expand(2.0);
    painter.rect_filled(bezel, CornerRadius::same(LCD_R + 2), Metal::shade(150));
    painter.line_segment(
        [bezel.left_bottom() + vec2(6.0, 0.5), bezel.right_bottom() + vec2(-6.0, 0.5)],
        Stroke::new(1.0, Metal::light(Metal::LIP_SOFT)),
    );
    painter.rect_filled(r, CornerRadius::same(LCD_R), m.lcd_glass);
    let mut mesh = Mesh::default();
    let c = r.center();
    mesh.colored_vertex(c, Metal::alpha(m.lcd_lit, 0.04 + 0.08 * level.clamp(0.0, 1.0)));
    let n = 40;
    for k in 0..=n {
        let a = k as f32 / n as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(pos2(c.x + a.cos() * r.width() * 0.56, c.y + a.sin() * r.height() * 0.95), Metal::alpha(m.lcd_lit, 0.0));
    }
    for k in 1..=n as u32 {
        mesh.add_triangle(0, k, k + 1);
    }
    painter.with_clip_rect(r.shrink(1.0)).add(Shape::mesh(mesh));
}

/// The glass over a display's readings: a fine row texture like an LCD's cells, a shadow
/// under the bezel's upper lip, a frosted sheen across the top and the glass's own edge.
pub fn lcd_glass(painter: &Painter, r: Rect) {
    let cp = painter.with_clip_rect(r.shrink(1.0));
    let mut y = r.top() + 2.0;
    while y < r.bottom() - 1.0 {
        cp.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, Metal::shade(18)));
        y += 3.0;
    }
    cp.add(vgrad(Rect::from_min_size(r.min, vec2(r.width(), 6.0)), Metal::shade(120), Metal::shade(0)));
    cp.add(vgrad(Rect::from_min_max(pos2(r.left(), r.top() + 1.0), pos2(r.right(), r.center().y)), Metal::light(16), Metal::light(0)));
    let w = r.width();
    let mut sheen = Mesh::default();
    sheen.colored_vertex(pos2(r.left() + w * 0.08, r.top()), Metal::light(9));
    sheen.colored_vertex(pos2(r.left() + w * 0.22, r.top()), Metal::light(9));
    sheen.colored_vertex(pos2(r.left() + w * 0.12, r.bottom()), Metal::light(0));
    sheen.colored_vertex(pos2(r.left() - w * 0.02, r.bottom()), Metal::light(0));
    sheen.add_triangle(0, 1, 2);
    sheen.add_triangle(0, 2, 3);
    cp.add(Shape::mesh(sheen));
    painter.rect_stroke(r, CornerRadius::same(LCD_R), Stroke::new(1.0, Metal::light(Metal::LIP)), StrokeKind::Inside);
}

/// Segments a–g lit for each figure, a the top, then clockwise, g the middle.
const SEGMENTS: [u8; 10] = [0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111, 0b1111111, 0b1101111];

/// The forward slant of a display's figures, as a run over rise.
const SLANT: f32 = 0.07;

/// The seven segments of a digit `w` by `h` as hexagons, slanted about the baseline, in
/// order a–g, with `o` the digit's top-left before the slant.
fn segments(o: Pos2, w: f32, h: f32) -> [[Pos2; 6]; 7] {
    let t = (h * 0.12).max(2.0);
    let g = t * 0.16;
    let half = t / 2.0;
    let horiz = |y: f32| {
        let (a, b) = (half + g, w - half - g);
        [pos2(a, y), pos2(a + half, y - half), pos2(b - half, y - half), pos2(b, y), pos2(b - half, y + half), pos2(a + half, y + half)]
    };
    let vert = |x: f32, y0: f32, y1: f32| {
        let (a, b) = (y0 + g, y1 - g);
        [pos2(x, a), pos2(x + half, a + half), pos2(x + half, b - half), pos2(x, b), pos2(x - half, b - half), pos2(x - half, a + half)]
    };
    let mid = h / 2.0;
    [horiz(half), vert(w - half, half, mid), vert(w - half, mid, h - half), horiz(h - half), vert(half, mid, h - half), vert(half, half, mid), horiz(mid)]
        .map(|seg| seg.map(|p| pos2(o.x + p.x + (h - p.y) * SLANT, o.y + p.y)))
}

/// Width seven-segment `text` takes at digit height `h` (see [`lcd_digits`]).
pub fn lcd_digits_width(text: &str, h: f32) -> f32 {
    let w = h * 0.54;
    text.chars().map(|c| if c == '.' { h * 0.2 } else { w + h * 0.16 }).sum::<f32>() - h * 0.16
}

/// Seven-segment figures, drawn rather than set in type, from `o` (top-left) at height
/// `h`: each digit of `text` lit in `color` with a soft halo over the ghost of its unlit
/// segments, a `.` as a square on the baseline, and any other character (a space) as a
/// ghost alone, for a blank leading place.
pub fn lcd_digits(painter: &Painter, o: Pos2, text: &str, h: f32, color: Color32) {
    let w = h * 0.54;
    let t = (h * 0.12).max(2.0);
    let ghost = Metal::alpha(color, 0.07);
    let none = Metal::alpha(color, 0.0);
    let mut x = o.x;
    for ch in text.chars() {
        if ch == '.' {
            let dot = Rect::from_min_size(pos2(x - h * 0.06, o.y + h - t), vec2(t, t));
            painter.rect_filled(dot, CornerRadius::ZERO, color);
            x += h * 0.2;
            continue;
        }
        let mask = ch.to_digit(10).map_or(0, |d| SEGMENTS[d as usize]);
        let shapes = segments(pos2(x, o.y), w, h);
        for (k, seg) in shapes.iter().enumerate() {
            if mask & (1 << k) == 0 {
                painter.add(Shape::convex_polygon(seg.to_vec(), ghost, Stroke::NONE));
            } else {
                painter.add(Shape::convex_polygon(seg.to_vec(), none, Stroke::new(t * 0.9, Metal::alpha(color, 0.10))));
            }
        }
        for (k, seg) in shapes.iter().enumerate() {
            if mask & (1 << k) != 0 {
                painter.add(Shape::convex_polygon(seg.to_vec(), color, Stroke::NONE));
            }
        }
        x += w + h * 0.16;
    }
}

/// A display's annunciator, `r` its box: lit, a block of `color` with the word cut out
/// of it (in the glass's black); unlit, the word faintly there, as an unpowered segment.
pub fn lcd_tag(painter: &Painter, m: &Metal, r: Rect, galley: std::sync::Arc<egui::Galley>, color: Color32, lit: bool) {
    let at = r.center() - galley.size() / 2.0;
    if lit {
        painter.rect_filled(r.expand(1.5), CornerRadius::same(3), Metal::alpha(color, 0.14));
        painter.rect_filled(r, CornerRadius::same(2), Metal::alpha(color, 0.88));
        painter.galley_with_override_text_color(at, galley, m.lcd_glass);
    } else {
        painter.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, Metal::alpha(color, 0.18)), StrokeKind::Inside);
        painter.galley_with_override_text_color(at, galley, Metal::alpha(color, 0.38));
    }
}

/// A turned steel cap of radius `r` at `c` (a switch's knob): the steel ramp across it,
/// in a dark collar. `lit` (0…1) brightens it.
pub fn cap(painter: &Painter, pal: &Palette, m: &Metal, c: Pos2, r: f32, lit: f32) {
    let (hi, lo) = match m.steel {
        Some(s) => (s[0], s[2]),
        None => (pal.text, pal.faint),
    };
    let dim = 0.62 + 0.38 * lit.clamp(0.0, 1.0);
    let outline = lamp_outline(LampShape::Round, c, r);
    painter.circle_filled(c, r + 1.0, Metal::shade(200));
    radial_fan(
        painter,
        c + vec2(0.0, -r * 0.35),
        &outline,
        Metal::lerp(pal.sunken, hi, dim),
        Metal::lerp(pal.sunken, lo, dim),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brushed_tile_is_faint_light() {
        let ctx = egui::Context::default();
        let a = brushed(&ctx);
        let b = brushed(&ctx);
        assert_eq!(a.id(), b.id(), "made once");
        assert_eq!(a.size(), [TILE.0, TILE.1]);
    }

    #[test]
    fn digits_light_the_right_segments() {
        assert_eq!(SEGMENTS[8].count_ones(), 7);
        assert_eq!(SEGMENTS[1].count_ones(), 2);
        assert_eq!(SEGMENTS[0] & (1 << 6), 0, "a zero has no middle bar");
        // A point takes less room than a digit, and places add up.
        let h = 30.0;
        assert!(lcd_digits_width("12.5", h) < lcd_digits_width("1205", h));
        assert!((lcd_digits_width("88", h) - (2.0 * h * 0.54 + h * 0.16)).abs() < 1e-3);
    }

    #[test]
    fn lamps_keep_their_shapes() {
        let c = pos2(10.0, 10.0);
        assert_eq!(lamp_outline(LampShape::Triangle, c, 4.0).len(), 3);
        assert_eq!(lamp_outline(LampShape::Square, c, 4.0).len(), 4);
        assert!(lamp_outline(LampShape::Round, c, 4.0).len() > 12);
    }
}
