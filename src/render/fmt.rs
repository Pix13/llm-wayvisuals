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

fn find_ci(s: &str, pat: &str) -> Option<usize> {
    let (b, p) = (s.as_bytes(), pat.as_bytes());
    if p.is_empty() || b.len() < p.len() {
        return None;
    }
    (0..=b.len() - p.len()).find(|&i| b[i..i + p.len()].eq_ignore_ascii_case(p) && s.is_char_boundary(i))
}

/// Short display name of a GPU: strips a leading "NVIDIA " / "GeForce ",
/// a "-PCIE…" / "-SXM…" suffix and trailing memory sizes ("32GB").
/// "NVIDIA GeForce RTX 3090" -> "RTX 3090", "Tesla V100-PCIE-32GB" ->
/// "Tesla V100", "NVIDIA A100-SXM4-80GB" -> "A100". Capped at 256 chars.
pub fn short_gpu_name(n: &str) -> String {
    let mut n = super::draw::cap(n).trim();
    n = n.strip_prefix("NVIDIA ").unwrap_or(n);
    n = n.strip_prefix("GeForce ").unwrap_or(n);
    for pat in ["-pcie", "-sxm"] {
        if let Some(i) = find_ci(n, pat) {
            n = &n[..i];
        }
    }
    // trailing " 32GB" / "-32GB"
    let t = n.trim_end();
    if t.len() > 2 && t[t.len() - 2..].eq_ignore_ascii_case("gb") {
        let body = &t[..t.len() - 2];
        let digits = body.chars().rev().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            let cut = body.len() - digits;
            if cut > 0 && matches!(body.as_bytes()[cut - 1], b' ' | b'-') {
                n = &body[..cut - 1];
            }
        }
    }
    n.trim().to_string()
}

/// "2× RTX 3090" for identical GPUs, "2 GPUs" for mixed ones, the bare
/// name for a single GPU, `--` for none.
pub fn gpu_group_label(names: &[Option<&str>]) -> String {
    let short: Vec<String> = names.iter().map(|n| n.map(short_gpu_name).unwrap_or_default()).collect();
    match short.len() {
        0 => NONE.into(),
        1 if short[0].is_empty() => NONE.into(),
        1 => short[0].clone(),
        n if !short[0].is_empty() && short.iter().all(|s| *s == short[0]) => format!("{n}× {}", short[0]),
        n => format!("{n} GPUs"),
    }
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
        assert_eq!(short_gpu_name("Tesla V100-PCIE-32GB"), "Tesla V100");
        assert_eq!(short_gpu_name("NVIDIA A100-SXM4-80GB"), "A100");
        assert_eq!(short_gpu_name("NVIDIA RTX A6000"), "RTX A6000");
        assert_eq!(short_gpu_name("AMD Radeon RX 7900 XTX"), "Radeon RX 7900 XTX".replace("Radeon", "AMD Radeon"));
        assert_eq!(short_gpu_name("Apple M3 Max"), "Apple M3 Max");
        assert_eq!(short_gpu_name("Tesla V100-32GB"), "Tesla V100");
        assert_eq!(short_gpu_name("Quadro RTX 8000 48GB"), "Quadro RTX 8000");
        assert_eq!(short_gpu_name("NVIDIA GeForce RTX 4090 D"), "RTX 4090 D");
        assert_eq!(short_gpu_name(&"é".repeat(1000)).chars().count(), 256);
        assert_eq!(gpu_group_label(&[Some("NVIDIA GeForce RTX 3090"), Some("NVIDIA GeForce RTX 3090")]), "2× RTX 3090");
        assert_eq!(gpu_group_label(&[Some("NVIDIA GeForce RTX 3090"), Some("Tesla V100-PCIE-32GB")]), "2 GPUs");
        assert_eq!(gpu_group_label(&[Some("NVIDIA A100-SXM4-80GB")]), "A100");
        assert_eq!(gpu_group_label(&[]), "--");
    }
}
