//! Tiny statistics helpers — median, percentiles, formatting.
//!
//! Std-only and intentionally not generic: we measure latencies in `u64`
//! milliseconds and there is exactly one shape of input the harness deals
//! in. A general histogram crate would be heavier than the math it replaces.

/// Median of `samples` (the lower of two middles for even N — fine for our
/// small `reps` and matches `ra-latency`'s existing convention).
pub fn median(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    Some(s[s.len() / 2])
}

/// Linear-interpolation percentile, `pct` in `[0.0, 100.0]`. Returns `None`
/// on empty input. Robust to N=1 (returns the single sample).
pub fn percentile(samples: &[u64], pct: f64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    if s.len() == 1 {
        return Some(s[0]);
    }
    let p = pct.clamp(0.0, 100.0) / 100.0;
    let rank = p * (s.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return Some(s[lo]);
    }
    let frac = rank - lo as f64;
    let v = s[lo] as f64 + (s[hi] - s[lo]) as f64 * frac;
    Some(v.round() as u64)
}

/// `min` / `max` / `median` / `p90` summary, formatted on one line for the
/// report. `samples` allowed to be empty — emits a clear sentinel.
pub fn summary_line(samples: &[u64]) -> String {
    if samples.is_empty() {
        return "n=0 (no samples)".to_string();
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let med = median(&s).unwrap();
    let p90 = percentile(&s, 90.0).unwrap();
    format!(
        "n={} min={} median={} p90={} max={} samples_ms={:?}",
        s.len(),
        s.first().unwrap(),
        med,
        p90,
        s.last().unwrap(),
        s
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_empty_is_none() {
        assert!(median(&[]).is_none());
    }

    #[test]
    fn median_odd_picks_middle() {
        assert_eq!(median(&[3, 1, 2]), Some(2));
        assert_eq!(median(&[10, 20, 30, 40, 50]), Some(30));
    }

    #[test]
    fn median_even_picks_upper_of_two_middles_after_sort() {
        // [1, 2, 3, 4] sorted; len/2 = 2 -> s[2] = 3.
        assert_eq!(median(&[2, 4, 1, 3]), Some(3));
    }

    #[test]
    fn percentile_singleton() {
        assert_eq!(percentile(&[42], 0.0), Some(42));
        assert_eq!(percentile(&[42], 50.0), Some(42));
        assert_eq!(percentile(&[42], 100.0), Some(42));
    }

    #[test]
    fn percentile_min_max() {
        let s = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&s, 0.0), Some(10));
        assert_eq!(percentile(&s, 100.0), Some(50));
    }

    #[test]
    fn percentile_interpolates() {
        // [1, 2, 3, 4]; p90 -> rank = 0.9 * 3 = 2.7
        // lo=2 hi=3 -> 3 + (4-3)*0.7 = 3.7 -> rounded 4
        assert_eq!(percentile(&[1, 2, 3, 4], 90.0), Some(4));
    }

    #[test]
    fn summary_line_handles_empty() {
        let s = summary_line(&[]);
        assert!(s.contains("n=0"));
    }

    #[test]
    fn summary_line_has_all_fields() {
        let s = summary_line(&[10, 30, 20]);
        for tag in ["n=", "min=", "median=", "p90=", "max=", "samples_ms="] {
            assert!(s.contains(tag), "missing {tag} in {s}");
        }
    }
}
