//! Article cells: a job's articles drawn as small cells in file and segment order,
//! downloaded in the accent, waiting as quiet wells, missing on every server as a cell
//! outlined in the negative color. A held job's downloaded cells rest in the muted tone.
//! Three shapes share the rules: the [`grid`] (the whole map, square cells), the [`strip`]
//! under each job row (one thin line of cells) and the per-file [`lanes`]. When there are
//! more articles than cells, a cell holds several and says so: it shows missing when any
//! of them is missing and is shaded by how much of it arrived. Cells that just filled
//! glow briefly while motion is allowed. Every cell comes from the service's article map,
//! which only holds what the engine reported: nothing here is estimated, and no cell
//! claims an article is being fetched (the engine does not report that).

use eframe::egui::{self, epaint, Color32, Id, Pos2, Rect, Response, RichText, Sense, Ui, Vec2};
use newskite_service::{ArticleCounts, ArticleMap, ArticleState};

use super::fonts;
use super::motion;
use super::serverstrip::thousands;
use super::theme::Palette;

/// Largest and smallest square cell of a grid, px. Smaller cells blur into texture, so a
/// job with more articles than smallest cells folds several articles into each.
const MAX_SIDE: f32 = 9.0;
const MIN_SIDE: f32 = 6.0;
/// Gap between grid cells, px.
const GAP: f32 = 2.0;
/// Most files a hover over a folded cell names.
const HOVER_FILES: usize = 3;
/// Seconds a freshly filled cell glows.
const GLOW_SECS: f64 = 0.9;
/// Buckets a job's map is folded into for its row strip (drawn with fewer cells when the
/// row is narrower).
pub const STRIP_BUCKETS: usize = 240;
/// Height of a row strip, its cell pitch (cell plus gap) and most cells, px.
pub const STRIP_H: f32 = 5.0;
const STRIP_PITCH: f32 = 8.0;
const STRIP_MAX_CELLS: usize = 160;
/// Height of one per-file lane and its cell pitch, px.
const LANE_H: f32 = 8.0;
const LANE_PITCH: f32 = 6.0;
/// Most lanes drawn; the rest are summed up in one line.
pub const MAX_LANES: usize = 24;

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
        let cols = ((size.x + GAP) / (side + GAP)).floor().max(0.0) as usize;
        let rows = ((size.y + GAP) / (side + GAP)).floor().max(0.0) as usize;
        Self { side, gap: GAP, cols, capacity: cols * rows }
    }

    /// The largest cells (9 px down to 6 px) at which each of `n` articles gets its own
    /// cell in `size`; when even 6 px cells cannot hold them all, 6 px cells that each
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
        let l = Self::at(MIN_SIDE, size);
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

/// The fill of a cell with these counts (glow aside). Missing cells are drawn by
/// [`paint_cell`] as an outline; their fill here is the negative color for cells too
/// small to outline. `held` rests downloaded articles in the muted tone.
pub fn cell_color(pal: &Palette, c: &ArticleCounts, held: bool) -> Color32 {
    let done = if held { pal.faint } else { pal.accent };
    if c.missing > 0 {
        pal.negative
    } else if c.total() > 0 && c.done == c.total() {
        done
    } else if c.done > 0 {
        // Partly arrived: the downloaded color over the well, as strong as the share.
        let share = c.done as f32 / c.total() as f32;
        pal.well.lerp_to_gamma(done, 0.3 + 0.55 * share)
    } else {
        pal.well
    }
}

