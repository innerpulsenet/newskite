//! The innerpulse mark, painted with the egui painter, and its two motions.
//!
//! Geometry (brand kit, `references/implementation.md` §1): six bars, 22u wide with 4u
//! corners, in a 220u box; bar `i` sits at `x_i = −92 + 36.8·i` and spans `−h_i−11 …
//! h_i+11` with `h_i = rest(84·|sin(2π·i/5 + φ)|)`, `φ = −π/30`. `rest()` is the 12u optical
//! floor with a cubic soft landing. The motions are copied from `pulse.mjs`
//! (`drawInHeights`, `beatHeights`): the draw-in once when the mark first appears, then the
//! beat (a 16u sin² bump passing left to right) only while there is a reason to move.
//! The bars are always one solid color: ember on dark grounds, the deep ember on light.

use std::f64::consts::PI;
use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Vec2};

/// Half the distance between the outer bars' centers, in mark units.
pub const SPAN: f64 = 92.0;
/// Amplitude of the sampled beat.
pub const AMP: f64 = 84.0;
/// The optical minimum (a rest).
pub const MIN_BAR: f64 = 12.0;
/// Above this a bar is its sample; between [`MIN_BAR`] and this, the soft landing.
pub const EASE: f64 = 30.0;
/// Samples per beat (six bars = five gaps).
pub const BEAT: f64 = 5.0;
/// The phase that puts a rest at both ends.
pub const PHASE: f64 = -PI / 30.0;
/// Bar width.
pub const BAR_W: f64 = 22.0;
/// Bar corner radius.
pub const CORNER: f64 = 4.0;
/// Side of the mark's box.
pub const VIEW: f64 = 220.0;

/// Seconds between beats of an ambient live mark (heroes, the About dialog).
pub const PERIOD_AMBIENT: f64 = 4.5;
/// The working tempo while a job is active but no bytes move (connecting, waiting).
pub const PERIOD_CALM: f64 = 2.4;
/// The working tempo while bytes are moving.
pub const PERIOD_BUSY: f64 = 1.2;
/// Every bar, stubs included, rises this much as the beat passes.
pub const RISE: f64 = 16.0;
/// A bar's rise lasts this fraction of the period.
pub const WIDTH: f64 = 0.19;
/// Each bar starts this fraction of the period after the one to its left.
pub const LAG: f64 = 0.0245;
/// Seconds each bar takes to grow from its stub to its rest on first appearance.
pub const DRAW_IN: f64 = 0.55;
/// Seconds between the start of one bar's draw-in and the next.
pub const DRAW_LAG: f64 = 0.09;
/// Repaint interval while the mark moves (40 fps).
pub const FRAME: Duration = Duration::from_millis(25);

/// The soft floor: [`MIN_BAR`] below it, `v` above [`EASE`], a cubic Hermite join between.
pub fn rest(v: f64) -> f64 {
    if v <= MIN_BAR {
        return MIN_BAR;
    }
    if v >= EASE {
        return v;
    }
    let w = EASE - MIN_BAR;
    let t = (v - MIN_BAR) / w;
    let (t2, t3) = (t * t, t * t * t);
    (2.0 * t3 - 3.0 * t2 + 1.0) * MIN_BAR + (-2.0 * t3 + 3.0 * t2) * EASE + (t3 - t2) * w
}

/// Center of bar `i` (0…5) in mark units.
pub fn bar_x(i: usize) -> f64 {
    -SPAN + 2.0 * SPAN * i as f64 / BEAT
}

/// The six resting half-heights: 12, 76.74, 56.21, 42, 82.16, 12.
pub fn rest_heights() -> [f64; 6] {
    std::array::from_fn(|i| rest(AMP * (2.0 * PI * i as f64 / BEAT + PHASE).sin().abs()))
}

/// Seconds the draw-in takes in all (≈ 1 s).
pub fn draw_in_duration() -> f64 {
    DRAW_IN + BEAT * DRAW_LAG
}

