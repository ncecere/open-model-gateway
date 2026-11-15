//! Human-readable price strings: exact USD and one display line per price line.
use super::*;

/// Exact USD from integer micro-USD: whole dollars without decimals, otherwise
/// at least two decimals with no unnecessary trailing zeros.
pub fn format_usd(microusd: i64) -> String {
    let (sign, n) = if microusd < 0 {
        ("-", microusd.unsigned_abs())
    } else {
        ("", microusd as u64)
    };
    let (whole, frac) = (n / 1_000_000, n % 1_000_000);
    if frac == 0 {
        return format!("{sign}${whole}");
    }
    let mut digits = format!("{frac:06}");
    while digits.len() > 2 && digits.ends_with('0') {
        digits.pop();
    }
    format!("{sign}${whole}.{digits}")
}
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
/// Human display such as `$0.10/M input tokens` or `$0.0205/image (768)`.
pub fn display_line(l: &PriceLine) -> String {
    let noun = || {
        let noun = l.meter.noun();
        let mut c = noun.chars();
        let first = c.next().map(|f| f.to_ascii_uppercase()).unwrap_or(' ');
        format!("{first}{}", c.as_str())
    };
    let Some(r) = l.rate else {
        return format!("{}: not applicable", noun());
    };
    // An explicit zero rate reads "Free" everywhere (never "$0/unit").
    let mut s = if r.microusd_per_batch == 0 {
        format!("{}: Free", noun())
    } else {
        format!(
            "{}{}",
            format_usd(r.microusd_per_batch),
            l.meter.display_unit(r.batch)
        )
    };
    let mut qualifiers = Vec::new();
    if let Some(v) = l.variant {
        qualifiers.push(v.as_str().to_owned());
    }
    if let Some(m) = l.min_prompt_tokens {
        qualifiers.push(format!("prompt > {} tokens", grouped(m)));
    }
    if !qualifiers.is_empty() {
        s.push_str(&format!(" ({})", qualifiers.join(", ")));
    }
    s
}
/// One display string per serialized (stored) line, in order.
pub fn display_lines(lines: &PriceLines) -> Vec<String> {
    lines.0.iter().map(display_line).collect()
}
pub fn display_summary(lines: &PriceLines) -> String {
    display_lines(lines).join(" · ")
}
