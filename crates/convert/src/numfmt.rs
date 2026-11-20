//! A spreadsheet number as the text its cell shows: the number formats of everyday sheets
//! (General, fixed decimals, thousands separators, percent, currency symbols, dates and times).
//! A format it does not know shows the number as General.

use chrono::{Duration, NaiveDate};

/// The pieces of a number format that matter for showing a value.
#[derive(Default)]
struct Parsed {
    prefix: String,
    suffix: String,
    decimals: usize,
    thousands: bool,
    percent: bool,
    placeholders: bool,
    date: bool,
    time: bool,
    scientific: bool,
}

/// The text of `n` under the format `code`.
pub(crate) fn format(n: f64, code: &str, date_1904: bool) -> String {
    if !n.is_finite() {
        return "#NUM!".to_string();
    }
    let first = first_section(code);
    let p = parse(first);
    if p.date || p.time {
        if let Some(text) = date_text(n, &p, date_1904) {
            return text;
        }
        return general(n);
    }
    if !p.placeholders || p.scientific {
        return general(n);
    }
    let value = if p.percent { n * 100.0 } else { n };
    let digits = fixed(value.abs(), p.decimals, p.thousands);
    let sign = if value < 0.0 && digits.chars().any(|c| c.is_ascii_digit() && c != '0') { "-" } else { "" };
    format!("{sign}{}{digits}{}", p.prefix, p.suffix)
}

/// The number as Excel's General format shows it: whole numbers plainly, others to about ten
/// significant digits without trailing zeros.
fn general(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        // A whole number below 1e15 is exactly an i64.
        return format!("{}", n as i64);
    }
    let rounded: f64 = format!("{n:.9e}").parse().unwrap_or(n);
    format!("{rounded}")
}

/// `v` (not negative) with `decimals` places, with thousands separators when asked.
fn fixed(v: f64, decimals: usize, thousands: bool) -> String {
    let text = format!("{v:.decimals$}");
    if !thousands {
        return text;
    }
    let (int, frac) = text.split_once('.').map_or((text.as_str(), None), |(i, f)| (i, Some(f)));
    let mut out = String::new();
    for (k, c) in int.chars().enumerate() {
        if k > 0 && (int.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    out
}

/// The format's first section: up to the first `;` outside quotes.
fn first_section(code: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in code.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return code.get(..i).unwrap_or(code),
            _ => {}
        }
    }
    code
}

fn parse(code: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut chars = code.chars().peekable();
    let mut in_number = false;
    let mut after_point = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut lit = String::new();
                for q in chars.by_ref() {
                    if q == '"' {
                        break;
                    }
                    lit.push(q);
                }
                push_literal(&mut p, in_number, &lit);
            }
            '\\' | '_' => {
                if let Some(next) = chars.next()
                    && c == '\\'
                {
                    push_literal(&mut p, in_number, &next.to_string());
                }
            }
            '*' => {
                chars.next();
            }
            '[' => {
                let mut tag = String::new();
                for q in chars.by_ref() {
                    if q == ']' {
                        break;
                    }
                    tag.push(q);
                }
                // `[$€-407]` is a currency symbol; `[Red]`, `[h]` and the rest are not text.
                if let Some(rest) = tag.strip_prefix('$') {
                    let symbol: String = rest.split('-').next().unwrap_or_default().to_string();
                    push_literal(&mut p, in_number, &symbol);
                } else if matches!(tag.to_ascii_lowercase().as_str(), "h" | "hh" | "m" | "mm" | "s" | "ss") {
                    p.time = true;
                }
            }
            '0' | '#' | '?' => {
                in_number = true;
                p.placeholders = true;
                if after_point {
                    p.decimals += usize::from(c != '#');
                }
            }
            '.' if in_number || chars.peek().is_some_and(|n| matches!(n, '0' | '#' | '?')) => {
                in_number = true;
                after_point = true;
            }
            ',' if in_number => {
                p.thousands = true;
            }
            '%' => {
                p.percent = true;
                push_literal(&mut p, in_number, "%");
            }
            'E' | 'e' if chars.peek().is_some_and(|n| matches!(n, '+' | '-')) => p.scientific = true,
            'y' | 'Y' | 'd' | 'D' => p.date = true,
            'h' | 'H' | 's' | 'S' => p.time = true,
            'm' | 'M' => p.date |= !p.time,
            c if c.is_alphabetic() => {}
            c => push_literal(&mut p, in_number, &c.to_string()),
        }
    }
    // `m` beside hours or seconds is minutes.
    if p.time {
        p.date &= code.chars().any(|c| matches!(c, 'y' | 'Y' | 'd' | 'D'));
    }
    p
}

