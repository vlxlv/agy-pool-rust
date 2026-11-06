use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const DEFAULT_CLIENT_ID_HEX: &str = "6b6a6d6b6a6a6c6a6c6a6f636b772e3732292933346832686b3639283f68696f2c2e35363530326e3d6e6a693f2a743b2a2a29743d35353d363f2f293f283935342e3f342e74393537";
pub const DEFAULT_CLIENT_SECRET_HEX: &str = "1d1519090a0277116f621c0d086e626c163e16106b371618622902196e206c2b1e1b3c";

pub fn decode_cred(h: &str, k: u8) -> String {
    let b = match hex::decode(h) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let out: Vec<u8> = b.iter().map(|&v| v ^ k).collect();
    String::from_utf8(out).unwrap_or_default()
}

pub static DEFAULT_CLIENT_ID: LazyLock<String> = LazyLock::new(|| decode_cred(DEFAULT_CLIENT_ID_HEX, 0x5A));

pub static DEFAULT_CLIENT_SECRET: LazyLock<String> = LazyLock::new(|| decode_cred(DEFAULT_CLIENT_SECRET_HEX, 0x5A));

pub const OAUTH_SCOPES: &str = concat!(
    "openid email profile ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile ",
    "https://www.googleapis.com/auth/cloud-platform ",
    "https://www.googleapis.com/auth/cclog ",
    "https://www.googleapis.com/auth/experimentsandconfigs ",
    "https://www.googleapis.com/auth/aicode"
);

pub fn client_id() -> String {
    if let Ok(val) = std::env::var("AGY_CLIENT_ID")
        && !val.is_empty()
    {
        return val;
    }
    DEFAULT_CLIENT_ID.clone()
}

pub fn client_secret() -> String {
    if let Ok(val) = std::env::var("AGY_CLIENT_SECRET")
        && !val.is_empty()
    {
        return val;
    }
    DEFAULT_CLIENT_SECRET.clone()
}

pub fn get_client_id() -> String {
    client_id()
}

pub fn get_client_secret() -> String {
    client_secret()
}

pub const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

static TOKEN_URL: RwLock<Option<String>> = RwLock::new(None);

pub fn token_url() -> String {
    TOKEN_URL
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string())
}

pub struct UrlGuard(Option<String>);

pub fn set_token_url_for_test(url: &str) -> UrlGuard {
    let mut w = TOKEN_URL.write().unwrap_or_else(|e| e.into_inner());
    let old = w.replace(url.to_string());
    UrlGuard(old)
}

impl Drop for UrlGuard {
    fn drop(&mut self) {
        *TOKEN_URL.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn decode_jwt_payload(jwt_str: &str) -> std::collections::HashMap<String, serde_json::Value> {
    let parts: Vec<&str> = jwt_str.split('.').collect();
    if parts.len() < 2 {
        return std::collections::HashMap::new();
    }
    let mut payload_b64 = parts[1].to_string();
    let rem = payload_b64.len() % 4;
    if rem != 0 {
        payload_b64.push_str(&"=".repeat(4 - rem));
    }
    use base64::Engine;
    let data = match base64::engine::general_purpose::URL_SAFE.decode(payload_b64.as_bytes()) {
        Ok(d) => d,
        Err(_) => return std::collections::HashMap::new(),
    };
    serde_json::from_slice::<std::collections::HashMap<String, serde_json::Value>>(&data).unwrap_or_default()
}

pub fn is_validation_error(status: u16, body: &[u8]) -> bool {
    if status != 403 {
        return false;
    }
    let lower = String::from_utf8_lossy(body).to_lowercase();
    let markers = [
        "validation_required",
        "verify your account",
        "verify your account to continue",
    ];
    for m in markers {
        if lower.contains(m) {
            return true;
        }
    }
    false
}

#[derive(serde::Deserialize)]
struct ErrorDetailLink {
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: String,
}

#[derive(serde::Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    metadata: std::collections::HashMap<String, String>,
    #[serde(default)]
    links: Vec<ErrorDetailLink>,
}

#[derive(serde::Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    details: Vec<ErrorDetail>,
}

#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorPayload,
}

