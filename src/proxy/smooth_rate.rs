use crate::storage::Account;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, SystemTime};

static SMOOTH_RATE_STATE: LazyLock<RwLock<HashMap<String, SystemTime>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// ResetSmoothRate clears the dispatch history (used in tests).
pub fn reset_smooth_rate() {
    let mut s = SMOOTH_RATE_STATE.write().unwrap_or_else(|e| e.into_inner());
    s.clear();
}

/// ModelSmoothInterval returns the minimum recommended interval between requests for a given model
/// to stay well within upstream provider rate-limit bounds (RPM).
pub fn model_smooth_interval(model: &str) -> Duration {
    if model.to_lowercase().contains("pro") {
        Duration::from_secs(6)
    } else {
        Duration::from_secs(1)
    }
}

/// RecordModelDispatch records that a request for model was dispatched to accountID.
pub fn record_model_dispatch(account_id: &str, model: &str, now: SystemTime) {
    if account_id.is_empty() || model.is_empty() {
        return;
    }
    let mut s = SMOOTH_RATE_STATE.write().unwrap_or_else(|e| e.into_inner());
    let key = format!("{account_id}\0{model}");
    s.insert(key, now);
}

/// SmoothModelRate reorders candidates to prioritize accounts that have rested past the model's
/// smooth interval, preventing burst requests from hammering a single account and triggering upstream 429s.
/// It only reorders within the top tier so rate-limited, depleted or reserve accounts are never promoted.
pub fn smooth_model_rate(accounts: &[Account], candidates: &[usize], model: &str, now: SystemTime) -> Vec<usize> {
    if candidates.len() < 2 || model.is_empty() {
        return candidates.to_vec();
    }

    let cand_accounts: Vec<Account> = candidates.iter().filter_map(|&i| accounts.get(i).cloned()).collect();
    if cand_accounts.len() < 2 {
        return candidates.to_vec();
    }

    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let n = crate::scheduler::top_tier_count(&cand_accounts, now_secs);
    if n < 2 {
        return candidates.to_vec();
    }

    let interval = model_smooth_interval(model);
    let s = SMOOTH_RATE_STATE.read().unwrap_or_else(|e| e.into_inner());

    let mut ready = Vec::with_capacity(n);
    let mut cooling = Vec::with_capacity(n);

    for &idx in &candidates[..n] {
        if let Some(acc) = accounts.get(idx) {
            let key = format!("{}\0{}", acc.id, model);
            let is_ready = match s.get(&key) {
                None => true,
                Some(&last) => now >= last && now.duration_since(last).unwrap_or_default() >= interval,
            };
            if is_ready {
                ready.push(idx);
            } else {
                cooling.push(idx);
            }
        }
    }

    let mut out = Vec::with_capacity(candidates.len());
    out.extend(ready);
    out.extend(cooling);
    out.extend_from_slice(&candidates[n..]);
    out
}

/// Helper that works directly on Account slices.
pub fn smooth_model_rate_accounts(accounts: &[Account], model: &str, now: SystemTime) -> Vec<Account> {
    let indices: Vec<usize> = (0..accounts.len()).collect();
    let smoothed = smooth_model_rate(accounts, &indices, model, now);
    smoothed.into_iter().map(|i| accounts[i].clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::QuotaState;

    #[test]
    fn model_smooth_interval_tests() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(model_smooth_interval("gemini-2.5-pro"), Duration::from_secs(6));
        assert_eq!(model_smooth_interval("models/gemini-pro"), Duration::from_secs(6));
        assert_eq!(model_smooth_interval("gemini-2.5-flash"), Duration::from_secs(1));
    }

    #[test]
    fn smooth_model_rate_reorders_cooling_accounts() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_smooth_rate();

        let frac = 0.9;
        let acc1 = Account {
            id: "acc-1".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc-2".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let model = "gemini-2.5-pro";

        let t0 = SystemTime::now();
        // acc1 just served this model 1 second ago (needs 6s)
        record_model_dispatch(&acc1.id, model, t0 - Duration::from_secs(1));

        let accounts = vec![acc1, acc2];
        let cands = vec![0, 1];

        // At t0, acc1 is cooling, acc2 is ready
        let reordered = smooth_model_rate(&accounts, &cands, model, t0);
        assert_eq!(reordered.len(), 2);
        assert_eq!(accounts[reordered[0]].id, "acc-2");
        assert_eq!(accounts[reordered[1]].id, "acc-1");

        // At t0 + 7s, acc1 is rested (> 6s)
        let rested = smooth_model_rate(&accounts, &cands, model, t0 + Duration::from_secs(7));
        assert_eq!(accounts[rested[0]].id, "acc-1");

        reset_smooth_rate();
    }

    #[test]
    fn smooth_model_rate_all_cooling_preserves_order() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_smooth_rate();

        let frac = 0.9;
        let acc1 = Account {
            id: "acc-1".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc-2".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let model = "gemini-2.5-pro";

        let t0 = SystemTime::now();
        record_model_dispatch(&acc1.id, model, t0 - Duration::from_secs(1));
        record_model_dispatch(&acc2.id, model, t0 - Duration::from_secs(2));

        let accounts = vec![acc1, acc2];
        let cands = vec![0, 1];
        let reordered = smooth_model_rate(&accounts, &cands, model, t0);
        assert_eq!(accounts[reordered[0]].id, "acc-1");
        assert_eq!(accounts[reordered[1]].id, "acc-2");

        reset_smooth_rate();
    }

    #[test]
    fn smooth_model_rate_keeps_healthy_ahead_of_rate_limited() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now();
        let now_secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let future_rl = now_secs + 3600.0;
        let frac = 0.9;

        let healthy = Account {
            id: "healthy".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let limited = Account {
            id: "limited".to_string(),
            rate_limited_until: Some(future_rl),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = crate::storage::Pool {
            accounts: vec![healthy, limited],
            strategy: crate::config::STRATEGY_SMART.to_string(),
            ..Default::default()
        };

        let cands = crate::scheduler::order_candidates(&pool.accounts, crate::config::STRATEGY_SMART, &pool, now_secs);
        reset_smooth_rate();

        record_model_dispatch("healthy", "gemini-3.8-flash-high", now - Duration::from_millis(500));

        let smoothed = smooth_model_rate(&pool.accounts, &cands, "gemini-3.8-flash-high", now);
        assert!(smoothed.len() >= 2);
        assert_eq!(pool.accounts[smoothed[0]].id, "healthy");

        reset_smooth_rate();
    }
}
