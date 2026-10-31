//! NZB manifest parsing.
//!
//! An NZB is an XML document (usually in the `http://www.newzbin.com/DTD/2003/nzb` namespace)
//! listing `<file>` entries, each with `<groups>` and `<segments>`. Every `<segment>` names one
//! Usenet article by message-id, its part number and its encoded size.
//!
//! The parser is tolerant of the things real-world NZBs get wrong: namespace prefixes, entities
//! in subjects and message-ids, whitespace around message-ids, angle brackets around ids,
//! segments listed out of order and duplicate segment numbers (the first occurrence wins).
//! Files without any usable segment are dropped; a document without any usable file is
//! reported as [`NzbError::Empty`].

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Maximum length, in bytes, of a sanitized filename.
pub const MAX_FILENAME_BYTES: usize = 200;

/// Name used when sanitizing leaves nothing usable.
const FALLBACK_NAME: &str = "unnamed";

/// A parsed NZB manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Nzb {
    /// `<head><meta type="…">` entries. Keys are lowercased and trimmed; when a type repeats,
    /// the first value is kept.
    pub meta: BTreeMap<String, String>,
    /// Files in document order. Every file has at least one segment.
    pub files: Vec<NzbFile>,
}

/// One `<file>` entry of an NZB.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NzbFile {
    /// The Usenet subject line (entities already resolved).
    pub subject: String,
    /// The poster (`From`) as listed in the NZB.
    pub poster: String,
    /// Posting date as unix seconds (0 when absent or unparseable).
    pub date: i64,
    /// Newsgroups the file was posted to.
    pub groups: Vec<String>,
    /// Segments sorted by number, with duplicate numbers removed (first occurrence kept).
    pub segments: Vec<Segment>,
}

/// One article (segment) of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    /// 1-based segment number.
    pub number: u32,
    /// Encoded article size in bytes, as declared by the NZB (0 when absent).
    pub bytes: u64,
    /// Message-id without surrounding `<>` and without whitespace.
    pub message_id: String,
}

/// Errors returned by [`parse`] and [`parse_file`].
#[derive(Debug, thiserror::Error)]
pub enum NzbError {
    /// The document is not well-formed XML or is not an NZB.
    #[error("invalid NZB XML: {0}")]
    Xml(String),
    /// The document parsed but lists no file with at least one segment.
    #[error("the NZB contains no files or segments")]
    Empty,
    /// The NZB file could not be read.
    #[error("could not read NZB: {0}")]
    Io(#[from] std::io::Error),
}

impl Nzb {
    /// Sum of the declared segment sizes of every file.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(NzbFile::total_bytes).sum()
    }

    /// Total number of segments across all files.
    pub fn segment_count(&self) -> usize {
        self.files.iter().map(|f| f.segments.len()).sum()
    }

    /// Display/folder name for the job: the `name` meta entry, else `title`, else the stem of
    /// `fallback_path` (the NZB file). Always sanitized with [`sanitize_filename`].
    pub fn job_name(&self, fallback_path: &Path) -> String {
        let from_meta = ["name", "title"]
            .iter()
            .filter_map(|k| self.meta.get(*k))
            .map(|v| v.trim())
            .find(|v| !v.is_empty());
        match from_meta {
            Some(name) => sanitize_filename(name),
            None => {
                let stem = fallback_path
                    .file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or(Cow::Borrowed(""));
                sanitize_filename(&stem)
            }
        }
    }
}

impl NzbFile {
    /// Output filename derived from the subject, always sanitized.
    ///
    /// Preference order: the first non-empty text inside double quotes; else the last token
    /// before `yEnc` that looks like `name.ext`; else the whole subject.
    pub fn filename(&self) -> String {
        sanitize_filename(extract_filename(&self.subject))
    }

    /// Sum of the declared segment sizes.
    pub fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }
}

/// Reads and parses an NZB file from disk.
pub fn parse_file(path: &Path) -> Result<Nzb, NzbError> {
    let bytes = std::fs::read(path)?;
    parse(&bytes)
}

/// Which leaf element's text is currently being collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    Meta,
    Group,
    Segment,
}

