#![allow(clippy::collapsible_if)]

use std::sync::RwLock;

use super::{AccountsError, is_digits};

pub const CLR_RESET: &str = "\x1b[0m";
pub const CLR_BOLD: &str = "\x1b[1m";
pub const CLR_DIM: &str = "\x1b[2m";
pub const CLR_GREEN: &str = "\x1b[32m";
pub const CLR_YELLOW: &str = "\x1b[33m";
pub const CLR_CYAN: &str = "\x1b[36m";
pub const CLR_RED: &str = "\x1b[31m";

pub type SmartCandidateOrderFunc =
    Box<dyn Fn(&[&crate::storage::Account], &crate::storage::Pool, f64) -> Vec<crate::storage::Account> + Send + Sync>;

static SMART_ORDER_HOOK: RwLock<Option<SmartCandidateOrderFunc>> = RwLock::new(None);

pub struct SmartOrderGuard(Option<SmartCandidateOrderFunc>);

impl Drop for SmartOrderGuard {
    fn drop(&mut self) {
        *SMART_ORDER_HOOK.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_smart_order_hook(hook: Option<SmartCandidateOrderFunc>) -> SmartOrderGuard {
    let mut w = SMART_ORDER_HOOK.write().unwrap_or_else(|e| e.into_inner());
    let old = std::mem::replace(&mut *w, hook);
    SmartOrderGuard(old)
}

// To allow executing the hook without cloning Box:
fn with_smart_order_hook<R>(f: impl FnOnce(Option<&SmartCandidateOrderFunc>) -> R) -> R {
    let r = SMART_ORDER_HOOK.read().unwrap_or_else(|e| e.into_inner());
    f(r.as_ref())
}

#[derive(Debug, Clone, PartialEq)]
pub struct MoveResult {
    pub orig_index: usize,
    pub dest_index: usize,
    pub account: crate::storage::Account,
    pub accounts: Vec<crate::storage::Account>,
    pub active_id: Option<String>,
    pub is_same: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwapResult {
    pub index1: usize,
    pub index2: usize,
    pub account1: crate::storage::Account,
    pub account2: crate::storage::Account,
    pub accounts: Vec<crate::storage::Account>,
    pub active_id: Option<String>,
    pub is_same: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortResult {
    pub criterion: String,
    pub reversed: bool,
    pub accounts: Vec<crate::storage::Account>,
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReorderResult {
    pub accounts: Vec<crate::storage::Account>,
    pub active_id: Option<String>,
}

pub fn resolve_account_index(accounts: &[crate::storage::Account], target: &str) -> Option<usize> {
    let target_str = target.trim();
    if target_str.is_empty() || accounts.is_empty() {
        return None;
    }

    // 1. 1-based integer index
    if is_digits(target_str) {
        if let Ok(idx) = target_str.parse::<usize>() {
            if idx >= 1 && idx <= accounts.len() {
                return Some(idx - 1);
            }
            return None;
        }
    }

    // 2. Exact match on 'id', 'email', or 'name'
    for (i, a) in accounts.iter().enumerate() {
        if a.id == target_str || a.email == target_str || a.name == target_str {
            return Some(i);
        }
    }

    // 3. Case-insensitive exact match on email or name
    let target_lower = target_str.to_ascii_lowercase();
    for (i, a) in accounts.iter().enumerate() {
        if a.email.to_ascii_lowercase() == target_lower || a.name.to_ascii_lowercase() == target_lower {
            return Some(i);
        }
    }

    // 4. Unique substring match on email or name
    let mut matches = Vec::new();
    for (i, a) in accounts.iter().enumerate() {
        let email_lower = a.email.to_ascii_lowercase();
        let name_lower = a.name.to_ascii_lowercase();
        if (!email_lower.is_empty() && email_lower.contains(&target_lower))
            || (!name_lower.is_empty() && name_lower.contains(&target_lower))
        {
            matches.push(i);
        }
    }
    if matches.len() == 1 {
        return Some(matches[0]);
    }

    None
}

pub fn move_account(target: &str, destination: &str) -> Result<MoveResult, AccountsError> {
    let t_str = target.trim();
    let d_str = destination.trim();
    if t_str.is_empty() || d_str.is_empty() {
        return Err(AccountsError::Message(
            "both target account and destination position must be specified".to_string(),
        ));
    }

    let mut res = None;
    crate::storage::pool_transaction(|pool| {
        if pool.accounts.is_empty() {
            return Err(crate::storage::StorageError::Other("account pool is empty".to_string()));
        }

        let orig_idx = match resolve_account_index(&pool.accounts, target) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target
                )));
            }
        };

        let n = pool.accounts.len();
        let dest_str = destination.trim().to_ascii_lowercase();
        let dest_idx = match dest_str.as_str() {
            "top" | "first" | "head" | "begin" => 0,
            "bottom" | "last" | "tail" | "end" => n - 1,
            "up" | "-1" => {
                if orig_idx == 0 {
                    0
                } else {
                    orig_idx - 1
                }
            }
            "down" | "+1" => {
                if orig_idx + 1 >= n {
                    n - 1
                } else {
                    orig_idx + 1
                }
            }
            _ => {
                if let Some(suffix) = dest_str.strip_prefix('+') {
                    if is_digits(suffix) {
                        let delta: usize = suffix.parse().unwrap_or(0);
                        std::cmp::min(orig_idx + delta, n - 1)
                    } else {
                        return Err(crate::storage::StorageError::Other(format!(
                            "invalid destination '{}'. Use a 1-based number (1..{}) or top/bottom/up/down",
                            destination, n
                        )));
                    }
                } else if let Some(suffix) = dest_str.strip_prefix('-') {
                    if is_digits(suffix) {
                        let delta: usize = suffix.parse().unwrap_or(0);
                        orig_idx.saturating_sub(delta)
                    } else {
                        return Err(crate::storage::StorageError::Other(format!(
                            "invalid destination '{}'. Use a 1-based number (1..{}) or top/bottom/up/down",
                            destination, n
                        )));
                    }
                } else if is_digits(&dest_str) {
                    let d: usize = dest_str.parse().unwrap_or(0);
                    if d < 1 || d > n {
                        return Err(crate::storage::StorageError::Other(format!(
                            "destination position {} is out of range. Must be between 1 and {}",
                            d, n
                        )));
                    }
                    d - 1
                } else if let Some(other_idx) = resolve_account_index(&pool.accounts, destination) {
                    other_idx
                } else {
                    return Err(crate::storage::StorageError::Other(format!(
                        "invalid destination '{}'. Use a 1-based number (1..{}) or top/bottom/up/down",
                        destination, n
                    )));
                }
            }
        };

        if orig_idx == dest_idx {
            res = Some(MoveResult {
                orig_index: orig_idx,
                dest_index: dest_idx,
                account: pool.accounts[orig_idx].clone(),
                accounts: pool.accounts.clone(),
                active_id: pool.active_account_id.clone(),
                is_same: true,
            });
            return Ok(());
        }

        let item = pool.accounts.remove(orig_idx);
        pool.accounts.insert(dest_idx, item.clone());

        // Keep active account valid
        let has_active = pool
            .active_account_id
            .as_ref()
            .map(|act| pool.accounts.iter().any(|a| &a.id == act))
            .unwrap_or(false);
        if !has_active && !pool.accounts.is_empty() {
            pool.active_account_id = Some(pool.accounts[0].id.clone());
        }

        res = Some(MoveResult {
            orig_index: orig_idx,
            dest_index: dest_idx,
            account: item,
            accounts: pool.accounts.clone(),
            active_id: pool.active_account_id.clone(),
            is_same: false,
        });
        Ok(())
    })?;

