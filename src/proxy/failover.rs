use crate::storage::{Account, StorageError};
use hyper::http;
use std::sync::atomic::{AtomicBool, Ordering};

static RECORD_SUCCESS_INJECTED_ERROR: AtomicBool = AtomicBool::new(false);
static POOL_TX_INJECTED_ERROR: AtomicBool = AtomicBool::new(false);

pub fn set_record_success_injected_error_for_test(injected: bool) {
    RECORD_SUCCESS_INJECTED_ERROR.store(injected, Ordering::SeqCst);
}

pub fn set_pool_tx_injected_error_for_test(injected: bool) {
    POOL_TX_INJECTED_ERROR.store(injected, Ordering::SeqCst);
}

fn run_pool_transaction<F>(mutator: F) -> Result<(), StorageError>
where
    F: FnOnce(&mut crate::storage::Pool) -> Result<(), StorageError>,
{
    if POOL_TX_INJECTED_ERROR.load(Ordering::SeqCst) {
        crate::observability::record_persistence_error();
        return Err(StorageError::Other("injected persistence failure".to_string()));
    }
    let res = crate::storage::pool_transaction(mutator);
    if res.is_err() {
        crate::observability::record_persistence_error();
    }
    res
}

/// RecordSuccess updates account counters and promotes active account under transaction.
pub fn record_success(account: &Account, is_generation: bool) -> Result<(), StorageError> {
    if RECORD_SUCCESS_INJECTED_ERROR.load(Ordering::SeqCst) {
        crate::observability::record_persistence_error();
        return Err(StorageError::Other("injected persistence failure".to_string()));
    }
    let now = crate::clock::now_unix() as i64;
    let res = crate::storage::record_success(account, is_generation, now);
    if res.is_err() {
        crate::observability::record_persistence_error();
    }
    res
}

