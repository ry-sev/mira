//! Helpers for header values that are comma-separated lists.
//!
//! `Connection`, `Upgrade`, `Transfer-Encoding` and friends carry a list
//! of tokens (RFC 7230 §7). Two shortcuts go wrong on them: comparing the
//! whole value for equality misses `Connection: keep-alive, close`, and a
//! substring search matches too much — `Connection: x-upgrade-hint`
//! contains "upgrade" without listing it. Splitting on commas and
//! comparing whole tokens is the only reading that matches the grammar.

/// Whether a comma-separated header value lists `token` as one of its
/// elements, compared case-insensitively and ignoring surrounding
/// whitespace.
pub fn header_list_contains(value: &str, token: &str) -> bool {
    header_list_tokens(value).any(|element| element.eq_ignore_ascii_case(token))
}

/// Iterate the elements of a comma-separated header value, trimmed.
///
/// Empty elements are skipped: `a,,b` and `a, b` list the same two.
pub fn header_list_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|element| !element.is_empty())
}

/// Every value of header `name`, across repeated fields, in order.
pub(crate) fn header_values<'a>(
    headers: &'a [(String, String)],
    name: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .iter()
        .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The body length a message's `Content-Length` fields declare.
///
/// `Ok(None)` when there is none. Repeated fields, or a list such as
/// `5, 5`, are accepted when every element is the same number (RFC 9112
/// §6.3). Anything else is an `Err`: a value that is not plain digits,
/// or values that disagree. `"+5".parse()` would succeed, so digits are
/// checked first.
pub(crate) fn declared_content_length(headers: &[(String, String)]) -> Result<Option<usize>, ()> {
    let mut declared = None;
    for element in header_values(headers, "content-length").flat_map(|v| v.split(',')) {
        let element = element.trim();
        if element.is_empty() || !element.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        let length: usize = element.parse().map_err(|_overflow| ())?;
        match declared {
            Some(previous) if previous != length => return Err(()),
            _ => declared = Some(length),
        }
    }
    Ok(declared)
}

/// Whether a message carries any `Transfer-Encoding` field.
pub(crate) fn has_transfer_encoding(headers: &[(String, String)]) -> bool {
    header_values(headers, "transfer-encoding").next().is_some()
}

/// Whether the last transfer coding, across every `Transfer-Encoding`
/// field, is `chunked`.
///
/// Only the final coding frames the message (RFC 9112 §6.3). With
/// `chunked, gzip` the body is not chunk-framed at all.
pub(crate) fn final_coding_is_chunked(headers: &[(String, String)]) -> bool {
    header_values(headers, "transfer-encoding")
        .flat_map(header_list_tokens)
        .last()
        .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn test_declared_content_length() {
        assert_eq!(declared_content_length(&fields(&[])), Ok(None));
        assert_eq!(
            declared_content_length(&fields(&[("Content-Length", "5")])),
            Ok(Some(5))
        );
        // Agreeing repeats are one length.
        assert_eq!(
            declared_content_length(&fields(&[("Content-Length", "5, 5")])),
            Ok(Some(5))
        );
        assert_eq!(
            declared_content_length(&fields(&[("content-length", "5"), ("Content-Length", "5")])),
            Ok(Some(5))
        );
        // Disagreement and junk are errors, not a guess.
        for bad in [
            fields(&[("Content-Length", "5"), ("Content-Length", "6")]),
            fields(&[("Content-Length", "+5")]),
            fields(&[("Content-Length", "-1")]),
            fields(&[("Content-Length", "5 5")]),
            fields(&[("Content-Length", "")]),
            fields(&[("Content-Length", "99999999999999999999999")]),
        ] {
            assert_eq!(declared_content_length(&bad), Err(()), "{bad:?}");
        }
    }

    #[test]
    fn test_final_coding_is_chunked() {
        assert!(final_coding_is_chunked(&fields(&[(
            "Transfer-Encoding",
            "gzip, chunked"
        )])));
        assert!(final_coding_is_chunked(&fields(&[
            ("Transfer-Encoding", "gzip"),
            ("Transfer-Encoding", "chunked"),
        ])));
        assert!(!final_coding_is_chunked(&fields(&[(
            "Transfer-Encoding",
            "chunked, gzip"
        )])));
        assert!(!final_coding_is_chunked(&fields(&[
            ("Transfer-Encoding", "chunked"),
            ("Transfer-Encoding", "gzip"),
        ])));
        assert!(!final_coding_is_chunked(&fields(&[])));
    }

    #[test]
    fn test_header_list_contains_single_token() {
        assert!(header_list_contains("close", "close"));
        assert!(header_list_contains("Close", "close"));
        assert!(header_list_contains(" close ", "close"));
    }

    #[test]
    fn test_header_list_contains_finds_a_token_in_a_list() {
        assert!(header_list_contains("keep-alive, close", "close"));
        assert!(header_list_contains("close, keep-alive", "close"));
        assert!(header_list_contains("a,b,upgrade,c", "upgrade"));
    }

    #[test]
    fn test_header_list_contains_rejects_substrings() {
        // The bug this helper exists to prevent: a token that merely
        // contains the word is not the word.
        assert!(!header_list_contains("x-upgrade-hint", "upgrade"));
        assert!(!header_list_contains(
            "keep-alive, x-upgrade-hint",
            "upgrade"
        ));
        assert!(!header_list_contains("closed", "close"));
        assert!(!header_list_contains("not-close", "close"));
    }

    #[test]
    fn test_header_list_contains_handles_empty_values() {
        assert!(!header_list_contains("", "close"));
        assert!(!header_list_contains(" , ", "close"));
        assert!(header_list_contains("a,,close", "close"));
    }

    #[test]
    fn test_header_list_tokens_trims_and_skips_empties() {
        let tokens: Vec<&str> = header_list_tokens(" gzip ,, deflate,").collect();
        assert_eq!(tokens, vec!["gzip", "deflate"]);
    }
}
