//! The article map of a job: for every article of every file, whether it arrived, is
//! missing on every server, or is still waiting. The service builds it from engine
//! events ([`crate::EngineEvent::Layout`] and the per-article events after it), saves it
//! when a run ends, and the GUI draws it as a grid. Every state in it was reported by
//! the engine; nothing is estimated.

use serde::{Deserialize, Serialize};

use crate::events::FileLayout;

/// What became of one article.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ArticleState {
    /// Not fetched yet (or the run stopped before it).
    #[default]
    Waiting,
    /// Downloaded, decoded and written.
    Done,
    /// No server had it (or it could not be decoded from any of them).
    Missing,
}

impl ArticleState {
    fn code(self) -> char {
        match self {
            ArticleState::Waiting => 'w',
            ArticleState::Done => 'd',
            ArticleState::Missing => 'm',
        }
    }

    fn from_code(c: char) -> Option<Self> {
        match c {
            'w' => Some(ArticleState::Waiting),
            'd' => Some(ArticleState::Done),
            'm' => Some(ArticleState::Missing),
            _ => None,
        }
    }
}

/// The articles of one file, in segment order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileArticles {
    /// The file's name from the NZB.
    pub name: String,
    pub states: Vec<ArticleState>,
}

/// Counts of each state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArticleCounts {
    pub done: usize,
    pub missing: usize,
    pub waiting: usize,
}

impl ArticleCounts {
    pub fn total(&self) -> usize {
        self.done + self.missing + self.waiting
    }

    fn add(&mut self, s: ArticleState) {
        match s {
            ArticleState::Waiting => self.waiting += 1,
            ArticleState::Done => self.done += 1,
            ArticleState::Missing => self.missing += 1,
        }
    }
}

/// A job's article map: its files in NZB order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArticleMap {
    pub files: Vec<FileArticles>,
}

/// Longest stored map text accepted when loading (a corrupt row must not exhaust memory).
const MAX_STORED_ARTICLES: usize = 50_000_000;

#[derive(Serialize, Deserialize)]
struct StoredFile {
    name: String,
    /// Run-length text: a count and a state code per run, e.g. `1200d3m40w`.
    runs: String,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    v: u32,
    files: Vec<StoredFile>,
}

impl ArticleMap {
    /// A fresh map for a run: files found complete from an earlier run are done, every
    /// other article waits.
    pub fn from_layout(files: &[FileLayout]) -> Self {
        Self {
            files: files
                .iter()
                .map(|f| FileArticles {
                    name: f.name.clone(),
                    states: vec![if f.complete { ArticleState::Done } else { ArticleState::Waiting }; f.segments],
                })
                .collect(),
        }
    }

    /// Records the state of article `index` of file `file`; positions outside the map are
    /// ignored. Returns whether anything changed.
    pub fn set(&mut self, file: usize, index: usize, state: ArticleState) -> bool {
        match self.files.get_mut(file).and_then(|f| f.states.get_mut(index)) {
            Some(s) if *s != state => {
                *s = state;
                true
            }
            _ => false,
        }
    }

    /// Marks every article of `file` done (a file found complete on disk).
    pub fn complete_file(&mut self, file: usize) {
        if let Some(f) = self.files.get_mut(file) {
            f.states.fill(ArticleState::Done);
        }
    }

