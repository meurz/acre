//! Number formatting.

/// Allocated size in base 1024, with about three significant digits.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// Count with thousands separators.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Percentage with one decimal place; values below 0.1% display as <0.1%.
pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "—".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p > 0.0 && p < 0.1 { "<0.1%".into() } else { format!("{p:.1}%") }
}
