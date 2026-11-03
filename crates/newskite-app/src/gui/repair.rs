//! The repair budget: a job's PAR2 recovery blocks as a line of cells with the damage drawn
//! against them, from the engine's [`RepairOutlook`] (nothing here is estimated beyond what
//! the outlook says; an upper bound is marked `≤`).
//!
//! Cells run left to right: **damaged** slices, each using up a block (the warning tone);
//! **spare** blocks; a limit tick where the blocks that can still arrive end; then the
//! **short** blocks a hopeless job would need beyond it (the negative tone) and the
//! **lost** blocks, posted but gone with missing PAR2 articles (hollow). With more blocks
//! than cells, a cell holds several and shows the most serious kind among them. The fill
//! glides to each new figure while motion is allowed.

use eframe::egui::{self, epaint, Align, Color32, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use newskite_service::RepairOutlook;

use super::fonts;
use super::serverstrip::thousands;
use super::theme::Palette;
use super::widgets::section_title;

/// Height of a budget cell, its gap and its widest and narrowest width, px.
const CELL_H: f32 = 12.0;
const GAP: f32 = 2.0;
const MAX_CELL_W: f32 = 14.0;
const MIN_CELL_W: f32 = 4.0;
/// Share of the blocks the damage may use before the budget reads as tight.
const TIGHT: f32 = 0.75;

/// The state of a job's repair budget in one word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No PAR2 read yet.
    Waiting,
    /// Every file identified, none of them PAR2.
    NoPar2,
    /// Nothing damaged.
    Intact,
    Repairable,
    /// Most of the blocks are used up.
    Tight,
    /// More is damaged than can be rebuilt.
    Hopeless,
}

impl Verdict {
    pub fn of(o: &RepairOutlook) -> Self {
        if !o.known {
            if o.no_par2 {
                Verdict::NoPar2
            } else {
                Verdict::Waiting
            }
        } else if o.hopeless() {
            Verdict::Hopeless
        } else if o.needed == 0 {
            Verdict::Intact
        } else if o.used() >= TIGHT {
            Verdict::Tight
        } else {
            Verdict::Repairable
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Verdict::Waiting => "Waiting for PAR2",
            Verdict::NoPar2 => "No PAR2",
            Verdict::Intact => "Intact",
            Verdict::Repairable => "Repairable",
            Verdict::Tight => "Tight",
            Verdict::Hopeless => "Can't be repaired",
        }
    }

    /// Text color and chip fill.
    pub fn colors(self, pal: &Palette) -> (Color32, Color32) {
        match self {
            Verdict::Waiting | Verdict::NoPar2 => (pal.muted, pal.well),
            Verdict::Intact | Verdict::Repairable => (pal.positive, pal.positive_soft),
            Verdict::Tight => (pal.warning, pal.warning_soft),
            Verdict::Hopeless => (pal.negative, pal.negative_soft),
        }
    }
}

/// What a budget cell stands for, least to most serious.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Spare,
    Lost,
    Damaged,
    Short,
}

/// The bar in blocks: damaged (within the budget), spare, short (beyond it) and lost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub damaged: f32,
    pub spare: f32,
    pub short: f32,
    pub lost: f32,
}

impl Bar {
    /// `needed` is the (possibly gliding) damage; the rest comes from `o`.
    pub fn new(o: &RepairOutlook, needed: f32) -> Self {
        let available = o.available as f32;
        let needed = needed.max(0.0);
        Self {
            damaged: needed.min(available),
            spare: (available - needed).max(0.0),
            short: (needed - available).max(0.0),
            lost: o.posted.saturating_sub(o.available) as f32,
        }
    }

    pub fn total(&self) -> f32 {
        self.damaged + self.spare + self.short + self.lost
    }

    /// Blocks where the spare ones end: the limit tick.
    pub fn limit(&self) -> f32 {
        self.damaged + self.spare
    }

