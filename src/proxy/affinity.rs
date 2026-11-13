use crate::storage::Account;
use hyper::http;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_AFFINITY_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_AFFINITY_IN_FLIGHT: i64 = 2;
pub const AFFINITY_MIN_5H_FLOOR: f64 = 0.35;
pub const AFFINITY_MIN_WEEKLY_FLOOR: f64 = 0.25;

const AFFINITY_PRUNE_AT: usize = 512;
const AFFINITY_MAX_ENTRIES: usize = 1024;
pub const SESSION_AFFINITIES_KEY: &str = "session_affinities";

pub mod rfc3339_time {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let dur = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let ts = jiff::Timestamp::from_nanosecond(dur.as_nanos() as i128).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&ts.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let ts: jiff::Timestamp = s.parse().map_err(serde::de::Error::custom)?;
        let dur = Duration::from_nanos(ts.as_nanosecond() as u64);
        Ok(UNIX_EPOCH + dur)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedAffinity {
    pub account_id: String,
    #[serde(with = "rfc3339_time")]
    pub expires_at: SystemTime,
}

#[derive(Debug, Clone)]
struct AffinityRecord {
    account_id: String,
    expires_at: SystemTime,
}

pub struct AffinityManager {
    entries: RwLock<HashMap<String, AffinityRecord>>,
    ttl: Duration,
}

static GLOBAL_AFFINITY_MGR: OnceLock<AffinityManager> = OnceLock::new();
static PERSIST_AFFINITY: AtomicBool = AtomicBool::new(false);

pub fn affinity_manager() -> &'static AffinityManager {
    GLOBAL_AFFINITY_MGR.get_or_init(|| AffinityManager::new(DEFAULT_AFFINITY_TTL))
}

pub fn set_persist_affinity(enabled: bool) {
    PERSIST_AFFINITY.store(enabled, Ordering::SeqCst);
}

pub fn load_affinities() {
    set_persist_affinity(true);
    let raw = match crate::storage::load_runtime_value(SESSION_AFFINITIES_KEY) {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let saved: HashMap<String, PersistedAffinity> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return,
    };
    let now = crate::clock::now();
    let mgr = affinity_manager();
    let mut entries = mgr.entries.write().unwrap_or_else(|e| e.into_inner());
    for (k, p) in saved {
        if p.expires_at > now {
            entries.insert(
                k,
                AffinityRecord {
                    account_id: p.account_id,
                    expires_at: p.expires_at,
                },
            );
        }
    }
}

pub fn save_affinities() {
    if !PERSIST_AFFINITY.load(Ordering::SeqCst) {
        return;
    }
    let mgr = affinity_manager();
    let now = crate::clock::now();
    let out: HashMap<String, PersistedAffinity> = {
        let entries = mgr.entries.read().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .filter(|(_, rec)| rec.expires_at > now)
            .map(|(k, rec)| {
                (
                    k.clone(),
                    PersistedAffinity {
                        account_id: rec.account_id.clone(),
                        expires_at: rec.expires_at,
                    },
                )
            })
            .collect()
    };
    let value = if !out.is_empty() {
        serde_json::to_string(&out).unwrap_or_default()
    } else {
        String::new()
    };
    let _ = crate::storage::save_runtime_value(SESSION_AFFINITIES_KEY, &value);
}

