pub mod format;
pub mod history;
pub mod refresh;

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use format::*;
pub use history::*;
pub use refresh::*;

use crate::config;
use crate::storage::{Account, QuotaWindow};

pub const BACKEND_HOST: &str = "daily-cloudcode-pa.googleapis.com";
pub const BACKEND_URL_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const DEFAULT_UA: &str = "antigravity/cli";

/// Helper trait to accept different types as quota fraction.
pub trait AsQuotaFraction {
    fn to_quota_fraction(&self) -> Option<f64>;
}

impl AsQuotaFraction for f64 {
    fn to_quota_fraction(&self) -> Option<f64> {
        Some(*self)
    }
}

impl AsQuotaFraction for f32 {
    fn to_quota_fraction(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsQuotaFraction for i64 {
    fn to_quota_fraction(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsQuotaFraction for i32 {
    fn to_quota_fraction(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsQuotaFraction for usize {
    fn to_quota_fraction(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsQuotaFraction for &str {
    fn to_quota_fraction(&self) -> Option<f64> {
        self.trim().parse::<f64>().ok()
    }
}

impl AsQuotaFraction for String {
    fn to_quota_fraction(&self) -> Option<f64> {
        self.as_str().to_quota_fraction()
    }
}

impl AsQuotaFraction for serde_json::Value {
    fn to_quota_fraction(&self) -> Option<f64> {
        match self {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.as_str().to_quota_fraction(),
            _ => None,
        }
    }
}

impl AsQuotaFraction for &serde_json::Value {
    fn to_quota_fraction(&self) -> Option<f64> {
        (*self).to_quota_fraction()
    }
}

impl<T: AsQuotaFraction> AsQuotaFraction for Option<T> {
    fn to_quota_fraction(&self) -> Option<f64> {
        self.as_ref().and_then(|v| v.to_quota_fraction())
    }
}

/// ParseQuotaFraction returns a clamped [0.0, 1.0] fraction or None if invalid.
pub fn parse_quota_fraction(v: impl AsQuotaFraction) -> Option<f64> {
    let raw = v.to_quota_fraction()?;
    if raw.is_nan() || raw.is_infinite() {
        return None;
    }
    if raw < 0.0 {
        Some(0.0)
    } else if raw > 1.0 {
        Some(1.0)
    } else {
        Some(raw)
    }
}

/// Helper trait to parse ISO or timestamp from various input types.
pub trait AsIsoOrTimestamp {
    fn to_unix_seconds(&self) -> Option<f64>;
}

impl AsIsoOrTimestamp for f64 {
    fn to_unix_seconds(&self) -> Option<f64> {
        if self.is_nan() || self.is_infinite() {
            None
        } else {
            Some(*self)
        }
    }
}

impl AsIsoOrTimestamp for f32 {
    fn to_unix_seconds(&self) -> Option<f64> {
        let f = *self as f64;
        if f.is_nan() || f.is_infinite() { None } else { Some(f) }
    }
}

impl AsIsoOrTimestamp for i64 {
    fn to_unix_seconds(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsIsoOrTimestamp for i32 {
    fn to_unix_seconds(&self) -> Option<f64> {
        Some(*self as f64)
    }
}

impl AsIsoOrTimestamp for &str {
    fn to_unix_seconds(&self) -> Option<f64> {
        let s = self.trim();
        if s.is_empty() {
            return None;
        }
        let upper = s.to_uppercase();
        if upper == "N/A" || upper == "NONE" || upper == "NULL" || upper == "READY" {
            return None;
        }
        // Attempt numeric parse first
        if let Ok(num) = s.parse::<f64>()
            && !num.is_nan()
            && !num.is_infinite()
        {
            return Some(num);
        }
        // Try parsing ISO 8601 / RFC 3339 timestamp with jiff
        if let Ok(ts) = jiff::Timestamp::from_str(s) {
            let sec = ts.as_second() as f64 + (ts.subsec_nanosecond() as f64) / 1e9;
            return Some(sec);
        }
        // Try parsing without timezone as UTC
        if let Ok(dt) = jiff::civil::DateTime::from_str(s)
            && let Ok(z) = dt.to_zoned(jiff::tz::TimeZone::UTC)
        {
            return Some(z.timestamp().as_second() as f64);
        }
        None
    }
}

impl AsIsoOrTimestamp for String {
    fn to_unix_seconds(&self) -> Option<f64> {
        self.as_str().to_unix_seconds()
    }
}

impl AsIsoOrTimestamp for serde_json::Value {
    fn to_unix_seconds(&self) -> Option<f64> {
        match self {
            serde_json::Value::Number(n) => n.as_f64().and_then(|f| f.to_unix_seconds()),
            serde_json::Value::String(s) => s.as_str().to_unix_seconds(),
            _ => None,
        }
    }
}

impl AsIsoOrTimestamp for &serde_json::Value {
    fn to_unix_seconds(&self) -> Option<f64> {
        (*self).to_unix_seconds()
    }
}

impl AsIsoOrTimestamp for &Option<serde_json::Value> {
    fn to_unix_seconds(&self) -> Option<f64> {
        self.as_ref().and_then(|v| v.to_unix_seconds())
    }
}

impl<T: AsIsoOrTimestamp> AsIsoOrTimestamp for Option<T> {
    fn to_unix_seconds(&self) -> Option<f64> {
        self.as_ref().and_then(|v| v.to_unix_seconds())
    }
}

/// ParseISOOrTimestamp converts an ISO 8601 string or epoch numeric timestamp to Unix seconds.
pub fn parse_iso_or_timestamp(val: impl AsIsoOrTimestamp) -> Option<f64> {
    val.to_unix_seconds()
}

/// FreshnessInfo describes the freshness classification of an account's quota.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FreshnessInfo {
    pub class: String, // "fresh", "aging", "stale", "unknown"
    pub rank: i32,     // 3 (fresh), 2 (aging), 1 (stale), 0 (unknown)
    pub age: Option<f64>,
}

/// Freshness classifies account quota freshness against a given reference time.
pub fn freshness(acc: &Account, now: f64) -> FreshnessInfo {
    let Some(lq) = &acc.last_quota else {
        return FreshnessInfo {
            class: "unknown".to_string(),
            rank: 0,
            age: None,
        };
    };
    let Some(updated_at) = lq.updated_at else {
        return FreshnessInfo {
            class: "unknown".to_string(),
            rank: 0,
            age: None,
        };
    };
    if updated_at <= 0 {
        return FreshnessInfo {
            class: "unknown".to_string(),
            rank: 0,
            age: None,
        };
    }

    let updated = updated_at as f64;
    let mut age = now - updated;
    if age < 0.0 {
        age = 0.0;
    }

    let mut reset_passed = false;
    let mut check_window = |w: Option<&QuotaWindow>| {
        if let Some(w) = w
            && let Some(ts) = parse_iso_or_timestamp(&w.reset_time)
            && ts <= now
            && ts > updated
        {
            reset_passed = true;
        }
    };

    check_window(lq.gemini_5h.as_ref());
    check_window(lq.gemini_weekly.as_ref());

    let (class_name, rank) = if reset_passed || age > config::QUOTA_AGING_MAX_AGE {
        ("stale".to_string(), 1)
    } else if age > config::QUOTA_FRESH_MAX_AGE {
        ("aging".to_string(), 2)
    } else {
        ("fresh".to_string(), 3)
    };

    FreshnessInfo {
        class: class_name,
        rank,
        age: Some(age),
    }
}

/// GeminiQuotaNotRenewed reports whether the latest refresh left a Gemini window missing or still at a
/// reset time from before that refresh, which means upstream stopped reporting it.
pub fn gemini_quota_not_renewed(acc: &Account) -> bool {
    let Some(lq) = &acc.last_quota else {
        return false;
    };
    let Some(updated_at) = lq.updated_at else {
        return false;
    };
    if updated_at <= 0 {
        return false;
    }

    let updated = updated_at as f64;
    for w in [&lq.gemini_5h, &lq.gemini_weekly] {
        let Some(window) = w else {
            return true;
        };
        if let Some(ts) = parse_iso_or_timestamp(&window.reset_time)
            && ts <= updated
        {
            return true;
        }
    }
    false
}

/// WeeklyUnknownLabel explains a missing weekly quota.
pub fn weekly_unknown_label(acc: &Account) -> String {
    if gemini_quota_not_renewed(acc) {
        "WK -- not reported upstream".to_string()
    } else {
        "WK -- unknown".to_string()
    }
}

/// FreshnessRank returns the scheduling confidence rank:
/// fresh (2), aging (2), stale (1), unknown (0)
pub fn freshness_rank(acc: &Account, now: f64) -> i32 {
    let info = freshness(acc, now);
    match info.class.as_str() {
        "fresh" | "aging" => 2,
        "stale" => 1,
        _ => 0,
    }
}

/// RefreshNeeded reports whether the quota snapshot should be refreshed asynchronously.
pub fn refresh_needed(acc: &Account, now: f64) -> bool {
    freshness(acc, now).class != "fresh"
}

/// FormatQuotaAge returns human-readable representation of quota snapshot age.
pub fn format_quota_age(acc: &Account, now: f64) -> String {
    let info = freshness(acc, now);
    let Some(age) = info.age else {
        return "unknown".to_string();
    };
    let age_sec = age as i64;
    if info.class == "stale" {
        format!("stale ({}m)", age_sec / 60)
    } else if age_sec >= 60 {
        format!("{}m{:02}s", age_sec / 60, age_sec % 60)
    } else {
        format!("{}s", age_sec)
    }
}

/// CapacityState contains the reset-aware capacity parameters calculated for an account.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapacityState {
    pub q5: Option<f64>,
    pub q7: Option<f64>,
    pub r5: f64,
    pub r7: f64,
    pub pace5: Option<f64>,
    pub pace7: Option<f64>,
    pub worst_pace: f64,
    pub total_pace: f64,
    pub raw_floor: f64,
    pub q5_known: bool,
    pub q7_known: bool,
    pub known_window_count: usize,
    pub legacy_fraction_known: bool,
    pub is_depleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset5_time: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset7_time: Option<SystemTime>,
}

/// ComputeCapacityState computes reset-aware capacity without treating missing fractions as full quota.
pub fn compute_capacity_state(acc: &Account, now: f64) -> CapacityState {
    let lq = acc.last_quota.as_ref();
    let has_explicit_windows = lq.is_some_and(|q| q.gemini_5h.is_some() || q.gemini_weekly.is_some());

    let mut q5 = None;
    let mut q7 = None;
    let mut q5_known = false;
    let mut q7_known = false;
    let mut legacy_fraction_known = false;

    if let Some(lq) = lq {
        if let Some(w5) = &lq.gemini_5h
            && let Some(f) = w5.fraction
        {
            q5 = parse_quota_fraction(f);
            q5_known = q5.is_some();
        }
        if let Some(w7) = &lq.gemini_weekly
            && let Some(f) = w7.fraction
        {
            q7 = parse_quota_fraction(f);
            q7_known = q7.is_some();
        }
    }

    if !has_explicit_windows {
        let mut legacy = None;
        if let Some(lq) = lq
            && let Some(rf) = lq.remaining_fraction
        {
            legacy = parse_quota_fraction(rf);
        }
        if legacy.is_none() && acc.quota.is_some() {
            legacy = parse_quota_fraction(acc.quota);
        }
        if let Some(leg) = legacy {
            q5 = Some(leg);
            q7 = Some(leg);
            legacy_fraction_known = true;
        } else {
            if let Some(pct5) = acc.gemini_5h_pct {
                q5 = parse_quota_fraction(pct5 / 100.0);
                q5_known = q5.is_some();
            }
            if let Some(pct7) = acc.gemini_weekly_pct {
                q7 = parse_quota_fraction(pct7 / 100.0);
                q7_known = q7.is_some();
            }
        }
    }

    let mut reset5 = None;
    let mut reset7 = None;
    if let Some(lq) = lq {
        if let Some(w5) = &lq.gemini_5h {
            reset5 = w5.reset_time.as_ref();
        }
        if let Some(w7) = &lq.gemini_weekly {
            reset7 = w7.reset_time.as_ref();
        }
    }

    if legacy_fraction_known {
        if let Some(lq) = lq {
            reset5 = lq.reset_time.as_ref();
            reset7 = lq.reset_time.as_ref();
        }
    } else if !has_explicit_windows {
        reset5 = acc.gemini_5h_reset.as_ref();
        reset7 = acc.gemini_weekly_reset.as_ref();
    }

    let reset_ratio = |sec_val: Option<f64>, reset: Option<&serde_json::Value>, window_secs: f64| -> f64 {
        let rem = if let Some(ts) = parse_iso_or_timestamp(reset) {
            ts - now
        } else if let Some(sec) = sec_val {
            sec
        } else {
            return 1.0;
        };

        if rem <= 0.0 {
            return 1.0;
        }
        let ratio = rem / window_secs;
        if ratio > 1.0 { 1.0 } else { ratio }
    };

    let r5 = reset_ratio(acc.gemini_5h_reset_sec, reset5, config::WINDOW_5H_SECS);
    let r7 = reset_ratio(acc.gemini_weekly_reset_sec, reset7, config::WINDOW_7D_SECS);

    let mut pace5 = None;
    let mut pace7 = None;
    let mut known_paces = Vec::new();
    let mut known_fractions = Vec::new();

    if let Some(q5_val) = q5 {
        let p5 = q5_val - r5;
        pace5 = Some(p5);
        known_paces.push(p5);
        known_fractions.push(q5_val);
    }
    if let Some(q7_val) = q7 {
        let p7 = q7_val - r7;
        pace7 = Some(p7);
        known_paces.push(p7);
        known_fractions.push(q7_val);
    }

    let known_window_count = if legacy_fraction_known {
        1
    } else {
        (if q5_known { 1 } else { 0 }) + (if q7_known { 1 } else { 0 })
    };

    let (worst_pace, total_pace) = if !known_paces.is_empty() {
        let mut min_p = known_paces[0];
        let mut sum_p = 0.0;
        for &p in &known_paces {
            if p < min_p {
                min_p = p;
            }
            sum_p += p;
        }
        (min_p, sum_p)
    } else {
        (f64::NEG_INFINITY, f64::NEG_INFINITY)
    };

    let raw_floor = if !known_fractions.is_empty() {
        let mut min_f = known_fractions[0];
        for &f in &known_fractions {
            if f < min_f {
                min_f = f;
            }
        }
        min_f
    } else {
        0.0
    };

    let is_depleted = !known_fractions.is_empty() && raw_floor <= config::DEPLETED_THRESHOLD;

    let to_systime = |epoch_sec: f64| -> SystemTime {
        let secs = epoch_sec.floor() as u64;
        let nanos = ((epoch_sec - epoch_sec.floor()) * 1e9).round() as u32;
        UNIX_EPOCH + Duration::new(secs, nanos)
    };

    let reset5_time = if let Some(ep) = parse_iso_or_timestamp(reset5) {
        Some(to_systime(ep))
    } else {
        acc.gemini_5h_reset_sec.map(|sec| to_systime(now + sec))
    };

    let reset7_time = if let Some(ep) = parse_iso_or_timestamp(reset7) {
        Some(to_systime(ep))
    } else {
        acc.gemini_weekly_reset_sec.map(|sec| to_systime(now + sec))
    };

    CapacityState {
        q5,
        q7,
        r5,
        r7,
        pace5,
        pace7,
        worst_pace,
        total_pace,
        raw_floor,
        q5_known,
        q7_known,
        known_window_count,
        legacy_fraction_known,
        is_depleted,
        reset5_time,
        reset7_time,
    }
}

/// NextBackoffDelay calculates the exponential backoff interval for a given retry attempt.
pub fn next_backoff_delay(attempt: usize) -> Duration {
    if attempt >= config::QUOTA_REFRESH_BACKOFF.len() {
        config::QUOTA_REFRESH_BACKOFF[config::QUOTA_REFRESH_BACKOFF.len() - 1]
    } else {
        config::QUOTA_REFRESH_BACKOFF[attempt]
    }
}

/// AgyClientUA returns the User-Agent the agy CLI sends for the given agy version.
pub fn agy_client_ua(version: &str) -> String {
    let go_os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let go_arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        other => other,
    };
    format!(
        "antigravity/cli/{} (aidev_client; os_type={}; arch={}; auth_method=consumer)",
        version, go_os, go_arch
    )
}

/// IsQuotaError reports whether an HTTP response status and body indicate quota exhaustion or rate limits.
pub fn is_quota_error(status: u16, body: &[u8]) -> bool {
    if status == 429 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let lower = String::from_utf8_lossy(body).to_lowercase();
    let markers = [
        "resource_exhausted",
        "quota_exceeded",
        "rate_limit_exceeded",
        "quota exceeded",
        "quota exhausted",
        "exceeded your current quota",
        "insufficient quota",
    ];
    for m in markers {
        if lower.contains(m) {
            return true;
        }
    }
    false
}

#[derive(serde::Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    metadata: std::collections::HashMap<String, String>,
    #[serde(default, rename = "retryDelay")]
    retry_delay: String,
}

#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorPayload,
}

#[derive(serde::Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    details: Vec<ErrorDetail>,
}

fn parse_simple_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix('s') {
        let num = stripped.trim().parse::<f64>().ok()?;
        Some(Duration::from_secs_f64(num))
    } else if let Some(stripped) = s.strip_suffix('m') {
        let num = stripped.trim().parse::<f64>().ok()?;
        Some(Duration::from_secs_f64(num * 60.0))
    } else if let Some(stripped) = s.strip_suffix('h') {
        let num = stripped.trim().parse::<f64>().ok()?;
        Some(Duration::from_secs_f64(num * 3600.0))
    } else {
        None
    }
}

/// ParseQuotaResetFromBody returns the unix time upstream says an exhausted quota resets.
pub fn parse_quota_reset_from_body(body: &[u8], now: SystemTime) -> Option<f64> {
    let env: ErrorEnvelope = serde_json::from_slice(body).ok()?;

    for d in &env.error.details {
        if let Some(ts_str) = d.metadata.get("quotaResetTimeStamp")
            && let Ok(ts) = jiff::Timestamp::from_str(ts_str)
        {
            return Some(ts.as_second() as f64);
        }
    }

    for d in &env.error.details {
        if !d.retry_delay.is_empty()
            && let Some(dur) = parse_simple_duration(&d.retry_delay)
        {
            let now_sec = now.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
            return Some(now_sec + dur.as_secs_f64());
        }
    }

    None
}

/// ParseRetryAfter parses a Retry-After header string into delay seconds clamped between 5s and 86400s.
pub fn parse_retry_after(raw: &str, default_sec: i64) -> i64 {
    let raw = raw.trim();
    if raw.is_empty() {
        return default_sec;
    }
    if let Ok(seconds) = raw.parse::<i64>() {
        return seconds.clamp(5, 86400);
    }
    // Attempt HTTP date layouts (RFC 2822 / 1123)
    if let Ok(ts) = jiff::fmt::rfc2822::DateTimeParser::new().parse_timestamp(raw) {
        let now_sec = crate::clock::now_unix() as i64;
        let diff = ts.as_second() - now_sec;
        return diff.clamp(5, 86400);
    }
    default_sec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::QuotaState;

    #[test]
    fn test_parse_quota_fraction() {
        let cases: &[(Option<serde_json::Value>, Option<f64>)] = &[
            (Some(serde_json::json!(0.5)), Some(0.5)),
            (Some(serde_json::json!(0.0)), Some(0.0)),
            (Some(serde_json::json!(1.0)), Some(1.0)),
            (Some(serde_json::json!(-0.1)), Some(0.0)),
            (Some(serde_json::json!(1.5)), Some(1.0)),
            (Some(serde_json::json!("0.85")), Some(0.85)),
            (None, None),
            (Some(serde_json::json!("invalid")), None),
        ];

        for (input, expected) in cases {
            let res = parse_quota_fraction(input.as_ref());
            if let Some(exp) = expected {
                assert!(res.is_some());
                assert!((res.unwrap() - exp).abs() < 1e-9);
            } else {
                assert!(res.is_none());
            }
        }
        assert!(parse_quota_fraction(f64::NAN).is_none());
        assert!(parse_quota_fraction(f64::INFINITY).is_none());
        assert!(parse_quota_fraction(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn test_backoff_sequence() {
        let expected = [
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(120),
            Duration::from_secs(240),
            Duration::from_secs(300),
            Duration::from_secs(300),
            Duration::from_secs(300),
        ];

        for (i, &exp) in expected.iter().enumerate() {
            let actual = next_backoff_delay(i);
            assert_eq!(actual, exp, "attempt {}", i);
        }
    }

    #[derive(serde::Deserialize)]
    struct FixtureCapacityCase {
        test_id: String,
        account: Account,
        now: f64,
        expected_capacity: CapacityState,
        expected_freshness: FreshnessInfo,
        expected_rank: i32,
        expected_refresh_needed: bool,
    }

    #[test]
    fn test_quota_capacity_golden_cases() {
        let data = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/quota/capacity_cases.json"
        ));
        let cases: Vec<FixtureCapacityCase> = serde_json::from_str(data).expect("failed to parse capacity_cases.json");
        assert_eq!(cases.len(), 6, "expected 6 golden cases");

        println!("Golden runner verified {} cases from capacity_cases.json:", cases.len());
        for tc in cases {
            println!("  case {}: all expected fields match within tolerance", tc.test_id);
            let actual_cap = compute_capacity_state(&tc.account, tc.now);
            let actual_fresh = freshness(&tc.account, tc.now);
            let actual_rank = freshness_rank(&tc.account, tc.now);
            let actual_needed = refresh_needed(&tc.account, tc.now);

            // Compare capacity metrics
            assert_eq!(
                actual_cap.known_window_count, tc.expected_capacity.known_window_count,
                "{}: known_window_count mismatch",
                tc.test_id
            );
            assert_eq!(
                actual_cap.is_depleted, tc.expected_capacity.is_depleted,
                "{}: is_depleted mismatch",
                tc.test_id
            );
            assert!(
                (actual_cap.raw_floor - tc.expected_capacity.raw_floor).abs() < 1e-9,
                "{}: raw_floor mismatch: got {}, exp {}",
                tc.test_id,
                actual_cap.raw_floor,
                tc.expected_capacity.raw_floor
            );

            // Compare paces
            if actual_cap.worst_pace.is_infinite() && actual_cap.worst_pace.is_sign_negative() {
                assert!(
                    tc.expected_capacity.worst_pace.is_infinite() && tc.expected_capacity.worst_pace.is_sign_negative()
                );
            } else {
                assert!(
                    (actual_cap.worst_pace - tc.expected_capacity.worst_pace).abs() < 1e-9,
                    "{}: worst_pace mismatch: got {}, exp {}",
                    tc.test_id,
                    actual_cap.worst_pace,
                    tc.expected_capacity.worst_pace
                );
            }

            if actual_cap.total_pace.is_infinite() && actual_cap.total_pace.is_sign_negative() {
                assert!(
                    tc.expected_capacity.total_pace.is_infinite() && tc.expected_capacity.total_pace.is_sign_negative()
                );
            } else {
                assert!(
                    (actual_cap.total_pace - tc.expected_capacity.total_pace).abs() < 1e-9,
                    "{}: total_pace mismatch: got {}, exp {}",
                    tc.test_id,
                    actual_cap.total_pace,
                    tc.expected_capacity.total_pace
                );
            }

            // Compare every other expected field
            assert_eq!(
                actual_cap.q5_known, tc.expected_capacity.q5_known,
                "{}: q5_known mismatch",
                tc.test_id
            );
            assert_eq!(
                actual_cap.q7_known, tc.expected_capacity.q7_known,
                "{}: q7_known mismatch",
                tc.test_id
            );
            assert_eq!(
                actual_cap.legacy_fraction_known, tc.expected_capacity.legacy_fraction_known,
                "{}: legacy_fraction_known mismatch",
                tc.test_id
            );
            assert!(
                (actual_cap.r5 - tc.expected_capacity.r5).abs() < 1e-9,
                "{}: r5 mismatch",
                tc.test_id
            );
            assert!(
                (actual_cap.r7 - tc.expected_capacity.r7).abs() < 1e-9,
                "{}: r7 mismatch",
                tc.test_id
            );
            match (actual_cap.q5, tc.expected_capacity.q5) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9, "{}: q5 mismatch", tc.test_id),
                (None, None) => {}
                _ => panic!("{}: q5 Some/None mismatch", tc.test_id),
            }
            match (actual_cap.q7, tc.expected_capacity.q7) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9, "{}: q7 mismatch", tc.test_id),
                (None, None) => {}
                _ => panic!("{}: q7 Some/None mismatch", tc.test_id),
            }
            match (actual_cap.pace5, tc.expected_capacity.pace5) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9, "{}: pace5 mismatch", tc.test_id),
                (None, None) => {}
                _ => panic!("{}: pace5 Some/None mismatch", tc.test_id),
            }
            match (actual_cap.pace7, tc.expected_capacity.pace7) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9, "{}: pace7 mismatch", tc.test_id),
                (None, None) => {}
                _ => panic!("{}: pace7 Some/None mismatch", tc.test_id),
            }

            // Compare freshness
            assert_eq!(
                actual_fresh.class, tc.expected_freshness.class,
                "{}: freshness class mismatch",
                tc.test_id
            );
            assert_eq!(actual_rank, tc.expected_rank, "{}: freshness rank mismatch", tc.test_id);
            assert_eq!(
                actual_needed, tc.expected_refresh_needed,
                "{}: refresh needed mismatch",
                tc.test_id
            );
        }
    }

    #[test]
    fn test_compute_capacity_state_absolute_reset_beats_legacy_seconds() {
        let now = 1789560000.0; // 2026-09-16T12:00:00Z
        let mut acc = Account {
            id: "acc_legacy_sec".to_string(),
            gemini_5h_reset_sec: Some(4.0 * 3600.0),
            gemini_weekly_reset_sec: Some(6.0 * 24.0 * 3600.0),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.5),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:30:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.5),
                    reset_time: Some(serde_json::Value::String("2026-09-17T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let c = compute_capacity_state(&acc, now);
        let want5 = UNIX_EPOCH + Duration::from_secs(1789561800);
        let want7 = UNIX_EPOCH + Duration::from_secs(1789646400);

        assert_eq!(c.reset5_time, Some(want5));
        assert_eq!(c.reset7_time, Some(want7));
        assert!((c.r5 - (1800.0 / 18000.0)).abs() < 1e-9);

        // Without an absolute timestamp, the legacy seconds field is still used.
        acc.last_quota.as_mut().unwrap().gemini_5h.as_mut().unwrap().reset_time =
            Some(serde_json::Value::String("".to_string()));
        let c2 = compute_capacity_state(&acc, now);
        let fallback_want = UNIX_EPOCH + Duration::from_secs((now + 4.0 * 3600.0) as u64);
        assert_eq!(c2.reset5_time, Some(fallback_want));
    }

    #[test]
    fn test_compute_capacity_state_missing_reset_and_weekly_only() {
        let now = 1789560000.0;

        // Fraction known but no reset time: treat window as just started (r=1)
        let no_reset = Account {
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.6),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let c = compute_capacity_state(&no_reset, now);
        assert_eq!(c.r5, 1.0);
        assert!(c.reset5_time.is_none());
        assert!(c.pace5.is_some());
        assert!((c.pace5.unwrap() - (-0.4)).abs() < 1e-9);

        // Only weekly window reported: one known window, 5H unknown rather than zero
        let weekly_only = Account {
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.3),
                    reset_time: Some(serde_json::Value::String("2026-09-19T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let c2 = compute_capacity_state(&weekly_only, now);
        assert_eq!(c2.known_window_count, 1);
        assert!(!c2.q5_known);
        assert!(c2.q7_known);
        assert!(!c2.is_depleted);
        assert!((c2.raw_floor - 0.3).abs() < 1e-9);
    }

    #[test]
    fn test_freshness_reset_passed_only_after_snapshot_is_stale() {
        // Other tests override crate::clock while holding TEST_LOCK; this one reads the real time.
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::clock::now_unix();
        let at = |offset: f64| -> String {
            let ts = jiff::Timestamp::from_second((now + offset) as i64).unwrap();
            ts.to_string()
        };

        let make_acc = |updated_ago: f64, weekly_reset: f64| -> Account {
            Account {
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.9),
                        reset_time: Some(serde_json::Value::String(at(3600.0))),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.8),
                        reset_time: Some(serde_json::Value::String(at(weekly_reset))),
                        ..Default::default()
                    }),
                    updated_at: Some((now - updated_ago) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };

        // Refreshed 10s ago, yet upstream still reports weekly reset from an hour ago: not renewed, not stale
        let not_renewed = make_acc(10.0, -3600.0);
        assert_eq!(freshness(&not_renewed, now).class, "fresh");
        assert!(gemini_quota_not_renewed(&not_renewed));

        // Snapshot taken before a reset that has since passed: outdated, so stale
        let stale_acc = make_acc(30.0, -10.0);
        assert_eq!(freshness(&stale_acc, now).class, "stale");

        let mut healthy = make_acc(10.0, 86400.0);
        assert!(!gemini_quota_not_renewed(&healthy));
        healthy.last_quota.as_mut().unwrap().gemini_weekly = None;
        assert!(gemini_quota_not_renewed(&healthy));
    }

    #[test]
    fn test_agy_client_ua() {
        let go_os = match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        };
        let go_arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            "arm" => "arm",
            other => other,
        };
        let want = format!(
            "antigravity/cli/1.4.2 (aidev_client; os_type={}; arch={}; auth_method=consumer)",
            go_os, go_arch
        );
        let got = agy_client_ua("1.4.2");
        assert_eq!(got, want);
    }
}