    /// The kind of each of `cells` cells over the bar: the most serious kind any block in
    /// the cell has (a cell partly damaged shows damaged).
    pub fn cells(&self, cells: usize) -> Vec<Kind> {
        let total = self.total();
        if cells == 0 || total <= 0.0 {
            return Vec::new();
        }
        let per = total / cells as f32;
        let edges = [
            (self.damaged, Kind::Damaged),
            (self.limit(), Kind::Spare),
            (self.limit() + self.short, Kind::Short),
            (total, Kind::Lost),
        ];
        (0..cells)
            .map(|i| {
                let (a, b) = (i as f32 * per, (i + 1) as f32 * per);
                let mut start = 0.0;
                let mut worst = None;
                for &(end, kind) in &edges {
                    // A segment overlaps the cell by more than a sliver.
                    if end - start > 1e-3 && start < b - 1e-3 && end > a + 1e-3 {
                        worst = worst.max(Some(kind));
                    }
                    start = end;
                }
                worst.unwrap_or(Kind::Spare)
            })
            .collect()
    }
}

/// Cells a bar of `blocks` blocks gets in `width`: one per block when they fit at the
/// narrowest width, else as many as fit. Returns (cells, blocks per cell).
pub fn layout(blocks: f32, width: f32) -> (usize, usize) {
    let blocks = blocks.ceil().max(0.0) as usize;
    let fit = (((width + GAP) / (MIN_CELL_W + GAP)).floor().max(1.0)) as usize;
    if blocks <= fit {
        (blocks, 1)
    } else {
        let per = blocks.div_ceil(fit);
        (blocks.div_ceil(per), per)
    }
}

/// The one-line reading under the bar.
pub fn reading(o: &RepairOutlook) -> String {
    let up_to = if o.exact { "" } else { "up to " };
    match Verdict::of(o) {
        Verdict::Waiting => "Measured once the PAR2 index arrives.".to_owned(),
        Verdict::NoPar2 => "This job posts no recovery data, so a missing article cannot be rebuilt.".to_owned(),
        Verdict::Intact => format!("No damage so far; {up_to}{} recovery blocks to spare.", thousands(o.available)),
        Verdict::Repairable | Verdict::Tight => format!(
            "{} of {} slices damaged; {up_to}{} recovery blocks to spare.",
            thousands(o.needed),
            thousands(o.blocks),
            thousands(o.available - o.needed)
        ),
        Verdict::Hopeless => format!(
            "{} slices damaged, but at most {} recovery blocks can arrive: {} short.",
            thousands(o.needed),
            thousands(o.available),
            thousands(o.needed - o.available)
        ),
    }
}

/// The Repair budget section of the inspector.
pub fn section(ui: &mut Ui, pal: &Palette, id: egui::Id, o: &RepairOutlook, ground: Color32, reduced: bool) {
    let verdict = Verdict::of(o);
    let (tone, soft) = verdict.colors(pal);
    let needed = super::motion::ease_to(ui.ctx(), id.with("needed"), o.needed as f32, 0.5, reduced);
    let bar = Bar::new(o, needed);
    let width = ui.available_width();
    let (cells, per) = layout(bar.total(), width);
    ui.horizontal(|ui| {
        section_title(ui, pal, "Repair budget");
        if per > 1 {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("1 cell = {per} blocks")).font(fonts::mono_id(10.5)).color(pal.faint));
            });
        }
    });
    ui.add_space(6.0);

    // Headline: damaged over available, and the verdict as a chip.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if o.known {
            ui.label(RichText::new(thousands(needed.round() as u64)).font(fonts::mono_id(20.0)).color(tone));
            let avail = format!("/ {}{}", if o.exact { "" } else { "≤" }, thousands(o.available));
            ui.label(RichText::new(avail).font(fonts::mono_id(13.0)).color(pal.muted));
            ui.label(RichText::new("BLOCKS NEEDED / AVAILABLE").font(fonts::mono_id(9.5)).color(pal.faint));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let galley = ui.painter().layout_no_wrap(verdict.word().to_owned(), fonts::semibold_id(11.5), tone);
            let size = galley.size() + Vec2::new(16.0, 8.0);
            let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, verdict.word()));
            ui.painter().rect_filled(r, CornerRadius::same(255), soft);
            ui.painter().galley(r.center() - galley.size() / 2.0, galley, tone);
        });
    });

    if cells > 0 {
        ui.add_space(6.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, CELL_H + 6.0), Sense::hover());
        let label = format!(
            "Repair budget: {} damaged, {} spare, {} short, {} lost blocks",
            thousands(bar.damaged.round() as u64),
            thousands(bar.spare.round() as u64),
            thousands(bar.short.round() as u64),
            thousands(bar.lost.round() as u64)
        );
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, &label));
        if ui.is_rect_visible(rect) {
            paint_bar(ui.painter(), pal, rect, &bar, cells, ground);
        }
    }
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(reading(o)).small().color(pal.muted)).wrap());
    if cells > 0 {
        legend(ui, pal, &bar, ground);
    }
}

