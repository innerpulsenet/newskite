//! Status bar (outline §6.4).

use eframe::egui::{Align, Layout, Pos2, RichText, Stroke, Ui};
use newskite_service::JobState;
use newskite_types::format;

use super::app::{Action, Data};
use super::brand;
use super::motion::Beat;
use super::theme::Palette;
use super::widgets::{credit, link_label, status_chip_live, Status, CREDIT_URL};

/// Fraction of the bytes of every unfinished job that is already down (0…1; 0 when
/// nothing is pending). The status bar's hairline while a run is active.
pub fn pending_fraction(data: &Data) -> f32 {
    let (done, total) =
        data.jobs
            .iter()
            .filter(|j| j.state != JobState::Done)
            .fold((0u64, 0u64), |(d, t), j| {
                (
                    d.saturating_add(data.shown_done(j)),
                    t.saturating_add(j.bytes_total),
                )
            });
    format::fraction(done, total)
}

/// What the status pill shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pill {
    /// A server is configured and the queue is idle.
    Online,
    /// A run is active (speed text).
    Downloading(String),
    /// Pause requested while a job is still downloading (speed text): the pause is
    /// cooperative and takes effect once the current job finishes.
    Pausing(String),
    /// Paused, and no job is downloading.
    Paused,
    /// The last run ended with an error.
    Error(String),
    /// No usable server.
    NoServer,
}

impl Pill {
    /// Label text.
    pub fn text(&self) -> String {
        match self {
            Pill::Online => "Online".into(),
            Pill::Downloading(speed) => format!("Downloading · {speed}"),
            Pill::Pausing(speed) => format!("Downloading · {speed} · pausing after this job"),
            Pill::Paused => "Paused".into(),
            Pill::Error(e) => short_cause(e),
            Pill::NoServer => "No server".into(),
        }
    }
}

/// Longest error cause shown in the chip; the full text is in its tooltip.
const CAUSE_CHARS: usize = 48;

/// The chip words for an error: what happened, on one line, elided to
/// [`CAUSE_CHARS`] (the brand's chips say what happened, never just "Error").
pub fn short_cause(error: &str) -> String {
    let line = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.is_empty() {
        return "Failed".into();
    }
    if line.chars().count() <= CAUSE_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(CAUSE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Derives the pill from the current data. Pause is cooperative: while the current job
/// still downloads, the pill keeps the speed and says the pause is pending.
pub fn pill_state(data: &Data) -> Pill {
    let st = &data.status;
    if st.running && st.paused {
        Pill::Pausing(format::speed(data.speed()))
    } else if st.running {
        Pill::Downloading(format::speed(data.speed()))
    } else if st.paused {
        Pill::Paused
    } else if let Some(e) = &st.last_error {
        Pill::Error(e.clone())
    } else if data.settings.first_usable_server().is_none() {
        Pill::NoServer
    } else {
        Pill::Online
    }
}

impl Pill {
    /// The chip status: online ok, downloading live, paused and no server warn, error err.
    pub fn status(&self) -> Status {
        match self {
            Pill::Online => Status::Ok,
            Pill::Downloading(_) | Pill::Pausing(_) => Status::Live,
            Pill::Paused | Pill::NoServer => Status::Warn,
            Pill::Error(_) => Status::Err,
        }
    }
}

/// Draws the status bar: the support link at the left; the quiet innerpulse credit at
/// the right end, the state chip beside it (its live dot breathing with the busy beat).
/// While a run is active a 2 px accent hairline along the top edge shows how much of
/// the pending bytes is down.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    reduced_motion: bool,
    actions: &mut Vec<Action>,
) {
    let st = &data.status;
    let beat = Beat::new(
        ui.ctx(),
        st.running && data.speed() > 0,
        reduced_motion,
        brand::PERIOD_BUSY,
    );
    if st.running {
        let bar = ui.max_rect();
        let f = pending_fraction(data);
        let y = bar.top() - 2.0;
        ui.painter().line_segment(
            [
                Pos2::new(bar.left() - 10.0, y),
                Pos2::new(bar.right() + 10.0, y),
            ],
            Stroke::new(2.0, pal.well),
        );
        if f > 0.0 {
            let x = bar.left() - 10.0 + (bar.width() + 20.0) * f;
            ui.painter().line_segment(
                [Pos2::new(bar.left() - 10.0, y), Pos2::new(x, y)],
                Stroke::new(2.0, pal.accent),
            );
        }
    }
    ui.horizontal_centered(|ui| {
        link_label(ui, pal, "Support Issues?").on_hover_text("Help resources are not set up yet.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if credit(ui, pal).clicked() {
                actions.push(Action::OpenUrl(CREDIT_URL.to_owned()));
            }
            ui.add_space(14.0);
            let state = pill_state(data);
            let breath = beat.moving.then(|| beat.breath());
            let r = status_chip_live(ui, pal, state.status(), &state.text(), breath);
            if let Pill::Error(e) = &state {
                r.on_hover_text(e);
            }
            if data.totals.jobs_pending > 0 {
                ui.label(
                    RichText::new(format!("{} pending", data.totals.jobs_pending))
                        .small()
                        .color(pal.muted),
                );
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pill_priorities() {
        let mut d = Data::default();
        d.settings.servers[0].host = String::new();
        assert_eq!(pill_state(&d), Pill::NoServer);
        d.settings.servers[0].host = "h".into();
        assert_eq!(pill_state(&d), Pill::Online);
        d.status.last_error = Some("auth failed".into());
        assert_eq!(pill_state(&d), Pill::Error("auth failed".into()));
        d.status.running = true;
        d.status.speed_bps = 2048;
        assert_eq!(pill_state(&d).text(), "Downloading · 2.0 KB/s");
        // Pause requested while a job still downloads: still live, with the speed.
        d.status.paused = true;
        assert_eq!(pill_state(&d), Pill::Pausing("2.0 KB/s".into()));
        assert_eq!(pill_state(&d).status(), Status::Live);
        assert!(pill_state(&d).text().starts_with("Downloading · 2.0 KB/s"));
        // The run has stopped: paused.
        d.status.running = false;
        assert_eq!(pill_state(&d), Pill::Paused);
    }

    #[test]
    fn pending_fraction_counts_unfinished_jobs_only() {
        use newskite_service::Job;
        use std::path::PathBuf;
        let job = |id, state, done, total| Job {
            id,
            name: String::new(),
            state,
            bytes_total: total,
            bytes_done: done,
            dest_dir: PathBuf::new(),
            source_path: PathBuf::new(),
            created_at: 0,
            error: None,
            password: None,
        };
        let mut d = Data::default();
        assert_eq!(pending_fraction(&d), 0.0);
        d.jobs = vec![
            job(1, JobState::Done, 100, 100),
            job(2, JobState::Downloading, 25, 100),
            job(3, JobState::Queued, 0, 100),
        ];
        assert!((pending_fraction(&d) - 0.125).abs() < 1e-6);
    }

    #[test]
    fn error_chip_says_what_happened() {
        assert_eq!(
            Pill::Error("auth failed · 481".into()).text(),
            "auth failed · 481"
        );
        assert_eq!(
            Pill::Error("\n  first line\nsecond".into()).text(),
            "first line"
        );
        assert_eq!(Pill::Error(String::new()).text(), "Failed");
        let long = "x".repeat(200);
        let t = Pill::Error(long).text();
        assert_eq!(t.chars().count(), CAUSE_CHARS);
        assert!(t.ends_with('…'));
        assert_ne!(Pill::Error("boom".into()).text(), "Error");
    }
}