/// Adds one cell to `mesh`: a filled rectangle, or for a missing cell an outline in the
/// negative color around `ground` (cells under 5 px are filled instead, and grown by a
/// pixel so a handful among thousands still show).
fn paint_cell(mesh: &mut epaint::Mesh, pal: &Palette, rect: Rect, c: &ArticleCounts, fill: Color32, ground: Color32) {
    if c.missing == 0 {
        mesh.add_colored_rect(rect, fill);
    } else if rect.width().min(rect.height()) >= 5.0 {
        mesh.add_colored_rect(rect, pal.negative);
        mesh.add_colored_rect(rect.shrink(1.25), ground);
    } else {
        mesh.add_colored_rect(rect.expand(1.0), pal.negative);
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

/// The files a cell of `count` articles starting at global article `start` (in file
/// order) covers: each file's name, the cell's articles in it as 0-based `first..end`, and
/// the file's article count. A folded cell can span the end of one file and the start of
/// the next.
fn cell_files(map: &ArticleMap, start: usize, count: usize) -> Vec<(&str, usize, usize, usize)> {
    let end = start + count;
    let mut out = Vec::new();
    let mut offset = 0;
    for f in &map.files {
        let len = f.states.len();
        let (a, b) = (start.max(offset), end.min(offset + len));
        if a < b {
            out.push((f.name.as_str(), a - offset, b - offset, len));
        }
        offset += len;
        if offset >= end {
            break;
        }
    }
    out
}

/// What hovering cell `cell` (of `per` articles, counted in `c`) says: the file and state
/// of a single article, or for a folded cell the articles of each file it covers and their
/// counts.
fn hover_text(map: &ArticleMap, cell: usize, per: usize, c: &ArticleCounts) -> String {
    let parts = cell_files(map, cell * per, c.total());
    if per == 1 {
        let Some((name, k, _, len)) = parts.first() else { return String::new() };
        let state = if c.missing > 0 { "missing" } else if c.done > 0 { "downloaded" } else { "waiting" };
        return format!("{name}\narticle {} of {}: {state}", thousands(*k as u64 + 1), thousands(*len as u64));
    }
    let mut lines: Vec<String> = parts
        .iter()
        .take(HOVER_FILES)
        .map(|(name, a, b, len)| {
            let span = if b - a == 1 {
                format!("article {}", thousands(*a as u64 + 1))
            } else {
                format!("articles {}–{}", thousands(*a as u64 + 1), thousands(*b as u64))
            };
            format!("{name}  ·  {span} of {}", thousands(*len as u64))
        })
        .collect();
    if parts.len() > HOVER_FILES {
        let more = parts.len() - HOVER_FILES;
        lines.push(format!("+{more} more file{}", if more == 1 { "" } else { "s" }));
    }
    lines.push(format!("{} downloaded, {} missing, {} waiting", c.done, c.missing, c.waiting));
    lines.join("\n")
}

/// Draws `map` as a grid filling `size` on `ground` (the color behind it). `id` keeps the
/// grid's memory for the glow of freshly filled cells, which is skipped under reduced
/// motion. Hovering a cell names its file and articles.
#[allow(clippy::too_many_arguments)]
pub fn grid(
    ui: &mut Ui,
    pal: &Palette,
    id: Id,
    map: &ArticleMap,
    size: Vec2,
    held: bool,
    ground: Color32,
    reduced: bool,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let counts = map.counts();
    let label = format!(
        "Article map: {} downloaded, {} missing, {} waiting",
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

    // Cells whose downloaded count grew since the last frame start to glow.
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

    // One mesh for every cell: thousands of rectangles stay cheap. Missing cells go last
    // so their grown fill is not covered by a neighbor.
    let shine = if pal.dark { pal.text } else { pal.raised };
    let mut mesh = epaint::Mesh::default();
    let mut missing = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if c.missing > 0 {
            missing.push(i);
            continue;
        }
        let mut color = cell_color(pal, c, held);
        if glow[i] > 0.0 {
            color = color.lerp_to_gamma(shine, 0.6 * glow[i] * glow[i]);
        }
        mesh.add_colored_rect(layout.cell(rect.min, i), color);
    }
    for i in missing {
        paint_cell(&mut mesh, pal, layout.cell(rect.min, i), &cells[i], pal.negative, ground);
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
        let text = hover_text(map, i, per, &cells[i]);
        if !text.is_empty() {
            return resp.on_hover_text_at_pointer(text);
        }
    }
    resp
}

/// `cells` (equal buckets of a job's articles, in order) merged into `n` cells of about
/// equal share. Pure, for tests.
pub fn merge_cells(cells: &[ArticleCounts], n: usize) -> Vec<ArticleCounts> {
    let len = cells.len();
    if n == 0 || len == 0 {
        return Vec::new();
    }
    if n >= len {
        return cells.to_vec();
    }
    (0..n)
        .map(|j| {
            let (a, b) = (j * len / n, ((j + 1) * len / n).max(j * len / n + 1));
            cells[a..b.min(len)].iter().fold(ArticleCounts::default(), |acc, c| ArticleCounts {
                done: acc.done + c.done,
                missing: acc.missing + c.missing,
                waiting: acc.waiting + c.waiting,
            })
        })
        .collect()
}

/// The thin strip under a job row: `cells` (the job's map in [`STRIP_BUCKETS`] buckets)
/// as one line of cells across `width`, on `ground`. `label` is read out for it.
pub fn strip(ui: &mut Ui, pal: &Palette, cells: &[ArticleCounts], held: bool, width: f32, ground: Color32, label: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, STRIP_H), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, label));
    if !ui.is_rect_visible(rect) || cells.is_empty() {
        return resp;
    }
    let n = (((width + 1.5) / STRIP_PITCH).floor().max(1.0) as usize).min(STRIP_MAX_CELLS);
    let merged = merge_cells(cells, n);
    let w = (width - 1.5 * (merged.len() as f32 - 1.0)) / merged.len() as f32;
    let mut mesh = epaint::Mesh::default();
    for (i, c) in merged.iter().enumerate() {
        let x = rect.left() + i as f32 * (w + 1.5);
        let r = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(w, STRIP_H));
        paint_cell(&mut mesh, pal, r, c, cell_color(pal, c, held), ground);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    resp
}

