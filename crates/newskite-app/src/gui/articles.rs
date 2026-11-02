//! Article grids: a job's articles as a matrix of small cells, in file and segment order:
//! arrived in the positive color, missing on every server in the negative color, still
//! waiting as quiet wells. A big job folds several articles into each cell and says how
//! many; such a cell shows red when any of its articles is missing and is shaded by how
//! much of it arrived. A cell that just filled glows briefly while motion is allowed.
//! Every cell comes from the service's article map, which only holds what the engine
//! reported: nothing here is estimated.

use eframe::egui::{self, epaint, Color32, Id, Pos2, Rect, Response, RichText, Sense, Ui, Vec2};
use newskite_service::{ArticleCounts, ArticleMap};

use super::fonts;
use super::motion;
use super::serverstrip::thousands;
use super::theme::Palette;

/// Largest and smallest cell side, px.
const MAX_SIDE: f32 = 9.0;
const MIN_SIDE: f32 = 2.0;
/// Cell side once articles are folded into cells.
const FOLDED_SIDE: f32 = 3.0;
/// Seconds a freshly filled cell glows.
const GLOW_SECS: f64 = 0.9;

/// How the cells of a grid are laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridLayout {
    /// Cell side and the gap between cells, px.
    pub side: f32,
    pub gap: f32,
    pub cols: usize,
    /// Most cells the area holds.
    pub capacity: usize,
}

impl GridLayout {
    fn at(side: f32, size: Vec2) -> Self {
        let gap = if side >= 5.0 { 2.0 } else { 1.0 };
        let cols = ((size.x + gap) / (side + gap)).floor().max(0.0) as usize;
        let rows = ((size.y + gap) / (side + gap)).floor().max(0.0) as usize;
        Self { side, gap, cols, capacity: cols * rows }
    }

    /// The largest cells (9 px down to 2 px) at which each of `n` articles gets its own
    /// cell in `size`; when even 2 px cells cannot hold them all, 3 px cells that each
    /// take several articles. `None` when the area holds no cell at all.
    pub fn for_articles(n: usize, size: Vec2) -> Option<Self> {
        if !(size.x.is_finite() && size.y.is_finite()) {
            return None;
        }
        let mut side = MAX_SIDE;
        while side >= MIN_SIDE {
            let l = Self::at(side, size);
            if l.capacity >= n && l.cols > 0 {
                return Some(l);
            }
            side -= 1.0;
        }
        let l = Self::at(FOLDED_SIDE, size);
        (l.capacity > 0).then_some(l)
    }

    /// The rectangle of cell `i` with the grid's top-left corner at `origin`.
    fn cell(&self, origin: Pos2, i: usize) -> Rect {
        let (col, row) = (i % self.cols, i / self.cols);
        let step = self.side + self.gap;
        Rect::from_min_size(origin + Vec2::new(col as f32 * step, row as f32 * step), Vec2::splat(self.side))
    }

    /// The cell under `p`, if any.
    fn cell_at(&self, origin: Pos2, p: Pos2, cells: usize) -> Option<usize> {
        let step = self.side + self.gap;
        let d = p - origin;
        if d.x < 0.0 || d.y < 0.0 {
            return None;
        }
        let (col, row) = ((d.x / step) as usize, (d.y / step) as usize);
        let i = row * self.cols + col;
        (col < self.cols && i < cells && self.cell(origin, i).contains(p)).then_some(i)
    }
}

/// What a grid remembers between frames to notice cells that just filled.
#[derive(Clone, Default)]
struct Memory {
    /// The layout the counts belong to: (articles, articles per cell, cells).
    key: (usize, usize, usize),
    done: Vec<usize>,
    /// egui time each cell last gained arrived articles.
    lit_at: Vec<f64>,
}

/// The color of a cell with these counts (glow aside).
pub fn cell_color(pal: &Palette, c: &ArticleCounts) -> Color32 {
    if c.missing > 0 {
        pal.negative
    } else if c.total() > 0 && c.done == c.total() {
        pal.positive
    } else if c.done > 0 {
        // Partly arrived: the positive color over the well, as strong as the share.
        let share = c.done as f32 / c.total() as f32;
        pal.well.lerp_to_gamma(pal.positive, 0.3 + 0.55 * share)
    } else {
        pal.well
    }
}

/// `"5,412 of 5,600 articles · 3 missing"`.
pub fn summary(c: &ArticleCounts) -> String {
    let mut s = format!("{} of {} articles", thousands(c.done as u64), thousands(c.total() as u64));
    if c.missing > 0 {
        s.push_str(&format!("  ·  {} missing", thousands(c.missing as u64)));
    }
    s
}

/// The file and position of global article `k` (in file order).
fn locate(map: &ArticleMap, mut k: usize) -> Option<(&str, usize, usize)> {
    for f in &map.files {
        if k < f.states.len() {
            return Some((&f.name, k, f.states.len()));
        }
        k -= f.states.len();
    }
    None
}