/// The draw-in `t` seconds after the mark appears: each bar from the stub to its rest,
/// eased out, bar `i` starting `i · DRAW_LAG` late (`drawInHeights` in `pulse.mjs`).
pub fn draw_in_heights(t: f64) -> [f64; 6] {
    let rest = rest_heights();
    std::array::from_fn(|i| {
        let u = ((t - i as f64 * DRAW_LAG) / DRAW_IN).clamp(0.0, 1.0);
        MIN_BAR + (rest[i] - MIN_BAR) * (1.0 - (1.0 - u).powi(3))
    })
}

/// The heights `t` seconds into a beat cycle of `period` seconds (`beatHeights` in
/// `pulse.mjs`): bar `i` rises by `RISE · sin²(πu)` while `0 < u < 1`, with
/// `u = (T − i·LAG·period) / (WIDTH·period)` and `T = t mod period`. A non-positive or
/// non-finite period (or time) gives the rest drawing.
pub fn beat_heights(t: f64, period: f64) -> [f64; 6] {
    let rest = rest_heights();
    if period.is_nan() || period <= 0.0 || !period.is_finite() || !t.is_finite() {
        return rest;
    }
    let tt = t.rem_euclid(period);
    std::array::from_fn(|i| {
        let u = (tt - i as f64 * LAG * period) / (WIDTH * period);
        if u > 0.0 && u < 1.0 {
            rest[i] + RISE * (PI * u).sin().powi(2)
        } else {
            rest[i]
        }
    })
}

/// Seconds into a beat cycle after which every bar has settled again.
pub fn pass_duration(period: f64) -> f64 {
    (5.0 * LAG + WIDTH) * period
}

/// Paints the mark in a square `rect` (the 220u box) in one solid `color`.
///
/// Bar edges are snapped to the physical pixel grid (whole-pixel width and pitch) so the
/// mark stays crisp at toolbar and credit sizes; heights stay continuous so the motion
/// is smooth.
pub fn paint_mark(painter: &egui::Painter, rect: Rect, heights: &[f64; 6], color: Color32) {
    let side = rect.width().min(rect.height());
    if side.is_nan() || side <= 0.0 {
        return;
    }
    let ppp = painter.pixels_per_point().max(0.25);
    let k = f64::from(side) / VIEW;
    let phys = |pts: f64| pts * f64::from(ppp);
    let bar_px = phys(BAR_W * k).round().max(1.0);
    let pitch_px = phys(2.0 * SPAN / BEAT * k).round().max(bar_px + 1.0);
    let total_px = BEAT * pitch_px + bar_px;
    let center = rect.center();
    let left_px = (phys(f64::from(center.x)) - total_px / 2.0).round();
    let radius_px = phys(CORNER * k);
    let corner = if radius_px >= 1.0 {
        CornerRadius::same((CORNER * k).round().clamp(1.0, 255.0) as u8)
    } else {
        CornerRadius::ZERO
    };
    for (i, h) in heights.iter().enumerate() {
        let h = if h.is_finite() { h.max(0.0) } else { MIN_BAR };
        let x0 = (left_px + i as f64 * pitch_px) / f64::from(ppp);
        let half = (h + BAR_W / 2.0) * k;
        let r = Rect::from_min_size(
            Pos2::new(x0 as f32, center.y - half as f32),
            Vec2::new((bar_px / f64::from(ppp)) as f32, (2.0 * half) as f32),
        );
        painter.rect_filled(r, corner, color);
    }
}

/// Drives a live mark: the draw-in once, then the beat while a tempo is requested.
///
/// A pass that has started always finishes before the mark rests or changes tempo, so
/// the motion never jumps. Time is the egui input clock in seconds.
#[derive(Clone, Debug, Default)]
pub struct MarkClock {
    born: Option<f64>,
    beat: Option<(f64, f64)>,
    skip_draw_in: bool,
}