/// A plain progress line of the strip's height, for jobs without an article map (never
/// run since maps were introduced): it claims nothing about which articles arrived.
pub fn progress_line(ui: &mut Ui, pal: &Palette, fraction: f32, held: bool, width: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, STRIP_H), Sense::hover());
    if ui.is_rect_visible(rect) {
        let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
        let r = egui::CornerRadius::same(2);
        ui.painter().rect_filled(rect, r, pal.well);
        if f > 0.0 {
            let mut done = rect;
            done.set_width((rect.width() * f).max(STRIP_H));
            ui.painter().rect_filled(done, r, if held { pal.faint } else { pal.accent });
        }
    }
    resp
}

/// One lane per file (B's file lanes): the file's name, its articles as a line of cells
/// and `"n / N"` downloaded, in NZB order. Complete files are named in the muted tone.
/// At most [`MAX_LANES`] lanes; the rest are summed up underneath.
pub fn lanes(ui: &mut Ui, pal: &Palette, map: &ArticleMap, held: bool, ground: Color32) {
    let name_w = (ui.available_width() * 0.34).clamp(90.0, 190.0);
    let fig_w = 92.0;
    for f in map.files.iter().take(MAX_LANES) {
        let c = f.states.iter().fold(ArticleCounts::default(), |mut acc, s| {
            match s {
                ArticleState::Done => acc.done += 1,
                ArticleState::Missing => acc.missing += 1,
                ArticleState::Waiting => acc.waiting += 1,
            }
            acc
        });
        let complete = c.total() > 0 && c.done == c.total();
        let figure = format!("{} / {}", thousands(c.done as u64), thousands(c.total() as u64));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.allocate_ui_with_layout(Vec2::new(name_w, 16.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&f.name).font(fonts::mono_id(11.0)).color(if complete { pal.muted } else { pal.text }),
                    )
                    .truncate(),
                );
            });
            let cells_w = (ui.available_width() - fig_w - 10.0).max(24.0);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(cells_w, LANE_H), Sense::hover());
            if ui.is_rect_visible(rect) {
                let n = ((cells_w + 2.0) / LANE_PITCH).floor().max(1.0) as usize;
                let fine = ArticleMap { files: vec![f.clone()] }.cells(n).1;
                let w = (cells_w - 2.0 * (fine.len() as f32 - 1.0)) / fine.len().max(1) as f32;
                let mut mesh = epaint::Mesh::default();
                for (i, cc) in fine.iter().enumerate() {
                    let r = Rect::from_min_size(Pos2::new(rect.left() + i as f32 * (w + 2.0), rect.top()), Vec2::new(w, LANE_H));
                    paint_cell(&mut mesh, pal, r, cc, cell_color(pal, cc, held), ground);
                }
                ui.painter().add(egui::Shape::mesh(mesh));
            }
            let label = format!("{}: {figure} articles", f.name);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, &label));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(figure).font(fonts::mono_id(11.0)).color(pal.muted));
            });
        });
    }
    let more = map.files.len().saturating_sub(MAX_LANES);
    if more > 0 {
        ui.label(
            RichText::new(format!("+{more} more file{}", if more == 1 { "" } else { "s" }))
                .font(fonts::mono_id(11.0))
                .color(pal.faint),
        );
    }
}

