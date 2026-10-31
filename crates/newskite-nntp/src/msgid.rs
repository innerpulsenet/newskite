//! Message-id validation.

use crate::NntpError;

/// Normalizes a message id for use on a command line.
///
/// Accepts ids with or without surrounding angle brackets (surrounding whitespace is
/// ignored) and returns the bracketed form `<id>`. Rejects empty ids and ids containing
/// whitespace, control characters (CR, LF, NUL, ...) or stray `<`/`>` inside.
pub fn normalize_message_id(raw: &str) -> Result<String, NntpError> {
    let trimmed = raw.trim();
    let inner = match trimmed.strip_prefix('<') {
        Some(rest) => rest
            .strip_suffix('>')
            .ok_or_else(|| NntpError::InvalidMessageId(raw.to_owned()))?,
        None => trimmed,
    };
    let bad = inner.is_empty()
        || inner
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>');
    if bad {
        return Err(NntpError::InvalidMessageId(raw.to_owned()));
    }
    Ok(format!("<{inner}>"))
}

/// The id without brackets, for error values.
pub(crate) fn bare(bracketed: &str) -> &str {
    bracketed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(bracketed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_brackets_when_missing() {
        assert_eq!(normalize_message_id("abc@example").unwrap(), "<abc@example>");
        assert_eq!(normalize_message_id("<abc@example>").unwrap(), "<abc@example>");
        assert_eq!(normalize_message_id("  abc@x  ").unwrap(), "<abc@x>");
        assert_eq!(bare("<abc@x>"), "abc@x");
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in [
            "", "<>", "a b", "a\r\nQUIT", "a\nb", "a\0b", "<abc", "a<b", "a>b", "a\tb",
        ] {
            assert!(
                matches!(normalize_message_id(bad), Err(NntpError::InvalidMessageId(_))),
                "{bad:?} should be rejected"
            );
        }
    }
}