    /// Articles in the map.
    pub fn len(&self) -> usize {
        self.files.iter().map(|f| f.states.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many articles are in each state.
    pub fn counts(&self) -> ArticleCounts {
        let mut c = ArticleCounts::default();
        for s in self.files.iter().flat_map(|f| f.states.iter()) {
            c.add(*s);
        }
        c
    }

    /// The articles of every file, in order, folded into at most `max_cells` cells of
    /// equal size: returns the articles per cell and each cell's counts. With
    /// `max_cells` at least [`ArticleMap::len`], every cell is one article.
    pub fn cells(&self, max_cells: usize) -> (usize, Vec<ArticleCounts>) {
        let n = self.len();
        if n == 0 || max_cells == 0 {
            return (1, Vec::new());
        }
        let per = n.div_ceil(max_cells);
        let mut out = Vec::with_capacity(n.div_ceil(per));
        let mut cur = ArticleCounts::default();
        for s in self.files.iter().flat_map(|f| f.states.iter()) {
            cur.add(*s);
            if cur.total() == per {
                out.push(std::mem::take(&mut cur));
            }
        }
        if cur.total() > 0 {
            out.push(cur);
        }
        (per, out)
    }

    /// The map as compact text for storage (JSON with run-length encoded states).
    pub fn to_stored(&self) -> String {
        let files = self
            .files
            .iter()
            .map(|f| {
                let mut runs = String::new();
                let mut iter = f.states.iter().peekable();
                while let Some(&s) = iter.next() {
                    let mut n = 1usize;
                    while iter.peek() == Some(&&s) {
                        iter.next();
                        n += 1;
                    }
                    runs.push_str(&n.to_string());
                    runs.push(s.code());
                }
                StoredFile { name: f.name.clone(), runs }
            })
            .collect();
        serde_json::to_string(&Stored { v: 1, files }).unwrap_or_default()
    }

    /// Reads [`ArticleMap::to_stored`] text; `None` when it is not a map this build
    /// understands (or claims an absurd size).
    pub fn from_stored(text: &str) -> Option<Self> {
        let stored: Stored = serde_json::from_str(text).ok()?;
        if stored.v != 1 {
            return None;
        }
        let mut total = 0usize;
        let mut files = Vec::with_capacity(stored.files.len());
        for f in stored.files {
            let mut states = Vec::new();
            let mut digits = String::new();
            for c in f.runs.chars() {
                if c.is_ascii_digit() {
                    digits.push(c);
                    continue;
                }
                let n: usize = digits.parse().ok()?;
                digits.clear();
                total = total.checked_add(n)?;
                if total > MAX_STORED_ARTICLES {
                    return None;
                }
                states.extend(std::iter::repeat_n(ArticleState::from_code(c)?, n));
            }
            if !digits.is_empty() {
                return None;
            }
            files.push(FileArticles { name: f.name, states });
        }
        Some(Self { files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ArticleState::*;

    fn layout() -> Vec<FileLayout> {
        vec![
            FileLayout { name: "a.rar".into(), segments: 4, complete: false },
            FileLayout { name: "b.rar".into(), segments: 3, complete: true },
            FileLayout { name: "c.par2".into(), segments: 2, complete: false },
        ]
    }

    #[test]
    fn layout_marks_complete_files_done() {
        let m = ArticleMap::from_layout(&layout());
        assert_eq!(m.len(), 9);
        assert_eq!(m.counts(), ArticleCounts { done: 3, missing: 0, waiting: 6 });
    }

    #[test]
    fn set_ignores_positions_outside_the_map() {
        let mut m = ArticleMap::from_layout(&layout());
        assert!(m.set(0, 1, Done));
        assert!(!m.set(0, 1, Done), "no change");
        assert!(m.set(2, 1, Missing));
        assert!(!m.set(0, 99, Done));
        assert!(!m.set(7, 0, Done));
        m.complete_file(2);
        assert_eq!(m.files[2].states, vec![Done, Done]);
        assert_eq!(m.counts(), ArticleCounts { done: 6, missing: 0, waiting: 3 });
    }

    #[test]
    fn cells_fold_in_order_and_keep_every_article() {
        let mut m = ArticleMap::from_layout(&layout());
        m.set(0, 0, Done);
        m.set(0, 3, Missing);
        let (per, cells) = m.cells(100);
        assert_eq!(per, 1);
        assert_eq!(cells.len(), 9);
        assert_eq!(cells[3], ArticleCounts { done: 0, missing: 1, waiting: 0 });
        let (per, cells) = m.cells(4);
        assert_eq!(per, 3);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells.iter().map(ArticleCounts::total).sum::<usize>(), 9);
        assert_eq!(cells[1], ArticleCounts { done: 2, missing: 1, waiting: 0 });
        assert_eq!(ArticleMap::default().cells(10), (1, Vec::new()));
        assert_eq!(m.cells(0).1.len(), 0);
    }

    #[test]
    fn stored_text_round_trips() {
        let mut m = ArticleMap::from_layout(&layout());
        for i in 0..4 {
            m.set(0, i, Done);
        }
        m.set(2, 0, Missing);
        let text = m.to_stored();
        assert!(text.contains("4d") && text.contains("3d") && text.contains("1m1w"), "{text}");
        assert_eq!(ArticleMap::from_stored(&text), Some(m));
        assert_eq!(ArticleMap::from_stored(&ArticleMap::default().to_stored()), Some(ArticleMap::default()));
    }

    #[test]
    fn bad_stored_text_is_refused() {
        for bad in [
            "",
            "not json",
            r#"{"v":2,"files":[]}"#,
            r#"{"v":1,"files":[{"name":"x","runs":"3q"}]}"#,
            r#"{"v":1,"files":[{"name":"x","runs":"12"}]}"#,
            r#"{"v":1,"files":[{"name":"x","runs":"d"}]}"#,
            r#"{"v":1,"files":[{"name":"x","runs":"99999999999d"}]}"#,
        ] {
            assert_eq!(ArticleMap::from_stored(bad), None, "{bad}");
        }
    }
}