    res.ok_or_else(|| AccountsError::Message("move operation failed".to_string()))
}

pub fn swap_accounts(target1: &str, target2: &str) -> Result<SwapResult, AccountsError> {
    let t1 = target1.trim();
    let t2 = target2.trim();
    if t1.is_empty() || t2.is_empty() {
        return Err(AccountsError::Message(
            "two target accounts must be specified for swap".to_string(),
        ));
    }

    let mut res = None;
    crate::storage::pool_transaction(|pool| {
        if pool.accounts.is_empty() {
            return Err(crate::storage::StorageError::Other("account pool is empty".to_string()));
        }

        let idx1 = match resolve_account_index(&pool.accounts, target1) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target1
                )));
            }
        };
        let idx2 = match resolve_account_index(&pool.accounts, target2) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target2
                )));
            }
        };

        if idx1 == idx2 {
            res = Some(SwapResult {
                index1: idx1,
                index2: idx2,
                account1: pool.accounts[idx1].clone(),
                account2: pool.accounts[idx2].clone(),
                accounts: pool.accounts.clone(),
                active_id: pool.active_account_id.clone(),
                is_same: true,
            });
            return Ok(());
        }

        pool.accounts.swap(idx1, idx2);

        res = Some(SwapResult {
            index1: idx1,
            index2: idx2,
            account1: pool.accounts[idx2].clone(), // originally at idx1, now at idx2
            account2: pool.accounts[idx1].clone(), // originally at idx2, now at idx1
            accounts: pool.accounts.clone(),
            active_id: pool.active_account_id.clone(),
            is_same: false,
        });
        Ok(())
    })?;

    res.ok_or_else(|| AccountsError::Message("swap operation failed".to_string()))
}

