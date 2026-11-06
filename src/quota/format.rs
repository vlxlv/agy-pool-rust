use super::parse_quota_fraction;
use crate::storage::{Account, QuotaState};

pub const CLR_RESET: &str = "\x1b[0m";
pub const CLR_DIM: &str = "\x1b[2m";
pub const CLR_GREEN: &str = "\x1b[32m";
pub const CLR_YELLOW: &str = "\x1b[33m";
pub const CLR_RED: &str = "\x1b[31m";

/// Helper trait for types that can represent a quota reset time.
pub trait AsResetValue {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)>;
}

impl AsResetValue for Option<&serde_json::Value> {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        match *self {
            None => None,
            Some(v) => v.extract_reset_raw(),
        }
    }
}

impl AsResetValue for Option<serde_json::Value> {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        self.as_ref().and_then(|v| v.extract_reset_raw())
    }
}

impl AsResetValue for serde_json::Value {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        match self {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => {
                let ts = super::parse_iso_or_timestamp(s.as_str());
                Some((ts, s.clone()))
            }
            serde_json::Value::Number(n) => {
                let f = n.as_f64();
                Some((f, n.to_string()))
            }
            other => Some((None, other.to_string())),
        }
    }
}

impl AsResetValue for &serde_json::Value {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        (*self).extract_reset_raw()
    }
}

impl AsResetValue for &str {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        let ts = super::parse_iso_or_timestamp(*self);
        Some((ts, (*self).to_string()))
    }
}

impl AsResetValue for Option<&str> {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        self.map(|s| {
            let ts = super::parse_iso_or_timestamp(s);
            (ts, s.to_string())
        })
    }
}

impl AsResetValue for Option<String> {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        self.as_deref().and_then(|s| s.extract_reset_raw())
    }
}

impl AsResetValue for String {
    fn extract_reset_raw(&self) -> Option<(Option<f64>, String)> {
        self.as_str().extract_reset_raw()
    }
}

/// RenderProgressBar renders the visual quota gauge matching Go/Python reference.
pub fn render_progress_bar(fraction: Option<f64>, width: usize) -> String {
    let width = if width == 0 { 10 } else { width };
    let Some(mut f) = fraction else {
        return format!("[{}{}{}]   N/A", CLR_DIM, "─".repeat(width), CLR_RESET);
    };

    f = f.clamp(0.0, 1.0);

    let mut filled = (f * width as f64).round() as usize;
    if filled > width {
        filled = width;
    }
    let empty = width - filled;

    let pct = format!("{:5.1}%", f * 100.0);
    let color = if f > 0.4 {
        CLR_GREEN
    } else if f > 0.15 {
        CLR_YELLOW
    } else {
        CLR_RED
    };

    let filled_bar = "━".repeat(filled);
    let empty_bar = "─".repeat(empty);

    format!(
        "[{}{}{}{}{}{}] {}{}{}",
        color, filled_bar, CLR_RESET, CLR_DIM, empty_bar, CLR_RESET, color, pct, CLR_RESET
    )
}

/// RenderCompactProgressBar renders a compact progress bar (e.g. for preview lists).
pub fn render_compact_progress_bar(fraction: Option<f64>, width: usize) -> String {
    let width = if width == 0 { 8 } else { width };
    let Some(mut f) = fraction else {
        return format!("[{}{}{}] N/A", CLR_DIM, "─".repeat(width), CLR_RESET);
    };

    f = f.clamp(0.0, 1.0);

    let mut filled = (f * width as f64).round() as usize;
    if filled > width {
        filled = width;
    }
    let empty = width - filled;

    let pct = format!("{}%", (f * 100.0).round() as i64);
    let color = if f > 0.4 {
        CLR_GREEN
    } else if f > 0.15 {
        CLR_YELLOW
    } else {
        CLR_RED
    };

    let filled_bar = "━".repeat(filled);
    let empty_bar = "─".repeat(empty);

    format!(
        "[{}{}{}{}{}{}] {}{}{}",
        color, filled_bar, CLR_RESET, CLR_DIM, empty_bar, CLR_RESET, color, pct, CLR_RESET
    )
}

/// FormatRemainingTime formats a reset timestamp into relative string matching Python/Go reference.
pub fn format_remaining_time(reset_time: impl AsResetValue) -> String {
    let Some((ts, raw_str)) = reset_time.extract_reset_raw() else {
        return "N/A".to_string();
    };

    let Some(ts) = ts else {
        if raw_str.len() > 16 {
            return raw_str[..16].to_string();
        }
        return raw_str;
    };

    let now_sec = (crate::clock::now_unix() as i64) as f64;
    let diff_sec = (ts - now_sec) as i64;
    if diff_sec <= 0 {
        return "Ready".to_string();
    }

    let days = diff_sec / 86400;
    let rem = diff_sec % 86400;
    let hours = rem / 3600;
    let rem = rem % 3600;
    let minutes = rem / 60;

    if days > 0 {
        format!("in {}d {}h", days, hours)
    } else if hours > 0 {
        format!("in {}h {}m", hours, minutes)
    } else {
        format!("in {}m", minutes)
    }
}

/// DisplayQuotaFractions extracts (q5, q7) fractions from last_quota with legacy fallback.
pub fn display_quota_fractions(q: Option<&QuotaState>) -> (Option<f64>, Option<f64>) {
    let Some(q) = q else {
        return (None, None);
    };

    let has_explicit = q.gemini_5h.is_some() || q.gemini_weekly.is_some();
    let mut q5 = None;
    let mut q7 = None;

    if let Some(w) = &q.gemini_5h
        && let Some(f) = w.fraction
    {
        q5 = parse_quota_fraction(f);
    }
    if let Some(w) = &q.gemini_weekly
        && let Some(f) = w.fraction
    {
        q7 = parse_quota_fraction(f);
    }

    if !has_explicit && let Some(rf) = q.remaining_fraction {
        let legacy = parse_quota_fraction(rf);
        q5 = legacy;
        q7 = legacy;
    }

    (q5, q7)
}

