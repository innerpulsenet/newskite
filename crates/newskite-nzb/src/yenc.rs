//! yEnc decoding and encoding.
//!
//! yEnc maps each byte `b` to `(b + 42) mod 256`. Encoded bytes that would be NUL, LF, CR or
//! `=` (and, depending on position, TAB, space and a leading `.`) are escaped as `=` followed
//! by `(encoded + 64) mod 256`. Decoding therefore subtracts 64 from an escaped byte and then
//! 42 from every byte.
//!
//! [`decode`] expects an article body that the NNTP layer has already dot-unstuffed (it must
//! not be unstuffed twice, so lines starting with `..` here are genuine data). Both CRLF and LF
//! line endings are accepted, and any junk before the `=ybegin` line is skipped.

use std::collections::HashMap;
use std::fmt::Write as _;

/// Default line length used when `0` is passed to the encoder.
pub const DEFAULT_LINE_LEN: usize = 128;

/// One decoded yEnc article (a whole single-part file or one part of a multipart file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct YencPart {
    /// `name=` from `=ybegin` (the rest of the header line; may contain spaces). Unsanitized.
    pub name: String,
    /// Total size of the whole file (`size=` from `=ybegin`).
    pub size: u64,
    /// `part=` from `=ybegin`, if present.
    pub part: Option<u32>,
    /// `total=` from `=ybegin`, if present.
    pub total: Option<u32>,
    /// First byte offset of this part in the file, 1-based inclusive.
    pub begin: u64,
    /// Last byte offset of this part in the file, 1-based inclusive.
    pub end: u64,
    /// Decoded bytes; `data.len() == end - begin + 1`.
    pub data: Vec<u8>,
    /// Result of the CRC check: `pcrc32` when present, else `crc32` when this article holds
    /// the whole file. `None` when no usable checksum was supplied.
    pub crc_ok: Option<bool>,
}

/// Errors returned by [`decode`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum YencError {
    /// No `=ybegin` line was found.
    #[error("no =ybegin header found")]
    NoHeader,
    /// A header line (`=ybegin`, `=ypart`, `=yend`) is missing a required key or is malformed.
    #[error("malformed yEnc header: {0}")]
    BadHeader(String),
    /// The amount of decoded data does not match the declared size or range.
    #[error("yEnc size mismatch: expected {expected} bytes, got {got}")]
    SizeMismatch {
        /// Byte count declared by the headers.
        expected: u64,
        /// Byte count actually decoded.
        got: u64,
    },
    /// The `=ypart` range is empty, starts at 0 or extends past the file size.
    #[error("yEnc part range is invalid")]
    BadRange,
}

/// Parsed `key=value` pairs of a header line; `name=` captures the rest of the line.
#[derive(Debug, Default)]
struct Header {
    fields: HashMap<String, String>,
    name: Option<String>,
}

impl Header {
    fn parse(line: &[u8], keyword: &str) -> Header {
        let text = String::from_utf8_lossy(line);
        let mut rest = text.get(keyword.len()..).unwrap_or("");
        let mut header = Header::default();
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                break;
            }
            if let Some(name) = rest.strip_prefix("name=") {
                header.name = Some(name.trim_end().to_string());
                break;
            }
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let token = &rest[..end];
            if let Some((k, v)) = token.split_once('=') {
                header
                    .fields
                    .entry(k.to_ascii_lowercase())
                    .or_insert_with(|| v.to_string());
            }
            rest = &rest[end..];
        }
        header
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// Parses an optional numeric field; present-but-invalid is an error.
    fn num<T: std::str::FromStr>(&self, line: &str, key: &str) -> Result<Option<T>, YencError> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.parse::<T>().map(Some).map_err(|_| {
                YencError::BadHeader(format!("{line}: invalid {key}={v}"))
            }),
        }
    }

    fn crc(&self, key: &str) -> Option<u32> {
        let v = self.get(key)?;
        let v = v
            .strip_prefix("0x")
            .or_else(|| v.strip_prefix("0X"))
            .unwrap_or(v);
        if v.is_empty() || v.len() > 8 {
            return None;
        }
        u32::from_str_radix(v, 16).ok()
    }
}