/// Incremental parser state.
#[derive(Default)]
struct State {
    /// Lowercased local names of open elements.
    stack: Vec<String>,
    root_seen: bool,
    meta: BTreeMap<String, String>,
    files: Vec<NzbFile>,
    file: Option<NzbFile>,
    meta_type: Option<String>,
    seg_number: Option<u32>,
    seg_bytes: u64,
    text: String,
    capture: Option<Capture>,
}

impl State {
    fn parent(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    fn capturing(&self) -> bool {
        self.capture.is_some()
    }

    fn start(&mut self, e: &BytesStart<'_>) -> Result<(), NzbError> {
        let name = e.local_name().into_inner().to_ascii_lowercase();
        if self.stack.is_empty() {
            if self.root_seen {
                return Err(NzbError::Xml("more than one root element".into()));
            }
            if name != "nzb" {
                return Err(NzbError::Xml(format!(
                    "root element is <{name}>, expected <nzb>"
                )));
            }
            self.root_seen = true;
        }
        // Any element nested inside a leaf we capture is ignored, but its text still counts.
        if !self.capturing() {
            match (self.parent(), name.as_str()) {
                (Some("head"), "meta") => {
                    self.meta_type = attr(e, "type").map(|t| t.trim().to_ascii_lowercase());
                    self.begin_capture(Capture::Meta);
                }
                (Some("nzb"), "file") => {
                    self.file = Some(NzbFile {
                        subject: attr(e, "subject").unwrap_or_default(),
                        poster: attr(e, "poster").unwrap_or_default(),
                        date: attr(e, "date")
                            .and_then(|d| d.trim().parse::<i64>().ok())
                            .unwrap_or(0),
                        groups: Vec::new(),
                        segments: Vec::new(),
                    });
                }
                (Some("groups"), "group") if self.file.is_some() => {
                    self.begin_capture(Capture::Group);
                }
                (Some("segments"), "segment") if self.file.is_some() => {
                    self.seg_number = attr(e, "number")
                        .and_then(|n| n.trim().parse::<u32>().ok())
                        .filter(|n| *n > 0);
                    self.seg_bytes = attr(e, "bytes")
                        .and_then(|b| b.trim().parse::<u64>().ok())
                        .unwrap_or(0);
                    self.begin_capture(Capture::Segment);
                }
                _ => {}
            }
        }
        self.stack.push(name);
        Ok(())
    }

    fn begin_capture(&mut self, c: Capture) {
        self.text.clear();
        self.capture = Some(c);
    }

    fn end(&mut self) -> Result<(), NzbError> {
        let Some(name) = self.stack.pop() else {
            return Err(NzbError::Xml("unexpected closing tag".into()));
        };
        let leaf_closed = match (self.capture, name.as_str()) {
            (Some(Capture::Meta), "meta") => {
                if let Some(key) = self.meta_type.take().filter(|k| !k.is_empty()) {
                    let value = self.text.trim().to_string();
                    self.meta.entry(key).or_insert(value);
                }
                true
            }
            (Some(Capture::Group), "group") => {
                let group = self.text.trim();
                if let Some(file) = self.file.as_mut() {
                    if !group.is_empty() {
                        file.groups.push(group.to_string());
                    }
                }
                true
            }
            (Some(Capture::Segment), "segment") => {
                let id = clean_message_id(&self.text);
                if let (Some(number), Some(file)) = (self.seg_number.take(), self.file.as_mut()) {
                    if !id.is_empty() {
                        file.segments.push(Segment {
                            number,
                            bytes: self.seg_bytes,
                            message_id: id,
                        });
                    }
                }
                true
            }
            _ => false,
        };
        if leaf_closed {
            self.capture = None;
            self.text.clear();
        } else if name == "file" && self.parent() == Some("nzb") {
            if let Some(mut file) = self.file.take() {
                // Stable sort keeps document order among equal numbers, so dedup keeps the first.
                file.segments.sort_by_key(|s| s.number);
                file.segments.dedup_by_key(|s| s.number);
                if !file.segments.is_empty() {
                    self.files.push(file);
                }
            }
        }
        Ok(())
    }

    fn push_text(&mut self, s: &str) {
        if self.capturing() {
            self.text.push_str(s);
        }
    }

    fn push_ref(&mut self, r: &BytesRef<'_>) -> Result<(), NzbError> {
        if !self.capturing() {
            return Ok(());
        }
        if r.is_char_ref() {
            match r.resolve_char_ref() {
                Ok(Some(c)) => self.text.push(c),
                Ok(None) => {}
                Err(e) => return Err(NzbError::Xml(format!("bad character reference: {e}"))),
            }
            return Ok(());
        }
        let name = r.xml10_content();
        match resolve_predefined_entity(&name) {
            Some(v) => self.text.push_str(v),
            // Unknown entities cannot be resolved without a DTD; keep them verbatim.
            None => {
                self.text.push('&');
                self.text.push_str(&name);
                self.text.push(';');
            }
        }
        Ok(())
    }
}

/// Returns the unescaped value of attribute `key` (matched on its local name, ASCII
/// case-insensitively). Unknown entities fall back to the raw attribute text.
fn attr(e: &BytesStart<'_>, key: &str) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        if a.key.local_name().into_inner().eq_ignore_ascii_case(key) {
            return Some(match a.normalized_value(XmlVersion::Implicit1_0) {
                Ok(v) => v.into_owned(),
                Err(_) => a.value.into_owned(),
            });
        }
    }
    None
}

