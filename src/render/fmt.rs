//! Number formatting. A missing value is always `--`, never 0.

pub const NONE: &str = "--";

/// tok/s: one decimal under 100, integer above.
pub fn tps(v: Option<f64>) -> String {
    match v {
        None => NONE.into(),
        Some(v) if !v.is_finite() => NONE.into(),
        Some(v) if v < 100.0 => format!("{v:.1}"),
        Some(v) => format!("{v:.0}"),
    }
}

/// Fixed-width (5 cells) digits for the giant display: `058.7`, `123.4`,
/// `02310`. `None` gives `---.-`.
pub fn big_tps(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() && v >= 0.0 => {
            if v < 1000.0 {
                format!("{v:05.1}")
            } else {
                format!("{:05.0}", v.min(99999.0))
            }
        }
        _ => "---.-".into(),
    }
}

/// Number of leading zeros of a `big_tps` string drawn as faint "ghost" cells.
pub fn ghost_zeros(s: &str) -> usize {
    let b = s.as_bytes();
    let mut n = 0;
    while n + 1 < b.len() && b[n] == b'0' && b[n + 1].is_ascii_digit() {
        n += 1;
    }
    n
}

pub fn pct(v: Option<f64>) -> String {
    match v {
        None => NONE.into(),
        Some(v) if v.is_nan() => NONE.into(),
        Some(v) if v > 0.0 && v < 0.1 => "<0.1%".into(),
        Some(v) if v < 10.0 && v.fract() != 0.0 => format!("{v:.1}%"),
        Some(v) => format!("{v:.0}%"),
    }
}

pub fn temp(v: Option<f64>) -> String {
    v.map_or(NONE.into(), |v| format!("{v:.0}°C"))
}

pub fn watts(v: Option<f64>) -> String {
    v.map_or(NONE.into(), |v| format!("{v:.0}W"))
}

pub fn mhz(v: Option<u32>) -> String {
    v.map_or(NONE.into(), |v| format!("{v}MHz"))
}

pub fn gb_from_mb(mb: Option<u64>) -> String {
    mb.map_or(NONE.into(), |m| format!("{:.1}GB", m as f64 / 1024.0))
}

pub fn secs(v: Option<f64>) -> String {
    match v {
        None => NONE.into(),
        Some(v) if v < 10.0 => format!("{v:.2}s"),
        Some(v) => format!("{v:.1}s"),
    }
}

/// Compact counts: 999, 1.2K, 524K, 2.1M.
pub fn count(v: Option<u64>) -> String {
    match v {
        None => NONE.into(),
        Some(v) if v < 1000 => v.to_string(),
        Some(v) if v < 10_000 => format!("{:.1}K", v as f64 / 1000.0),
        Some(v) if v < 1_000_000 => format!("{}K", (v as f64 / 1000.0).round() as u64),
        Some(v) => format!("{:.1}M", v as f64 / 1e6),
    }
}

/// "NVIDIA GeForce RTX 3090" -> "RTX 3090".
pub fn short_gpu_name(n: &str) -> String {
    let n = n.trim();
    let n = n.strip_prefix("NVIDIA ").unwrap_or(n);
    let n = n.strip_prefix("GeForce ").unwrap_or(n);
    n.to_string()
}

pub fn opt_str(v: Option<&str>) -> String {
    v.filter(|s| !s.is_empty()).map_or(NONE.into(), |s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_is_dashes() {
        assert_eq!(tps(None), "--");
        assert_eq!(pct(None), "--");
        assert_eq!(temp(None), "--");
        assert_eq!(count(None), "--");
        assert_eq!(secs(None), "--");
        assert_eq!(big_tps(None), "---.-");
        assert_eq!(opt_str(None), "--");
        assert_eq!(opt_str(Some("")), "--");
    }

    #[test]
    fn tps_rules() {
        assert_eq!(tps(Some(53.76)), "53.8");
        assert_eq!(tps(Some(0.0)), "0.0");
        assert_eq!(tps(Some(2310.4)), "2310");
        assert_eq!(big_tps(Some(58.7)), "058.7");
        assert_eq!(big_tps(Some(123.45)), "123.5");
        assert_eq!(big_tps(Some(2310.0)), "02310");
        assert_eq!(big_tps(Some(0.0)), "000.0");
    }

    #[test]
    fn ghosts() {
        assert_eq!(ghost_zeros("058.7"), 1);
        assert_eq!(ghost_zeros("003.2"), 2);
        assert_eq!(ghost_zeros("000.0"), 2);
        assert_eq!(ghost_zeros("123.4"), 0);
        assert_eq!(ghost_zeros("---.-"), 0);
    }

    #[test]
    fn compact_counts() {
        assert_eq!(count(Some(333)), "333");
        assert_eq!(count(Some(1234)), "1.2K");
        assert_eq!(count(Some(524288)), "524K");
        assert_eq!(count(Some(2_100_000)), "2.1M");
        assert_eq!(pct(Some(0.0635)), "<0.1%");
        assert_eq!(pct(Some(0.35)), "0.3%");
        assert_eq!(pct(Some(97.4)), "97%");
        assert_eq!(short_gpu_name("NVIDIA GeForce RTX 3090"), "RTX 3090");
    }
}
