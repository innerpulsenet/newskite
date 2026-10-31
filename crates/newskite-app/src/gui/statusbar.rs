//! Status bar (outline §6.4).

use eframe::egui::{Align, Layout, RichText, Ui};
use newskite_types::format;

use super::app::Data;
use super::theme::Palette;
use super::widgets::{link_label, pill};

/// What the status pill shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pill {
    /// A server is configured and the queue is idle.
    Online,
    /// A run is active (speed text).
    Downloading(String),
    /// Pause requested or in effect.
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
            Pill::Paused => "Paused".into(),
            Pill::Error(_) => "Error".into(),
            Pill::NoServer => "No server".into(),
        }
    }
}

/// Derives the pill from the current data.
pub fn pill_state(data: &Data) -> Pill {
    let st = &data.status;
    if st.running && !st.paused {
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

/// Draws the status bar.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data) {
    ui.horizontal_centered(|ui| {
        link_label(ui, pal, "Support Issues?").on_hover_text("Help resources are not set up yet.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let state = pill_state(data);
            let (fg, bg) = match &state {
                Pill::Online => (pal.positive, pal.positive_soft),
                Pill::Downloading(_) => (pal.accent_fg, pal.accent_soft),
                Pill::Paused | Pill::NoServer => (pal.warning, pal.warning_soft),
                Pill::Error(_) => (pal.negative, pal.negative_soft),
            };
            let r = pill(ui, &state.text(), fg, bg);
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
        d.status.paused = true;
        assert_eq!(pill_state(&d), Pill::Paused);
    }
}