/// RecordQuotaError parses Retry-After and applies rate limit cooldown under transaction.
/// Without Retry-After, the reset time in the RESOURCE_EXHAUSTED body is used; a weekly limit can be days away.
pub fn record_quota_error(
    account: &Account,
    headers: Option<&http::HeaderMap>,
    body: Option<&[u8]>,
) -> Result<i64, StorageError> {
    let retry_after_str = headers
        .and_then(|h| h.get("Retry-After"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut delay = crate::quota::parse_retry_after(retry_after_str, 300);
    let now = crate::clock::now_unix();

    if retry_after_str.is_empty()
        && let Some(b) = body
        && let Some(ts) = crate::quota::parse_quota_reset_from_body(b, crate::clock::now())
        && ts > now
    {
        delay = (ts - now) as i64;
    }

    let acc_id = account.id.clone();
    let acc_email = account.email.clone();

    run_pool_transaction(move |pool| {
        if let Some(stored) = pool
            .accounts
            .iter_mut()
            .find(|a| (!acc_id.is_empty() && a.id == acc_id) || (!acc_email.is_empty() && a.email == acc_email))
        {
            let cooldown = now + (delay as f64);
            stored.rate_limited_until = Some(cooldown);
            if let Some(ref mut lq) = stored.last_quota {
                lq.updated_at = Some(0);
            }
            stored.error_count += 1;
        }
        Ok(())
    })?;

    Ok(delay)
}

/// RecordValidationError marks account as validation_required with 24h cooldown under transaction.
pub fn record_validation_error(account: &Account, err_body: Option<&[u8]>) -> Result<(), StorageError> {
    let v_url = err_body.map(crate::auth::extract_validation_url).unwrap_or_default();
    let now = crate::clock::now_unix();
    let acc_id = account.id.clone();
    let acc_email = account.email.clone();

    run_pool_transaction(move |pool| {
        if let Some(stored) = pool
            .accounts
            .iter_mut()
            .find(|a| (!acc_id.is_empty() && a.id == acc_id) || (!acc_email.is_empty() && a.email == acc_email))
        {
            stored.status = "validation_required".to_string();
            if !v_url.is_empty() {
                stored.validation_url = Some(v_url);
            }
            stored.rate_limited_until = Some(now + 86400.0);
            stored.error_count += 1;
            if let Some(ref mut lq) = stored.last_quota {
                lq.remaining_fraction = Some(0.0);
            }
        }
        Ok(())
    })
}

/// RecordAuthError marks account as auth_error with 1h cooldown under transaction.
pub fn record_auth_error(account: &Account) -> Result<(), StorageError> {
    let now = crate::clock::now_unix();
    let acc_id = account.id.clone();
    let acc_email = account.email.clone();

    run_pool_transaction(move |pool| {
        if let Some(stored) = pool
            .accounts
            .iter_mut()
            .find(|a| (!acc_id.is_empty() && a.id == acc_id) || (!acc_email.is_empty() && a.email == acc_email))
        {
            stored.status = "auth_error".to_string();
            stored.rate_limited_until = Some(now + 3600.0);
            stored.error_count += 1;
            if let Some(ref mut lq) = stored.last_quota {
                lq.remaining_fraction = Some(0.0);
            }
        }
        Ok(())
    })
}

/// IsLocationError checks if a 400 or 403 error represents a temporary location restriction.
pub fn is_location_error(status: u16, body: &[u8]) -> bool {
    if status != 400 && status != 403 {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_lowercase();
    let markers = [
        "user location is not supported",
        "location is not supported",
        "location_not_supported",
        "user_location_not_supported",
        "not supported in your country",
        "not supported in your region",
        "country not supported",
        "region not supported",
    ];
    for m in markers {
        if text.contains(m) {
            return true;
        }
    }
    if text.contains("failed_precondition") && text.contains("location") {
        return true;
    }
    false
}

/// RecordSoftCooldown marks account with a soft 30s cooldown under transaction.
pub fn record_soft_cooldown(account: &Account) -> Result<(), StorageError> {
    let now = crate::clock::now_unix();
    let acc_id = account.id.clone();
    let acc_email = account.email.clone();

    run_pool_transaction(move |pool| {
        if let Some(stored) = pool
            .accounts
            .iter_mut()
            .find(|a| (!acc_id.is_empty() && a.id == acc_id) || (!acc_email.is_empty() && a.email == acc_email))
        {
            stored.rate_limited_until = Some(now + 30.0);
            stored.error_count += 1;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::clear_all_verification_confirmation_claims;
    use crate::storage::{Pool, QuotaState, QuotaWindow};

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
            set_record_success_injected_error_for_test(false);
            set_pool_tx_injected_error_for_test(false);
        }
    }

    fn setup_test_env() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        TestEnv { _lock: lock, _dir: dir }
    }

    #[test]
    fn record_quota_error_429_atomicity() {
        let _env = setup_test_env();

        let now = crate::clock::now_unix() as i64;
        let frac5h = 0.85;
        let frac_weekly = 0.92;

        let acc = Account {
            id: "acc_429_test".to_string(),
            email: "rate_limited@example.com".to_string(),
            error_count: 0,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac5h),
                    reset_time: Some(serde_json::json!(1789646400.0)),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac_weekly),
                    reset_time: Some(serde_json::json!(1790208000.0)),
                    ..Default::default()
                }),
                remaining_fraction: Some(frac5h),
                updated_at: Some(now),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = Pool {
            accounts: vec![acc.clone()],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).expect("failed to save test pool");

        let mut headers = http::HeaderMap::new();
        headers.insert("Retry-After", http::HeaderValue::from_static("120"));

        let delay = record_quota_error(&acc, Some(&headers), None).expect("record_quota_error failed");
        assert_eq!(delay, 120);

        let loaded = crate::storage::load_pool().expect("failed to load pool");
        let stored = crate::storage::find_account(&loaded, &acc).expect("stored account not found");

        // 1. Cooldown must be set
        assert!(stored.rate_limited_until.unwrap_or(0.0) > now as f64);
        // 2. ErrorCount must be incremented
        assert_eq!(stored.error_count, 1);
        // 3. Quota freshness must be invalidated (updated_at = 0)
        assert_eq!(stored.last_quota.as_ref().and_then(|q| q.updated_at), Some(0));
        // 4. Cached quota windows MUST be preserved intact
        assert_eq!(
            stored
                .last_quota
                .as_ref()
                .and_then(|q| q.gemini_5h.as_ref())
                .and_then(|w| w.fraction),
            Some(0.85)
        );
        assert_eq!(
            stored
                .last_quota
                .as_ref()
                .and_then(|q| q.gemini_weekly.as_ref())
                .and_then(|w| w.fraction),
            Some(0.92)
        );
    }

    #[test]
    fn is_location_error_matrix() {
        let cases = [
            (
                400,
                r#"{"error":{"message":"User location is not supported for the API use."}}"#,
                true,
            ),
            (400, r#"{"error":{"message":"Location is not supported"}}"#, true),
            (400, r#"{"error":{"message":"location_not_supported"}}"#, true),
            (400, r#"{"error":{"message":"Not supported in your country."}}"#, true),
            (400, r#"{"error":{"message":"Region not supported"}}"#, true),
            (
                400,
                r#"{"error":{"status":"FAILED_PRECONDITION","message":"Location is restricted"}}"#,
                true,
            ),
            (
                403,
                r#"{"error":{"message":"User location is not supported for the API use"}}"#,
                true,
            ),
            (
                400,
                r#"{"error":{"message":"Invalid argument: prompt is required"}}"#,
                false,
            ),
            (404, r#"{"error":{"message":"Not found"}}"#, false),
            (500, r#"{"error":{"message":"Internal server error"}}"#, false),
            (200, r#"{"result":"ok"}"#, false),
        ];

        for (status, body, want) in cases {
            assert_eq!(
                is_location_error(status, body.as_bytes()),
                want,
                "status={status}, body={body}"
            );
        }
    }

    #[test]
    fn record_quota_error_uses_body_reset_when_no_retry_after() {
        let _env = setup_test_env();

        let acc = Account {
            id: "acc_weekly".to_string(),
            email: "weekly@example.com".to_string(),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![acc.clone()],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).expect("failed to save test pool");

        let reset_secs = crate::clock::now_unix() + 30.0 * 3600.0;
        let ts = jiff::Timestamp::from_second(reset_secs as i64).unwrap();
        let reset_str = ts.to_string();

        let body = format!(
            r#"{{"error":{{"code":429,"message":"Individual quota reached.","status":"RESOURCE_EXHAUSTED","details":[{{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED","metadata":{{"quotaResetTimeStamp":"{reset_str}"}}}},{{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"100514.6s"}}]}}}}"#
        );

        let delay = record_quota_error(&acc, None, Some(body.as_bytes())).expect("record_quota_error failed");
        assert!(
            (29 * 3600..=31 * 3600).contains(&delay),
            "expected cooldown until quotaResetTimeStamp (~30h), got {delay}s"
        );
    }

    #[test]
    fn test_lifecycle_persistence_errors_are_observable() {
        let _env = setup_test_env();
        let acc = Account {
            id: "acc_1".to_string(),
            ..Default::default()
        };

        set_pool_tx_injected_error_for_test(true);
        set_record_success_injected_error_for_test(true);

        assert!(record_validation_error(&acc, None).is_err());
        assert!(record_auth_error(&acc).is_err());
        assert!(record_quota_error(&acc, None, None).is_err());
        assert!(record_success(&acc, true).is_err());
    }

    #[test]
    fn verification_pending_success_race_with_new_validation() {
        let _env = setup_test_env();
        clear_all_verification_confirmation_claims();

        let fraction = 0.9;
        let val_url = "https://accounts.google.com/verify?id=race_val".to_string();

        let p1 = Account {
            id: "pending_1".to_string(),
            email: "p1@example.com".to_string(),
            status: "validation_required".to_string(),
            validation_url: Some(val_url.clone()),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(fraction),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![p1],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).expect("SavePool failed");

        let dispatch_acc = Account {
            id: "pending_1".to_string(),
            status: crate::storage::STATUS_VERIFICATION_PENDING.to_string(),
            ..Default::default()
        };

        record_success(&dispatch_acc, true).expect("RecordSuccess failed");

        let stored_pool = crate::storage::load_pool().expect("LoadPool failed");
        let stored = crate::storage::find_account(&stored_pool, &dispatch_acc).expect("account not found");
        assert_eq!(stored.status, "validation_required");
        assert_eq!(stored.validation_url.as_deref(), Some(val_url.as_str()));
    }

    #[test]
    fn verification_pending_success_race_with_auth_error() {
        let _env = setup_test_env();
        clear_all_verification_confirmation_claims();

        let fraction = 0.9;
        let p1 = Account {
            id: "pending_1".to_string(),
            email: "p1@example.com".to_string(),
            status: "auth_error".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(fraction),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![p1],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).expect("SavePool failed");

        let dispatch_acc = Account {
            id: "pending_1".to_string(),
            status: crate::storage::STATUS_VERIFICATION_PENDING.to_string(),
            ..Default::default()
        };

        record_success(&dispatch_acc, true).expect("RecordSuccess failed");

        let stored_pool = crate::storage::load_pool().expect("LoadPool failed");
        let stored = crate::storage::find_account(&stored_pool, &dispatch_acc).expect("account not found");
        assert_eq!(stored.status, "auth_error");
    }
}