/// Whether `line` is a `keyword` header line: the keyword followed by whitespace or the
/// end. The slice may extend past the line (a line break counts as whitespace).
fn is_keyword_line(line: &[u8], keyword: &[u8]) -> bool {
    line.starts_with(keyword)
        && line
            .get(keyword.len())
            .is_none_or(|b| b.is_ascii_whitespace())
}

/// Decodes one yEnc article body.
///
/// `article_body` is the dot-unstuffed BODY payload; lines may end in CRLF or LF and anything
/// before the `=ybegin` line is ignored. Articles with `part=` (or a `=ypart` line) are
/// multipart and use the `=ypart begin/end` range; otherwise `begin = 1` and `end = size`.
/// A missing `=yend` line is tolerated (the remaining lines are data, no CRC is checked).
///
/// The data lines are decoded in a single pass over the byte slice: runs of plain bytes are
/// shifted in bulk and only the escape and line-break bytes are handled one at a time.
pub fn decode(article_body: &[u8]) -> Result<YencPart, YencError> {
    let begin_at = find_keyword_line(article_body, 0, b"=ybegin").ok_or(YencError::NoHeader)?;
    let (begin_line, mut pos) = take_line(article_body, begin_at);
    let ybegin = Header::parse(begin_line, "=ybegin");
    let size: u64 = ybegin
        .num("=ybegin", "size")?
        .ok_or_else(|| YencError::BadHeader("=ybegin: missing size".into()))?;
    let part: Option<u32> = ybegin.num("=ybegin", "part")?;
    let total: Option<u32> = ybegin.num("=ybegin", "total")?;
    let name = ybegin.name.clone().unwrap_or_default();

    let mut ypart = None;
    {
        let (line, next) = take_line(article_body, pos);
        if is_keyword_line(line, b"=ypart") {
            ypart = Some(Header::parse(line, "=ypart"));
            pos = next;
        }
    }

    let (begin, end) = match (&ypart, part) {
        (Some(h), _) => {
            let begin: u64 = h
                .num("=ypart", "begin")?
                .ok_or_else(|| YencError::BadHeader("=ypart: missing begin".into()))?;
            let end: u64 = h
                .num("=ypart", "end")?
                .ok_or_else(|| YencError::BadHeader("=ypart: missing end".into()))?;
            if begin == 0 || end < begin || end > size {
                return Err(YencError::BadRange);
            }
            (begin, end)
        }
        (None, Some(_)) => {
            return Err(YencError::BadHeader(
                "=ybegin declares part= but no =ypart line follows".into(),
            ))
        }
        (None, None) => (1, size),
    };
    // `end + 1 - begin` cannot underflow: begin >= 1 and end >= begin - 1 in both branches.
    let expected = end + 1 - begin;

    // The data region runs to the `=yend` line (or to the end of the body without one).
    let (data_end, yend) = match find_keyword_line(article_body, pos, b"=yend") {
        Some(at) => (at, Some(Header::parse(take_line(article_body, at).0, "=yend"))),
        None => (article_body.len(), None),
    };

    // Never trust the header for allocation size beyond what the body could possibly hold.
    let cap = usize::try_from(expected)
        .unwrap_or(usize::MAX)
        .min(data_end - pos);
    let mut data = Vec::with_capacity(cap);
    decode_into(&article_body[pos..data_end], &mut data);

    let got = data.len() as u64;
    if got != expected {
        return Err(YencError::SizeMismatch { expected, got });
    }
    let mut crc_ok = None;
    if let Some(h) = &yend {
        if let Some(declared) = h.num::<u64>("=yend", "size")? {
            if declared != got {
                return Err(YencError::SizeMismatch {
                    expected: declared,
                    got,
                });
            }
        }
        let whole_file = begin == 1 && end == size;
        let want = h
            .crc("pcrc32")
            .or_else(|| if whole_file { h.crc("crc32") } else { None });
        crc_ok = want.map(|w| crc32fast::hash(&data) == w);
    }

    Ok(YencPart {
        name,
        size,
        part,
        total,
        begin,
        end,
        data,
        crc_ok,
    })
}

