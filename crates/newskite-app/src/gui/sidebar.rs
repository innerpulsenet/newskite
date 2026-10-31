//! Bookmarks sidebar (outline §6.2, F7).

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, Align, Layout, RichText, Sense, TextEdit, Ui};
use newskite_service::{Bookmark, NewBookmark};

use super::app::{Action, Data};
use super::theme::{parse_user_color, Palette};
use super::widgets::{icon_button, paint_icon, section_title, Icon};

/// Pixels of indentation per nesting level.
const INDENT: f32 = 14.0;

/// Sidebar UI state.
#[derive(Default)]
pub struct SidebarState {
    /// The inline add form (visible when `Some`).
    pub form: Option<BookmarkForm>,
    /// Bookmark awaiting removal confirmation.
    pub confirm_remove: Option<i64>,
}

/// Fields of the inline add form.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct BookmarkForm {
    /// Name (required).
    pub name: String,
    /// Optional newsgroup.
    pub newsgroup: String,
    /// Color as text (`#rrggbb` or `r,g,b`); empty = neutral.
    pub color: String,
    /// Optional parent bookmark.
    pub parent: Option<i64>,
    /// Validation message.
    pub error: Option<String>,
}

impl BookmarkForm {
    /// Validates the form and builds the store request.
    pub fn to_new(&self) -> Result<NewBookmark, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Name is required.".into());
        }
        let color = self.color.trim();
        if !color.is_empty() && parse_user_color(color).is_none() {
            return Err("Color must be #rrggbb, #rgb or r,g,b.".into());
        }
        let group = self.newsgroup.trim();
        if group.chars().any(char::is_whitespace) {
            return Err("Newsgroup names cannot contain spaces.".into());
        }
        Ok(NewBookmark {
            parent_id: self.parent,
            name: name.to_owned(),
            newsgroup: (!group.is_empty()).then(|| group.to_owned()),
            color: (!color.is_empty()).then(|| color.to_owned()),
        })
    }
}

/// Depth of each bookmark (0 = top level). Cycles and dangling parents count as top level.
pub fn depths(bookmarks: &[Bookmark]) -> HashMap<i64, usize> {
    let parent: HashMap<i64, Option<i64>> = bookmarks.iter().map(|b| (b.id, b.parent_id)).collect();
    bookmarks
        .iter()
        .map(|b| {
            let mut depth = 0;
            let mut seen = HashSet::from([b.id]);
            let mut cur = b.parent_id;
            while let Some(p) = cur {
                if !seen.insert(p) || !parent.contains_key(&p) {
                    break;
                }
                depth += 1;
                cur = parent.get(&p).copied().flatten();
            }
            (b.id, depth)
        })
        .collect()
}

/// Bookmarks hidden because an ancestor is collapsed.
fn hidden(bookmarks: &[Bookmark]) -> HashSet<i64> {
    let by_id: HashMap<i64, &Bookmark> = bookmarks.iter().map(|b| (b.id, b)).collect();
    bookmarks
        .iter()
        .filter(|b| {
            let mut seen = HashSet::from([b.id]);
            let mut cur = b.parent_id;
            while let Some(p) = cur {
                if !seen.insert(p) {
                    return false;
                }
                match by_id.get(&p) {
                    Some(pb) if !pb.expanded => return true,
                    Some(pb) => cur = pb.parent_id,
                    None => return false,
                }
            }
            false
        })
        .map(|b| b.id)
        .collect()
}

/// Draws the sidebar.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    state: &mut SidebarState,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        section_title(ui, pal, "Bookmarks");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let tip = if state.form.is_some() {
                "Close the add form"
            } else {
                "Add a bookmark"
            };
            let icon = if state.form.is_some() {
                Icon::Close
            } else {
                Icon::Plus
            };
            if icon_button(ui, pal, icon, pal.text, tip).clicked() {
                state.form = if state.form.is_some() {
                    None
                } else {
                    Some(BookmarkForm::default())
                };
            }
        });
    });
    ui.add_space(4.0);

    if let Some(form) = state.form.as_mut() {
        add_form(ui, pal, data, form, actions);
        ui.add_space(6.0);
    }

    if data.bookmarks.is_empty() {
        ui.add_space(12.0);
        ui.label(RichText::new("No bookmarks yet").color(pal.muted));
        return;
    }

    let depth = depths(&data.bookmarks);
    let hidden = hidden(&data.bookmarks);
    let has_children: HashSet<i64> = data.bookmarks.iter().filter_map(|b| b.parent_id).collect();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for b in data.bookmarks.iter().filter(|b| !hidden.contains(&b.id)) {
                let d = depth.get(&b.id).copied().unwrap_or(0);
                row(ui, pal, b, d, has_children.contains(&b.id), state, actions);
            }
        });
}

