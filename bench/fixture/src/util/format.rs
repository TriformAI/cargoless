//! Dependency-free number formatting. Kept non-trivial (grouping logic,
//! rounding) so the crate has a bit of real branching for analysis.

/// Group an integer with thousands separators: `1234567 -> "1,234,567"`.
pub fn thousands(n: i64) -> String {
    let neg = n < 0;
    let mut digits: Vec<u8> = n.unsigned_abs().to_string().into_bytes();
    digits.reverse();

    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3);
    for (i, d) in digits.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(b',');
        }
        out.push(*d);
    }
    out.reverse();

    let mut s = String::from_utf8(out).unwrap_or_default();
    if neg {
        s.insert(0, '-');
    }
    s
}

/// Render a percentage with one decimal place: `33.333 -> "33.3%"`.
pub fn percent(p: f64) -> String {
    let clamped = if p.is_finite() { p } else { 0.0 };
    let rounded = (clamped * 10.0).round() / 10.0;
    format!("{rounded:.1}%")
}

/// Truncate with an ellipsis. Char-boundary safe.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-98765), "-98,765");
    }

    #[test]
    fn formats_percent() {
        assert_eq!(percent(33.333), "33.3%");
        assert_eq!(percent(100.0), "100.0%");
    }

    #[test]
    fn ellipsizes() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("hello world", 5), "hell…");
    }
}