fn paint_bar(painter: &egui::Painter, pal: &Palette, rect: Rect, bar: &Bar, cells: usize, ground: Color32) {
    let kinds = bar.cells(cells);
    let n = kinds.len() as f32;
    let w = ((rect.width() - GAP * (n - 1.0)) / n).min(MAX_CELL_W);
    let top = rect.top() + 3.0;
    let mut mesh = epaint::Mesh::default();
    let mut limit_x = None;
    let per = bar.total() / n;
    for (i, kind) in kinds.iter().enumerate() {
        let x = rect.left() + i as f32 * (w + GAP);
        let r = Rect::from_min_size(Pos2::new(x, top), Vec2::new(w, CELL_H));
        match kind {
            Kind::Damaged => mesh.add_colored_rect(r, pal.warning),
            Kind::Spare => mesh.add_colored_rect(r, pal.well.lerp_to_gamma(pal.positive, 0.55)),
            Kind::Short => mesh.add_colored_rect(r, pal.negative),
            Kind::Lost => {
                mesh.add_colored_rect(r, pal.faint);
                mesh.add_colored_rect(r.shrink(1.25), ground);
            }
        }
        // The tick sits in the gap after the last cell inside the limit.
        if limit_x.is_none() && (i + 1) as f32 * per >= bar.limit() - 1e-3 && i + 1 < kinds.len() {
            limit_x = Some(x + w + GAP / 2.0);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
    if let Some(x) = limit_x.filter(|_| bar.short + bar.lost > 0.0) {
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.5, pal.text));
    }
}

fn legend(ui: &mut Ui, pal: &Palette, bar: &Bar, ground: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let items = [
            (Kind::Damaged, "damaged", bar.damaged > 0.0),
            (Kind::Spare, "spare", bar.spare > 0.0),
            (Kind::Short, "short", bar.short > 0.0),
            (Kind::Lost, "lost", bar.lost > 0.0),
        ];
        for (kind, word, show) in items {
            if !show {
                continue;
            }
            let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
            let p = ui.painter();
            let round = CornerRadius::same(2);
            match kind {
                Kind::Damaged => p.rect_filled(r, round, pal.warning),
                Kind::Spare => p.rect_filled(r, round, pal.well.lerp_to_gamma(pal.positive, 0.55)),
                Kind::Short => p.rect_filled(r, round, pal.negative),
                Kind::Lost => {
                    p.rect_filled(r, round, pal.faint);
                    p.rect_filled(r.shrink(1.5), CornerRadius::same(1), ground)
                }
            };
            ui.label(RichText::new(word).font(fonts::mono_id(10.5)).color(pal.muted));
            ui.add_space(8.0);
        }
    });
}