/// Draws `map` as a grid filling `size`. `id` keeps the grid's memory (for the glow of
/// freshly filled cells, which is skipped under reduced motion). Hovering a cell names
/// its file and articles.
pub fn grid(ui: &mut Ui, pal: &Palette, id: Id, map: &ArticleMap, size: Vec2, reduced: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let counts = map.counts();
    let label = format!(
        "Article grid: {} arrived, {} missing, {} waiting",
        counts.done, counts.missing, counts.waiting
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, &label));
    let n = map.len();
    let Some(layout) = GridLayout::for_articles(n, rect.size()) else { return resp };
    if n == 0 || !ui.is_rect_visible(rect) {
        return resp;
    }
    let (per, cells) = map.cells(if layout.capacity >= n { n } else { layout.capacity });
    let now = ui.input(|i| i.time);

    // Cells whose arrived count grew since the last frame start to glow.
    let glow: Vec<f32> = ui.ctx().data_mut(|d| {
        let m = d.get_temp_mut_or_default::<Memory>(id);
        let key = (n, per, cells.len());
        if m.key != key {
            *m = Memory { key, done: cells.iter().map(|c| c.done).collect(), lit_at: vec![f64::NEG_INFINITY; cells.len()] };
        }
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if c.done > m.done[i] {
                    m.lit_at[i] = now;
                }
                m.done[i] = c.done;
                let age = now - m.lit_at[i];
                if reduced || !(0.0..GLOW_SECS).contains(&age) { 0.0 } else { (1.0 - age / GLOW_SECS) as f32 }
            })
            .collect()
    });
    if glow.iter().any(|g| *g > 0.0) {
        ui.ctx().request_repaint_after(motion::FRAME);
    }

    // One mesh for every cell: thousands of rectangles stay cheap.
    let shine = if pal.dark { pal.text } else { pal.raised };
    let mut mesh = epaint::Mesh::default();
    let mut missing = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        let mut color = cell_color(pal, c);
        if glow[i] > 0.0 && c.missing == 0 {
            color = color.lerp_to_gamma(shine, 0.6 * glow[i] * glow[i]);
        }
        mesh.add_colored_rect(layout.cell(rect.min, i), color);
        if c.missing > 0 {
            missing.push(i);
        }
    }
    // Missing cells are drawn again on top, a pixel larger on every side when cells are
    // small, so a handful of them among thousands still catch the eye.
    let grow = if layout.side <= 4.0 { 1.0 } else { 0.0 };
    for i in missing {
        mesh.add_colored_rect(layout.cell(rect.min, i).expand(grow), pal.negative);
    }
    ui.painter().add(egui::Shape::mesh(mesh));

    // Hover: what the cell holds.
    let hover = resp.hover_pos().and_then(|p| layout.cell_at(rect.min, p, cells.len()));
    if let Some(i) = hover {
        ui.painter().rect_stroke(
            layout.cell(rect.min, i).expand(1.0),
            egui::CornerRadius::same(1),
            egui::Stroke::new(1.0, pal.text),
            egui::StrokeKind::Outside,
        );
        let first = i * per;
        let c = cells[i];
        let text = match locate(map, first) {
            Some((name, k, len)) if per == 1 => {
                let state = if c.missing > 0 { "missing" } else if c.done > 0 { "arrived" } else { "waiting" };
                format!("{name}\narticle {} of {len}: {state}", k + 1)
            }
            Some((name, k, _)) => format!(
                "{name}\narticles {}–{}: {} arrived, {} missing, {} waiting",
                k + 1,
                k + c.total(),
                c.done,
                c.missing,
                c.waiting
            ),
            None => String::new(),
        };
        if !text.is_empty() {
            return resp.on_hover_text_at_pointer(text);
        }
    }
    resp
}

/// Height the [`legend`] takes at `width`: one line, or two when the cell size note does
/// not fit beside the key.
pub fn legend_height(ui: &Ui, pal: &Palette, per_cell: usize, width: f32) -> f32 {
    let font = fonts::mono_id(10.5);
    let w = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), pal.muted).size().x;
    let key: f32 = ["arrived", "missing", "waiting"].iter().map(|t| 8.0 + 4.0 + w(t) + 4.0 + 6.0).sum();
    let note = if per_cell > 1 { w(&format!("1 cell = {per_cell} articles")) } else { 0.0 };
    if key + note <= width { 16.0 } else { 34.0 }
}

