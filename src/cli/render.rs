use crate::cli::width::strip_ansi;
use crate::cli::{CLR_CYAN, CLR_DIM, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

pub fn terminal_size(stdout_fd: Option<i32>) -> (u16, u16) {
    if let Some(fd) = stdout_fd
        && crate::cli::term::is_terminal(fd)
        && let Some((w, h)) = crate::cli::term::get_size(fd)
        && w > 0
        && h > 0
    {
        return (w, h);
    }
    (100, 24)
}

pub fn terminal_width(stdout_fd: Option<i32>) -> usize {
    terminal_size(stdout_fd).0 as usize
}

pub fn choose_bar_width(line_width: usize, non_bar_len: usize, max_desired: usize, min_useful: usize) -> usize {
    if line_width < non_bar_len {
        return 0;
    }
    let available = line_width - non_bar_len;
    if available >= max_desired {
        return max_desired;
    }
    if available >= min_useful {
        return available;
    }
    0
}

pub fn truncate_runes(s: &str, max_runes: usize) -> String {
    if max_runes == 0 {
        return String::new();
    }
    let runes: Vec<char> = s.chars().collect();
    if runes.len() <= max_runes {
        return s.to_string();
    }
    if max_runes == 1 {
        return "…".to_string();
    }
    let mut prefix: String = runes[..max_runes - 1].iter().collect();
    prefix.push('…');
    prefix
}

pub fn weekly_blocked(gw_frac: Option<f64>) -> bool {
    gw_frac.map(|f| f <= crate::config::DEPLETED_THRESHOLD).unwrap_or(false)
}

pub fn gated_bar(blocked: bool, bar: &str) -> String {
    if !blocked {
        return bar.to_string();
    }
    format!("{}{}{}", CLR_DIM, strip_ansi(bar), CLR_RESET)
}

pub fn get_account_state(
    acc: &crate::storage::Account,
    pool: &crate::storage::Pool,
    now_ts: i64,
    in_flight: Option<i64>,
) -> (String, &'static str) {
    let in_flight = in_flight.unwrap_or(0);
    let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(acc.last_quota.as_ref());
    let is_cooling = acc.rate_limited_until.map(|t| t > now_ts as f64).unwrap_or(false);
    let is_exhausted = (q5_frac.map(|f| f <= crate::config::DEPLETED_THRESHOLD).unwrap_or(false))
        || (gw_frac.map(|f| f <= crate::config::DEPLETED_THRESHOLD).unwrap_or(false));

    let is_active = pool.active_account_id.as_ref().map(|id| id == &acc.id).unwrap_or(false);

    let status = &acc.status;
    if status == crate::storage::STATUS_DISABLED {
        if is_active {
            return ("CLI Base (Disabled)".to_string(), CLR_DIM);
        }
        return ("Disabled".to_string(), CLR_DIM);
    }
    if status == crate::storage::STATUS_VALIDATION_REQUIRED {
        return ("Verify Required".to_string(), CLR_YELLOW);
    }
    if status == crate::storage::STATUS_VERIFICATION_PENDING {
        return ("Verification Pending".to_string(), CLR_CYAN);
    }
    if status == crate::storage::STATUS_AUTH_ERROR {
        return ("Auth Error".to_string(), CLR_RED);
    }
    if is_cooling {
        if is_active {
            return ("CLI Base (Cooldown)".to_string(), CLR_YELLOW);
        }
        return ("Cooldown".to_string(), CLR_YELLOW);
    }
    if is_exhausted {
        if is_active {
            return ("CLI Base (Exhausted)".to_string(), CLR_RED);
        }
        return ("Exhausted".to_string(), CLR_DIM);
    }
    if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
        if is_active {
            return ("CLI Base (Protected)".to_string(), CLR_YELLOW);
        }
        return ("Protected".to_string(), CLR_YELLOW);
    }
    if is_active {
        return ("CLI Base".to_string(), CLR_GREEN);
    }
    if in_flight > 0 {
        return (format!("⚡ Running ({in_flight})"), CLR_CYAN);
    }
    ("Ready".to_string(), CLR_CYAN)
}

pub fn extract_reset_times(
    acc: &crate::storage::Account,
    q: Option<&crate::storage::QuotaState>,
    now_ts: i64,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let mut legacy_reset = None;
    if let Some(q) = q
        && q.gemini_5h.is_none()
        && q.gemini_weekly.is_none()
    {
        legacy_reset = q.reset_time.clone();
    }
    let mut reset5 = None;
    let mut reset7 = None;
    if let Some(q) = q {
        if let Some(w) = &q.gemini_5h {
            reset5 = w.reset_time.clone();
        } else {
            reset5 = legacy_reset.clone();
        }
        if let Some(w) = &q.gemini_weekly {
            reset7 = w.reset_time.clone();
        } else {
            reset7 = legacy_reset;
        }
    }
    if reset5.is_none()
        && let Some(sec) = acc.gemini_5h_reset_sec
    {
        reset5 = Some(serde_json::json!(now_ts as f64 + sec));
    }
    if reset7.is_none()
        && let Some(sec) = acc.gemini_weekly_reset_sec
    {
        reset7 = Some(serde_json::json!(now_ts as f64 + sec));
    }
    (reset5, reset7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choose_bar_width() {
        let cases = [
            (80, 20, 10, 4, 10),
            (50, 15, 10, 4, 10),
            (25, 18, 10, 4, 7), // shrinks before wrapping
            (22, 18, 10, 4, 4), // minimum useful
            (21, 18, 10, 4, 0), // below minUseful -> removed
            (15, 18, 10, 4, 0), // negative available -> removed
        ];

        for (line_width, non_bar_len, max_desired, min_useful, expected) in cases {
            let actual = choose_bar_width(line_width, non_bar_len, max_desired, min_useful);
            assert_eq!(
                actual, expected,
                "choose_bar_width({line_width}, {non_bar_len}, {max_desired}, {min_useful}): expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn test_get_account_state_protected() {
        // Other tests override crate::clock while holding TEST_LOCK; this one reads the real time.
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let qw = 0.08;
        let q5 = 0.40;
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Backup".to_string(),
            email: "backup@example.com".to_string(),
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(q5),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(qw),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            accounts: vec![acc.clone()],
            ..Default::default()
        };

        let now_ts = crate::clock::now_unix() as i64;
        let (state, _) = get_account_state(&acc, &pool, now_ts, None);
        assert_eq!(state, "Protected", "got state {state:?}, want 'Protected'");
    }

    #[test]
    fn test_weekly_blocked_dims_other_bars() {
        let zero = 0.0;
        let low = 0.0035;
        let ok = 0.5;
        assert!(weekly_blocked(Some(zero)));
        assert!(weekly_blocked(Some(low)));
        assert!(!weekly_blocked(Some(ok)));
        assert!(!weekly_blocked(None));

        let full = 0.98;
        let bar = crate::quota::render_block_progress_bar(Some(full), 10);
        let got_unblocked = gated_bar(false, &bar);
        assert_eq!(got_unblocked, bar, "unblocked bar must be unchanged");

        let got_blocked = gated_bar(true, &bar);
        assert_eq!(
            got_blocked,
            format!("{}{}{}", CLR_DIM, strip_ansi(&bar), CLR_RESET),
            "blocked bar must be dimmed"
        );
    }
}
