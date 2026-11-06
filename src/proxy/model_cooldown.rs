use crate::storage::Account;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

pub const MODEL_COOLDOWN_MIN: Duration = Duration::from_secs(3600);
pub const MODEL_COOLDOWN_MAX: Duration = Duration::from_secs(24 * 3600);
pub const DEFAULT_MODEL_SLOW_TTFB: Duration = Duration::from_secs(30);
pub const MODEL_PAUSES_KEY: &str = "model_pauses";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedModelCooldown {
    #[serde(with = "crate::proxy::affinity::rfc3339_time")]
    pub until: SystemTime,
    #[serde(with = "duration_nanos")]
    pub backoff: Duration,
}

pub mod duration_nanos {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(d: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(d.as_nanos() as i64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let nanos = i64::deserialize(deserializer)?;
        if nanos <= 0 {
            Ok(Duration::ZERO)
        } else {
            Ok(Duration::from_nanos(nanos as u64))
        }
    }
}

#[derive(Debug, Clone)]
struct ModelCooldown {
    until: SystemTime,
    backoff: Duration,
}

struct State {
    cooldowns: HashMap<String, ModelCooldown>,
    slow_strikes: HashMap<String, bool>,
    last_model: String,
    persist: bool,
    slow_ttfb: Duration,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        cooldowns: HashMap::new(),
        slow_strikes: HashMap::new(),
        last_model: String::new(),
        persist: false,
        slow_ttfb: DEFAULT_MODEL_SLOW_TTFB,
    })
});

fn model_cooldown_key(account_id: &str, model: &str) -> String {
    format!("{account_id}\0{model}")
}

fn save_model_cooldowns_locked(state: &State) {
    if !state.persist {
        return;
    }
    let value = if !state.cooldowns.is_empty() {
        let out: HashMap<String, PersistedModelCooldown> = state
            .cooldowns
            .iter()
            .map(|(k, c)| {
                (
                    k.clone(),
                    PersistedModelCooldown {
                        until: c.until,
                        backoff: c.backoff,
                    },
                )
            })
            .collect();
        serde_json::to_string(&out).unwrap_or_default()
    } else {
        String::new()
    };
    let _ = crate::storage::save_runtime_value(MODEL_PAUSES_KEY, &value);
}

/// LoadModelCooldowns restores pauses saved by a previous daemon run and turns on saving
/// for later changes.
pub fn load_model_cooldowns() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.persist = true;
    let raw = match crate::storage::load_runtime_value(MODEL_PAUSES_KEY) {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let saved: HashMap<String, PersistedModelCooldown> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return,
    };
    for (key, p) in saved {
        state.cooldowns.insert(
            key,
            ModelCooldown {
                until: p.until,
                backoff: p.backoff,
            },
        );
    }
}

/// RecordModelCapacityError puts the account on cooldown for this model, doubling the previous backoff
/// (1h, 2h, 4h ... capped at 24h) until a success clears it. Returns the applied cooldown.
pub fn record_model_capacity_error(account_id: &str, model: &str) -> Duration {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let key = model_cooldown_key(account_id, model);
    let prev = state.cooldowns.get(&key).map(|c| c.backoff).unwrap_or(Duration::ZERO);
    let backoff = if prev.is_zero() {
        MODEL_COOLDOWN_MIN
    } else {
        (prev * 2).clamp(MODEL_COOLDOWN_MIN, MODEL_COOLDOWN_MAX)
    };
    let now = crate::clock::now();
    state.cooldowns.insert(
        key.clone(),
        ModelCooldown {
            until: now + backoff,
            backoff,
        },
    );
    state.slow_strikes.remove(&key);
    save_model_cooldowns_locked(&state);
    backoff
}

/// ClearModelCooldown forgets the account's backoff for this model after it served the model successfully.
pub fn clear_model_cooldown(account_id: &str, model: &str) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let key = model_cooldown_key(account_id, model);
    state.slow_strikes.remove(&key);
    let entry = match state.cooldowns.remove(&key) {
        Some(e) => e,
        None => return,
    };
    let now = crate::clock::now();
    let was_active = now < entry.until;
    if was_active {
        // A probe succeeding while still paused indicates capacity is back, so unpause all accounts for this model.
        let model_suffix = format!("\0{model}");
        state.cooldowns.retain(|k, _| !k.ends_with(&model_suffix));
        state.slow_strikes.retain(|k, _| !k.ends_with(&model_suffix));
    }
    save_model_cooldowns_locked(&state);
}

/// ActiveModelPauses returns accountID -> model -> pause end for pauses still running at now.
pub fn active_model_pauses(now: SystemTime) -> HashMap<String, HashMap<String, SystemTime>> {
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: HashMap<String, HashMap<String, SystemTime>> = HashMap::new();
    for (key, c) in &state.cooldowns {
        if now >= c.until {
            continue;
        }
        if let Some((id, model)) = key.split_once('\0') {
            out.entry(id.to_string())
                .or_default()
                .insert(model.to_string(), c.until);
        }
    }
    out
}

