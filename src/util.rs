//! Human-facing formatting helpers: sizes, durations, path shortening.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Format a byte count the way a human reads it: `1.4 GB`, `812 MB`, `4.0 KB`.
///
/// Uses base-1024 units but the familiar KB/MB/GB labels, which is what
/// Finder-adjacent tools and developers expect when eyeballing disk usage.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Format a plain count with thousands separators (`12,345`).
pub fn human_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

/// Seconds since the unix epoch, right now.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render "how long ago" for a unix timestamp: `3 mo ago`, `12 d ago`, `just now`.
pub fn human_ago(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "unknown".to_string();
    };
    let delta = now_secs() - ts;
    if delta < 0 {
        return "just now".to_string();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    match delta {
        d if d < MIN => "just now".to_string(),
        d if d < HOUR => format!("{} min ago", d / MIN),
        d if d < DAY => format!("{} h ago", d / HOUR),
        d if d < MONTH => format!("{} d ago", d / DAY),
        d if d < YEAR => format!("{} mo ago", d / MONTH),
        d => {
            let years = d / YEAR;
            let months = (d % YEAR) / MONTH;
            // Kept under 12 columns so the TUI's LAST USED cell never clips.
            if months > 0 && years < 3 {
                format!("{years}y {months}mo ago")
            } else {
                format!("{years} y ago")
            }
        }
    }
}

/// Number of whole days since `ts`, or `None` when the timestamp is unknown.
pub fn days_since(ts: Option<i64>) -> Option<i64> {
    ts.map(|t| (now_secs() - t).max(0) / 86_400)
}

/// Replace the user's home prefix with `~`.
pub fn shorten_home(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().to_string();
        if !home.is_empty() && s.starts_with(&home) {
            return format!("~{}", &s[home.len()..]);
        }
    }
    s
}

/// Middle-elide a string to `width` display columns, keeping the head and the
/// tail (the tail is what identifies an artifact, the head names the project).
pub fn elide_middle(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    if width <= 4 {
        let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
        return format!("…{tail}");
    }
    let keep = width - 1; // one column for the ellipsis
    let head = keep / 3;
    let tail = keep - head;
    let mut out = String::with_capacity(width * 2);
    out.extend(chars[..head].iter());
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// Parse a human size string: `50MB`, `1.5g`, `900k`, `2048`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err("empty size".into());
    }
    let digits_end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(digits_end);
    let num: f64 = num.parse().map_err(|_| format!("bad size: {s}"))?;
    let mult: f64 = match suffix.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit: {other}")),
    };
    Ok((num * mult) as u64)
}

/// Parse a duration in days: `90d`, `6mo`, `1y`, `30` (bare number == days).
pub fn parse_days(s: &str) -> Result<i64, String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err("empty duration".into());
    }
    let digits_end = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(digits_end);
    let num: f64 = num.parse().map_err(|_| format!("bad duration: {s}"))?;
    let days: f64 = match suffix.trim() {
        "" | "d" | "day" | "days" => num,
        "w" | "week" | "weeks" => num * 7.0,
        "mo" | "month" | "months" => num * 30.0,
        "y" | "year" | "years" => num * 365.0,
        other => return Err(format!("unknown duration unit: {other}")),
    };
    Ok(days.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_human() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(150 * 1024 * 1024), "150 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn counts_get_separators() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1000), "1,000");
        assert_eq!(human_count(1234567), "1,234,567");
    }

    #[test]
    fn ago_is_readable() {
        let now = now_secs();
        assert_eq!(human_ago(None), "unknown");
        assert_eq!(human_ago(Some(now)), "just now");
        assert_eq!(human_ago(Some(now - 3600 * 5)), "5 h ago");
        assert_eq!(human_ago(Some(now - 86400 * 12)), "12 d ago");
        assert_eq!(human_ago(Some(now - 86400 * 95)), "3 mo ago");
        assert_eq!(human_ago(Some(now - 86400 * 400)), "1y 1mo ago");
        // Every rendered form must fit the TUI's 12-column cell.
        for days in [0, 1, 20, 400, 800, 1500, 4000] {
            let s = human_ago(Some(now - days * 86400));
            assert!(s.chars().count() <= 12, "{s:?} is too wide");
        }
    }

    #[test]
    fn days_since_counts_whole_days() {
        let now = now_secs();
        assert_eq!(days_since(Some(now)), Some(0));
        assert_eq!(days_since(Some(now - 86400 * 91)), Some(91));
        assert_eq!(days_since(None), None);
    }

    #[test]
    fn eliding_keeps_head_and_tail() {
        let p = "~/projects/some-really-long-project-name/node_modules";
        assert_eq!(elide_middle(p, 200), p);
        let e = elide_middle(p, 30);
        assert_eq!(e.chars().count(), 30);
        assert!(e.contains('…'));
        assert!(e.ends_with("node_modules"));
        assert!(e.starts_with("~/proj"));
        // Degenerate widths must not panic and must respect the budget.
        for w in 0..8 {
            assert!(elide_middle(p, w).chars().count() <= w.max(1));
        }
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("50MB").unwrap(), 50 * 1024 * 1024);
        assert_eq!(parse_size("1.5g").unwrap(), 1610612736);
        assert!(parse_size("12 parsecs").is_err());
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_days("90d").unwrap(), 90);
        assert_eq!(parse_days("30").unwrap(), 30);
        assert_eq!(parse_days("6mo").unwrap(), 180);
        assert_eq!(parse_days("1y").unwrap(), 365);
        assert_eq!(parse_days("2w").unwrap(), 14);
        assert!(parse_days("soon").is_err());
    }
}