impl AffinityManager {
    pub fn new(ttl: Duration) -> Self {
        let ttl = if ttl.is_zero() { DEFAULT_AFFINITY_TTL } else { ttl };
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    pub fn get(&self, session_key: &str, now: SystemTime) -> Option<String> {
        if session_key.is_empty() {
            return None;
        }
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let rec = entries.get(session_key)?;
        if now > rec.expires_at {
            return None;
        }
        Some(rec.account_id.clone())
    }

    pub fn set(&self, session_key: &str, account_id: &str, now: SystemTime) {
        if session_key.is_empty() || account_id.is_empty() {
            return;
        }
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        if !entries.contains_key(session_key) && entries.len() >= AFFINITY_PRUNE_AT {
            entries.retain(|_, rec| now <= rec.expires_at);
            if entries.len() >= AFFINITY_MAX_ENTRIES
                && let Some(oldest_key) = entries
                    .iter()
                    .min_by_key(|(_, rec)| rec.expires_at)
                    .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest_key);
            }
        }
        entries.insert(
            session_key.to_string(),
            AffinityRecord {
                account_id: account_id.to_string(),
                expires_at: now + self.ttl,
            },
        );
    }

    pub fn prune(&self, now: SystemTime) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, rec| now <= rec.expires_at);
    }

    pub fn delete(&self, session_key: &str) {
        if session_key.is_empty() {
            return;
        }
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.remove(session_key);
    }

    pub fn clear(&self) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.clear();
    }

    pub fn apply_affinity(
        &self,
        accounts: &[Account],
        candidates: &[usize],
        session_key: &str,
        now: SystemTime,
    ) -> (Vec<usize>, bool) {
        if session_key.is_empty() || candidates.len() < 2 {
            return (candidates.to_vec(), false);
        }
        let Some(bound_id) = self.get(session_key, now) else {
            return (candidates.to_vec(), false);
        };
        if bound_id.is_empty() {
            return (candidates.to_vec(), false);
        }

        let mut target_idx = None;
        for (pos, &cand_idx) in candidates.iter().enumerate() {
            if let Some(acc) = accounts.get(cand_idx)
                && acc.id == bound_id
            {
                if affinity_healthy(acc, now)
                    && crate::observability::get_in_flight_generation(&acc.id) < MAX_AFFINITY_IN_FLIGHT
                {
                    target_idx = Some(pos);
                }
                break;
            }
        }

        let target_pos = match target_idx {
            Some(0) => return (candidates.to_vec(), true),
            Some(pos) => pos,
            None => return (candidates.to_vec(), false),
        };

        if let (Some(&bound_cand), Some(&top_cand)) = (candidates.get(target_pos), candidates.first())
            && let (Some(bound_acc), Some(top_acc)) = (accounts.get(bound_cand), accounts.get(top_cand))
            && should_yield_affinity_for_quota(bound_acc, top_acc, now)
        {
            return (candidates.to_vec(), false);
        }

        let mut reordered = Vec::with_capacity(candidates.len());
        reordered.push(candidates[target_pos]);
        reordered.extend_from_slice(&candidates[..target_pos]);
        reordered.extend_from_slice(&candidates[target_pos + 1..]);
        (reordered, true)
    }

    pub fn apply_affinity_accounts(
        &self,
        accounts: &[Account],
        session_key: &str,
        now: SystemTime,
    ) -> (Vec<Account>, bool) {
        let indices: Vec<usize> = (0..accounts.len()).collect();
        let (reordered_indices, matched) = self.apply_affinity(accounts, &indices, session_key, now);
        let reordered = reordered_indices.into_iter().map(|i| accounts[i].clone()).collect();
        (reordered, matched)
    }

    pub fn held_by_load(&self, accounts: &[Account], candidates: &[usize], session_key: &str, now: SystemTime) -> bool {
        let Some(bound_id) = self.get(session_key, now) else {
            return false;
        };
        if candidates.is_empty() {
            return false;
        }
        for &cand_idx in candidates {
            if let Some(acc) = accounts.get(cand_idx)
                && acc.id == bound_id
            {
                if !affinity_healthy(acc, now) {
                    return false;
                }
                if let Some(&top_cand) = candidates.first()
                    && let Some(top_acc) = accounts.get(top_cand)
                {
                    return !should_yield_affinity_for_quota(acc, top_acc, now);
                }
                return true;
            }
        }
        false
    }

    pub fn held_by_load_accounts(&self, accounts: &[Account], session_key: &str, now: SystemTime) -> bool {
        let indices: Vec<usize> = (0..accounts.len()).collect();
        self.held_by_load(accounts, &indices, session_key, now)
    }
}

pub fn should_yield_affinity_for_quota(bound_acc: &Account, top_acc: &Account, now: SystemTime) -> bool {
    let now_secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let bound_cap = crate::quota::compute_capacity_state(bound_acc, now_secs);
    let top_cap = crate::quota::compute_capacity_state(top_acc, now_secs);

    if bound_cap.q5_known
        && bound_cap.q5.is_some_and(|q| q < AFFINITY_MIN_5H_FLOOR)
        && top_cap.q5_known
        && top_cap.q5.is_some_and(|q| q >= 0.60)
    {
        return true;
    }

    if bound_cap.q7_known
        && bound_cap.q7.is_some_and(|q| q < AFFINITY_MIN_WEEKLY_FLOOR)
        && top_cap.q7_known
        && top_cap.q7.is_some_and(|q| q >= 0.50)
    {
        return true;
    }

    false
}