pub fn sort_accounts(by: &str, reverse: bool, refresh: bool) -> Result<SortResult, AccountsError> {
    let mut criterion = by.trim().to_ascii_lowercase();
    if criterion.is_empty() {
        criterion = "quota".to_string();
    }

    match criterion.as_str() {
        "smart" | "quota" | "hits" | "usage" | "name" | "email" | "status" | "id" | "created" | "time" => {}
        _ => {
            return Err(AccountsError::Message(format!(
                "unknown sort criterion '{}'. Available criteria: smart, quota, hits, name, email, status, id, created",
                by
            )));
        }
    }

    if refresh && (criterion == "quota" || criterion == "smart") {
        if let Ok(pool) = crate::storage::load_pool() {
            if !pool.accounts.is_empty() {
                if let Some((prober, _)) = super::get_quota_prober() {
                    for mut acc in pool.accounts {
                        let _ = prober(&mut acc);
                    }
                }
            }
        }
    }

    let now_ts = crate::clock::now_unix() as i64;
    let is_reverse = if criterion == "quota" || criterion == "hits" || criterion == "usage" {
        !reverse
    } else {
        reverse
    };

    let mut res = None;
    crate::storage::pool_transaction(|pool| {
        if pool.accounts.is_empty() {
            return Err(crate::storage::StorageError::Other("account pool is empty".to_string()));
        }

        if criterion == "smart" {
            let mut ordered = with_smart_order_hook(|hook_opt| {
                if let Some(hook) = hook_opt {
                    let c_refs: Vec<&crate::storage::Account> = pool.accounts.iter().collect();
                    hook(&c_refs, pool, now_ts as f64)
                } else {
                    pool.accounts.clone()
                }
            });

            use std::collections::HashSet;
            let mut seen = HashSet::new();
            for a in &ordered {
                seen.insert(a.id.clone());
            }
            for a in &pool.accounts {
                if !seen.contains(&a.id) {
                    ordered.push(a.clone());
                }
            }

            if reverse {
                ordered.reverse();
            }
            pool.accounts = ordered;
        } else {
            let less = |a: &crate::storage::Account, b: &crate::storage::Account| {
                let cmp = compare_accounts(a, b, &criterion, now_ts);
                if is_reverse { cmp > 0 } else { cmp < 0 }
            };
            pool.accounts.sort_by(|a, b| {
                if less(a, b) {
                    std::cmp::Ordering::Less
                } else if less(b, a) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });
        }

        let has_active = pool
            .active_account_id
            .as_ref()
            .map(|act| pool.accounts.iter().any(|a| &a.id == act))
            .unwrap_or(false);
        if !has_active && !pool.accounts.is_empty() {
            pool.active_account_id = Some(pool.accounts[0].id.clone());
        }

        res = Some(SortResult {
            criterion: criterion.clone(),
            reversed: reverse,
            accounts: pool.accounts.clone(),
            active_id: pool.active_account_id.clone(),
        });
        Ok(())
    })?;

    res.ok_or_else(|| AccountsError::Message("sort operation failed".to_string()))
}