/// Strips whitespace anywhere and a surrounding `<…>` from a message-id.
fn clean_message_id(raw: &str) -> String {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let s = compact.strip_prefix('<').unwrap_or(&compact);
    let s = s.strip_suffix('>').unwrap_or(s);
    s.to_string()
}

/// Parses an NZB document.
///
/// Returns [`NzbError::Xml`] for malformed XML or a root element other than `<nzb>`, and
/// [`NzbError::Empty`] when no file with at least one segment is present (including an empty
/// or whitespace-only document).
pub fn parse(bytes: &[u8]) -> Result<Nzb, NzbError> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    // NZBs are UTF-8 in practice; legacy encodings degrade to replacement characters rather
    // than failing the whole import.
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err(NzbError::Empty);
    }
    let mut reader = Reader::from_str(&text);
    reader.config_mut().check_end_names = true;

    let mut st = State::default();
    loop {
        let event = reader.read_event().map_err(|e| {
            NzbError::Xml(format!("{e} (at byte {})", reader.error_position()))
        })?;
        match event {
            Event::Start(e) => st.start(&e)?,
            Event::Empty(e) => {
                st.start(&e)?;
                st.end()?;
            }
            Event::End(_) => st.end()?,
            Event::Text(t) => st.push_text(&t.xml10_content()),
            Event::CData(c) => st.push_text(&c.xml10_content()),
            Event::GeneralRef(r) => st.push_ref(&r)?,
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {}
        }
    }
    if !st.stack.is_empty() {
        return Err(NzbError::Xml(format!(
            "unexpected end of document inside <{}>",
            st.stack.last().map(String::as_str).unwrap_or("?")
        )));
    }
    if !st.root_seen {
        return Err(NzbError::Xml("no <nzb> root element".into()));
    }
    if st.files.is_empty() {
        return Err(NzbError::Empty);
    }
    Ok(Nzb {
        meta: st.meta,
        files: st.files,
    })
}

/// Picks the raw (unsanitized) filename out of a Usenet subject line.
fn extract_filename(subject: &str) -> &str {
    // 1. First non-empty quoted run: `Release [01/12] - "file.part01.rar" yEnc (1/50)`.
    let mut rest = subject;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let inner = after[..close].trim();
        if !inner.is_empty() {
            return inner;
        }
        rest = &after[close + 1..];
    }
    // 2. Last token that looks like `name.ext` before "yEnc" (or anywhere if no yEnc marker).
    let region = match find_ascii_ci(subject, "yenc") {
        Some(i) => &subject[..i],
        None => subject,
    };
    let candidate = region
        .split_whitespace()
        .rev()
        .map(|t| t.trim_matches(|c: char| "[](){}<>'\",;:-".contains(c)))
        .find(|t| looks_like_filename(t));
    if let Some(name) = candidate {
        return name;
    }
    // 3. The subject itself.
    subject.trim()
}