fn row(
    ui: &mut Ui,
    pal: &Palette,
    b: &Bookmark,
    depth: usize,
    parent: bool,
    state: &mut SidebarState,
    actions: &mut Vec<Action>,
) {
    let confirming = state.confirm_remove == Some(b.id);
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.add_space(depth as f32 * INDENT);
        // Expander (or a spacer so names line up).
        let (exp_rect, exp) = ui.allocate_exact_size(
            egui::vec2(14.0, 14.0),
            if parent {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        if parent {
            let icon = if b.expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            paint_icon(ui.painter(), exp_rect, icon, pal.muted);
            if exp
                .on_hover_text(if b.expanded { "Collapse" } else { "Expand" })
                .clicked()
            {
                actions.push(Action::ToggleBookmark(b.id));
            }
        }
        // Color swatch, falling back to a neutral dot.
        let color = b.color.as_deref().and_then(parse_user_color);
        let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
        match color {
            Some(c) => {
                ui.painter().circle_filled(dot.center(), 5.0, c);
            }
            None => {
                ui.painter()
                    .circle_stroke(dot.center(), 4.0, egui::Stroke::new(1.5, pal.muted));
            }
        }

        if confirming {
            ui.label(RichText::new("Remove?").color(pal.negative));
            if ui.small_button("Yes").clicked() {
                actions.push(Action::RemoveBookmark(b.id));
                state.confirm_remove = None;
            }
            if ui.small_button("No").clicked() {
                state.confirm_remove = None;
            }
            return;
        }

        let name = ui.add(
            egui::Label::new(RichText::new(&b.name).color(pal.text))
                .truncate()
                .sense(Sense::click()),
        );
        let tip = match &b.newsgroup {
            Some(g) => format!("{g}\nBrowsing newsgroups needs an indexer; not available yet."),
            None => "Browsing needs an indexer; not available yet.".to_owned(),
        };
        name.on_hover_text(tip);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, pal, Icon::Close, pal.muted, "Remove bookmark").clicked() {
                state.confirm_remove = Some(b.id);
            }
            if let Some(g) = &b.newsgroup {
                ui.add(egui::Label::new(RichText::new(g).small().color(pal.muted)).truncate());
            }
        });
    });
}

fn add_form(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    form: &mut BookmarkForm,
    actions: &mut Vec<Action>,
) {
    super::widgets::card(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let r_name = ui.add(
            TextEdit::singleline(&mut form.name)
                .hint_text("Name (required)")
                .desired_width(f32::INFINITY),
        );
        let r_group = ui.add(
            TextEdit::singleline(&mut form.newsgroup)
                .hint_text("alt.binaries.example (optional)")
                .desired_width(f32::INFINITY),
        );
        let mut submitted = (r_name.lost_focus() || r_group.lost_focus())
            && ui.input(|i| i.key_pressed(egui::Key::Enter));
        // Swatches drawn from token colors; free text accepts any hex / r,g,b value.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let swatches = [
                ("Accent", pal.accent),
                ("Green", pal.positive),
                ("Orange", pal.warning),
                ("Red", pal.negative),
                ("Gray", pal.muted),
            ];
            for (name, c) in swatches {
                let hex = format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
                let selected = form.color.eq_ignore_ascii_case(&hex);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::click());
                ui.painter().circle_filled(rect.center(), 7.0, c);
                if selected {
                    ui.painter().circle_stroke(
                        rect.center(),
                        8.5,
                        egui::Stroke::new(1.5, pal.text),
                    );
                }
                if resp.on_hover_text(name).clicked() {
                    form.color = hex;
                }
            }
            let r_color = ui.add(
                TextEdit::singleline(&mut form.color)
                    .hint_text("#hex")
                    .desired_width(64.0),
            );
            submitted |= r_color.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        });
        if !data.bookmarks.is_empty() {
            let current = form
                .parent
                .and_then(|id| data.bookmarks.iter().find(|b| b.id == id))
                .map(|b| b.name.clone())
                .unwrap_or_else(|| "Top level".to_owned());
            egui::ComboBox::from_id_salt("bookmark-parent")
                .selected_text(current)
                .width(ui.available_width() - 8.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.parent, None, "Top level");
                    for b in &data.bookmarks {
                        ui.selectable_value(&mut form.parent, Some(b.id), &b.name);
                    }
                });
        }
        if let Some(e) = &form.error {
            ui.label(RichText::new(e).color(pal.negative));
        }
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Add").strong()).clicked() || submitted {
                match form.to_new() {
                    Ok(nb) => {
                        form.error = None;
                        actions.push(Action::AddBookmark(nb));
                    }
                    Err(e) => form.error = Some(e),
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(id: i64, parent: Option<i64>, expanded: bool) -> Bookmark {
        Bookmark {
            id,
            parent_id: parent,
            name: format!("b{id}"),
            newsgroup: None,
            color: None,
            expanded,
            order: id,
        }
    }

    #[test]
    fn depth_and_cycles() {
        let list = vec![
            bm(1, None, true),
            bm(2, Some(1), true),
            bm(3, Some(2), true),
            bm(4, Some(99), true),
        ];
        let d = depths(&list);
        assert_eq!(d[&1], 0);
        assert_eq!(d[&2], 1);
        assert_eq!(d[&3], 2);
        assert_eq!(d[&4], 0, "dangling parent counts as top level");
        let cyc = vec![bm(5, Some(6), true), bm(6, Some(5), true)];
        let d = depths(&cyc);
        assert!(d[&5] <= 2 && d[&6] <= 2);
    }

    #[test]
    fn collapsed_parent_hides_descendants() {
        let list = vec![
            bm(1, None, false),
            bm(2, Some(1), true),
            bm(3, Some(2), true),
            bm(4, None, true),
        ];
        let h = hidden(&list);
        assert!(h.contains(&2) && h.contains(&3));
        assert!(!h.contains(&1) && !h.contains(&4));
    }

    #[test]
    fn form_validation() {
        let mut f = BookmarkForm::default();
        assert!(f.to_new().is_err(), "name required");
        f.name = "  Linux ISOs ".into();
        let nb = f.to_new().expect("valid");
        assert_eq!(nb.name, "Linux ISOs");
        assert_eq!(nb.newsgroup, None);
        assert_eq!(nb.color, None);
        f.color = "nope".into();
        assert!(f.to_new().is_err());
        f.color = "#3daee9".into();
        f.newsgroup = "alt.binaries.test".into();
        let nb = f.to_new().expect("valid");
        assert_eq!(nb.color.as_deref(), Some("#3daee9"));
        assert_eq!(nb.newsgroup.as_deref(), Some("alt.binaries.test"));
        f.newsgroup = "alt binaries".into();
        assert!(f.to_new().is_err());
    }
}