/// The line starting at `at` (without its line ending) and the offset of the next line.
fn take_line(body: &[u8], at: usize) -> (&[u8], usize) {
    let rest = &body[at.min(body.len())..];
    match memchr::memchr(b'\n', rest) {
        Some(n) => (rest[..n].strip_suffix(b"\r").unwrap_or(&rest[..n]), at + n + 1),
        None => (rest.strip_suffix(b"\r").unwrap_or(rest), body.len()),
    }
}

/// Offset of the first line at or after `from` that starts with `keyword` followed by
/// whitespace or the line end.
///
/// yEnc header lines all start with `=y`, a pair that cannot occur inside data (an escaped
/// `y` would stand for byte 15, whose encoding `9` is never escaped), so the body is
/// searched for `=y` at a line start instead of being split into lines.
fn find_keyword_line(body: &[u8], from: usize, keyword: &[u8]) -> Option<usize> {
    let finder = memchr::memmem::Finder::new(b"=y");
    let mut at = from;
    while at < body.len() {
        let rel = finder.find(&body[at..])?;
        let p = at + rel;
        if (p == 0 || body[p - 1] == b'\n') && is_keyword_line(&body[p..], keyword) {
            return Some(p);
        }
        at = p + 2;
    }
    None
}

/// Decodes yEnc data lines into `out`, in one pass over the bytes. CR and LF are dropped,
/// `=` escapes the next byte and an escape never spans a line break: a dangling `=` at the
/// end of a line is ignored.
fn decode_into(mut encoded: &[u8], out: &mut Vec<u8>) {
    loop {
        // Plain bytes up to the next escape or line break are shifted as one run.
        let (run, special) = match memchr::memchr3(b'=', b'\r', b'\n', encoded) {
            Some(k) => (&encoded[..k], Some(encoded[k])),
            None => (encoded, None),
        };
        out.extend(run.iter().map(|b| b.wrapping_sub(42)));
        let Some(special) = special else { break };
        encoded = &encoded[run.len() + 1..];
        if special == b'=' {
            match encoded.first() {
                Some(&e) if e != b'\r' && e != b'\n' => {
                    out.push(e.wrapping_sub(64).wrapping_sub(42));
                    encoded = &encoded[1..];
                }
                _ => {}
            }
        }
    }
}

/// Decodes raw yEnc data lines (no headers). CR and LF bytes are dropped, escapes are handled
/// and an escape never spans a line break.
pub fn decode_data(encoded: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded.len());
    decode_into(encoded, &mut out);
    out
}