/// ASCII case-insensitive substring search; returns a byte index on a char boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    // A match starts with an ASCII byte, so the index is always a char boundary.
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// True for `stem.ext` where ext is 1–10 ASCII alphanumerics.
fn looks_like_filename(token: &str) -> bool {
    match token.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !stem.chars().all(|c| c == '.')
                && (1..=10).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Windows reserved device names (checked on every platform so files stay portable).
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
    "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³", "CONIN$", "CONOUT$",
];

/// Turns arbitrary text into a single safe path component, portable to Linux, Windows and
/// macOS.
///
/// * keeps only the last path component (`/` and `\` both separate), dropping `.`/`..`;
/// * drops a leading Windows drive prefix (`C:`);
/// * removes control characters (including NUL) and replaces `< > : " | ? *` with `_`;
/// * trims leading whitespace and trailing whitespace/dots;
/// * prefixes Windows reserved device names (`CON`, `nul.txt`, `COM1`, …) with `_`;
/// * caps the result at [`MAX_FILENAME_BYTES`] bytes on a char boundary, preserving a short
///   extension.
///
/// Never returns an empty string, `.` or `..` (falls back to `unnamed`).
pub fn sanitize_filename(raw: &str) -> String {
    let normalized = raw.replace('\\', "/");
    let mut s: &str = &normalized;
    // Drive prefix such as `C:` or `c:` at the very start.
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s = &s[2..];
    }
    let component = s
        .split('/')
        .rev()
        .find(|c| {
            let t = c.trim();
            !t.is_empty() && !t.chars().all(|ch| ch == '.')
        })
        .unwrap_or("");

    let mut name: String = component
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            other => other,
        })
        .collect();
    name = trim_edges(&name).to_string();

    if is_reserved(&name) {
        name.insert(0, '_');
    }
    if name.len() > MAX_FILENAME_BYTES {
        name = truncate_preserving_extension(&name, MAX_FILENAME_BYTES);
        name = trim_edges(&name).to_string();
    }
    if name.is_empty() || name.chars().all(|c| c == '.') {
        return FALLBACK_NAME.to_string();
    }
    name
}

/// Trims leading whitespace and trailing whitespace and dots.
fn trim_edges(s: &str) -> &str {
    s.trim_start()
        .trim_end_matches(|c: char| c.is_whitespace() || c == '.')
}

fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    // Superscript digits have no case, so uppercasing the stem is enough for every entry.
    let upper = stem.to_uppercase();
    RESERVED.contains(&upper.as_str())
}

/// Largest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn truncate_preserving_extension(name: &str, max: usize) -> String {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        let ext_len = ext.len() + 1;
        if !stem.is_empty() && !ext.is_empty() && ext_len <= 16 && ext_len < max {
            let stem = truncate_on_boundary(stem, max - ext_len);
            return format!("{stem}.{ext}");
        }
    }
    truncate_on_boundary(name, max).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_cleanup() {
        assert_eq!(clean_message_id("  <abc@def>\n "), "abc@def");
        assert_eq!(clean_message_id("abc @\tdef"), "abc@def");
    }

    #[test]
    fn filename_heuristics() {
        assert_eq!(
            extract_filename(r#"Some.Release [01/12] - "file.part01.rar" yEnc (1/50)"#),
            "file.part01.rar"
        );
        assert_eq!(
            extract_filename("Some Release [01/12] - file.part01.rar yEnc (1/50)"),
            "file.part01.rar"
        );
        assert_eq!(extract_filename("[1/2] (movie.mkv) yEnc"), "movie.mkv");
        assert_eq!(extract_filename(r#""" "real.bin" yEnc"#), "real.bin");
        assert_eq!(extract_filename("just words here"), "just words here");
    }

    #[test]
    fn ascii_ci_search() {
        assert_eq!(find_ascii_ci("a YeNc b", "yenc"), Some(2));
        assert_eq!(find_ascii_ci("é yEnc", "yenc"), Some(3));
        assert_eq!(find_ascii_ci("abc", "yenc"), None);
    }
}