/// The height a grid of `n` articles needs at `width`, at most `max_h`: the largest cells
/// that fit, with no empty rows below them. When articles fold, only the rows the folded
/// cells fill.
pub fn fit_height(n: usize, width: f32, max_h: f32) -> f32 {
    let mut side = MAX_SIDE;
    while side >= MIN_SIDE {
        let cols = ((width + GAP) / (side + GAP)).floor().max(1.0) as usize;
        let rows = n.div_ceil(cols).max(1);
        let h = rows as f32 * (side + GAP) - GAP;
        if h <= max_h {
            return h;
        }
        side -= 1.0;
    }
    let Some(l) = GridLayout::for_articles(n, Vec2::new(width, max_h)) else { return max_h };
    let cells = n.div_ceil(n.div_ceil(l.capacity));
    cells.div_ceil(l.cols).max(1) as f32 * (l.side + GAP) - GAP
}

/// Articles per cell `grid` uses for `map` in `size` (for the key).
pub fn per_cell(map: &ArticleMap, size: Vec2) -> usize {
    let n = map.len();
    match GridLayout::for_articles(n, size) {
        Some(l) if l.capacity < n => n.div_ceil(l.capacity),
        _ => 1,
    }
}

/// The key: a swatch and word for each state (missing as its outline).
pub fn legend(ui: &mut Ui, pal: &Palette, ground: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (state, word) in [(0, "downloaded"), (1, "missing"), (2, "waiting")] {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
            let painter = ui.painter();
            match state {
                0 => {
                    painter.rect_filled(r, egui::CornerRadius::same(2), pal.accent);
                }
                1 => {
                    painter.rect_filled(r, egui::CornerRadius::same(2), pal.negative);
                    painter.rect_filled(r.shrink(1.5), egui::CornerRadius::same(1), ground);
                }
                _ => {
                    painter.rect_filled(r, egui::CornerRadius::same(2), pal.well);
                }
            }
            ui.label(RichText::new(word).font(fonts::mono_id(10.5)).color(pal.muted));
            ui.add_space(8.0);
        }
    });
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
        // 400 articles fit one per cell at 6 px (37 × 12 cells).
        let l = GridLayout::for_articles(400, size).unwrap();
        assert!(l.side == MIN_SIDE && l.capacity >= 400, "{l:?}");
        // 1,500 do not, and cells never shrink below 6 px: they fold instead.
        let l = GridLayout::for_articles(1_500, size).unwrap();
        assert!(l.side == MIN_SIDE && l.capacity < 1_500, "{l:?}");
        assert!(GridLayout::for_articles(10, Vec2::ZERO).is_none());
        assert!(GridLayout::for_articles(10, Vec2::new(f32::NAN, 5.0)).is_none());
    }

    #[test]
    fn grids_take_only_the_rows_they_need() {
        // 40 articles at 9 px in 300 px: 27 per row, two rows.
        assert_eq!(fit_height(40, 300.0, 120.0), 2.0 * 11.0 - 2.0);
        // 2,000 fold: 4 per cell at most height, 500 cells in 14 of the 15 rows.
        let full = per_cell(&map_of(2_000), Vec2::new(300.0, 120.0));
        let h = fit_height(2_000, 300.0, 120.0);
        assert_eq!(h, 14.0 * 8.0 - 2.0);
        assert_eq!(per_cell(&map_of(2_000), Vec2::new(300.0, h)), full, "the fitted height folds the same");
        assert!(fit_height(1_000_000, 300.0, 120.0) <= 120.0);
    }

    fn map_of(n: usize) -> ArticleMap {
        ArticleMap { files: vec![newskite_service::FileArticles { name: "a.rar".into(), states: vec![ArticleState::Waiting; n] }] }
    }

    #[test]
    fn folded_cells_name_every_file_they_span() {
        use newskite_service::FileArticles;
        let file = |name: &str, states: Vec<ArticleState>| FileArticles { name: name.into(), states };
        let (d, w) = (ArticleState::Done, ArticleState::Waiting);
        let map = ArticleMap {
            files: vec![file("a.rar", vec![d; 5]), file("b.rar", vec![d, w, w]), file("c.par2", vec![w])],
        };
        assert_eq!(cell_files(&map, 4, 3), vec![("a.rar", 4, 5, 5), ("b.rar", 0, 2, 3)]);
        assert_eq!(cell_files(&map, 6, 3), vec![("b.rar", 1, 3, 3), ("c.par2", 0, 1, 1)]);
        let (per, cells) = map.cells(3);
        assert_eq!(per, 3);
        assert_eq!(
            hover_text(&map, 1, per, &cells[1]),
            "a.rar  ·  articles 4–5 of 5\nb.rar  ·  article 1 of 3\n3 downloaded, 0 missing, 0 waiting"
        );
        let (_, single) = map.cells(9);
        assert_eq!(hover_text(&map, 6, 1, &single[6]), "b.rar\narticle 2 of 3: waiting");
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
        assert_eq!(cell_color(&pal, &c(3, 0, 0), false), pal.accent);
        assert_eq!(cell_color(&pal, &c(3, 0, 0), true), pal.faint, "held rests in the muted tone");
        assert_eq!(cell_color(&pal, &c(2, 1, 0), false), pal.negative);
        assert_eq!(cell_color(&pal, &c(0, 0, 3), false), pal.well);
        let partial = cell_color(&pal, &c(1, 0, 2), false);
        assert!(partial != pal.well && partial != pal.accent);
    }

    #[test]
    fn merged_cells_keep_every_article_in_order() {
        let c = |done, missing, waiting| ArticleCounts { done, missing, waiting };
        let cells = vec![c(2, 0, 0), c(1, 1, 0), c(0, 0, 2), c(0, 0, 2), c(2, 0, 0)];
        let m = merge_cells(&cells, 2);
        assert_eq!(m.len(), 2);
        assert_eq!(m.iter().map(ArticleCounts::total).sum::<usize>(), 10);
        assert_eq!(m[0], c(3, 1, 0));
        assert_eq!(merge_cells(&cells, 10), cells, "never more cells than buckets");
        assert!(merge_cells(&cells, 0).is_empty());
        assert!(merge_cells(&[], 4).is_empty());
    }

    #[test]
    fn summaries_count_honestly() {
        assert_eq!(summary(&ArticleCounts { done: 5412, missing: 3, waiting: 185 }), "5,412 of 5,600 articles  ·  3 missing");
        assert_eq!(summary(&ArticleCounts { done: 0, missing: 0, waiting: 8 }), "0 of 8 articles");
    }
}