/// LastGenerationModel returns the model of the latest generation request, or "" if none yet.
pub fn last_generation_model() -> String {
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.last_model.clone()
}

/// ExcludeModelCooldowns drops accounts cooling down for model, keeping relative order, and returns how many
/// were dropped. Paused accounts are not tried, except that when all are paused the one whose pause ends soonest
/// is tried so a model is never shut off for hours by a pool-wide 503 burst.
pub fn exclude_model_cooldowns(
    accounts: &[Account],
    candidates: &[usize],
    model: &str,
    now: SystemTime,
) -> (Vec<usize>, usize) {
    if model.is_empty() {
        return (candidates.to_vec(), 0);
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.last_model = model.to_string();
    if candidates.is_empty() || state.cooldowns.is_empty() {
        return (candidates.to_vec(), 0);
    }

    let mut ready = Vec::new();
    for &idx in candidates {
        if let Some(acc) = accounts.get(idx) {
            let key = model_cooldown_key(&acc.id, model);
            let on_cooldown = match state.cooldowns.get(&key) {
                Some(c) => now < c.until,
                None => false,
            };
            if !on_cooldown {
                ready.push(idx);
            }
        }
    }

    if ready.is_empty() {
        let mut best: Option<(usize, SystemTime)> = None;
        for &idx in candidates {
            if let Some(acc) = accounts.get(idx) {
                let key = model_cooldown_key(&acc.id, model);
                if let Some(c) = state.cooldowns.get(&key) {
                    match best {
                        None => best = Some((idx, c.until)),
                        Some((_, best_until)) if c.until < best_until => {
                            best = Some((idx, c.until));
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some((best_idx, _)) = best {
            return (vec![best_idx], candidates.len() - 1);
        }
    }

    let excluded = candidates.len() - ready.len();
    (ready, excluded)
}

pub fn exclude_model_cooldowns_accounts(accounts: &[Account], model: &str, now: SystemTime) -> (Vec<Account>, usize) {
    let indices: Vec<usize> = (0..accounts.len()).collect();
    let (chosen, excluded) = exclude_model_cooldowns(accounts, &indices, model, now);
    let accs = chosen.into_iter().map(|i| accounts[i].clone()).collect();
    (accs, excluded)
}

pub fn record_model_latency(account: Option<&Account>, model: &str, ttfb: Duration) {
    let Some(acc) = account else { return };
    if model.is_empty() {
        return;
    }
    let slow_ttfb = {
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.slow_ttfb
    };
    if ttfb < slow_ttfb {
        clear_model_cooldown(&acc.id, model);
        return;
    }
    let is_first = {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let key = model_cooldown_key(&acc.id, model);
        if *state.slow_strikes.get(&key).unwrap_or(&false) {
            false
        } else {
            state.slow_strikes.insert(key, true);
            true
        }
    };
    if is_first {
        return;
    }
    record_model_capacity_error(&acc.id, model);
}

pub fn upstream_error_reason(body: &[u8]) -> String {
    let val: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    if let Some(details) = val.pointer("/error/details").and_then(|d| d.as_array()) {
        for d in details {
            if let Some(reason) = d.get("reason").and_then(|r| r.as_str())
                && !reason.is_empty()
            {
                return reason.to_string();
            }
        }
    }
    val.pointer("/error/status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn reset_model_cooldowns() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.cooldowns.clear();
    state.slow_strikes.clear();
    state.last_model.clear();
    state.persist = false;
    state.slow_ttfb = DEFAULT_MODEL_SLOW_TTFB;
}

pub fn set_persist_model_cooldown(enabled: bool) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.persist = enabled;
}

pub fn set_model_slow_ttfb_for_test(d: Duration) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.slow_ttfb = d;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_NOW_SECS: AtomicU64 = AtomicU64::new(0);

    fn test_clock_fn() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(TEST_NOW_SECS.load(Ordering::SeqCst))
    }

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        _cg: crate::clock::ClockGuard,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
            reset_model_cooldowns();
        }
    }

    fn setup_test_env(now_secs: u64) -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        reset_model_cooldowns();
        TEST_NOW_SECS.store(now_secs, Ordering::SeqCst);
        let cg = crate::clock::set_now_for_test(test_clock_fn);
        TestEnv {
            _lock: lock,
            _dir: dir,
            _cg: cg,
        }
    }

    #[test]
    fn model_cooldown_backoff_and_ordering() {
        let base_secs = 1_800_000_000u64;
        let _env = setup_test_env(base_secs);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        for want in [
            Duration::from_secs(3600),
            Duration::from_secs(7200),
            Duration::from_secs(14400),
        ] {
            let got = record_model_capacity_error("a", "m");
            assert_eq!(got, want);
        }
        for _ in 0..10 {
            record_model_capacity_error("a", "m");
        }
        let capped = record_model_capacity_error("a", "m");
        assert_eq!(capped, MODEL_COOLDOWN_MAX);

        let a = Account {
            id: "a".to_string(),
            ..Default::default()
        };
        let b = Account {
            id: "b".to_string(),
            ..Default::default()
        };
        let c = Account {
            id: "c".to_string(),
            ..Default::default()
        };
        let accounts = vec![a, b, c];

        let exclude = |model: &str, at: SystemTime| -> (String, usize) {
            let (got, n) = exclude_model_cooldowns_accounts(&accounts, model, at);
            let ids = got.iter().map(|acc| acc.id.as_str()).collect::<Vec<_>>().join(",");
            (ids, n)
        };

        assert_eq!(exclude("m", now), ("b,c".to_string(), 1));
        assert_eq!(exclude("other", now), ("a,b,c".to_string(), 0));
        assert_eq!(exclude("m", now + MODEL_COOLDOWN_MAX), ("a,b,c".to_string(), 0));

        clear_model_cooldown("a", "m");
        assert_eq!(record_model_capacity_error("a", "m"), Duration::from_secs(3600));
    }

    #[test]
    fn model_cooldown_published_state() {
        let base_secs = 1_800_000_000u64;
        let _env = setup_test_env(base_secs);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        assert_eq!(last_generation_model(), "");
        assert!(active_model_pauses(now).is_empty());

        record_model_capacity_error("a", "m");
        exclude_model_cooldowns_accounts(&[], "m2", now);

        assert_eq!(last_generation_model(), "m2");
        let pauses = active_model_pauses(now);
        let until = pauses.get("a").and_then(|m| m.get("m")).copied();
        assert_eq!(until, Some(now + Duration::from_secs(3600)));
        assert!(active_model_pauses(now + Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn model_cooldown_survives_restart() {
        let base_secs = 1_800_000_000u64;
        let _env = setup_test_env(base_secs);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        crate::storage::save_pool(&crate::storage::Pool::default()).unwrap();

        let restart = || {
            {
                let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
                state.cooldowns.clear();
            }
            load_model_cooldowns();
        };

        load_model_cooldowns();
        record_model_capacity_error("a", "m");
        record_model_capacity_error("a", "m"); // backoff 2h

        // Pool save must not wipe model pauses
        crate::storage::save_pool(&crate::storage::Pool::default()).unwrap();
        restart();

        let pauses = active_model_pauses(now);
        let until = pauses.get("a").and_then(|m| m.get("m")).copied();
        assert_eq!(until, Some(now + Duration::from_secs(7200)));
        assert_eq!(record_model_capacity_error("a", "m"), Duration::from_secs(14400));

        clear_model_cooldown("a", "m");
        restart();
        assert!(active_model_pauses(now).is_empty());
    }

    #[test]
    fn model_cooldown_all_paused_fallback_and_clear() {
        let base_secs = 1_800_000_000u64;
        let _env = setup_test_env(base_secs);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        let a = Account {
            id: "a".to_string(),
            ..Default::default()
        };
        let b = Account {
            id: "b".to_string(),
            ..Default::default()
        };
        let c = Account {
            id: "c".to_string(),
            ..Default::default()
        };
        let accounts = vec![a, b, c];

        // Pause b (ends soonest: now + 1h)
        TEST_NOW_SECS.store(base_secs, Ordering::SeqCst);
        record_model_capacity_error("b", "m");

        // Pause a (now + 10m + 1h)
        TEST_NOW_SECS.store(base_secs + 600, Ordering::SeqCst);
        record_model_capacity_error("a", "m");

        // Pause c (now + 20m + 1h)
        TEST_NOW_SECS.store(base_secs + 1200, Ordering::SeqCst);
        record_model_capacity_error("c", "m");

        // At time `now`, all are paused. ExcludeModelCooldowns returns only b with count 2.
        TEST_NOW_SECS.store(base_secs, Ordering::SeqCst);
        let (got, count) = exclude_model_cooldowns_accounts(&accounts, "m", now);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");
        assert_eq!(count, 2);

        // Clear b while its pause is active -> unpauses all accounts for model "m"
        clear_model_cooldown("b", "m");
        let (got2, count2) = exclude_model_cooldowns_accounts(&accounts, "m", now);
        assert_eq!(got2.len(), 3);
        assert_eq!(count2, 0);
        assert_eq!(got2[0].id, "a");
        assert_eq!(got2[1].id, "b");
        assert_eq!(got2[2].id, "c");

        // Pause a and b again
        TEST_NOW_SECS.store(base_secs, Ordering::SeqCst);
        record_model_capacity_error("a", "m"); // until = now + 1h
        record_model_capacity_error("b", "m");
        record_model_capacity_error("b", "m"); // until = now + 2h

        // ClearModelCooldown on an account whose pause already expired does NOT clear others
        let expired_secs = base_secs + 90 * 60;
        let expired_time = SystemTime::UNIX_EPOCH + Duration::from_secs(expired_secs);
        TEST_NOW_SECS.store(expired_secs, Ordering::SeqCst);
        clear_model_cooldown("a", "m");

        let (got, count) = exclude_model_cooldowns_accounts(&accounts, "m", expired_time);
        assert_eq!(got.len(), 2);
        assert_eq!(count, 1);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[1].id, "c");
    }
}