/// A literal before the number's digits goes in the prefix, after them in the suffix.
fn push_literal(p: &mut Parsed, in_number: bool, text: &str) {
    if in_number || p.placeholders {
        p.suffix.push_str(text);
    } else {
        p.prefix.push_str(text);
    }
}

/// A serial date and time as `YYYY-MM-DD`, `HH:MM[:SS]` or both.
fn date_text(serial: f64, p: &Parsed, date_1904: bool) -> Option<String> {
    if !(0.0..2_958_466.0).contains(&serial) {
        return None;
    }
    let whole = serial.floor();
    let seconds = ((serial - whole) * 86_400.0).round() as i64;
    let (mut days, seconds) = (whole as i64, seconds);
    let mut out = String::new();
    if p.date {
        // The 1900 system counts a 29 February 1900 that did not exist: serials before March
        // are one day further on than the calendar says.
        let base = if date_1904 {
            NaiveDate::from_ymd_opt(1904, 1, 1)?
        } else {
            if days < 61 {
                days += 1;
            }
            NaiveDate::from_ymd_opt(1899, 12, 30)?
        };
        let date = base.checked_add_signed(Duration::try_days(days)?)?;
        out.push_str(&date.format("%Y-%m-%d").to_string());
    }
    if p.time {
        if !out.is_empty() {
            out.push(' ');
        }
        let (h, m, s) = (seconds / 3600 % 24, seconds / 60 % 60, seconds % 60);
        out.push_str(&format!("{h:02}:{m:02}"));
        if s != 0 {
            out.push_str(&format!(":{s:02}"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn general_numbers() {
        assert_eq!(format(3.0, "General", false), "3");
        assert_eq!(format(0.1 + 0.2, "General", false), "0.3");
        assert_eq!(format(-12.5, "General", false), "-12.5");
        assert_eq!(format(1234567.891, "General", false), "1234567.891");
    }

    #[test]
    fn fixed_percent_and_thousands() {
        assert_eq!(format(12.3456, "0.00", false), "12.35");
        assert_eq!(format(1234567.5, "#,##0.00", false), "1,234,567.50");
        assert_eq!(format(0.256, "0%", false), "26%");
        assert_eq!(format(0.2567, "0.00%", false), "25.67%");
        assert_eq!(format(1234.5, "\"$\"#,##0.00", false), "$1,234.50");
        assert_eq!(format(-5.0, "0", false), "-5");
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(format(45292.0, "m/d/yyyy", false), "2024-01-01");
        assert_eq!(format(45292.5, "m/d/yyyy h:mm", false), "2024-01-01 12:00");
        assert_eq!(format(0.75, "h:mm", false), "18:00");
        assert_eq!(format(1.0, "d-mmm-yy", false), "1900-01-01");
        assert_eq!(format(61.0, "yyyy-mm-dd", false), "1900-03-01");
        assert_eq!(format(0.0, "yyyy-mm-dd", true), "1904-01-01");
    }

    #[test]
    fn unknown_formats_show_general() {
        assert_eq!(format(1500.0, "0.00E+00", false), "1500");
        assert_eq!(format(2.5, "@", false), "2.5");
        assert_eq!(format(f64::NAN, "0", false), "#NUM!");
    }
}