pub fn reorder_accounts(order_args: &[&str]) -> Result<ReorderResult, AccountsError> {
    let mut flattened = Vec::new();
    for arg in order_args {
        for part in arg.replace(',', " ").split_whitespace() {
            flattened.push(part.to_string());
        }
    }

    let pool = crate::storage::load_pool()?;
    if pool.accounts.is_empty() {
        return Err(AccountsError::Message("account pool is empty".to_string()));
    }

    let n = pool.accounts.len();
    if flattened.len() != n {
        return Err(AccountsError::Message(format!(
            "reorder sequence count ({}) does not match pool account count ({})",
            flattened.len(),
            n
        )));
    }

    let mut resolved_indices = Vec::with_capacity(n);
    let mut seen = std::collections::HashSet::new();
    for item in flattened {
        let idx = match resolve_account_index(&pool.accounts, &item) {
            Some(i) => i,
            None => {
                return Err(AccountsError::Message(format!(
                    "account '{}' not found or invalid",
                    item
                )));
            }
        };
        if seen.contains(&idx) {
            let acc_name = super::display_account_name(Some(&pool.accounts[idx]), Some(false));
            return Err(AccountsError::Message(format!(
                "duplicate account specified in reorder sequence: [{}] {}",
                idx + 1,
                acc_name
            )));
        }
        seen.insert(idx);
        resolved_indices.push(idx);
    }

    let mut res = None;
    crate::storage::pool_transaction(|p| {
        if p.accounts.len() != n {
            return Err(crate::storage::StorageError::Other(
                "account pool changed during reorder, please retry".to_string(),
            ));
        }
        let mut new_accs = Vec::with_capacity(n);
        for idx in &resolved_indices {
            new_accs.push(p.accounts[*idx].clone());
        }
        p.accounts = new_accs;

        let has_active = p
            .active_account_id
            .as_ref()
            .map(|act| p.accounts.iter().any(|a| &a.id == act))
            .unwrap_or(false);
        if !has_active && !p.accounts.is_empty() {
            p.active_account_id = Some(p.accounts[0].id.clone());
        }

        res = Some(ReorderResult {
            accounts: p.accounts.clone(),
            active_id: p.active_account_id.clone(),
        });
        Ok(())
    })?;

    res.ok_or_else(|| AccountsError::Message("reorder operation failed".to_string()))
}

pub fn print_account_order_preview(
    w: &mut dyn std::io::Write,
    accounts: &[crate::storage::Account],
    highlight_indices: &[usize],
    active_id: Option<&str>,
    show_email_opt: Option<bool>,
) -> std::io::Result<()> {
    let show_email = show_email_opt.unwrap_or_else(|| super::is_show_email_enabled(None));
    let mut is_highlighted = std::collections::HashSet::new();
    for idx in highlight_indices {
        is_highlighted.insert(*idx);
    }

    writeln!(
        w,
        "\n{}Account Pool Order ({} accounts):{}",
        CLR_BOLD,
        accounts.len(),
        CLR_RESET
    )?;

    for (i, acc) in accounts.iter().enumerate() {
        let is_active = active_id.map(|aid| aid == acc.id).unwrap_or(false);
        let active_tag = if is_active {
            format!("{}[* Active]{}", CLR_GREEN, CLR_RESET)
        } else {
            format!("{}          {}", CLR_DIM, CLR_RESET)
        };

        let disp_name = super::display_account_name(Some(acc), Some(show_email));
        let hits = acc.request_count;
        let (q5_frac, _) = display_quota_fractions(acc.last_quota.as_ref());
        let quota_str = match q5_frac {
            Some(f) => format!("  Quota: {}", render_compact_bar(Some(f), 8)),
            None => String::new(),
        };
        let hits_str = if hits > 0 {
            format!("  Hits: {}", hits)
        } else {
            String::new()
        };

        let (prefix, name_str) = if is_highlighted.contains(&i) {
            (
                format!("{}➜ [{:2}]{}", CLR_CYAN, i + 1, CLR_RESET),
                format!("{}{}{}{}", CLR_BOLD, CLR_CYAN, disp_name, CLR_RESET),
            )
        } else {
            (format!("  [{:2}]", i + 1), disp_name)
        };

        writeln!(w, " {} {} {}{}{}", prefix, active_tag, name_str, quota_str, hits_str)?;
    }
    writeln!(w)?;
    Ok(())
}