pub fn extract_validation_url(body: &[u8]) -> String {
    let data: ErrorEnvelope = match serde_json::from_slice(body) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    for item in &data.error.details {
        if let Some(u) = item.metadata.get("validation_url")
            && !u.is_empty()
        {
            return u.clone();
        }
        for link in &item.links {
            let desc = link.description.to_lowercase();
            let u = link.url.to_lowercase();
            if desc.contains("verify") || u.contains("verify") {
                return link.url.clone();
            }
        }
    }
    String::new()
}

pub fn is_auth_error(status: u16, body: &[u8]) -> bool {
    if status == 401 {
        return true;
    }
    if status == 403 {
        let lower = String::from_utf8_lossy(body).to_lowercase();
        let markers = [
            "unauthenticated",
            "invalid_grant",
            "access_token_expired",
            "account is disabled",
            "user disabled",
        ];
        for m in markers {
            if lower.contains(m) {
                return true;
            }
        }
    }
    false
}

pub fn persist_account_fields(
    account: &crate::storage::Account,
    update_tokens: bool,
    update_status: bool,
    update_quota: bool,
) -> Result<(), crate::storage::StorageError> {
    crate::storage::pool_transaction(|pool| {
        let stored = pool
            .accounts
            .iter_mut()
            .find(|a| {
                (!account.id.is_empty() && a.id == account.id)
                    || (!account.email.is_empty() && a.email == account.email)
            })
            .ok_or_else(|| {
                crate::storage::StorageError::Other(format!("account '{}' does not exist in pool", account.id))
            })?;
        if update_tokens {
            stored.access_token = account.access_token.clone();
            if !account.refresh_token.is_empty() {
                stored.refresh_token = account.refresh_token.clone();
            }
            stored.token_expiry = account.token_expiry;
            stored.updated_at = account.updated_at;
            if !account.id_token.is_empty() {
                stored.id_token = account.id_token.clone();
            }
        }
        if update_status {
            stored.status = account.status.clone();
            stored.validation_url = account.validation_url.clone();
            stored.rate_limited_until = account.rate_limited_until;
        }
        if update_quota {
            stored.last_quota = account.last_quota.clone();
        }
        Ok(())
    })
}

fn safe_provider_reason(body: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Inner {
        #[serde(default)]
        code: String,
        #[serde(default)]
        error: String,
    }
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: Inner,
    }
    let env: Envelope = match serde_json::from_slice(body) {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let mut code = env.error.code;
    if code.is_empty() {
        code = env.error.error;
    }
    if code.is_empty() {
        return String::new();
    }
    match code.as_str() {
        "invalid_grant" | "invalid_client" | "invalid_request" | "unauthorized_client" | "access_denied" => {
            format!(": {}", code)
        }
        _ => String::new(),
    }
}

pub fn token_expiring_soon<'a>(account: impl Into<Option<&'a crate::storage::Account>>, d: Duration) -> bool {
    let account = match account.into() {
        Some(a) => a,
        None => return true,
    };
    if account.access_token.is_empty() || account.token_expiry.is_none() {
        return true;
    }
    let now_unix = crate::clock::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|dur| dur.as_secs() as f64)
        .unwrap_or(0.0);
    let expiry = account.token_expiry.unwrap();
    (expiry - now_unix) < d.as_secs_f64()
}