/// A queue row's compact reading: a short bar (damage over the blocks that can arrive) and
/// `"12 / 48 blocks"`, or why the job cannot be repaired. Only for jobs with damage.
pub fn row_reading(ui: &mut Ui, pal: &Palette, o: &RepairOutlook) {
    let verdict = Verdict::of(o);
    if !o.known || o.needed == 0 {
        return;
    }
    let (tone, _) = verdict.colors(pal);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 6.0), Sense::hover());
    let round = CornerRadius::same(3);
    ui.painter().rect_filled(rect, round, pal.well);
    let f = (o.used()).clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * f).max(rect.height()));
        ui.painter().rect_filled(fill, round, if verdict == Verdict::Hopeless { pal.negative } else { pal.warning });
    }
    ui.add_space(6.0);
    let text = if verdict == Verdict::Hopeless {
        "can't be repaired".to_owned()
    } else {
        format!("{} / {}{} recovery blocks", thousands(o.needed), if o.exact { "" } else { "≤" }, thousands(o.available))
    };
    ui.label(RichText::new(text).font(fonts::mono_id(11.0)).color(tone));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(needed: u64, available: u64, posted: u64) -> RepairOutlook {
        RepairOutlook { known: true, blocks: 100, needed, available, posted, exact: true, ..Default::default() }
    }

    #[test]
    fn verdicts() {
        assert_eq!(Verdict::of(&RepairOutlook::default()), Verdict::Waiting);
        assert_eq!(Verdict::of(&RepairOutlook { no_par2: true, ..Default::default() }), Verdict::NoPar2);
        assert_eq!(Verdict::of(&o(0, 10, 10)), Verdict::Intact);
        assert_eq!(Verdict::of(&o(5, 10, 10)), Verdict::Repairable);
        assert_eq!(Verdict::of(&o(8, 10, 10)), Verdict::Tight);
        assert_eq!(Verdict::of(&o(10, 10, 10)), Verdict::Tight, "exactly enough still repairs");
        assert_eq!(Verdict::of(&o(11, 10, 12)), Verdict::Hopeless);
    }

    #[test]
    fn bar_splits_blocks_and_cells_show_the_worst_kind() {
        let b = Bar::new(&o(3, 6, 8), 3.0);
        assert_eq!(b, Bar { damaged: 3.0, spare: 3.0, short: 0.0, lost: 2.0 });
        use Kind::*;
        assert_eq!(b.cells(8), vec![Damaged, Damaged, Damaged, Spare, Spare, Spare, Lost, Lost]);
        // Folded two blocks per cell; a cell straddling damaged and spare shows damaged.
        let b = Bar::new(&o(3, 6, 6), 3.0);
        assert_eq!(b.cells(3), vec![Damaged, Damaged, Spare]);
        let b = Bar::new(&o(9, 6, 8), 9.0);
        assert_eq!(b, Bar { damaged: 6.0, spare: 0.0, short: 3.0, lost: 2.0 });
        assert_eq!(b.cells(11), vec![Damaged, Damaged, Damaged, Damaged, Damaged, Damaged, Short, Short, Short, Lost, Lost]);
        assert!(Bar::new(&RepairOutlook::default(), 0.0).cells(5).is_empty());
    }

    #[test]
    fn layout_gives_a_cell_per_block_until_they_no_longer_fit() {
        assert_eq!(layout(12.0, 300.0), (12, 1));
        // 300 px holds 50 cells of 4 px plus gaps.
        assert_eq!(layout(50.0, 300.0), (50, 1));
        assert_eq!(layout(500.0, 300.0), (50, 10));
        assert_eq!(layout(0.0, 300.0), (0, 1));
    }

    #[test]
    fn readings_say_what_is_measured() {
        assert_eq!(reading(&o(2, 10, 10)), "2 of 100 slices damaged; 8 recovery blocks to spare.");
        let mut est = o(0, 1200, 1200);
        est.exact = false;
        assert_eq!(reading(&est), "No damage so far; up to 1,200 recovery blocks to spare.");
        assert_eq!(reading(&o(12, 10, 10)), "12 slices damaged, but at most 10 recovery blocks can arrive: 2 short.");
    }
}