pub fn display_quota_fractions(q: Option<&crate::storage::QuotaState>) -> (Option<f64>, Option<f64>) {
    let q = match q {
        Some(quota) => quota,
        None => return (None, None),
    };
    let mut q5 = None;
    if let Some(ref g5) = q.gemini_5h {
        if let Some(frac) = g5.fraction {
            q5 = Some(frac);
        }
    } else if let Some(frac) = q.remaining_fraction {
        q5 = Some(frac);
    }

    let mut qw = None;
    if let Some(ref gw) = q.gemini_weekly {
        if let Some(frac) = gw.fraction {
            qw = Some(frac);
        }
    }
    (q5, qw)
}

pub fn render_compact_bar(fraction: Option<f64>, width: usize) -> String {
    let w = if width == 0 { 8 } else { width };
    let f = match fraction {
        Some(val) => val.clamp(0.0, 1.0),
        None => {
            return format!("[{}{}{}] N/A", CLR_DIM, "─".repeat(w), CLR_RESET);
        }
    };

    let filled = std::cmp::min((f * (w as f64)).round() as usize, w);
    let empty = w - filled;
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

#[derive(Debug, PartialEq, Eq)]
struct NaturalIDKey {
    has_num: bool,
    num: usize,
    raw: String,
}

fn get_natural_id_key(id: &str) -> NaturalIDKey {
    let mut start = None;
    let mut end = 0;
    for (i, c) in id.char_indices() {
        if c.is_ascii_digit() {
            if start.is_none() {
                start = Some(i);
            }
            end = i + c.len_utf8();
        } else if start.is_some() {
            break;
        }
    }
    if let Some(s) = start {
        if let Ok(n) = id[s..end].parse::<usize>() {
            return NaturalIDKey {
                has_num: true,
                num: n,
                raw: id.to_string(),
            };
        }
    }
    NaturalIDKey {
        has_num: false,
        num: 0,
        raw: id.to_string(),
    }
}

fn compare_natural_id(k1: &NaturalIDKey, k2: &NaturalIDKey) -> i32 {
    if k1.has_num && k2.has_num {
        if k1.num != k2.num {
            if k1.num < k2.num {
                return -1;
            }
            return 1;
        }
        return match k1.raw.cmp(&k2.raw) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    if k1.has_num && !k2.has_num {
        return -1;
    }
    if !k1.has_num && k2.has_num {
        return 1;
    }
    match k1.raw.cmp(&k2.raw) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn account_status_rank(acc: Option<&crate::storage::Account>, now_ts: i64) -> i32 {
    let a = match acc {
        Some(account) => account,
        None => return 6,
    };
    if a.status == crate::storage::STATUS_AUTH_ERROR {
        return 5;
    }
    if a.status == crate::storage::STATUS_VALIDATION_REQUIRED {
        return 4;
    }
    let (q5, qw) = display_quota_fractions(a.last_quota.as_ref());
    let is_exhausted = (q5.is_some() && q5.unwrap() <= crate::config::DEPLETED_THRESHOLD)
        || (qw.is_some() && qw.unwrap() <= crate::config::DEPLETED_THRESHOLD);
    if is_exhausted {
        return 3;
    }
    if let Some(rlu) = a.rate_limited_until {
        if rlu > now_ts as f64 {
            return 2;
        }
    }
    1
}

pub fn compare_accounts(a: &crate::storage::Account, b: &crate::storage::Account, criterion: &str, now_ts: i64) -> i32 {
    let nat_a = get_natural_id_key(&a.id);
    let nat_b = get_natural_id_key(&b.id);

    match criterion {
        "quota" => {
            let mut penalty_a: f64 = 0.0;
            if a.status == crate::storage::STATUS_VALIDATION_REQUIRED || a.status == crate::storage::STATUS_AUTH_ERROR {
                penalty_a = -1.0;
            }
            let mut penalty_b: f64 = 0.0;
            if b.status == crate::storage::STATUS_VALIDATION_REQUIRED || b.status == crate::storage::STATUS_AUTH_ERROR {
                penalty_b = -1.0;
            }
            if (penalty_a - penalty_b).abs() > 1e-9 {
                if penalty_a < penalty_b {
                    return -1;
                }
                return 1;
            }

            let (q5_a, qw_a) = display_quota_fractions(a.last_quota.as_ref());
            let (q5_b, qw_b) = display_quota_fractions(b.last_quota.as_ref());
            let val5_a = q5_a.unwrap_or(0.0);
            let val5_b = q5_b.unwrap_or(0.0);
            if (val5_a - val5_b).abs() > 1e-6 {
                if val5_a < val5_b {
                    return -1;
                }
                return 1;
            }

            let val_w_a = qw_a.unwrap_or(1.0);
            let val_w_b = qw_b.unwrap_or(1.0);
            if (val_w_a - val_w_b).abs() > 1e-6 {
                if val_w_a < val_w_b {
                    return -1;
                }
                return 1;
            }

            let hits_a = a.request_count;
            let hits_b = b.request_count;
            if hits_a != hits_b {
                if hits_a > hits_b {
                    return -1;
                }
                return 1;
            }
            compare_natural_id(&nat_a, &nat_b)
        }
        "hits" | "usage" => {
            let hits_a = a.request_count;
            let hits_b = b.request_count;
            if hits_a != hits_b {
                if hits_a < hits_b {
                    return -1;
                }
                return 1;
            }
            let (q5_a, _) = display_quota_fractions(a.last_quota.as_ref());
            let (q5_b, _) = display_quota_fractions(b.last_quota.as_ref());
            let val5_a = q5_a.unwrap_or(0.0);
            let val5_b = q5_b.unwrap_or(0.0);
            if (val5_a - val5_b).abs() > 1e-6 {
                if val5_a < val5_b {
                    return -1;
                }
                return 1;
            }
            compare_natural_id(&nat_a, &nat_b)
        }
        "name" => {
            let name_a = super::display_account_name(Some(a), Some(false))
                .trim()
                .to_ascii_lowercase();
            let name_b = super::display_account_name(Some(b), Some(false))
                .trim()
                .to_ascii_lowercase();
            match name_a.cmp(&name_b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
                std::cmp::Ordering::Equal => compare_natural_id(&nat_a, &nat_b),
            }
        }
        "email" => {
            let email_a = a.email.trim().to_ascii_lowercase();
            let email_b = b.email.trim().to_ascii_lowercase();
            match email_a.cmp(&email_b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
                std::cmp::Ordering::Equal => compare_natural_id(&nat_a, &nat_b),
            }
        }
        "status" => {
            let rank_a = account_status_rank(Some(a), now_ts);
            let rank_b = account_status_rank(Some(b), now_ts);
            if rank_a != rank_b {
                if rank_a < rank_b {
                    return -1;
                }
                return 1;
            }
            let (q5_a, _) = display_quota_fractions(a.last_quota.as_ref());
            let (q5_b, _) = display_quota_fractions(b.last_quota.as_ref());
            let val5_a = q5_a.unwrap_or(0.0);
            let val5_b = q5_b.unwrap_or(0.0);
            if (val5_a - val5_b).abs() > 1e-6 {
                if val5_a > val5_b {
                    return -1;
                }
                return 1;
            }
            let hits_a = a.request_count;
            let hits_b = b.request_count;
            if hits_a != hits_b {
                if hits_a < hits_b {
                    return -1;
                }
                return 1;
            }
            compare_natural_id(&nat_a, &nat_b)
        }
        "id" => compare_natural_id(&nat_a, &nat_b),
        "created" | "time" => {
            let ts_a = a.created_at.or(a.updated_at).unwrap_or(0);
            let ts_b = b.created_at.or(b.updated_at).unwrap_or(0);
            if ts_a != ts_b {
                if ts_a < ts_b {
                    return -1;
                }
                return 1;
            }
            compare_natural_id(&nat_a, &nat_b)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Returns the TEST_LOCK guard: callers must hold it for the whole test, since the data dir is global.
    fn create_test_order_pool() -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        crate::storage::Pool,
    ) {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main CLI".to_string(),
            email: "alpha@test.com".to_string(),
            request_count: 100,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.8),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.9),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Backup Node".to_string(),
            email: "beta@test.com".to_string(),
            request_count: 200,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.5),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.6),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc3 = crate::storage::Account {
            id: "acc_3".to_string(),
            name: "Work Node".to_string(),
            email: "gamma@test.com".to_string(),
            request_count: 50,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.95),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.95),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc4 = crate::storage::Account {
            id: "acc_10".to_string(),
            name: "Dev Node".to_string(),
            email: "delta@test.com".to_string(),
            request_count: 10,
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            ..Default::default()
        };

        let mut p = crate::storage::Pool::new_empty();
        p.accounts = vec![acc1, acc2, acc3, acc4];
        p.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&p).unwrap();
        (lock, dir, p)
    }

    #[test]
    fn test_resolve_account_index() {
        let accs = vec![
            crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Alpha Dev".to_string(),
                email: "alpha@corp.com".to_string(),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_2".to_string(),
                name: "Beta Ops".to_string(),
                email: "beta@corp.com".to_string(),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_3".to_string(),
                name: "Gamma QA".to_string(),
                email: "gamma@corp.com".to_string(),
                ..Default::default()
            },
        ];

        // 1-based index
        assert_eq!(resolve_account_index(&accs, "1"), Some(0));
        assert_eq!(resolve_account_index(&accs, "3"), Some(2));
        assert_eq!(resolve_account_index(&accs, "0"), None);
        assert_eq!(resolve_account_index(&accs, "4"), None);

        // Exact ID, name, email
        assert_eq!(resolve_account_index(&accs, "acc_2"), Some(1));
        assert_eq!(resolve_account_index(&accs, "Alpha Dev"), Some(0));
        assert_eq!(resolve_account_index(&accs, "gamma@corp.com"), Some(2));

        // Case-insensitive
        assert_eq!(resolve_account_index(&accs, "alpha dev"), Some(0));
        assert_eq!(resolve_account_index(&accs, "BETA@CORP.COM"), Some(1));

        // Unique substring
        assert_eq!(resolve_account_index(&accs, "gamma"), Some(2));

        // Ambiguous substring ("corp.com" matches all)
        assert_eq!(resolve_account_index(&accs, "corp.com"), None);

        // Non-existent
        assert_eq!(resolve_account_index(&accs, "omega"), None);
    }

    #[test]
    fn test_move_account() {
        let (_lock, _dir, _) = create_test_order_pool();

        // Move acc_1 to bottom
        let res = move_account("acc_1", "bottom").unwrap();
        assert_eq!(res.dest_index, 3);
        assert_eq!(res.accounts[3].id, "acc_1");
        assert_eq!(res.accounts[0].id, "acc_2");

        // Move acc_1 back to top
        let res = move_account("acc_1", "top").unwrap();
        assert_eq!(res.dest_index, 0);
        assert_eq!(res.accounts[0].id, "acc_1");

        // Move down by 1 (+1)
        let res = move_account("acc_1", "down").unwrap();
        assert_eq!(res.dest_index, 1);
        assert_eq!(res.accounts[1].id, "acc_1");

        // Move up by 1 (-1)
        let res = move_account("acc_1", "up").unwrap();
        assert_eq!(res.dest_index, 0);
        assert_eq!(res.accounts[0].id, "acc_1");

        // Move to absolute 1-based index "3"
        let res = move_account("acc_1", "3").unwrap();
        assert_eq!(res.dest_index, 2);
        assert_eq!(res.accounts[2].id, "acc_1");

        // Move to same position
        let res = move_account("acc_1", "3").unwrap();
        assert!(res.is_same);

        // Out of range index
        let err = move_account("acc_1", "99").unwrap_err();
        assert!(err.to_string().contains("out of range"));

        // Invalid target
        let err = move_account("non_existent", "top").unwrap_err();
        assert!(err.to_string().contains("not found"));

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }

    #[test]
    fn test_swap_accounts() {
        let (_lock, _dir, _) = create_test_order_pool();

        // Swap acc_1 and acc_3
        let res = swap_accounts("acc_1", "acc_3").unwrap();
        assert_eq!(res.accounts[0].id, "acc_3");
        assert_eq!(res.accounts[2].id, "acc_1");

        // Swap same account
        let res = swap_accounts("acc_1", "acc_1").unwrap();
        assert!(res.is_same);

        // Unknown account
        let err = swap_accounts("acc_1", "unknown").unwrap_err();
        assert!(err.to_string().contains("not found"));

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }

    #[test]
    fn test_sort_accounts() {
        let (_lock, _dir, _) = create_test_order_pool();

        // Sort by quota (default descending: acc_3 (95%), acc_1 (80%), acc_2 (50%), acc_10 (blocked: penalty -1))
        let res = sort_accounts("quota", false, false).unwrap();
        assert_eq!(res.accounts[0].id, "acc_3");
        assert_eq!(res.accounts[1].id, "acc_1");
        assert_eq!(res.accounts[2].id, "acc_2");
        assert_eq!(res.accounts[3].id, "acc_10");

        // Reverse sort by quota
        let res = sort_accounts("quota", true, false).unwrap();
        assert_eq!(res.accounts[0].id, "acc_10");
        assert_eq!(res.accounts[3].id, "acc_3");

        // Sort by hits (descending: acc_2 (200), acc_1 (100), acc_3 (50), acc_10 (10))
        let res = sort_accounts("hits", false, false).unwrap();
        assert_eq!(res.accounts[0].id, "acc_2");
        assert_eq!(res.accounts[1].id, "acc_1");
        assert_eq!(res.accounts[2].id, "acc_3");
        assert_eq!(res.accounts[3].id, "acc_10");

        // Sort by name (ascending: "Backup Node", "Dev Node", "Main CLI", "Work Node")
        let res = sort_accounts("name", false, false).unwrap();
        assert_eq!(res.accounts[0].id, "acc_2");
        assert_eq!(res.accounts[1].id, "acc_10");
        assert_eq!(res.accounts[2].id, "acc_1");
        assert_eq!(res.accounts[3].id, "acc_3");

        // Sort by id (natural ascending: acc_1, acc_2, acc_3, acc_10)
        let res = sort_accounts("id", false, false).unwrap();
        assert_eq!(res.accounts[0].id, "acc_1");
        assert_eq!(res.accounts[1].id, "acc_2");
        assert_eq!(res.accounts[2].id, "acc_3");
        assert_eq!(res.accounts[3].id, "acc_10");

        // Sort by smart
        let _guard = set_smart_order_hook(Some(Box::new(|candidates, _pool, _now| {
            vec![candidates[2].clone(), candidates[0].clone(), candidates[1].clone()]
        })));

        let res = sort_accounts("smart", false, false).unwrap();
        assert_eq!(res.accounts.len(), 4);
        assert_eq!(res.accounts[0].id, "acc_3");
        assert_eq!(res.accounts[3].id, "acc_10");

        // Reverse sort by smart
        let res = sort_accounts("smart", true, false).unwrap();
        assert_eq!(res.accounts.len(), 4);
        assert_eq!(res.accounts[0].id, "acc_10");

        drop(_guard);

        // Unknown criterion
        let err = sort_accounts("invalid_crit", false, false).unwrap_err();
        assert!(err.to_string().contains("unknown sort criterion"));

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }

    #[test]
    fn test_reorder_accounts() {
        let (_lock, _dir, _) = create_test_order_pool();

        // Permute all 4 accounts: 4, 3, 2, 1
        let res = reorder_accounts(&["4", "3", "2", "1"]).unwrap();
        assert_eq!(res.accounts[0].id, "acc_10");
        assert_eq!(res.accounts[1].id, "acc_3");
        assert_eq!(res.accounts[2].id, "acc_2");
        assert_eq!(res.accounts[3].id, "acc_1");

        // Count mismatch
        let err = reorder_accounts(&["1", "2"]).unwrap_err();
        assert!(err.to_string().contains("does not match"));

        // Duplicate account
        let err = reorder_accounts(&["1", "1", "2", "3"]).unwrap_err();
        assert!(err.to_string().contains("duplicate account"));

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }

    #[test]
    fn test_print_account_order_preview() {
        let (_lock, _dir, p) = create_test_order_pool();
        let mut buf = Vec::new();
        print_account_order_preview(&mut buf, &p.accounts, &[1], p.active_account_id.as_deref(), Some(false)).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Account Pool Order (4 accounts)"));
        assert!(out.contains("[* Active]"));
        assert!(out.contains("➜ [ 2]"));

        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }
}
