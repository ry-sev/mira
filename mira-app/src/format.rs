//! Display formatting for counts, sizes, and durations.

/// An integer with thousands separators: `12,345`.
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1 request`, `12 requests`, `3 matches`.
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else if ["s", "x", "ch", "sh"].iter().any(|end| noun.ends_with(end)) {
        format!("{} {noun}es", count(n))
    } else {
        format!("{} {noun}s", count(n))
    }
}

/// A byte count: `812 B`, `14.2 KB`, `3.1 MB`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place"
)]
pub fn bytes(n: usize) -> String {
    let value = n as f64;
    if n < 1_000 {
        format!("{n} B")
    } else if n < 1_000_000 {
        format!("{:.*} KB", usize::from(n < 10_000), value / 1e3)
    } else if n < 1_000_000_000 {
        format!("{:.*} MB", usize::from(n < 10_000_000), value / 1e6)
    } else {
        format!("{:.1} GB", value / 1e9)
    }
}

/// A duration in milliseconds: `48 ms`, `1.24 s`, `38.5 s`, `2 m 5 s`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "whole minutes and seconds of a non-negative duration"
)]
pub fn ms(n: f64) -> String {
    if !n.is_finite() || n < 0.0 {
        return "—".to_owned();
    }
    if n < 1_000.0 {
        format!("{} ms", n.round())
    } else if n < 10_000.0 {
        format!("{:.2} s", n / 1_000.0)
    } else if n < 120_000.0 {
        format!("{:.1} s", n / 1_000.0)
    } else {
        let total = n as u64;
        format!(
            "{} m {} s",
            total / 60_000,
            (total % 60_000).div_ceil(1_000)
        )
    }
}

/// An offset from the start of the capture: `+7.240s`.
pub fn offset(n: f64) -> String {
    format!("+{:.3}s", n.max(0.0) / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn test_plural() {
        assert_eq!(plural(1, "match"), "1 match");
        assert_eq!(plural(3, "match"), "3 matches");
        assert_eq!(plural(2, "request"), "2 requests");
        assert_eq!(plural(0, "host"), "0 hosts");
    }

    #[test]
    fn test_bytes() {
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(1_500), "1.5 KB");
        assert_eq!(bytes(14_200), "14 KB");
        assert_eq!(bytes(3_100_000), "3.1 MB");
        assert_eq!(bytes(2_000_000_000), "2.0 GB");
    }

    #[test]
    fn test_ms() {
        assert_eq!(ms(48.4), "48 ms");
        assert_eq!(ms(1_240.0), "1.24 s");
        assert_eq!(ms(38_500.0), "38.5 s");
        assert_eq!(ms(125_000.0), "2 m 5 s");
        assert_eq!(ms(-1.0), "—");
    }

    #[test]
    fn test_offset() {
        assert_eq!(offset(7_240.0), "+7.240s");
    }
}