/// Encodes `data` as yEnc lines of about `line_len` output characters (an escape may push a
/// line one character over), each terminated by CRLF. `line_len == 0` means
/// [`DEFAULT_LINE_LEN`].
///
/// Always escapes NUL, LF, CR and `=`; escapes TAB and space at the start or end of a line and
/// `.` at the start of a line (so the output survives NNTP dot-stuffing and whitespace
/// trimming).
pub fn encode_data(data: &[u8], line_len: usize) -> Vec<u8> {
    let ll = if line_len == 0 {
        DEFAULT_LINE_LEN
    } else {
        line_len
    };
    let mut out = Vec::with_capacity(data.len() + data.len() / 16 + 2 * (data.len() / ll + 1));
    let mut col = 0usize;
    for (i, &b) in data.iter().enumerate() {
        let e = b.wrapping_add(42);
        let at_start = col == 0;
        let at_end = col + 1 >= ll || i + 1 == data.len();
        let escape = match e {
            0 | b'\n' | b'\r' | b'=' => true,
            b'\t' | b' ' => at_start || at_end,
            b'.' => at_start,
            _ => false,
        };
        if escape {
            out.push(b'=');
            out.push(e.wrapping_add(64));
            col += 2;
        } else {
            out.push(e);
            col += 1;
        }
        if col >= ll {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }
    if col > 0 {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Builds a complete yEnc article body (headers, data and trailer, CRLF line endings).
///
/// When `total <= 1`, `begin_1based == 1` and `data` is the whole file, a single-part article
/// (no `=ypart`, `crc32=`) is produced; otherwise a multipart article with `=ypart` and
/// `pcrc32=`. Control characters in `name` are removed so the header stays one line.
pub fn encode_article(
    name: &str,
    file_size: u64,
    part: u32,
    total: u32,
    begin_1based: u64,
    data: &[u8],
    line_len: usize,
) -> Vec<u8> {
    let ll = if line_len == 0 {
        DEFAULT_LINE_LEN
    } else {
        line_len
    };
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let len = data.len() as u64;
    let crc = crc32fast::hash(data);
    let single = total <= 1 && begin_1based == 1 && len == file_size;

    let mut head = String::new();
    let mut tail = String::new();
    // Writing to a String cannot fail.
    if single {
        let _ = write!(head, "=ybegin line={ll} size={file_size} name={name}\r\n");
        let _ = write!(tail, "=yend size={len} crc32={crc:08x}\r\n");
    } else {
        let end = (begin_1based + len).saturating_sub(1);
        let _ = write!(
            head,
            "=ybegin part={part} total={total} line={ll} size={file_size} name={name}\r\n\
             =ypart begin={begin_1based} end={end}\r\n"
        );
        let _ = write!(tail, "=yend size={len} part={part} pcrc32={crc:08x}\r\n");
    }
    let body = encode_data(data, ll);
    let mut out = Vec::with_capacity(head.len() + body.len() + tail.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(tail.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_name_takes_rest_of_line() {
        let h = Header::parse(
            b"=ybegin part=1 line=128 size=10 name=My File (1).bin  ",
            "=ybegin",
        );
        assert_eq!(h.name.as_deref(), Some("My File (1).bin"));
        assert_eq!(h.get("size"), Some("10"));
        assert_eq!(h.get("part"), Some("1"));
    }

    #[test]
    fn crc_parsing() {
        let h = Header::parse(b"=yend size=1 pcrc32=0xABCD crc32=zz", "=yend");
        assert_eq!(h.crc("pcrc32"), Some(0xabcd));
        assert_eq!(h.crc("crc32"), None);
    }

    #[test]
    fn keyword_lines_are_found_only_at_line_starts() {
        let body = b"junk =yend\r\n=ybegin size=3 name=a\r\nab=yc\r\n=yendx\r\n=yend size=3\r\n";
        assert_eq!(find_keyword_line(body, 0, b"=ybegin"), Some(12));
        assert_eq!(find_keyword_line(body, 0, b"=yend"), Some(50));
        assert_eq!(find_keyword_line(body, 51, b"=yend"), None);
        assert_eq!(find_keyword_line(b"=yend", 0, b"=yend"), Some(0));
        assert_eq!(take_line(body, 12), (&b"=ybegin size=3 name=a"[..], 35));
        assert_eq!(take_line(b"last", 0), (&b"last"[..], 4));
    }

    #[test]
    fn dangling_escape_and_bare_cr_are_ignored() {
        assert_eq!(decode_data(b"*=\r\n*"), vec![0, 0]);
        assert_eq!(decode_data(b"*=\n*"), vec![0, 0]);
        assert_eq!(decode_data(b"*\r*"), vec![0, 0]);
        assert_eq!(decode_data(b"="), Vec::<u8>::new());
        assert_eq!(decode_data(b""), Vec::<u8>::new());
    }

    #[test]
    fn keyword_detection() {
        assert!(is_keyword_line(b"=ybegin size=1", b"=ybegin"));
        assert!(is_keyword_line(b"=yend", b"=yend"));
        assert!(!is_keyword_line(b"=yendx", b"=yend"));
    }
}
