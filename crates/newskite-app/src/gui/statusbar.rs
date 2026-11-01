//! Status bar (outline §6.4).

use eframe::egui::{Align, Layout, RichText, Ui};
use newskite_types::format;

use super::app::{Action, Data};
use super::theme::Palette;
use super::widgets::{credit, link_label, status_chip, Status, CREDIT_URL};

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
/// the right end, the state chip beside it.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
    ui.horizontal_centered(|ui| {
        link_label(ui, pal, "Support Issues?").on_hover_text("Help resources are not set up yet.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if credit(ui, pal).clicked() {
                actions.push(Action::OpenUrl(CREDIT_URL.to_owned()));
            }
            ui.add_space(14.0);
            let state = pill_state(data);
            let r = status_chip(ui, pal, state.status(), &state.text());
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