/// The key under a grid: a swatch and word for each state, and how many articles a cell
/// holds when it holds more than one (on its own line when it does not fit).
pub fn legend(ui: &mut Ui, pal: &Palette, per_cell: usize) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (color, word) in [(pal.positive, "arrived"), (pal.negative, "missing"), (pal.well, "waiting")] {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
            ui.painter().rect_filled(r, egui::CornerRadius::same(1), color);
            ui.label(RichText::new(word).font(fonts::mono_id(10.5)).color(pal.muted));
            ui.add_space(6.0);
        }
        if per_cell > 1 {
            // Whole: it moves to the next line rather than breaking.
            ui.add(
                egui::Label::new(
                    RichText::new(format!("1 cell = {per_cell} articles"))
                        .font(fonts::mono_id(10.5))
                        .color(pal.faint),
                )
                .extend(),
            );
        }
    });
}

/// The height a grid of `n` articles needs at `width`, at most `max_h`: the largest cells
/// that fit, with no empty rows below them.
pub fn fit_height(n: usize, width: f32, max_h: f32) -> f32 {
    let mut side = MAX_SIDE;
    while side >= MIN_SIDE {
        let gap = if side >= 5.0 { 2.0 } else { 1.0 };
        let cols = ((width + gap) / (side + gap)).floor().max(1.0) as usize;
        let rows = n.div_ceil(cols).max(1);
        let h = rows as f32 * (side + gap) - gap;
        if h <= max_h {
            return h;
        }
        side -= 1.0;
    }
    max_h
}

/// Articles per cell `grid` uses for `map` in `size` (for the legend).
pub fn per_cell(map: &ArticleMap, size: Vec2) -> usize {
    let n = map.len();
    match GridLayout::for_articles(n, size) {
        Some(l) if l.capacity < n => n.div_ceil(l.capacity),
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_jobs_get_large_cells_and_big_ones_fold() {
        let size = Vec2::new(300.0, 100.0);
        let l = GridLayout::for_articles(40, size).unwrap();
        assert_eq!(l.side, MAX_SIDE);
        assert!(l.capacity >= 40);
        // 1,500 articles fit one per cell at 3 px (75 × 25 cells).
        let l = GridLayout::for_articles(1_500, size).unwrap();
        assert!(l.side == 3.0 && l.capacity >= 1_500, "{l:?}");
        // 5,000 do not fit even at 2 px (100 × 33), so cells fold.
        let l = GridLayout::for_articles(5_000, size).unwrap();
        assert!(l.side == FOLDED_SIDE && l.capacity < 5_000, "{l:?}");
        let l = GridLayout::for_articles(100_000, size).unwrap();
        assert_eq!(l.side, FOLDED_SIDE);
        assert!(l.capacity < 100_000);
        assert!(GridLayout::for_articles(10, Vec2::ZERO).is_none());
        assert!(GridLayout::for_articles(10, Vec2::new(f32::NAN, 5.0)).is_none());
    }

    #[test]
    fn grids_take_only_the_rows_they_need() {
        // 40 articles at 9 px in 300 px: 27 per row, two rows.
        assert_eq!(fit_height(40, 300.0, 120.0), 2.0 * 11.0 - 2.0);
        let h = fit_height(2_000, 300.0, 120.0);
        assert!(h <= 120.0);
        let l = GridLayout::for_articles(2_000, Vec2::new(300.0, h)).unwrap();
        assert!(l.capacity >= 2_000, "the fitted height still holds every article");
        assert_eq!(fit_height(1_000_000, 300.0, 120.0), 120.0);
    }

    #[test]
    fn cells_are_found_under_the_pointer() {
        let l = GridLayout::for_articles(40, Vec2::new(300.0, 100.0)).unwrap();
        let o = Pos2::new(10.0, 10.0);
        assert_eq!(l.cell_at(o, l.cell(o, 7).center(), 40), Some(7));
        assert_eq!(l.cell_at(o, Pos2::new(5.0, 5.0), 40), None);
        // The gap between cells belongs to no cell.
        let gap = l.cell(o, 0).right() + l.gap / 2.0;
        assert_eq!(l.cell_at(o, Pos2::new(gap, o.y + 1.0), 40), None);
    }

    #[test]
    fn cell_colors_follow_the_worst_state() {
        let pal = Palette::from_theme(&newskite_theme::fallback(true));
        let c = |done, missing, waiting| ArticleCounts { done, missing, waiting };
        assert_eq!(cell_color(&pal, &c(3, 0, 0)), pal.positive);
        assert_eq!(cell_color(&pal, &c(2, 1, 0)), pal.negative);
        assert_eq!(cell_color(&pal, &c(0, 0, 3)), pal.well);
        let partial = cell_color(&pal, &c(1, 0, 2));
        assert!(partial != pal.well && partial != pal.positive);
    }

    #[test]
    fn summaries_count_honestly() {
        assert_eq!(summary(&ArticleCounts { done: 5412, missing: 3, waiting: 185 }), "5,412 of 5,600 articles  ·  3 missing");
        assert_eq!(summary(&ArticleCounts { done: 0, missing: 0, waiting: 8 }), "0 of 8 articles");
    }
}