impl MarkClock {
    /// A clock whose mark draws in when first shown.
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock whose mark starts at rest (no draw-in).
    pub fn without_draw_in() -> Self {
        Self {
            skip_draw_in: true,
            ..Self::default()
        }
    }

    /// Restarts the clock (the draw-in plays again on the next frame).
    pub fn reset(&mut self) {
        self.born = None;
        self.beat = None;
    }

    /// Whether the draw-in is still playing at `now`.
    pub fn drawing_in(&self, now: f64) -> bool {
        !self.skip_draw_in && self.born.is_some_and(|b| now - b < draw_in_duration())
    }

    /// The heights at `now`, and when to repaint next (`None`: the mark rests and needs no
    /// repaint). `tempo` is the beat period wanted now (`None`: rest). Under reduced
    /// motion the mark always shows its rest drawing.
    pub fn frame(
        &mut self,
        now: f64,
        tempo: Option<f64>,
        reduced_motion: bool,
    ) -> ([f64; 6], Option<Duration>) {
        let rest = rest_heights();
        if reduced_motion || !now.is_finite() {
            self.beat = None;
            self.born.get_or_insert(now);
            return (rest, None);
        }
        let tempo = tempo.filter(|p| p.is_finite() && *p > 0.0);
        let born = *self.born.get_or_insert(now);
        if !self.skip_draw_in && now - born < draw_in_duration() {
            return (draw_in_heights((now - born).max(0.0)), Some(FRAME));
        }
        match self.beat {
            Some((start, period)) => {
                let tt = (now - start).max(0.0).rem_euclid(period);
                if tt < pass_duration(period) {
                    return (beat_heights(tt, period), Some(FRAME));
                }
                if tempo == Some(period) {
                    let wait = (period - tt).max(0.0);
                    return (rest, Some(Duration::from_secs_f64(wait).max(FRAME)));
                }
                self.beat = tempo.map(|p| (now, p));
                (rest, self.beat.map(|_| FRAME))
            }
            None => {
                self.beat = tempo.map(|p| (now, p));
                (rest, self.beat.map(|_| FRAME))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REST: [f64; 6] = [12.0, 76.74, 56.21, 42.0, 82.16, 12.0];

    fn close(a: &[f64; 6], b: &[f64; 6], eps: f64) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= eps)
    }

    #[test]
    fn rest_heights_match_the_kit() {
        let h = rest_heights();
        assert!(close(&h, &REST, 0.01), "{h:?}");
        for i in 0..6 {
            assert!((bar_x(i) - (-92.0 + 36.8 * i as f64)).abs() < 1e-9);
        }
    }

    #[test]
    fn soft_floor_is_continuous() {
        assert_eq!(rest(0.0), MIN_BAR);
        assert_eq!(rest(12.0), MIN_BAR);
        assert_eq!(rest(30.0), 30.0);
        assert_eq!(rest(50.0), 50.0);
        let mut prev = rest(12.0);
        for k in 1..=180 {
            let v = rest(12.0 + f64::from(k) * 0.1);
            assert!(v >= prev - 1e-9, "monotone");
            assert!(v - prev < 0.2, "no jump");
            prev = v;
        }
    }

    #[test]
    fn draw_in_starts_at_the_stub_and_ends_at_rest() {
        assert!(close(&draw_in_heights(0.0), &[MIN_BAR; 6], 1e-9));
        assert!(close(&draw_in_heights(draw_in_duration()), &REST, 0.01));
        assert!(close(&draw_in_heights(5.0), &REST, 0.01));
        // Bar by bar from the left: mid-way, the first bar is ahead of the last.
        let mid = draw_in_heights(0.3);
        assert!(mid[1] - MIN_BAR > mid[4] - MIN_BAR);
        assert!((draw_in_duration() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn beat_rises_and_returns_to_rest_between_beats() {
        for period in [PERIOD_AMBIENT, PERIOD_CALM, PERIOD_BUSY] {
            assert!(close(&beat_heights(0.0, period), &REST, 0.01));
            // After the last bar settles and before the next pass, the mark rests.
            let between = pass_duration(period) + 0.01;
            assert!(close(&beat_heights(between, period), &REST, 0.01));
            assert!(close(&beat_heights(period - 0.001, period), &REST, 0.01));
            // At its peak a bar is 16u above rest; nothing goes below rest.
            let peak = beat_heights(WIDTH * period / 2.0, period);
            assert!((peak[0] - (REST[0] + RISE)).abs() < 0.01, "{peak:?}");
            for t in 0..400 {
                let h = beat_heights(f64::from(t) * period / 400.0, period);
                for i in 0..6 {
                    assert!(h[i] >= REST[i] - 0.01 && h[i] <= REST[i] + RISE + 0.01);
                }
            }
            // Periodic.
            assert!(close(
                &beat_heights(0.3 * period, period),
                &beat_heights(1.3 * period, period),
                1e-9
            ));
        }
        assert!(close(&beat_heights(1.0, 0.0), &REST, 0.01));
        assert!(close(&beat_heights(f64::NAN, 1.0), &REST, 0.01));
    }

    #[test]
    fn clock_draws_in_then_beats_only_while_asked() {
        let mut c = MarkClock::new();
        let (h, next) = c.frame(10.0, None, false);
        assert!(close(&h, &[MIN_BAR; 6], 1e-9));
        assert!(next.is_some(), "draw-in repaints");
        let (h, next) = c.frame(11.5, None, false);
        assert!(close(&h, &REST, 0.01));
        assert!(next.is_none(), "at rest: no repaint");
        // Work starts: a pass begins, repainting at frame rate.
        let (_, next) = c.frame(12.0, Some(PERIOD_BUSY), false);
        assert_eq!(next, Some(FRAME));
        let (h, _) = c.frame(12.0 + 0.1, Some(PERIOD_BUSY), false);
        assert!(h[0] > REST[0]);
        // Work stops mid-pass: the pass finishes first.
        let (h, next) = c.frame(12.0 + 0.2, None, false);
        assert!(h.iter().zip(REST).any(|(a, b)| *a > b + 1e-6));
        assert_eq!(next, Some(FRAME));
        // After the pass, it rests and stops repainting.
        let (h, next) = c.frame(12.0 + 0.5, None, false);
        assert!(close(&h, &REST, 0.01));
        assert!(next.is_none());
    }

    #[test]
    fn clock_waits_between_passes_without_busy_repaints() {
        let mut c = MarkClock::without_draw_in();
        let _ = c.frame(0.0, Some(PERIOD_CALM), false);
        let (h, next) = c.frame(1.5, Some(PERIOD_CALM), false);
        assert!(close(&h, &REST, 0.01));
        let wait = next.expect("a wake-up for the next pass");
        assert!((wait.as_secs_f64() - 0.9).abs() < 0.01, "{wait:?}");
    }

    #[test]
    fn reduced_motion_always_rests() {
        let mut c = MarkClock::new();
        for t in [0.0, 0.2, 3.0] {
            let (h, next) = c.frame(t, Some(PERIOD_BUSY), true);
            assert!(close(&h, &REST, 0.01));
            assert!(next.is_none());
        }
    }

    #[test]
    fn painting_degenerate_rects_is_harmless() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let p = ui.painter();
                for side in [0.0, 1.0, 12.0, 24.0, 64.0] {
                    let r = Rect::from_min_size(Pos2::ZERO, Vec2::splat(side));
                    paint_mark(p, r, &rest_heights(), ui.visuals().text_color());
                }
                paint_mark(
                    p,
                    Rect::from_min_size(Pos2::ZERO, Vec2::splat(24.0)),
                    &[f64::NAN; 6],
                    ui.visuals().text_color(),
                );
            });
        });
    }
}