/// RenderBlockProgressBar renders a block gauge using full block (█) and light shade (░).
pub fn render_block_progress_bar(fraction: Option<f64>, width: i32) -> String {
    if width <= 0 {
        return String::new();
    }
    let width = width as usize;
    let Some(mut f) = fraction else {
        return format!("{}{}{}", CLR_DIM, "░".repeat(width), CLR_RESET);
    };

    f = f.clamp(0.0, 1.0);

    let mut filled = (f * width as f64).round() as usize;
    if filled > width {
        filled = width;
    }
    let empty = width - filled;

    let color = if f > 0.4 {
        CLR_GREEN
    } else if f > 0.15 {
        CLR_YELLOW
    } else {
        CLR_RED
    };

    let filled_bar = "█".repeat(filled);
    let empty_bar = "░".repeat(empty);

    format!(
        "{}{}{}{}{}{}",
        color, filled_bar, CLR_RESET, CLR_DIM, empty_bar, CLR_RESET
    )
}

/// FormatCompactRemainingTime formats a reset timestamp into compact form (e.g. "4h57m", "5d15h", "ready").
pub fn format_compact_remaining_time(reset_time: impl AsResetValue) -> String {
    let Some((ts, raw_str)) = reset_time.extract_reset_raw() else {
        return "N/A".to_string();
    };

    let Some(ts) = ts else {
        if raw_str.len() > 10 {
            return raw_str[..10].to_string();
        }
        return raw_str;
    };

    let now_sec = (crate::clock::now_unix() as i64) as f64;
    let diff_sec = (ts - now_sec) as i64;
    if diff_sec <= 0 {
        return "ready".to_string();
    }

    let days = diff_sec / 86400;
    let rem = diff_sec % 86400;
    let hours = rem / 3600;
    let rem = rem % 3600;
    let minutes = rem / 60;

    if days > 0 {
        format!("{}d{}h", days, hours)
    } else if hours > 0 {
        format!("{}h{}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

/// FormatShortQuotaAge returns a compact representation of snapshot age (e.g. "2m", "45s", "1h").
pub fn format_short_quota_age(acc: &Account, now: f64) -> String {
    let info = super::freshness(acc, now);
    let Some(age) = info.age else {
        return "unk".to_string();
    };
    let mut age_sec = age as i64;
    if age_sec < 0 {
        age_sec = 0;
    }
    if age_sec >= 3600 {
        format!("{}h", age_sec / 3600)
    } else if age_sec >= 60 {
        format!("{}m", age_sec / 60)
    } else {
        format!("{}s", age_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi_string(s: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for c in s.chars() {
            if c == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if c.is_alphabetic() {
                    in_escape = false;
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn test_render_block_progress_bar() {
        let cases: &[(Option<f64>, i32, &str)] = &[
            (None, 10, "░░░░░░░░░░"),
            (Some(1.0), 10, "██████████"),
            (Some(0.5), 10, "█████░░░░░"),
            (Some(0.0), 10, "░░░░░░░░░░"),
            (Some(0.2), 10, "██░░░░░░░░"),
            (Some(0.5), 0, ""),
            (Some(0.5), -1, ""),
        ];

        for &(frac, width, exp_chars) in cases {
            let out = render_block_progress_bar(frac, width);
            let clean = strip_ansi_string(&out);
            assert_eq!(
                clean, exp_chars,
                "frac {:?}: expected {:?}, got {:?}",
                frac, exp_chars, clean
            );
        }
    }

    #[test]
    fn test_format_compact_remaining_time() {
        // Other tests override crate::clock while holding TEST_LOCK; this one reads the real time.
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::clock::now_unix() as i64;

        let at = |offset: i64| -> String {
            let ts = jiff::Timestamp::from_second(now + offset).unwrap();
            ts.to_string()
        };

        // Ready in the past
        let past = at(-600);
        assert_eq!(format_compact_remaining_time(past.as_str()), "ready");

        // 4h57m
        let f4h57m = at(4 * 3600 + 57 * 60);
        assert_eq!(format_compact_remaining_time(f4h57m.as_str()), "4h57m");

        // 5d15h
        let f5d15h = at(5 * 86400 + 15 * 3600);
        assert_eq!(format_compact_remaining_time(f5d15h.as_str()), "5d15h");

        // nil -> N/A
        assert_eq!(format_compact_remaining_time(None::<&str>), "N/A");
    }

    #[test]
    fn test_format_short_quota_age() {
        let now = 1000000.0;

        // nil last quota -> unk
        let acc_nil = Account::default();
        assert_eq!(format_short_quota_age(&acc_nil, now), "unk");

        // 45s
        let up45 = (now - 45.0) as i64;
        let acc45 = Account {
            last_quota: Some(QuotaState {
                updated_at: Some(up45),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(format_short_quota_age(&acc45, now), "45s");

        // 2m
        let up2m = (now - 154.0) as i64;
        let acc2m = Account {
            last_quota: Some(QuotaState {
                updated_at: Some(up2m),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(format_short_quota_age(&acc2m, now), "2m");

        // 3h
        let up3h = (now - 3.0 * 3600.0 - 10.0) as i64;
        let acc3h = Account {
            last_quota: Some(QuotaState {
                updated_at: Some(up3h),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(format_short_quota_age(&acc3h, now), "3h");
    }
}
