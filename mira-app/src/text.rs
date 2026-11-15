//! Turning captured bytes into something safe to read.
//!
//! Captured traffic comes from whatever the client talked to. Nothing here
//! interprets it: the result is plain text for text elements, or rows of a
//! hex dump.

/// How much of a body is sampled to decide whether it is binary.
const SAMPLE: usize = 8192;

/// Bytes per row of a hex dump.
pub const HEX_WIDTH: usize = 16;

/// Whether `bytes` look like binary data rather than text.
pub fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(SAMPLE)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|&&b| b < 7 || (b > 13 && b < 32 && b != 27))
        .count();
    control * 10 > sample.len()
}

/// Whether `ch` changes how surrounding text displays without being seen:
/// a C0 control, DEL, a zero-width mark, or a bidirectional override.
fn is_invisible(ch: char) -> bool {
    matches!(ch,
        '\u{0}'..='\u{8}'
        | '\u{b}'
        | '\u{c}'
        | '\u{e}'..='\u{1f}'
        | '\u{7f}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{feff}')
}

/// The visible stand-in for an invisible character, one per character so
/// columns still line up.
fn stand_in(ch: char) -> char {
    let code = u32::from(ch);
    if code < 0x20 {
        char::from_u32(0x2400 + code).unwrap_or('⬚')
    } else if code == 0x7f {
        '␡'
    } else {
        '⬚'
    }
}

/// `text` with every invisible character replaced by a visible stand-in.
pub fn visible(text: &str) -> String {
    text.chars()
        .map(|ch| if is_invisible(ch) { stand_in(ch) } else { ch })
        .collect()
}

/// How many characters [`visible`] would replace.
pub fn invisible_count(text: &str) -> usize {
    text.chars().filter(|&ch| is_invisible(ch)).count()
}

/// `text` re-indented, if it is JSON.
pub fn pretty_json(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// One row of a hex dump: its offset, its bytes, and their printable ASCII.
pub fn hex_row(bytes: &[u8], row: usize) -> (String, String, String) {
    let start = row * HEX_WIDTH;
    let end = (start + HEX_WIDTH).min(bytes.len());
    let mut hex = String::with_capacity(HEX_WIDTH * 3 + 1);
    let mut ascii = String::with_capacity(HEX_WIDTH);
    for i in start..start + HEX_WIDTH {
        if i == start + 8 {
            hex.push(' ');
        }
        if let Some(&b) = bytes.get(i).filter(|_| i < end) {
            hex.push_str(&format!("{b:02x} "));
            ascii.push(if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '·'
            });
        } else {
            hex.push_str("   ");
        }
    }
    (format!("{start:08x}"), hex, ascii)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b"hello\nworld\t"));
        assert!(is_binary(b"\x89PNG\r\n\x1a\n\0\0\0"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_visible_replaces_hidden_characters_one_for_one() {
        let text = "a\u{202e}b\u{0}c\u{200b}";
        let shown = visible(text);
        assert_eq!(shown, "a⬚b␀c⬚");
        assert_eq!(shown.chars().count(), text.chars().count());
        assert_eq!(invisible_count(text), 3);
        assert_eq!(visible("tabs\tand\nnewlines"), "tabs\tand\nnewlines");
    }

    #[test]
    fn test_pretty_json() {
        assert_eq!(
            pretty_json(r#"{"a":1}"#).as_deref(),
            Some("{\n  \"a\": 1\n}")
        );
        assert_eq!(pretty_json("not json"), None);
        assert_eq!(pretty_json("{broken"), None);
    }

    #[test]
    fn test_hex_row() {
        let (offset, hex, ascii) = hex_row(b"Hi\x00", 0);
        assert_eq!(offset, "00000000");
        assert!(hex.starts_with("48 69 00 "));
        assert_eq!(ascii, "Hi·");
    }
}
