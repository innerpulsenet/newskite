//! A small, tolerant reader for KConfig-style INI files such as `kdeglobals`.
//!
//! Supported: `[Group]` headers including nested groups (`[Colors:Header][Inactive]`,
//! stored as `Colors:Header/Inactive`), `$`-flag suffixes on groups and keys (`[$i]`,
//! `[$e]`) which are dropped, `#`/`;` comment lines, a leading UTF-8 BOM, CRLF line
//! endings and KConfig value escapes (`\s`, `\t`, `\n`, `\r`, `\\`). Malformed lines are
//! skipped rather than failing the whole file. Later duplicates override earlier ones.

use std::collections::HashMap;

/// Parsed KConfig document: group name → (key → value).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct KConfig {
    groups: HashMap<String, HashMap<String, String>>,
}

impl KConfig {
    /// Parses `text`. Never fails; unparseable lines are ignored.
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut groups: HashMap<String, HashMap<String, String>> = HashMap::new();
        // Keys before the first header belong to the unnamed root group.
        let mut current: Option<String> = Some(String::new());

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                current = parse_header(line);
                continue;
            }
            let Some(group) = current.as_ref() else {
                continue; // inside a malformed group: drop its keys
            };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = strip_flags(key.trim());
            if key.is_empty() {
                continue;
            }
            groups
                .entry(group.clone())
                .or_default()
                .insert(key.to_owned(), unescape(value.trim()));
        }
        Self { groups }
    }

    /// Looks up `key` in `group` (exact, case-sensitive like KConfig).
    pub(crate) fn get(&self, group: &str, key: &str) -> Option<&str> {
        self.groups.get(group)?.get(key).map(String::as_str)
    }

    /// Whether the group exists (has at least one key).
    #[cfg(test)]
    pub(crate) fn has_group(&self, group: &str) -> bool {
        self.groups.contains_key(group)
    }
}

/// Parses a `[A][B][$i]` header into `A/B`. Returns `None` if malformed.
fn parse_header(line: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut rest = line;
    while let Some(after_open) = rest.strip_prefix('[') {
        let close = after_open.find(']')?;
        let name = after_open[..close].trim();
        if !name.starts_with('$') {
            parts.push(name);
        }
        rest = after_open[close + 1..].trim_start();
    }
    if !rest.is_empty() || parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(parts.join("/"))
}

/// Strips trailing `[$x]` flag suffixes from a key, keeping locale suffixes like `[de]`.
fn strip_flags(mut key: &str) -> &str {
    while key.ends_with(']') {
        match key.rfind('[') {
            Some(open) if key[open + 1..].starts_with('$') => key = key[..open].trim_end(),
            _ => break,
        }
    }
    key
}

/// Applies KConfig value escapes. Unknown escapes are kept verbatim.
fn unescape(v: &str) -> String {
    if !v.contains('\\') {
        return v.to_owned();
    }
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_groups_and_keys() {
        let k = KConfig::parse(
            "\u{feff}root=1\r\n# comment\n; also comment\n[General]\nColorScheme = BreezeDark \n\n[Colors:Window]\nBackgroundNormal=32,35,38\n",
        );
        assert_eq!(k.get("", "root"), Some("1"));
        assert_eq!(k.get("General", "ColorScheme"), Some("BreezeDark"));
        assert_eq!(k.get("Colors:Window", "BackgroundNormal"), Some("32,35,38"));
        assert_eq!(k.get("Colors:Window", "Missing"), None);
        assert_eq!(k.get("Nope", "BackgroundNormal"), None);
    }

    #[test]
    fn flags_and_nested_groups() {
        let k = KConfig::parse(
            "[General][$i]\nAccentColor[$e]=1,2,3\nName[de]=Dunkel\n[Colors:Header][Inactive]\nBackgroundNormal=1,1,1\n[Colors:Header]\nBackgroundNormal=2,2,2\n",
        );
        assert_eq!(k.get("General", "AccentColor"), Some("1,2,3"));
        assert_eq!(k.get("General", "Name[de]"), Some("Dunkel"));
        assert_eq!(
            k.get("Colors:Header/Inactive", "BackgroundNormal"),
            Some("1,1,1")
        );
        assert_eq!(k.get("Colors:Header", "BackgroundNormal"), Some("2,2,2"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let k = KConfig::parse(
            "[General]\nno equals sign\n=novalue\nGood=yes\n[Broken\nLeak=1\n[]\nLeak2=1\n[Ok]\nK=v=w\n",
        );
        assert_eq!(k.get("General", "Good"), Some("yes"));
        assert_eq!(k.get("General", "Leak"), None);
        assert_eq!(k.get("General", "Leak2"), None);
        assert_eq!(k.get("Ok", "K"), Some("v=w"));
        assert!(!k.has_group("Broken"));
    }

    #[test]
    fn later_duplicates_win_and_groups_merge() {
        let k = KConfig::parse("[A]\nx=1\ny=1\n[B]\nz=1\n[A]\nx=2\n");
        assert_eq!(k.get("A", "x"), Some("2"));
        assert_eq!(k.get("A", "y"), Some("1"));
    }

    #[test]
    fn escapes() {
        let k = KConfig::parse("[A]\nv=a\\sb\\tc\\\\d\\qe\\\n");
        assert_eq!(k.get("A", "v"), Some("a b\tc\\d\\qe\\"));
    }

    #[test]
    fn strip_flags_cases() {
        assert_eq!(strip_flags("Key[$i]"), "Key");
        assert_eq!(strip_flags("Key[$e][$i]"), "Key");
        assert_eq!(strip_flags("Key[en_US]"), "Key[en_US]");
        assert_eq!(strip_flags("Key"), "Key");
    }
}