static LOCK_MUTEXES: LazyLock<Mutex<std::collections::HashMap<std::path::PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn get_lock_mutex(path: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = LOCK_MUTEXES.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[derive(Debug)]
pub enum AuthError {
    Message(String),
    Storage(crate::storage::StorageError),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Message(m) => write!(f, "{}", m),
            AuthError::Storage(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<String> for AuthError {
    fn from(s: String) -> Self {
        AuthError::Message(s)
    }
}

impl From<&str> for AuthError {
    fn from(s: &str) -> Self {
        AuthError::Message(s.to_string())
    }
}

impl From<crate::storage::StorageError> for AuthError {
    fn from(e: crate::storage::StorageError) -> Self {
        AuthError::Storage(e)
    }
}

pub async fn refresh_token_within(account: &mut crate::storage::Account, d: Duration) -> Result<String, AuthError> {
    refresh_token_impl(account, false, d.as_secs_f64()).await
}

pub async fn refresh_token(account: &mut crate::storage::Account) -> Result<String, AuthError> {
    refresh_token_impl(account, false, 120.0).await
}

pub async fn refresh_token_force(account: &mut crate::storage::Account, force: bool) -> Result<String, AuthError> {
    refresh_token_impl(account, force, 120.0).await
}

async fn refresh_token_impl(
    account: &mut crate::storage::Account,
    force: bool,
    skew: f64,
) -> Result<String, AuthError> {
    let now = crate::clock::now();
    let now_unix = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0);

    // If valid for at least skew seconds and not forced, return existing access_token
    if !force
        && !account.access_token.is_empty()
        && account.token_expiry.is_some()
        && (account.token_expiry.unwrap() - now_unix > skew)
    {
        return Ok(account.access_token.clone());
    }

    if account.refresh_token.is_empty() {
        return Err(AuthError::Message("Missing refresh_token".to_string()));
    }

    let lock_key = if !account.id.is_empty() {
        &account.id
    } else if !account.email.is_empty() {
        &account.email
    } else {
        &account.refresh_token
    };

    let mut hasher = Sha256::new();
    hasher.update(lock_key.as_bytes());
    let hash = hasher.finalize();
    let lock_name = &hex::encode(hash)[..24];
    let lock_path = crate::config::locks_dir().join(format!("refresh-{}.lock", lock_name));

    // Acquire in-process async mutex first
    let mu = get_lock_mutex(&lock_path);
    let _mem_guard = mu.lock().await;

    // Acquire cross-process file lock
    let dir = lock_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if !dir.as_os_str().is_empty() {
        let _ = std::fs::create_dir_all(dir);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| AuthError::Message(format!("failed to open lock file: {}", e)))?;

    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));

    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open file descriptor for the lock file.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        return Err(AuthError::Message(format!(
            "failed to flock {:?}: {}",
            lock_path,
            std::io::Error::last_os_error()
        )));
    }

    struct FlockGuard(std::os::unix::io::RawFd);
    impl Drop for FlockGuard {
        fn drop(&mut self) {
            // SAFETY: self.0 is a valid locked fd.
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
            }
        }
    }
    let _flock_guard = FlockGuard(fd);

    // Verify account exists in state store before proceeding
    let stored_pool =
        crate::storage::load_pool().map_err(|e| AuthError::Message(format!("failed to load pool state: {}", e)))?;
    let stored = crate::storage::find_account(&stored_pool, account)
        .ok_or_else(|| AuthError::Message(format!("account '{}' not found", account.id)))?;

    // Another goroutine/process may have refreshed while waiting for lock.
    if !stored.access_token.is_empty() {
        account.access_token = stored.access_token.clone();
    }
    if !stored.refresh_token.is_empty() {
        account.refresh_token = stored.refresh_token.clone();
    }
    if stored.token_expiry.is_some() {
        account.token_expiry = stored.token_expiry;
    }
    if stored.updated_at.is_some() {
        account.updated_at = stored.updated_at;
    }
    if !stored.id_token.is_empty() {
        account.id_token = stored.id_token.clone();
    }

    let current_now = crate::clock::now();
    let current_now_secs = current_now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let current_now_unix = current_now_secs as f64;

    if !force
        && !account.access_token.is_empty()
        && account.token_expiry.is_some()
        && (account.token_expiry.unwrap() - current_now_unix > skew)
    {
        return Ok(account.access_token.clone());
    }

    let rf = account.refresh_token.clone();
    if rf.is_empty() {
        return Err(AuthError::Message("Missing refresh_token".to_string()));
    }

    let client_id_val = client_id();
    let client_secret_val = client_secret();
    let form = [
        ("client_id", client_id_val.as_str()),
        ("client_secret", client_secret_val.as_str()),
        ("refresh_token", rf.as_str()),
        ("grant_type", "refresh_token"),
    ];

    crate::observability::record_auth_refresh_attempt();
    if crate::config::is_test_mode() && token_url() == DEFAULT_TOKEN_URL {
        crate::observability::record_auth_refresh_failure();
        return Err(AuthError::Message(
            "[FAIL-CLOSED TEST GUARD] refusing external token refresh in test mode without mock backend".to_string(),
        ));
    }

    let (status, resp_body) = match crate::http::post_form(&token_url(), &form, Duration::from_secs(10)).await {
        Ok(res) => res,
        Err(crate::http::HttpError::Body(e)) => {
            crate::observability::record_auth_refresh_failure();
            return Err(AuthError::Message(format!(
                "failed to read token refresh response: {}",
                e
            )));
        }
        Err(e) => {
            crate::observability::record_auth_refresh_failure();
            return Err(AuthError::Message(format!(
                "token refresh network request failed: {}",
                e
            )));
        }
    };

    if !(200..300).contains(&status) {
        crate::observability::record_auth_refresh_failure();
        return Err(AuthError::Message(format!(
            "token refresh failed (HTTP {}{})",
            status,
            safe_provider_reason(&resp_body)
        )));
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        expires_in: Option<f64>,
    }

    let res: TokenResponse = match serde_json::from_slice(&resp_body) {
        Ok(r) => r,
        Err(e) => {
            crate::observability::record_auth_refresh_failure();
            return Err(AuthError::Message(format!("failed to parse token refresh JSON: {}", e)));
        }
    };

    if res.access_token.is_empty() {
        crate::observability::record_auth_refresh_failure();
        return Err(AuthError::Message(
            "Token refresh failed: No access_token returned".to_string(),
        ));
    }

    let expires_in = match res.expires_in {
        Some(e) if e > 0.0 => e,
        _ => 3600.0,
    };

    account.access_token = res.access_token.clone();
    if !res.refresh_token.is_empty() {
        account.refresh_token = res.refresh_token;
    }
    let expiry = current_now_unix + expires_in;
    account.token_expiry = Some(expiry);
    account.updated_at = Some(current_now_secs);

    if let Err(e) = persist_account_fields(account, true, false, false) {
        return Err(AuthError::Message(format!("failed to persist refreshed tokens: {}", e)));
    }

    crate::observability::record_auth_refresh_success();
    Ok(res.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestServer;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::UNIX_EPOCH;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_isolated_test_dir() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        TestEnv { _lock: lock, _dir: dir }
    }

    #[test]
    fn test_decode_cred() {
        assert!(!DEFAULT_CLIENT_ID.is_empty(), "DefaultClientID is empty");
        // Compared by SHA-256 so the repo holds no plaintext copy of the built-in OAuth client.
        let sha256 = |v: &str| hex::encode(<sha2::Sha256 as sha2::Digest>::digest(v.as_bytes()));
        assert_eq!(
            sha256(&DEFAULT_CLIENT_ID),
            "bf00c418024ba6bf606ccdc37120976e41bc429dd1d46ecf16a729aa532626ea"
        );
        assert_eq!(
            sha256(&DEFAULT_CLIENT_SECRET),
            "1d2f041093fd95aa8995a038c711d50a7960da09a505381c09a745d6ad0ecc60"
        );
    }

    #[test]
    fn test_decode_jwt_payload() {
        struct Case {
            jwt: &'static str,
            want_email: Option<&'static str>,
            want_empty: bool,
        }

        let cases = [
            Case {
                jwt: "header.eyJlbWFpbCI6ICJ1c2VyQGV4YW1wbGUuY29tIiwgInN1YiI6ICIxMjM0NSJ9.sig",
                want_email: Some("user@example.com"),
                want_empty: false,
            },
            Case {
                jwt: "not-a-jwt",
                want_email: None,
                want_empty: true,
            },
            Case {
                jwt: "header.!!!notb64!!!.sig",
                want_email: None,
                want_empty: true,
            },
            Case {
                jwt: "header.bm90LWpzb24.sig",
                want_email: None,
                want_empty: true,
            },
        ];

        for c in cases {
            let res = decode_jwt_payload(c.jwt);
            if c.want_empty {
                assert!(res.is_empty(), "expected empty claims for {}", c.jwt);
            } else {
                let email = res.get("email").and_then(|v| v.as_str());
                assert_eq!(email, c.want_email);
            }
        }
    }

    #[test]
    fn test_error_classifiers() {
        // IsValidationError
        assert!(!is_validation_error(200, b"validation_required"));
        assert!(is_validation_error(403, br#"{"error": "validation_required"}"#));
        assert!(is_validation_error(403, b"Please Verify Your Account to continue"));
        assert!(!is_validation_error(403, b"other 403 error"));

        // ExtractValidationURL
        let body_metadata = br#"{
            "error": {
                "details": [
                    {"metadata": {"validation_url": "https://accounts.google.com/verify?id=123"}}
                ]
            }
        }"#;
        assert_eq!(
            extract_validation_url(body_metadata),
            "https://accounts.google.com/verify?id=123"
        );

        let body_link = br#"{
            "error": {
                "details": [
                    {"links": [{"description": "Please verify", "url": "https://google.com/verify"}]}
                ]
            }
        }"#;
        assert_eq!(extract_validation_url(body_link), "https://google.com/verify");

        assert_eq!(extract_validation_url(b"invalid json"), "");

        // IsAuthError
        assert!(is_auth_error(401, b"Unauthorized"));
        assert!(is_auth_error(403, b"invalid_grant"));
        assert!(is_auth_error(403, b"Access_Token_Expired"));
        assert!(!is_auth_error(403, b"permission denied for resource"));
        assert!(!is_auth_error(500, b"invalid_grant"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_cached() {
        let _env = setup_isolated_test_dir();

        fn fixed_time() -> std::time::SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        let _clock_guard = crate::clock::set_now_for_test(fixed_time);

        let expiry = 1_800_000_000.0 + 300.0; // 5 minutes remaining (> 120s)
        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "cached-token-123".to_string(),
            token_expiry: Some(expiry),
            ..Default::default()
        };

        let tok = refresh_token(&mut acc).await.expect("unexpected error");
        assert_eq!(tok, "cached-token-123");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_within() {
        let _env = setup_isolated_test_dir();

        fn fixed_time() -> std::time::SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        let _clock_guard = crate::clock::set_now_for_test(fixed_time);

        let expiry = 1_800_000_000.0 + 300.0; // 5 minutes remaining (> 120s, but < 10m)
        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "cached-token-123".to_string(),
            token_expiry: Some(expiry),
            ..Default::default()
        };

        let tok = refresh_token(&mut acc).await.expect("expected cached token");
        assert_eq!(tok, "cached-token-123");

        let err = refresh_token_within(&mut acc, Duration::from_secs(600))
            .await
            .expect_err("expected missing refresh token error when skew 10m exceeded");
        assert!(err.to_string().contains("Missing refresh_token"), "got err: {}", err);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_missing_refresh_token() {
        let _env = setup_isolated_test_dir();

        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            ..Default::default()
        };

        let err = refresh_token(&mut acc)
            .await
            .expect_err("expected Missing refresh_token");
        assert_eq!(err.to_string(), "Missing refresh_token");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_upstream_failure_does_not_leak_body() {
        let _env = setup_isolated_test_dir();

        let server = TestServer::start(|_req| {
            (
                400,
                vec![("content-type".to_string(), "application/json".to_string())],
                br#"{"error":"invalid_grant","description":"raw-provider-body-marker access-secret"}"#.to_vec(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            refresh_token: "refresh-secret".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let err = refresh_token(&mut acc).await.expect_err("expected refresh error");
        let err_str = err.to_string();
        assert!(err_str.contains("HTTP 400"), "missing HTTP 400: {}", err_str);
        assert!(
            !err_str.contains("raw-provider-body-marker"),
            "leaked raw body: {}",
            err_str
        );
        assert!(!err_str.contains("access-secret"), "leaked access-secret: {}", err_str);
        assert!(
            !err_str.contains("refresh-secret"),
            "leaked refresh-secret: {}",
            err_str
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_upstream_success() {
        let _env = setup_isolated_test_dir();

        let server_hits = Arc::new(AtomicI32::new(0));
        let hits_clone = Arc::clone(&server_hits);

        let server = TestServer::start(move |req| {
            hits_clone.fetch_add(1, Ordering::SeqCst);
            if req.form_value("grant_type").as_deref() != Some("refresh_token") {
                return (400, vec![], b"bad grant_type".to_vec());
            }
            if req.form_value("refresh_token").as_deref() != Some("valid-rf") {
                return (401, vec![], b"unauthorized".to_vec());
            }

            let resp = serde_json::json!({
                "access_token": "new-at-456",
                "refresh_token": "new-rf-789",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut pool = crate::storage::Pool::new_empty();
        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "user@example.com".to_string(),
            refresh_token: "valid-rf".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let tok = refresh_token(&mut acc).await.expect("RefreshToken failed");
        assert_eq!(tok, "new-at-456");
        assert_eq!(acc.refresh_token, "new-rf-789");

        // Verify persistence in disk pool
        let loaded = crate::storage::load_pool().expect("failed to load pool");
        let stored = crate::storage::find_account(&loaded, &acc).expect("account not found in pool");
        assert_eq!(stored.access_token, "new-at-456");
        assert_eq!(stored.refresh_token, "new-rf-789");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_concurrent_single_flight() {
        let _env = setup_isolated_test_dir();

        let server_hits = Arc::new(AtomicI32::new(0));
        let hits_clone = Arc::clone(&server_hits);

        let server = TestServer::start(move |_req| {
            hits_clone.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            let resp = serde_json::json!({
                "access_token": "new-concurrent-at",
                "refresh_token": "valid-rf",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut pool = crate::storage::Pool::new_empty();
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "user@example.com".to_string(),
            refresh_token: "valid-rf".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc);
        crate::storage::save_pool(&pool).expect("failed to save pool");

        const TASKS: usize = 10;
        let mut handles = Vec::new();
        for _ in 0..TASKS {
            handles.push(tokio::spawn(async {
                let mut local_acc = crate::storage::Account {
                    id: "acc_1".to_string(),
                    refresh_token: "valid-rf".to_string(),
                    ..Default::default()
                };
                refresh_token(&mut local_acc).await
            }));
        }

        for h in handles {
            let tok = h.await.unwrap().expect("RefreshToken concurrent error");
            assert_eq!(tok, "new-concurrent-at");
        }

        let hits = server_hits.load(Ordering::SeqCst);
        assert_eq!(hits, 1, "expected exactly 1 upstream request, got {}", hits);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_removed_account_not_recreated() {
        let _env = setup_isolated_test_dir();

        let (refresh_reached_tx, refresh_reached_rx) = tokio::sync::oneshot::channel();
        let refresh_reached_tx = Arc::new(Mutex::new(Some(refresh_reached_tx)));
        let (allow_resp_tx, allow_resp_rx) = std::sync::mpsc::channel();
        let allow_rx = Arc::new(Mutex::new(allow_resp_rx));

        let server = TestServer::start(move |_req| {
            if let Some(tx) = refresh_reached_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = allow_rx.lock().unwrap().recv();
            let resp = serde_json::json!({
                "access_token": "new-at-after-remove",
                "refresh_token": "new-rf-after-remove",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut pool = crate::storage::Pool::new_empty();
        let acc = crate::storage::Account {
            id: "acc_victim".to_string(),
            email: "victim@example.com".to_string(),
            refresh_token: "valid-rf".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc);
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let handle = tokio::spawn(async move {
            let mut local_acc = crate::storage::Account {
                id: "acc_victim".to_string(),
                email: "victim@example.com".to_string(),
                refresh_token: "valid-rf".to_string(),
                ..Default::default()
            };
            refresh_token(&mut local_acc).await
        });

        // Wait until refresh is in-flight at upstream server
        refresh_reached_rx.await.unwrap();

        // Remove the account from storage while refresh request is in flight
        crate::storage::pool_transaction(|p| {
            p.accounts.clear();
            Ok(())
        })
        .expect("failed to delete account during refresh");

        let loaded = crate::storage::load_pool().expect("failed to load pool");
        assert!(
            loaded.accounts.is_empty(),
            "account was not deleted before refresh completed"
        );

        // Unblock upstream response
        let _ = allow_resp_tx.send(());

        // Refresh should complete with an error (account removed)
        let refresh_err = handle.await.unwrap();
        assert!(
            refresh_err.is_err(),
            "expected RefreshToken to report error when persisting to removed account"
        );

        // CRITICAL CHECK: Account MUST NOT have been recreated in storage!
        let reloaded = crate::storage::load_pool().expect("failed to reload pool");
        assert!(
            reloaded.accounts.is_empty(),
            "SECURITY VIOLATION: removed account was recreated: {:?}",
            reloaded.accounts
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_concurrent_active_switch() {
        let _env = setup_isolated_test_dir();

        let (refresh_reached_tx, refresh_reached_rx) = tokio::sync::oneshot::channel();
        let refresh_reached_tx = Arc::new(Mutex::new(Some(refresh_reached_tx)));
        let (allow_resp_tx, allow_resp_rx) = std::sync::mpsc::channel();
        let allow_rx = Arc::new(Mutex::new(allow_resp_rx));

        let server = TestServer::start(move |_req| {
            if let Some(tx) = refresh_reached_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = allow_rx.lock().unwrap().recv();
            let resp = serde_json::json!({
                "access_token": "new-at-acc1",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut pool = crate::storage::Pool::new_empty();
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "acc1@example.com".to_string(),
            refresh_token: "rf1".to_string(),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            email: "acc2@example.com".to_string(),
            refresh_token: "rf2".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc1);
        pool.accounts.push(acc2);
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let handle = tokio::spawn(async move {
            let mut local_acc1 = crate::storage::Account {
                id: "acc_1".to_string(),
                refresh_token: "rf1".to_string(),
                ..Default::default()
            };
            refresh_token(&mut local_acc1).await
        });

        refresh_reached_rx.await.unwrap();

        // Switch active account to acc_2 while acc_1 is refreshing
        crate::storage::pool_transaction(|p| {
            p.active_account_id = Some("acc_2".to_string());
            Ok(())
        })
        .expect("failed to switch active account");

        let _ = allow_resp_tx.send(());
        handle.await.unwrap().expect("refresh_token for acc_1 failed");

        // Verify active account remained acc_2
        let loaded = crate::storage::load_pool().expect("failed to load pool");
        assert_eq!(
            loaded.active_account_id.as_deref(),
            Some("acc_2"),
            "active account overwritten by refresh race"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_refresh_token_rotated_token_preserved() {
        let _env = setup_isolated_test_dir();

        let server = TestServer::start(|_req| {
            let resp = serde_json::json!({
                "access_token": "new-at-rotated",
                "refresh_token": "brand-new-rotated-rf-999",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        let mut pool = crate::storage::Pool::new_empty();
        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "user@example.com".to_string(),
            refresh_token: "initial-rf".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let tok = refresh_token(&mut acc).await.expect("RefreshToken failed");
        assert_eq!(tok, "new-at-rotated");

        let loaded = crate::storage::load_pool().expect("failed to load pool");
        assert_eq!(
            loaded.accounts[0].refresh_token, "brand-new-rotated-rf-999",
            "rotated refresh token was not preserved"
        );
    }

    #[test]
    fn test_token_expiring_soon() {
        // Other tests override crate::clock while holding TEST_LOCK; this one reads the real time.
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::clock::now_unix();

        assert!(
            token_expiring_soon(None, Duration::from_secs(600)),
            "expected None account to be expiring soon"
        );

        let no_token = crate::storage::Account {
            id: "no_tok".to_string(),
            ..Default::default()
        };
        assert!(
            token_expiring_soon(&no_token, Duration::from_secs(600)),
            "expected account without access token to be expiring soon"
        );

        let no_exp = crate::storage::Account {
            id: "no_exp".to_string(),
            access_token: "at".to_string(),
            ..Default::default()
        };
        assert!(
            token_expiring_soon(&no_exp, Duration::from_secs(600)),
            "expected account without token expiry to be expiring soon"
        );

        let soon = now + 300.0; // 5 minutes left
        let acc_soon = crate::storage::Account {
            id: "soon".to_string(),
            access_token: "at".to_string(),
            token_expiry: Some(soon),
            ..Default::default()
        };
        assert!(
            token_expiring_soon(&acc_soon, Duration::from_secs(600)),
            "expected account with 5m left to be expiring within 10m"
        );

        let plenty = now + 3600.0; // 60 minutes left
        let acc_plenty = crate::storage::Account {
            id: "plenty".to_string(),
            access_token: "at".to_string(),
            token_expiry: Some(plenty),
            ..Default::default()
        };
        assert!(
            !token_expiring_soon(&acc_plenty, Duration::from_secs(600)),
            "expected account with 60m left NOT to be expiring within 10m"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_observability_case_j_auth_refresh_cached_token() {
        let _env = setup_isolated_test_dir();
        crate::observability::reset();

        fn fixed_time() -> std::time::SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        let _clock_guard = crate::clock::set_now_for_test(fixed_time);

        let expiry = 1_800_000_000.0 + 300.0;
        let mut acc = crate::storage::Account {
            id: "acc_cached".to_string(),
            access_token: "valid-cached-token".to_string(),
            refresh_token: "refresh-tok".to_string(),
            token_expiry: Some(expiry),
            ..Default::default()
        };

        let tok = refresh_token(&mut acc).await.expect("unexpected error");
        assert_eq!(tok, "valid-cached-token");

        let snap = crate::observability::get_snapshot();
        assert_eq!(
            snap.auth_refresh_attempts, 0,
            "auth_refresh_attempts = {}, want 0 on cached token",
            snap.auth_refresh_attempts
        );
        assert_eq!(
            snap.auth_refresh_success, 0,
            "auth_refresh_success = {}, want 0 on cached token",
            snap.auth_refresh_success
        );
        assert_eq!(
            snap.auth_refresh_failure, 0,
            "auth_refresh_failure = {}, want 0 on cached token",
            snap.auth_refresh_failure
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises global overrides
    async fn test_observability_case_k_auth_refresh_real_success_and_failure() {
        let _env = setup_isolated_test_dir();
        crate::observability::reset();

        let server = TestServer::start(|req| {
            if req.form_value("refresh_token").as_deref() == Some("bad-token") {
                return (
                    400,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    br#"{"error":"invalid_grant"}"#.to_vec(),
                );
            }
            let resp = serde_json::json!({
                "access_token": "fresh-access-token",
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = set_token_url_for_test(&server.url());

        // 1. Successful refresh
        let mut pool = crate::storage::Pool::new_empty();
        let mut acc_good = crate::storage::Account {
            id: "acc_good".to_string(),
            refresh_token: "good-token".to_string(),
            ..Default::default()
        };
        let mut acc_bad = crate::storage::Account {
            id: "acc_bad".to_string(),
            refresh_token: "bad-token".to_string(),
            ..Default::default()
        };
        pool.accounts.push(acc_good.clone());
        pool.accounts.push(acc_bad.clone());
        crate::storage::save_pool(&pool).expect("failed to save pool");

        let tok = refresh_token(&mut acc_good).await.expect("RefreshToken good failed");
        assert_eq!(tok, "fresh-access-token");

        let snap = crate::observability::get_snapshot();
        assert_eq!(
            snap.auth_refresh_attempts, 1,
            "auth_refresh_attempts = {}, want 1",
            snap.auth_refresh_attempts
        );
        assert_eq!(
            snap.auth_refresh_success, 1,
            "auth_refresh_success = {}, want 1",
            snap.auth_refresh_success
        );
        assert_eq!(
            snap.auth_refresh_failure, 0,
            "auth_refresh_failure = {}, want 0",
            snap.auth_refresh_failure
        );

        // 2. Failed refresh
        let res = refresh_token(&mut acc_bad).await;
        assert!(res.is_err(), "expected error on bad refresh token");

        let snap = crate::observability::get_snapshot();
        assert_eq!(
            snap.auth_refresh_attempts, 2,
            "auth_refresh_attempts = {}, want 2",
            snap.auth_refresh_attempts
        );
        assert_eq!(
            snap.auth_refresh_success, 1,
            "auth_refresh_success = {}, want 1",
            snap.auth_refresh_success
        );
        assert_eq!(
            snap.auth_refresh_failure, 1,
            "auth_refresh_failure = {}, want 1",
            snap.auth_refresh_failure
        );
    }
}