pub fn affinity_healthy(acc: &Account, now: SystemTime) -> bool {
    let now_secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    !crate::scheduler::is_restricted(acc)
        && !crate::scheduler::is_reserve_candidate(acc, now_secs)
        && !crate::quota::compute_capacity_state(acc, now_secs).is_depleted
        && !crate::scheduler::over_weekly_pace(acc, now_secs)
        && (acc.rate_limited_until.is_none() || acc.rate_limited_until.unwrap() <= now_secs)
}

pub fn extract_session_key(headers: &http::HeaderMap, uri: Option<&http::Uri>, body: &[u8]) -> String {
    let (_, key) = inspect_request(headers, uri, body);
    key
}

pub fn inspect_request(headers: &http::HeaderMap, uri: Option<&http::Uri>, body: &[u8]) -> (String, String) {
    let mut model = String::new();

    let json_val: Option<serde_json::Value> = if !body.is_empty() {
        serde_json::from_slice(body).ok()
    } else {
        None
    };

    if let Some(ref val) = json_val
        && let Some(m) = val.get("model").and_then(|v| v.as_str())
    {
        model = m.to_string();
    }

    if model.is_empty()
        && let Some(u) = uri
    {
        let p = u.path();
        if let Some(idx) = p.find("/models/") {
            let after = &p[idx + 8..];
            let end = after.find(':').unwrap_or(after.len());
            let m = &after[..end];
            let clean = m.strip_prefix("models/").unwrap_or(m);
            model = clean.to_string();
        }
    }

    // 1. Check HTTP headers
    let header_names = [
        "X-Conversation-Id",
        "X-Session-Id",
        "X-Request-Session-Id",
        "Conversation-Id",
        "Session-Id",
    ];
    for name in header_names {
        if let Some(val) = headers.get(name)
            && let Ok(s) = val.to_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return (model, trimmed.to_string());
            }
        }
    }

    // 2. Check URL query parameters
    if let Some(u) = uri
        && let Some(query) = u.query()
    {
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches!(k.as_ref(), "conversation_id" | "session_id" | "cid") {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return (model, trimmed.to_string());
                }
            }
        }
    }

    // 3. Check JSON body explicit conversation fields and contents
    if let Some(ref val) = json_val {
        if let Some(cascade_id) = val.pointer("/request/labels/cascade_id").and_then(|v| v.as_str()) {
            let trimmed = cascade_id.trim();
            if !trimmed.is_empty() {
                return (model, trimmed.to_string());
            }
        }

        let targets = ["", "/request", "/metadata"];
        let field_names = ["conversationId", "conversation_id", "sessionId", "session_id"];
        for target in targets {
            for field in field_names {
                let path = if target.is_empty() {
                    format!("/{field}")
                } else {
                    format!("{target}/{field}")
                };
                if let Some(v) = val.pointer(&path).and_then(|v| v.as_str()) {
                    let trimmed = v.trim();
                    if !trimmed.is_empty() {
                        return (model, trimmed.to_string());
                    }
                }
            }
        }

        let contents = val
            .get("contents")
            .or_else(|| val.pointer("/request/contents"))
            .and_then(|v| v.as_array());

        if let Some(contents) = contents {
            for item in contents {
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if (role == "user" || (role.is_empty() && contents.len() == 1))
                    && let Some(parts) = item.get("parts").and_then(|v| v.as_array())
                {
                    let mut texts = Vec::new();
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                texts.push(trimmed);
                            }
                        }
                    }
                    if !texts.is_empty() {
                        use sha2::{Digest, Sha256};
                        let combined = texts.join("\n");
                        let hash = Sha256::digest(combined.as_bytes());
                        let hex_str = hex::encode(&hash[..8]);
                        return (model, format!("turn1:{hex_str}"));
                    }
                }
            }
        }
    }

    (model, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Pool, QuotaState, QuotaWindow};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_NOW_SECS: AtomicU64 = AtomicU64::new(0);

    fn test_clock_fn() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(TEST_NOW_SECS.load(Ordering::SeqCst))
    }

    // Fields drop in declaration order: restore the clock before releasing the lock,
    // or the next test can read this test's fake time.
    struct TestEnv {
        _cg: crate::clock::ClockGuard,
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            set_persist_affinity(false);
            affinity_manager().clear();
            crate::proxy::model_cooldown::reset_model_cooldowns();
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_test_env(now_secs: u64) -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        affinity_manager().clear();
        TEST_NOW_SECS.store(now_secs, Ordering::SeqCst);
        let cg = crate::clock::set_now_for_test(test_clock_fn);
        TestEnv {
            _lock: lock,
            _dir: dir,
            _cg: cg,
        }
    }

    #[test]
    fn test_extract_session_key() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // 1. Header takes precedence
        let mut h1 = http::HeaderMap::new();
        h1.insert("X-Conversation-Id", http::HeaderValue::from_static("conv-header-123"));
        assert_eq!(extract_session_key(&h1, None, b""), "conv-header-123");

        // 2. Query parameter fallback
        let uri2: http::Uri = "/v1beta/models/gemini:generateContent?conversation_id=conv-query-456"
            .parse()
            .unwrap();
        assert_eq!(
            extract_session_key(&http::HeaderMap::new(), Some(&uri2), b""),
            "conv-query-456"
        );

        // 3. Body JSON conversationId field
        let body3 = br#"{"conversationId": "conv-body-789", "contents": []}"#;
        assert_eq!(
            extract_session_key(&http::HeaderMap::new(), None, body3),
            "conv-body-789"
        );

        // 4. Body JSON contents first user text hash
        let body4 = br#"{"contents": [{"role": "user", "parts": [{"text": "Hello, write a poem"}]}]}"#;
        let key4 = extract_session_key(&http::HeaderMap::new(), None, body4);
        assert!(key4.starts_with("turn1:") && key4.len() > 10);

        // 5. Subsequent turn in same conversation shares same turn1 key
        let body4_turn2 = br#"{"contents": [
            {"role": "user", "parts": [{"text": "Hello, write a poem"}]},
            {"role": "model", "parts": [{"text": "Roses are red..."}]},
            {"role": "user", "parts": [{"text": "Make it longer"}]}
        ]}"#;
        let key4_turn2 = extract_session_key(&http::HeaderMap::new(), None, body4_turn2);
        assert_eq!(key4, key4_turn2);

        // 6. Cloud Code PA /v1internal nested request.contents and multi-part first user turn
        let body5_turn1 = br#"{"model": "models/gemini-2.5-pro", "request": {"contents": [
            {"role": "user", "parts": [{"text": "system preamble"}, {"text": "Refactor scheduler.go"}]}
        ]}}"#;
        let body5_turn2 = br#"{"model": "models/gemini-2.5-pro", "request": {"contents": [
            {"role": "user", "parts": [{"text": "system preamble"}, {"text": "Refactor scheduler.go"}]},
            {"role": "model", "parts": [{"text": "Done."}]},
            {"role": "user", "parts": [{"text": "Run tests"}]}
        ]}}"#;
        let body5_other = br#"{"model": "models/gemini-2.5-pro", "request": {"contents": [
            {"role": "user", "parts": [{"text": "system preamble"}, {"text": "Different task entirely"}]}
        ]}}"#;
        let key5_turn1 = extract_session_key(&http::HeaderMap::new(), None, body5_turn1);
        let key5_turn2 = extract_session_key(&http::HeaderMap::new(), None, body5_turn2);
        let key5_other = extract_session_key(&http::HeaderMap::new(), None, body5_other);
        assert_eq!(key5_turn1, key5_turn2);
        assert_ne!(key5_turn1, key5_other);

        // 7. Nested request.session_id
        let body6 = br#"{"request": {"session_id": "nested-sess-999"}}"#;
        assert_eq!(
            extract_session_key(&http::HeaderMap::new(), None, body6),
            "nested-sess-999"
        );

        // 8. agy labels.cascade_id
        let agy = |cascade: &str| -> Vec<u8> {
            format!(
                r#"{{"model":"gemini-3.8-flash-high","requestType":"agent","request":{{"sessionId":"-3750763034362895579","labels":{{"cascade_id":"{cascade}"}}}}}}"#
            ).into_bytes()
        };
        assert_eq!(
            extract_session_key(&http::HeaderMap::new(), None, &agy("cascade-a")),
            "cascade-a"
        );
        assert_eq!(
            extract_session_key(&http::HeaderMap::new(), None, &agy("cascade-b")),
            "cascade-b"
        );
        // agy checkpoint calls without cascade
        assert_eq!(
            extract_session_key(
                &http::HeaderMap::new(),
                None,
                br#"{"request":{"sessionId":"-3750763034362895579"}}"#
            ),
            "-3750763034362895579"
        );
    }

    #[test]
    fn test_affinity_manager_lifecycle() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mgr = AffinityManager::new(Duration::from_secs(300));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        assert_eq!(mgr.get("sess-1", now), None);
        mgr.set("sess-1", "acc-1", now);
        assert_eq!(mgr.get("sess-1", now), Some("acc-1".to_string()));

        // Active at now + 299s
        assert_eq!(
            mgr.get("sess-1", now + Duration::from_secs(299)),
            Some("acc-1".to_string())
        );
        // Expired at now + 301s
        assert_eq!(mgr.get("sess-1", now + Duration::from_secs(301)), None);

        // Delete
        mgr.set("sess-2", "acc-2", now);
        assert_eq!(mgr.get("sess-2", now), Some("acc-2".to_string()));
        mgr.delete("sess-2");
        assert_eq!(mgr.get("sess-2", now), None);

        // Clear
        mgr.set("sess-3", "acc-3", now);
        mgr.clear();
        assert_eq!(mgr.get("sess-3", now), None);
    }

    #[test]
    fn test_attach_debug_headers() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut h = http::HeaderMap::new();
        let acc = Account {
            id: "acc-test-1".to_string(),
            name: "TestWorker".to_string(),
            ..Default::default()
        };

        crate::proxy::attach_debug_headers(&mut h, Some(&acc), "smart", "hit");

        assert_eq!(h.get("X-Agy-Pool-Account").unwrap(), "TestWorker");
        assert_eq!(h.get("X-Agy-Pool-Account-Id").unwrap(), "acc-test-1");
        assert_eq!(h.get("X-Agy-Pool-Strategy").unwrap(), "smart");
        assert_eq!(h.get("X-Agy-Pool-Affinity").unwrap(), "hit");
    }

    #[test]
    fn test_apply_affinity_bypassed_on_high_in_flight() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();

        let mgr = AffinityManager::new(Duration::from_secs(300));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);

        let acc_a = Account {
            id: "acc-affinity-busy".to_string(),
            ..Default::default()
        };
        let acc_b = Account {
            id: "acc-affinity-idle".to_string(),
            ..Default::default()
        };
        let candidates = vec![acc_b, acc_a];

        mgr.set("session-concurrent", "acc-affinity-busy", now);

        // 1. in-flight = 0 -> applied, accA moved to front
        let (r1, applied1) = mgr.apply_affinity_accounts(&candidates, "session-concurrent", now);
        assert!(applied1);
        assert_eq!(r1[0].id, "acc-affinity-busy");

        // 2. in-flight = 1 (< 2) -> still applied
        let mut guard1 = crate::observability::start_in_flight_generation("acc-affinity-busy");
        let (r2, applied2) = mgr.apply_affinity_accounts(&candidates, "session-concurrent", now);
        assert!(applied2);
        assert_eq!(r2[0].id, "acc-affinity-busy");

        // 3. in-flight = 2 (>= 2) -> bypassed
        let mut guard2 = crate::observability::start_in_flight_generation("acc-affinity-busy");
        let (r3, applied3) = mgr.apply_affinity_accounts(&candidates, "session-concurrent", now);
        assert!(!applied3);
        assert_eq!(r3[0].id, "acc-affinity-idle");
        assert!(mgr.held_by_load_accounts(&candidates, "session-concurrent", now));

        // 4. Once requests finish, affinity resumes
        guard2.done();
        assert!(mgr.held_by_load_accounts(&candidates, "session-concurrent", now));
        guard1.done();

        let (r4, applied4) = mgr.apply_affinity_accounts(&candidates, "session-concurrent", now);
        assert!(applied4);
        assert_eq!(r4[0].id, "acc-affinity-busy");

        crate::observability::reset_in_flight();
    }

    #[test]
    fn test_apply_affinity_bypassed_on_depleted() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mgr = AffinityManager::new(Duration::from_secs(300));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let future_reset = 1_800_000_000.0 + 3600.0;

        let acc_a = Account {
            id: "acc-healthy".to_string(),
            ..Default::default()
        };
        let acc_b = Account {
            id: "acc-depleted".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.0),
                    reset_time: Some(serde_json::json!(future_reset)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let candidates = vec![acc_a, acc_b];

        mgr.set("session-depleted", "acc-depleted", now);

        let (reordered, matched) = mgr.apply_affinity_accounts(&candidates, "session-depleted", now);
        assert!(!matched);
        assert_eq!(reordered.len(), 2);
        assert_eq!(reordered[0].id, "acc-healthy");
        assert_eq!(reordered[1].id, "acc-depleted");
        assert!(!mgr.held_by_load_accounts(&candidates, "session-depleted", now));
    }

    #[test]
    fn test_apply_affinity_bypassed_over_weekly_pace() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mgr = AffinityManager::new(Duration::from_secs(300));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset5 = 1_800_000_000.0 + 4.0 * 3600.0;
        let reset7 = 1_800_000_000.0 + 6.0 * 24.0 * 3600.0;

        let bound = |q7: f64| Account {
            id: "acc-bound".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.9),
                    reset_time: Some(serde_json::json!(reset5)),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(q7),
                    reset_time: Some(serde_json::json!(reset7)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let other = Account {
            id: "acc-other".to_string(),
            ..Default::default()
        };
        mgr.set("session-pace", "acc-bound", now);

        // 40% weekly left with 6 of 7 days to go -> over-pace -> bypassed
        let (r1, m1) = mgr.apply_affinity_accounts(&[other.clone(), bound(0.40)], "session-pace", now);
        assert!(!m1);
        assert_eq!(r1[0].id, "acc-other");

        // 90% weekly left -> on pace -> kept
        let (r2, m2) = mgr.apply_affinity_accounts(&[other, bound(0.90)], "session-pace", now);
        assert!(m2);
        assert_eq!(r2[0].id, "acc-bound");
    }

    #[test]
    fn test_affinity_manager_prunes_expired_and_caps_entries() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = AffinityManager::new(Duration::from_secs(60));
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        m.set("old", "acc-1", t0);
        m.set("live", "acc-2", t0 + Duration::from_secs(50));
        m.prune(t0 + Duration::from_secs(70));
        assert_eq!(m.get("old", t0 + Duration::from_secs(70)), None);
        assert_eq!(m.get("live", t0 + Duration::from_secs(70)), Some("acc-2".to_string()));

        // Set sweeps expired bindings once reaching AFFINITY_PRUNE_AT
        m.clear();
        for i in 0..AFFINITY_PRUNE_AT {
            m.set(&format!("s{i}"), "acc", t0);
        }
        m.set("fresh", "acc", t0 + Duration::from_secs(120));
        let count = {
            let entries = m.entries.read().unwrap();
            entries.len()
        };
        assert_eq!(count, 1);

        // Active bindings never exceed AFFINITY_MAX_ENTRIES
        m.clear();
        for i in 0..(AFFINITY_MAX_ENTRIES + 10) {
            m.set(&format!("s{i}"), "acc", t0 + Duration::from_millis(i as u64));
        }
        let count = {
            let entries = m.entries.read().unwrap();
            entries.len()
        };
        assert_eq!(count, AFFINITY_MAX_ENTRIES);
        assert_eq!(m.get("s0", t0), None);
        let last_key = format!("s{}", AFFINITY_MAX_ENTRIES + 9);
        assert_eq!(m.get(&last_key, t0), Some("acc".to_string()));
    }

    #[test]
    fn test_inspect_request_model_and_session_key() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let agy_body = br#"{"project":"p","request":{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"labels":{"cascade_id":"c-1"},"sessionId":"-1"},"model":"m-1"}"#;
        let (m1, k1) = inspect_request(&http::HeaderMap::new(), None, agy_body);
        assert_eq!(m1, "m-1");
        assert_eq!(k1, "c-1");

        let mut hdr = http::HeaderMap::new();
        hdr.insert("X-Session-Id", http::HeaderValue::from_static("h-1"));
        let (m2, k2) = inspect_request(&hdr, None, agy_body);
        assert_eq!(m2, "m-1");
        assert_eq!(k2, "h-1");

        // Mistyped session field drops body session key but keeps model
        let (m3, k3) = inspect_request(&http::HeaderMap::new(), None, br#"{"sessionId":42,"model":"m-2"}"#);
        assert_eq!(m3, "m-2");
        assert_eq!(k3, "");

        let (m4, k4) = inspect_request(
            &http::HeaderMap::new(),
            None,
            br#"{"request":{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}}"#,
        );
        assert_eq!(m4, "");
        assert_eq!(k4.len(), "turn1:".len() + 16);

        let (m5, k5) = inspect_request(&http::HeaderMap::new(), None, b"{not json");
        assert_eq!(m5, "");
        assert_eq!(k5, "");
    }

    #[test]
    fn test_affinity_manager_persistence_roundtrip() {
        let base_secs = 1_800_000_000u64;
        let _env = setup_test_env(base_secs);
        set_persist_affinity(true);
        let mgr = affinity_manager();
        mgr.clear();

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        mgr.set("conv-1\0gemini-2.5-pro", "acc-pro", now);
        mgr.set("conv-2\0gemini-2.5-flash", "acc-flash", now);
        save_affinities();

        let val = crate::storage::load_runtime_value(SESSION_AFFINITIES_KEY).expect("load failed");
        assert!(val.contains("acc-pro") && val.contains("acc-flash"));

        // Simulate restart
        mgr.clear();
        assert_eq!(mgr.get("conv-1\0gemini-2.5-pro", now), None);
        assert_eq!(mgr.get("conv-2\0gemini-2.5-flash", now), None);

        load_affinities();
        assert_eq!(mgr.get("conv-1\0gemini-2.5-pro", now), Some("acc-pro".to_string()));
        assert_eq!(mgr.get("conv-2\0gemini-2.5-flash", now), Some("acc-flash".to_string()));
    }

    #[test]
    fn test_affinity_persistence_does_not_deadlock_with_reserve() {
        let _env = setup_test_env(1_800_000_000);
        set_persist_affinity(true);

        let pool = Pool {
            accounts: vec![
                Account {
                    id: "acc_1".to_string(),
                    email: "acc1@example.com".to_string(),
                    ..Default::default()
                },
                Account {
                    id: "acc_2".to_string(),
                    email: "acc2@example.com".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).expect("SavePool failed");

        let m = affinity_manager();
        m.clear();

        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let h1 = std::thread::spawn(|| {
                let m = affinity_manager();
                for i in 0..500 {
                    m.set(&format!("s{}", i % 50), "acc_1", SystemTime::now());
                    if i % 10 == 0 {
                        save_affinities();
                    }
                }
            });

            let h2 = std::thread::spawn(|| {
                let m = affinity_manager();
                let pool = crate::storage::load_pool().unwrap();
                for _ in 0..500 {
                    let res = crate::storage::reserve_in_flight(|_| {
                        let _ = m.apply_affinity_accounts(&pool.accounts, "s1", SystemTime::now());
                        "acc_1".to_string()
                    });
                    if let Ok((_, guard)) = res {
                        drop(guard);
                    }
                }
            });

            h1.join().unwrap();
            h2.join().unwrap();
            let _ = done_tx.send(());
        });

        match done_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(()) => {}
            Err(_) => panic!("deadlock: test_affinity_persistence_does_not_deadlock_with_reserve timed out"),
        }
    }

    #[test]
    fn test_apply_affinity_yields_on_quota_depletion() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mgr = AffinityManager::new(Duration::from_secs(300));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let reset5 = 1_800_000_000.0 + 4.0 * 3600.0;
        let reset7 = 1_800_000_000.0 + 5.0 * 24.0 * 3600.0;

        let make_acc = |id: &str, q5: f64, q7: f64| Account {
            id: id.to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(q5),
                    reset_time: Some(serde_json::json!(reset5)),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(q7),
                    reset_time: Some(serde_json::json!(reset7)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Case 1: Bound account 5H quota drops below 0.35 while top is healthy (0.90)
        let bound_low_5h = make_acc("acc-low-5h", 0.25, 0.80);
        let healthy_top = make_acc("acc-healthy-top", 0.90, 0.90);
        let cands1 = vec![healthy_top.clone(), bound_low_5h];

        mgr.set("session-low-5h", "acc-low-5h", now);
        let (r1, m1) = mgr.apply_affinity_accounts(&cands1, "session-low-5h", now);
        assert!(!m1, "expected affinity to yield on low 5h quota");
        assert_eq!(r1[0].id, "acc-healthy-top");
        assert!(!mgr.held_by_load_accounts(&cands1, "session-low-5h", now));

        // Case 2: Bound account Weekly quota drops below 0.25 while top is healthy (0.85)
        let bound_low_weekly = make_acc("acc-low-weekly", 0.90, 0.15);
        let cands2 = vec![healthy_top, bound_low_weekly];

        mgr.set("session-low-weekly", "acc-low-weekly", now);
        let (r2, m2) = mgr.apply_affinity_accounts(&cands2, "session-low-weekly", now);
        assert!(!m2, "expected affinity to yield on low weekly quota");
        assert_eq!(r2[0].id, "acc-healthy-top");

        // Case 3: Both accounts are low -> affinity is preserved
        let both_low_top = make_acc("acc-low-top", 0.30, 0.80);
        let both_low_bound = make_acc("acc-low-bound", 0.25, 0.80);
        let cands3 = vec![both_low_top, both_low_bound];

        mgr.set("session-both-low", "acc-low-bound", now);
        let (r3, m3) = mgr.apply_affinity_accounts(&cands3, "session-both-low", now);
        assert!(m3);
        assert_eq!(r3[0].id, "acc-low-bound");
    }

    #[test]
    fn persisted_affinity_and_pauses_match_go_json() {
        let base_secs = 1793966400u64; // 2026-11-06T12:00:00Z
        let _env = setup_test_env(base_secs);

        // Exact Go JSON generated via temporary internal/proxy/zz_runtime_json_test.go:
        // GO_AFFINITIES_JSON: {"sess_1":{"account_id":"acc_1","expires_at":"2026-11-06T12:05:00Z"}}
        // GO_MODEL_PAUSES_JSON: {"acc_1\u0000gemini-2.5-pro":{"until":"2026-11-06T13:00:00Z","backoff":3600000000000}}
        const EXPECTED_GO_AFFINITIES_JSON: &str =
            r#"{"sess_1":{"account_id":"acc_1","expires_at":"2026-11-06T12:05:00Z"}}"#;
        const EXPECTED_GO_MODEL_PAUSES_JSON: &str =
            r#"{"acc_1\u0000gemini-2.5-pro":{"until":"2026-11-06T13:00:00Z","backoff":3600000000000}}"#;

        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(base_secs);

        // 1. Write affinities
        set_persist_affinity(true);
        let mgr = affinity_manager();
        mgr.clear();
        mgr.set("sess_1", "acc_1", t0);
        save_affinities();

        let rust_aff_json = crate::storage::load_runtime_value(SESSION_AFFINITIES_KEY).expect("load affinities");
        assert_eq!(rust_aff_json, EXPECTED_GO_AFFINITIES_JSON);
        let go_aff_val: serde_json::Value = serde_json::from_str(EXPECTED_GO_AFFINITIES_JSON).unwrap();
        let rust_aff_val: serde_json::Value = serde_json::from_str(&rust_aff_json).unwrap();
        assert_eq!(rust_aff_val, go_aff_val);

        // 2. Write model pauses
        crate::proxy::model_cooldown::reset_model_cooldowns();
        crate::proxy::model_cooldown::set_persist_model_cooldown(true);
        crate::proxy::model_cooldown::record_model_capacity_error("acc_1", "gemini-2.5-pro");

        let rust_pauses_json =
            crate::storage::load_runtime_value(crate::proxy::model_cooldown::MODEL_PAUSES_KEY).expect("load pauses");
        assert_eq!(rust_pauses_json, EXPECTED_GO_MODEL_PAUSES_JSON);
        let go_pauses_val: serde_json::Value = serde_json::from_str(EXPECTED_GO_MODEL_PAUSES_JSON).unwrap();
        let rust_pauses_val: serde_json::Value = serde_json::from_str(&rust_pauses_json).unwrap();
        assert_eq!(rust_pauses_val, go_pauses_val);
    }
}
