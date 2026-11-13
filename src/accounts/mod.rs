#![allow(clippy::collapsible_if)]

use std::sync::RwLock;

pub mod crypto;
pub mod order;

pub use crypto::*;
pub use order::*;

#[derive(Debug)]
pub enum AccountsError {
    Message(String),
    Storage(crate::storage::StorageError),
    Auth(crate::auth::AuthError),
    Config(crate::config::ConfigError),
    Http(crate::http::HttpError),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for AccountsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountsError::Message(s) => write!(f, "{}", s),
            AccountsError::Storage(e) => write!(f, "{}", e),
            AccountsError::Auth(e) => write!(f, "{}", e),
            AccountsError::Config(e) => write!(f, "{}", e),
            AccountsError::Http(e) => write!(f, "{}", e),
            AccountsError::Io(e) => write!(f, "{}", e),
            AccountsError::Json(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AccountsError {}

impl From<crate::storage::StorageError> for AccountsError {
    fn from(e: crate::storage::StorageError) -> Self {
        AccountsError::Storage(e)
    }
}

impl From<crate::auth::AuthError> for AccountsError {
    fn from(e: crate::auth::AuthError) -> Self {
        AccountsError::Auth(e)
    }
}

impl From<crate::config::ConfigError> for AccountsError {
    fn from(e: crate::config::ConfigError) -> Self {
        AccountsError::Config(e)
    }
}

impl From<crate::http::HttpError> for AccountsError {
    fn from(e: crate::http::HttpError) -> Self {
        AccountsError::Http(e)
    }
}

impl From<std::io::Error> for AccountsError {
    fn from(e: std::io::Error) -> Self {
        AccountsError::Io(e)
    }
}

impl From<serde_json::Error> for AccountsError {
    fn from(e: serde_json::Error) -> Self {
        AccountsError::Json(e)
    }
}

pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationTransitionResult {
    pub cleared: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct RestrictionSnapshot {
    status: String,
    validation_url: Option<String>,
    rate_limited_until: Option<f64>,
    refresh_token: String,
}

fn snapshot_account(acc: &crate::storage::Account) -> RestrictionSnapshot {
    RestrictionSnapshot {
        status: acc.status.clone(),
        validation_url: acc.validation_url.clone(),
        rate_limited_until: acc.rate_limited_until,
        refresh_token: acc.refresh_token.clone(),
    }
}

impl RestrictionSnapshot {
    fn matches(&self, acc: &crate::storage::Account) -> bool {
        if acc.status != self.status {
            return false;
        }
        if acc.validation_url != self.validation_url {
            return false;
        }
        if acc.rate_limited_until != self.rate_limited_until {
            return false;
        }
        if acc.refresh_token != self.refresh_token {
            return false;
        }
        true
    }
}

// Hook callbacks
pub type BeforeClearHookFunc = Box<dyn Fn(&str) + Send + Sync>;
static BEFORE_CLEAR_HOOK: RwLock<Option<std::sync::Arc<BeforeClearHookFunc>>> = RwLock::new(None);

pub struct BeforeClearGuard;
impl Drop for BeforeClearGuard {
    fn drop(&mut self) {
        *BEFORE_CLEAR_HOOK.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_before_clear_hook(hook: Option<BeforeClearHookFunc>) -> BeforeClearGuard {
    let mut w = BEFORE_CLEAR_HOOK.write().unwrap_or_else(|e| e.into_inner());
    *w = hook.map(std::sync::Arc::new);
    BeforeClearGuard
}

pub type PoolTxMutator = Box<dyn FnOnce(&mut crate::storage::Pool) -> Result<(), AccountsError> + Send>;
pub type PoolTransactionFunc = Box<dyn Fn(PoolTxMutator) -> Result<(), AccountsError> + Send + Sync>;

static POOL_TRANSACTION_HOOK: RwLock<Option<std::sync::Arc<PoolTransactionFunc>>> = RwLock::new(None);

pub struct PoolTransactionGuard;
impl Drop for PoolTransactionGuard {
    fn drop(&mut self) {
        *POOL_TRANSACTION_HOOK.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_pool_transaction(tx_fn: Option<PoolTransactionFunc>) -> PoolTransactionGuard {
    let mut w = POOL_TRANSACTION_HOOK.write().unwrap_or_else(|e| e.into_inner());
    *w = tx_fn.map(std::sync::Arc::new);
    PoolTransactionGuard
}

pub fn execute_pool_transaction<F>(mutator: F) -> Result<(), AccountsError>
where
    F: FnOnce(&mut crate::storage::Pool) -> Result<(), AccountsError> + Send + 'static,
{
    let hook_opt = {
        let r = POOL_TRANSACTION_HOOK.read().unwrap_or_else(|e| e.into_inner());
        r.clone()
    };
    if let Some(hook) = hook_opt {
        hook(Box::new(mutator))
    } else {
        crate::storage::pool_transaction(|p| {
            mutator(p).map_err(|e| crate::storage::StorageError::Other(e.to_string()))
        })?;
        Ok(())
    }
}

pub type AccountFieldsPersisterFunc =
    Box<dyn Fn(&crate::storage::Account, bool, bool, bool) -> Result<(), AccountsError> + Send + Sync>;

static ACCOUNT_FIELDS_PERSISTER: RwLock<Option<std::sync::Arc<AccountFieldsPersisterFunc>>> = RwLock::new(None);

pub struct AccountFieldsPersisterGuard;
impl Drop for AccountFieldsPersisterGuard {
    fn drop(&mut self) {
        *ACCOUNT_FIELDS_PERSISTER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_account_fields_persister(persister: Option<AccountFieldsPersisterFunc>) -> AccountFieldsPersisterGuard {
    let mut w = ACCOUNT_FIELDS_PERSISTER.write().unwrap_or_else(|e| e.into_inner());
    *w = persister.map(std::sync::Arc::new);
    AccountFieldsPersisterGuard
}

pub fn is_account_fields_persister_replaced() -> bool {
    let r = ACCOUNT_FIELDS_PERSISTER.read().unwrap_or_else(|e| e.into_inner());
    r.is_some()
}

pub type QuotaProberFunc = Box<
    dyn Fn(&mut crate::storage::Account) -> Result<Option<crate::storage::QuotaState>, AccountsError> + Send + Sync,
>;
pub type QuotaFormatterFunc = Box<dyn Fn(&crate::storage::QuotaState) + Send + Sync>;

type QuotaProberEntry = (
    std::sync::Arc<QuotaProberFunc>,
    Option<std::sync::Arc<QuotaFormatterFunc>>,
);

static QUOTA_PROBER: RwLock<Option<QuotaProberEntry>> = RwLock::new(None);

pub struct QuotaProberGuard;
impl Drop for QuotaProberGuard {
    fn drop(&mut self) {
        *QUOTA_PROBER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_quota_prober(prober: Option<QuotaProberFunc>, formatter: Option<QuotaFormatterFunc>) -> QuotaProberGuard {
    let mut w = QUOTA_PROBER.write().unwrap_or_else(|e| e.into_inner());
    *w = prober.map(|p| (std::sync::Arc::new(p), formatter.map(std::sync::Arc::new)));
    QuotaProberGuard
}

pub fn get_quota_prober() -> Option<(
    std::sync::Arc<QuotaProberFunc>,
    Option<std::sync::Arc<QuotaFormatterFunc>>,
)> {
    let r = QUOTA_PROBER.read().unwrap_or_else(|e| e.into_inner());
    r.clone()
}

pub type TokenRefresherFunc = Box<
    dyn Fn(
            &mut crate::storage::Account,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, crate::auth::AuthError>> + Send>>
        + Send
        + Sync,
>;

static TOKEN_REFRESHER: RwLock<Option<std::sync::Arc<TokenRefresherFunc>>> = RwLock::new(None);

pub struct TokenRefresherGuard;
impl Drop for TokenRefresherGuard {
    fn drop(&mut self) {
        *TOKEN_REFRESHER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_token_refresher_for_test(f: Option<TokenRefresherFunc>) -> TokenRefresherGuard {
    let mut w = TOKEN_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
    *w = f.map(std::sync::Arc::new);
    TokenRefresherGuard
}

async fn do_refresh_token(account: &mut crate::storage::Account) -> Result<String, crate::auth::AuthError> {
    let hook = {
        let r = TOKEN_REFRESHER.read().unwrap_or_else(|e| e.into_inner());
        r.clone()
    };
    if let Some(f) = hook {
        f(account).await
    } else {
        crate::auth::refresh_token(account).await
    }
}

pub type TokenFileWriterFunc = Box<dyn Fn(&crate::storage::Account) -> Result<(), AccountsError> + Send + Sync>;

static TOKEN_FILE_WRITER: RwLock<Option<std::sync::Arc<TokenFileWriterFunc>>> = RwLock::new(None);

pub struct TokenFileWriterGuard;
impl Drop for TokenFileWriterGuard {
    fn drop(&mut self) {
        *TOKEN_FILE_WRITER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn set_token_file_writer_for_test(f: Option<TokenFileWriterFunc>) -> TokenFileWriterGuard {
    let mut w = TOKEN_FILE_WRITER.write().unwrap_or_else(|e| e.into_inner());
    *w = f.map(std::sync::Arc::new);
    TokenFileWriterGuard
}

fn do_write_token_file(account: &crate::storage::Account) -> Result<(), AccountsError> {
    let hook = {
        let r = TOKEN_FILE_WRITER.read().unwrap_or_else(|e| e.into_inner());
        r.clone()
    };
    if let Some(f) = hook {
        f(account)
    } else {
        write_agy_token_file_inner(account)
    }
}

pub fn is_show_email_enabled(pool_opt: Option<&crate::storage::Pool>) -> bool {
    if let Ok(val) = std::env::var("AGY_SHOW_EMAIL") {
        let v = val.trim().to_ascii_lowercase();
        if v == "1" || v == "true" || v == "yes" || v == "on" {
            return true;
        }
        if v == "0" || v == "false" || v == "no" || v == "off" {
            return false;
        }
    }

    if let Ok(val) = std::env::var("AGY_PRIVACY") {
        let v = val.trim().to_ascii_lowercase();
        if v == "0" || v == "false" || v == "no" || v == "off" {
            return true;
        }
        if v == "1" || v == "true" || v == "yes" || v == "on" {
            return false;
        }
    }

    let loaded_pool;
    let pool = match pool_opt {
        Some(p) => Some(p),
        None => {
            loaded_pool = crate::storage::load_pool().ok();
            loaded_pool.as_ref()
        }
    };

    if let Some(p) = pool {
        if let Some(show) = p.show_email {
            return show;
        }
    }

    false
}

pub fn display_account_name(account: Option<&crate::storage::Account>, show_email_opt: Option<bool>) -> String {
    let acc = match account {
        Some(a) => a,
        None => return "Account".to_string(),
    };

    let mut friendly = acc.name.trim().to_string();
    if friendly.is_empty() {
        let acc_id = acc.id.trim();
        if let Some(suffix) = acc_id.strip_prefix("acc_") {
            if is_digits(suffix) {
                if let Ok(n) = suffix.parse::<usize>() {
                    friendly = format!("Account {}", n);
                }
            }
        }
        if friendly.is_empty() {
            friendly = "Account".to_string();
        }
    }

    let show_email = show_email_opt.unwrap_or_else(|| is_show_email_enabled(None));
    let email = acc.email.trim();
    if show_email && !email.is_empty() {
        format!("{} ({})", friendly, email)
    } else {
        friendly
    }
}

pub fn find_account_by_target<'a>(
    accounts: &'a [crate::storage::Account],
    target: &str,
) -> Option<&'a crate::storage::Account> {
    let idx = resolve_account_index(accounts, target)?;
    accounts.get(idx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeIdentityState {
    Match,
    MismatchKnown,
    MismatchUnknown,
    Missing,
    Unknown,
    NoActive,
}

impl NativeIdentityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            NativeIdentityState::Match => "match",
            NativeIdentityState::MismatchKnown => "mismatch_known",
            NativeIdentityState::MismatchUnknown => "mismatch_unknown",
            NativeIdentityState::Missing => "missing",
            NativeIdentityState::Unknown => "unknown",
            NativeIdentityState::NoActive => "no_active",
        }
    }
}

pub fn inspect_native_agy_identity(
    pool_opt: Option<&crate::storage::Pool>,
) -> Result<NativeIdentityState, AccountsError> {
    let loaded_pool;
    let pool = match pool_opt {
        Some(p) => p,
        None => {
            loaded_pool = crate::storage::load_pool()?;
            &loaded_pool
        }
    };

    let active_id = match pool.active_account_id {
        Some(ref id) if !id.is_empty() => id,
        _ => return Ok(NativeIdentityState::NoActive),
    };
    let active = match find_account_by_target(&pool.accounts, active_id) {
        Some(a) => a,
        None => return Ok(NativeIdentityState::NoActive),
    };

    let token_file = crate::config::agy_token_file();
    let data = match std::fs::read(&token_file) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NativeIdentityState::Missing);
        }
        Err(_) => {
            return Err(AccountsError::Message(
                "native credential file cannot be read".to_string(),
            ));
        }
    };

    #[derive(serde::Deserialize)]
    struct RawToken {
        #[serde(default)]
        id_token: String,
    }
    let raw: RawToken = match serde_json::from_slice(&data) {
        Ok(r) => r,
        Err(_) => {
            return Err(AccountsError::Message(
                "native credential file is malformed".to_string(),
            ));
        }
    };

    let claims = crate::auth::decode_jwt_payload(&raw.id_token);
    let email = claims.get("email").and_then(|v| v.as_str()).unwrap_or("");
    if email.is_empty() {
        return Ok(NativeIdentityState::Unknown);
    }

    for account in &pool.accounts {
        if account.email == email {
            if account.id == active.id {
                return Ok(NativeIdentityState::Match);
            }
            return Ok(NativeIdentityState::MismatchKnown);
        }
    }

    Ok(NativeIdentityState::MismatchUnknown)
}

pub fn format_iso_expiry(ts: f64) -> String {
    let t = jiff::Timestamp::from_second(ts as i64).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

fn write_agy_token_file_inner(account: &crate::storage::Account) -> Result<(), AccountsError> {
    crate::storage::ensure_dirs()?;
    let token_file = crate::config::agy_token_file();
    crate::config::assert_safe_native_token_write(&token_file)?;

    if let Some(parent) = token_file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AccountsError::Message(format!("failed to create agy-cli directory: {}", e)))?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let expiry_ts = match account.token_expiry {
        Some(exp) if exp > 0.0 => exp as i64,
        _ => (crate::clock::now_unix() as i64) + 3600,
    };

    let payload = serde_json::json!({
        "auth_method": "consumer",
        "id_token": account.id_token,
        "token": {
            "access_token": account.access_token,
            "expiry": format_iso_expiry(expiry_ts as f64),
            "refresh_token": account.refresh_token,
            "token_type": "Bearer"
        }
    });

    crate::storage::atomic_json_write(&token_file, &payload)?;
    Ok(())
}

pub fn write_agy_token_file(account: &crate::storage::Account) -> Result<(), AccountsError> {
    let token_file = crate::config::agy_token_file();
    let lock_file = std::path::PathBuf::from(format!("{}.lock", token_file.display()));
    crate::config::assert_safe_native_token_write(&lock_file)?;
    crate::storage::with_file_lock(&lock_file, true, || {
        write_agy_token_file_inner(account).map_err(|e| crate::storage::StorageError::Other(e.to_string()))
    })
    .map_err(|e| AccountsError::Message(e.to_string()))
}

pub async fn sync_active_agy_token_file() -> Result<bool, AccountsError> {
    let token_file = crate::config::agy_token_file();
    crate::config::assert_safe_native_token_write(&token_file)?;
    let lock_file = std::path::PathBuf::from(format!("{}.lock", token_file.display()));
    crate::config::assert_safe_native_token_write(&lock_file)?;

    let dir = lock_file.parent().unwrap_or_else(|| std::path::Path::new("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_file)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&lock_file, std::fs::Permissions::from_mode(0o600));

    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: fd is an open file descriptor for the lock file.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        return Err(AccountsError::Message(format!(
            "failed to flock {:?}: {}",
            lock_file,
            std::io::Error::last_os_error()
        )));
    }

    struct FlockGuard(std::os::unix::io::RawFd);
    impl Drop for FlockGuard {
        fn drop(&mut self) {
            // SAFETY: self.0 is an open locked fd.
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
            }
        }
    }
    let _flock = FlockGuard(fd);

    for _attempt in 0..3 {
        let pool = crate::storage::load_pool()?;
        let active_id = match pool.active_account_id {
            Some(ref id) => id.clone(),
            None => return Ok(false),
        };
        let mut account = match pool.accounts.iter().find(|a| a.id == active_id).cloned() {
            Some(a) => a,
            None => return Ok(false),
        };

        let now = crate::clock::now_unix();
        let needs_refresh =
            account.access_token.is_empty() || account.token_expiry.map(|exp| exp - now <= 120.0).unwrap_or(false);

        if needs_refresh {
            if let Err(e) = do_refresh_token(&mut account).await {
                return Err(AccountsError::Message(format!(
                    "failed to refresh active account {}: {}",
                    account.id, e
                )));
            }
        }

        let refreshed_pool = crate::storage::load_pool()?;
        if refreshed_pool.active_account_id.as_deref() == Some(&active_id) {
            if let Err(e) = do_write_token_file(&account) {
                return Err(AccountsError::Message(format!(
                    "failed to write native agy token: {}",
                    e
                )));
            }
            return Ok(true);
        }
    }

    Ok(false)
}

async fn sync_account_to_agy(account: &mut crate::storage::Account) -> Result<(), AccountsError> {
    let now = crate::clock::now_unix();
    let needs_refresh =
        account.access_token.is_empty() || account.token_expiry.map(|exp| exp - now <= 120.0).unwrap_or(false);
    if needs_refresh {
        if let Err(e) = do_refresh_token(account).await {
            return Err(AccountsError::Message(format!(
                "failed to refresh account {}: {}",
                account.id, e
            )));
        }
    }
    if let Err(e) = do_write_token_file(account) {
        return Err(AccountsError::Message(format!(
            "failed to write native agy token for account {}: {}",
            account.id, e
        )));
    }
    Ok(())
}

pub async fn remove_account(target: &str) -> Result<crate::storage::Account, AccountsError> {
    let pool = crate::storage::load_pool()?;
    let found = match find_account_by_target(&pool.accounts, target) {
        Some(a) => a.clone(),
        None => {
            return Err(AccountsError::Message(format!("account '{}' not found", target)));
        }
    };

    let mut was_active = false;
    let mut replacement = None;
    if pool.active_account_id.as_deref() == Some(&found.id) {
        was_active = true;
        for candidate in &pool.accounts {
            if candidate.id != found.id {
                replacement = Some(candidate.clone());
                break;
            }
        }
        if let Some(mut rep) = replacement.clone() {
            if let Err(e) = sync_account_to_agy(&mut rep).await {
                return Err(AccountsError::Message(format!(
                    "cannot remove active account: replacement synchronization failed: {}",
                    e
                )));
            }
        }
    }

    let mut removed = None;
    let target_owned = target.to_string();
    let err = crate::storage::pool_transaction(|p| {
        let idx = match resolve_account_index(&p.accounts, &target_owned) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target_owned
                )));
            }
        };
        let rem = p.accounts.remove(idx);
        if p.active_account_id.as_deref() == Some(&rem.id) {
            if !p.accounts.is_empty() {
                p.active_account_id = Some(p.accounts[0].id.clone());
            } else {
                p.active_account_id = None;
            }
        }
        removed = Some(rem);
        Ok(())
    });

    if let Err(e) = err {
        if was_active && replacement.is_some() {
            let mut f = found.clone();
            let _ = sync_account_to_agy(&mut f).await;
        }
        return Err(e.into());
    }

    Ok(removed.unwrap_or(found))
}

pub async fn switch_account(target: &str) -> Result<crate::storage::Account, AccountsError> {
    let mut pool = crate::storage::load_pool()?;
    if pool.accounts.is_empty() {
        return Err(AccountsError::Message("account pool is empty".to_string()));
    }

    let is_auto = target.is_empty() || target == "auto";
    if is_auto {
        if let Some((prober, _)) = get_quota_prober() {
            for acc in &mut pool.accounts {
                if acc.status == crate::storage::STATUS_DISABLED {
                    continue;
                }
                if let Ok(Some(q)) = prober(acc) {
                    acc.last_quota = Some(q);
                    let _ = crate::auth::persist_account_fields(acc, true, true, true);
                }
            }
            pool = crate::storage::load_pool()?;
        }
    }

    let selected = if is_auto {
        let now_ts = crate::clock::now_unix();
        let score = |a: &crate::storage::Account| -> (i32, f64) {
            if a.status == crate::storage::STATUS_VALIDATION_REQUIRED
                || a.status == crate::storage::STATUS_VERIFICATION_PENDING
                || a.status == crate::storage::STATUS_AUTH_ERROR
                || a.status == crate::storage::STATUS_DISABLED
            {
                return (-2, 0.0);
            }
            if let Some(rlu) = a.rate_limited_until {
                if rlu > now_ts {
                    return (-1, 0.0);
                }
            }
            let frac = a.last_quota.as_ref().and_then(|q| q.remaining_fraction).unwrap_or(0.0);
            (1, frac)
        };

        let mut best = pool.accounts[0].clone();
        for candidate in &pool.accounts[1..] {
            let (best_tier, best_frac) = score(&best);
            let (cand_tier, cand_frac) = score(candidate);
            if cand_tier > best_tier || (cand_tier == best_tier && cand_frac > best_frac) {
                best = candidate.clone();
            }
        }
        best
    } else {
        // An exact id wins over list positions and name matches (the dashboard always passes ids).
        match pool
            .accounts
            .iter()
            .find(|a| a.id == target)
            .or_else(|| find_account_by_target(&pool.accounts, target))
        {
            Some(a) => a.clone(),
            None => {
                return Err(AccountsError::Message(format!("account '{}' not found", target)));
            }
        }
    };

    let previous = pool
        .active_account_id
        .as_ref()
        .and_then(|aid| find_account_by_target(&pool.accounts, aid).cloned());

    let mut selected_to_sync = selected.clone();
    sync_account_to_agy(&mut selected_to_sync).await?;

    let selected_id = selected.id.clone();
    let err = crate::storage::pool_transaction(|p| {
        if !p.accounts.iter().any(|a| a.id == selected_id) {
            return Err(crate::storage::StorageError::Other(format!(
                "account '{}' no longer exists",
                selected_id
            )));
        }
        p.active_account_id = Some(selected_id.clone());
        Ok(())
    });

    if let Err(e) = err {
        if let Some(mut prev) = previous {
            let _ = sync_account_to_agy(&mut prev).await;
        }
        return Err(e.into());
    }

    Ok(selected)
}

pub fn rename_account(target: &str, new_name: &str) -> Result<crate::storage::Account, AccountsError> {
    let name = new_name.trim();
    if name.is_empty() {
        return Err(AccountsError::Message("account name cannot be empty".to_string()));
    }

    let mut updated = None;
    let target_owned = target.to_string();
    let name_owned = name.to_string();
    crate::storage::pool_transaction(|pool| {
        let idx = match resolve_account_index(&pool.accounts, &target_owned) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target_owned
                )));
            }
        };
        pool.accounts[idx].name = name_owned;
        updated = Some(pool.accounts[idx].clone());
        Ok(())
    })?;

    updated.ok_or_else(|| AccountsError::Message("rename failed".to_string()))
}

pub fn disable_account(target: &str) -> Result<crate::storage::Account, AccountsError> {
    let mut updated = None;
    let target_owned = target.to_string();
    crate::storage::pool_transaction(|pool| {
        let idx = match resolve_account_index(&pool.accounts, &target_owned) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target_owned
                )));
            }
        };
        pool.accounts[idx].status = crate::storage::STATUS_DISABLED.to_string();
        updated = Some(pool.accounts[idx].clone());
        Ok(())
    })?;

    updated.ok_or_else(|| AccountsError::Message("disable failed".to_string()))
}

pub fn enable_account(target: &str) -> Result<crate::storage::Account, AccountsError> {
    let mut updated = None;
    let target_owned = target.to_string();
    crate::storage::pool_transaction(|pool| {
        let idx = match resolve_account_index(&pool.accounts, &target_owned) {
            Some(i) => i,
            None => {
                return Err(crate::storage::StorageError::Other(format!(
                    "account '{}' not found",
                    target_owned
                )));
            }
        };
        if pool.accounts[idx].status == crate::storage::STATUS_DISABLED {
            pool.accounts[idx].status.clear();
        }
        updated = Some(pool.accounts[idx].clone());
        Ok(())
    })?;

    updated.ok_or_else(|| AccountsError::Message("enable failed".to_string()))
}

pub async fn import_current() -> Result<crate::storage::Account, AccountsError> {
    let token_file = crate::config::agy_token_file();
    crate::config::assert_safe_read_path(&token_file)?;

    let data = std::fs::read(&token_file)
        .map_err(|e| AccountsError::Message(format!("no existing token file found: {}", e)))?;

    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        token: RawToken,
        #[serde(default)]
        id_token: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawToken {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: String,
    }

    let raw: Raw = serde_json::from_slice(&data)
        .map_err(|e| AccountsError::Message(format!("failed to parse token file: {}", e)))?;
    if raw.token.refresh_token.is_empty() {
        return Err(AccountsError::Message(
            "no refresh_token found in existing token file".to_string(),
        ));
    }

    let claims = crate::auth::decode_jwt_payload(&raw.id_token);
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("primary_user@gmail.com")
        .to_string();

    let now = crate::clock::now_unix() as i64;
    let zero_expiry: f64 = 0.0;
    let mut imported = None;

    crate::storage::pool_transaction(|pool| {
        for a in &mut pool.accounts {
            if a.email == email {
                a.refresh_token = raw.token.refresh_token.clone();
                a.access_token = raw.token.access_token.clone();
                a.token_expiry = Some(zero_expiry);
                a.id_token = raw.id_token.clone();
                imported = Some(a.clone());
                return Ok(());
            }
        }

        let acc = crate::storage::Account {
            id: crate::storage::next_account_id(&pool.accounts),
            email: email.clone(),
            refresh_token: raw.token.refresh_token.clone(),
            access_token: raw.token.access_token.clone(),
            token_expiry: Some(zero_expiry),
            id_token: raw.id_token.clone(),
            created_at: Some(now),
            request_count: 0,
            error_count: 0,
            ..Default::default()
        };
        pool.accounts.push(acc.clone());
        if pool.active_account_id.is_none() {
            pool.active_account_id = Some(acc.id.clone());
        }
        imported = Some(acc);
        Ok(())
    })?;

    let mut imp = imported.unwrap();
    if let Some((prober, _)) = get_quota_prober() {
        if let Ok(Some(q)) = prober(&mut imp) {
            imp.last_quota = Some(q);
            let _ = crate::auth::persist_account_fields(&imp, true, false, true);
        }
    }

    Ok(imp)
}

pub type PromptFn = Box<dyn Fn(&str) -> Result<String, AccountsError> + Send + Sync>;
pub type BrowserOpenerFn = Box<dyn Fn(&str) -> Result<(), AccountsError> + Send + Sync>;

pub struct LoginOptions {
    pub auth_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub prompt_fn: Option<PromptFn>,
    pub browser_opener: Option<BrowserOpenerFn>,
    pub port_range: Option<[u16; 2]>,
    pub timeout: Option<std::time::Duration>,
    pub stdout: Option<std::sync::Arc<std::sync::Mutex<dyn std::io::Write + Send>>>,
}

pub fn default_browser_opener(target_url: &str) -> Result<(), AccountsError> {
    #[cfg(test)]
    {
        let _ = target_url;
        panic!("default_browser_opener called in test");
    }
    #[cfg(not(test))]
    {
        for opener in &["termux-open-url", "termux-open", "xdg-open", "open"] {
            if *opener == "xdg-open" || *opener == "open" {
                if cfg!(target_os = "linux")
                    && std::env::var("DISPLAY").is_err()
                    && std::env::var("WAYLAND_DISPLAY").is_err()
                {
                    continue;
                }
            }
            if let Ok(mut child) = std::process::Command::new(opener)
                .arg(target_url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let start = std::time::Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if status.success() {
                                return Ok(());
                            }
                            break;
                        }
                        Ok(None) => {
                            if start.elapsed() >= std::time::Duration::from_millis(100) {
                                return Ok(());
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        Err(AccountsError::Message("no browser opener found".to_string()))
    }
}

fn find_free_port(start: u16, end: u16) -> Result<u16, AccountsError> {
    for p in start..=end {
        if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", p)) {
            drop(l);
            return Ok(p);
        }
    }
    Err(AccountsError::Message(format!(
        "no free port available in range {}-{}",
        start, end
    )))
}

pub async fn login(opts: LoginOptions) -> Result<crate::storage::Account, AccountsError> {
    let auth_endpoint = opts
        .auth_endpoint
        .unwrap_or_else(|| "https://accounts.google.com/o/oauth2/v2/auth".to_string());
    let token_endpoint = opts.token_endpoint.unwrap_or_else(crate::auth::token_url);
    let [start_port, end_port] = opts.port_range.unwrap_or([8085, 8135]);
    let timeout_duration = opts.timeout.unwrap_or(std::time::Duration::from_secs(180));

    let port = find_free_port(start_port, end_port)?;
    let redirect_uri = format!("http://localhost:{}/auth/callback", port);

    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &crate::auth::client_id())
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", crate::auth::OAUTH_SCOPES)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .finish();
    let auth_url = format!("{}?{}", auth_endpoint, params);

    let (code_tx, mut code_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (err_tx, mut err_rx) = tokio::sync::mpsc::channel::<AccountsError>(1);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| AccountsError::Message(format!("failed to listen on 127.0.0.1:{}: {}", port, e)))?;

    let listener_code_tx = code_tx.clone();
    let listener_err_tx = err_tx.clone();
    let server_handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(bytes) if bytes > 0 => bytes,
                _ => continue,
            };
            let req_str = String::from_utf8_lossy(&buf[..n]);
            let first_line = req_str.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let target_uri = parts[1];
            if target_uri.starts_with("/auth/callback") {
                let query = target_uri.split_once('?').map(|x| x.1).unwrap_or("");
                let mut code = None;
                let mut error = None;
                for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
                    if k == "code" {
                        code = Some(v.into_owned());
                    } else if k == "error" {
                        error = Some(v.into_owned());
                    }
                }
                if let Some(c) = code {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: 172\r\n\r\n<html><body style=\"font-family:sans-serif;text-align:center;padding:40px;background:#0f172a;color:#f8fafc;\"><h2>Authorization Successful!</h2><p>Antigravity multi-account token has been saved.</p></body></html>";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = listener_code_tx.send(c).await;
                    break;
                } else if let Some(e) = error {
                    let body = format!("Authorization failed: {}", e);
                    let resp = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = listener_err_tx
                        .send(AccountsError::Message(format!("OAuth callback error: {}", e)))
                        .await;
                    break;
                } else {
                    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            }
        }
    });

    let opener = opts.browser_opener.as_ref();
    let opener_res = match opener {
        Some(op) => op(&auth_url),
        None => default_browser_opener(&auth_url),
    };

    if let Some(ref stdout_arc) = opts.stdout {
        let mut out = stdout_arc.lock().unwrap_or_else(|e| e.into_inner());
        if opener_res.is_ok() {
            let _ = writeln!(out, "Opening browser for Google authentication...");
            let _ = writeln!(out, "Waiting for authorization...");
        } else {
            let _ = writeln!(out, "Could not open a browser automatically.");
            let _ = writeln!(out, "Open this URL:");
            let _ = writeln!(out, "{}", auth_url);
        }
    }

    if let Some(prompt) = opts.prompt_fn {
        let p_url = auth_url.clone();
        let prompt_code_tx = code_tx.clone();
        let prompt_err_tx = err_tx.clone();
        tokio::spawn(async move {
            match prompt(&p_url) {
                Ok(mut val) => {
                    val = val.trim().to_string();
                    if val.contains("code=") {
                        if let Ok(parsed) = url::Url::parse(&val) {
                            for (k, v) in parsed.query_pairs() {
                                if k == "code" {
                                    val = v.into_owned();
                                    break;
                                }
                            }
                        }
                    }
                    if !val.is_empty() {
                        let _ = prompt_code_tx.send(val).await;
                    }
                }
                Err(e) => {
                    let _ = prompt_err_tx.send(e).await;
                }
            }
        });
    }

    let code = tokio::select! {
        _ = tokio::time::sleep(timeout_duration) => {
            server_handle.abort();
            return Err(AccountsError::Message("login timed out or cancelled".to_string()));
        }
        err = err_rx.recv() => {
            server_handle.abort();
            return Err(err.unwrap_or_else(|| AccountsError::Message("login failed".to_string())));
        }
        c = code_rx.recv() => {
            server_handle.abort();
            c.ok_or_else(|| AccountsError::Message("no code received".to_string()))?
        }
    };

    let client_id_val = crate::auth::client_id();
    let client_secret_val = crate::auth::client_secret();
    let token_form = [
        ("code", code.as_str()),
        ("client_id", client_id_val.as_str()),
        ("client_secret", client_secret_val.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("grant_type", "authorization_code"),
    ];

    let (status, resp_body) = crate::http::post_form(&token_endpoint, &token_form, std::time::Duration::from_secs(10))
        .await
        .map_err(|e| AccountsError::Message(format!("OAuth token exchange failed: {}", e)))?;

    if !(200..300).contains(&status) {
        return Err(AccountsError::Message(format!("OAuth exchange failed HTTP {}", status)));
    }

    #[derive(serde::Deserialize)]
    struct TokenRes {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        id_token: String,
        #[serde(default)]
        expires_in: Option<f64>,
    }

    let token_res: TokenRes = serde_json::from_slice(&resp_body)
        .map_err(|e| AccountsError::Message(format!("failed to parse token response: {}", e)))?;

    if token_res.refresh_token.is_empty() {
        return Err(AccountsError::Message(
            "Google did not return a refresh_token. Please revoke access or re-run with prompt=consent".to_string(),
        ));
    }

    let claims = crate::auth::decode_jwt_payload(&token_res.id_token);
    let now = crate::clock::now_unix() as i64;
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("user_{}@gmail.com", now));

    let expires_in = match token_res.expires_in {
        Some(e) if e > 0.0 => e,
        _ => 3600.0,
    };
    let expiry_ts = (now as f64) + expires_in;

    let mut account = None;
    crate::storage::pool_transaction(|pool| {
        for a in &mut pool.accounts {
            if a.email == email {
                a.refresh_token = token_res.refresh_token.clone();
                a.access_token = token_res.access_token.clone();
                a.token_expiry = Some(expiry_ts);
                a.id_token = token_res.id_token.clone();
                a.updated_at = Some(now);
                a.status.clear();
                a.validation_url = None;
                a.rate_limited_until = None;
                account = Some(a.clone());
                return Ok(());
            }
        }

        let new_acc = crate::storage::Account {
            id: crate::storage::next_account_id(&pool.accounts),
            email: email.clone(),
            refresh_token: token_res.refresh_token.clone(),
            access_token: token_res.access_token.clone(),
            token_expiry: Some(expiry_ts),
            id_token: token_res.id_token.clone(),
            created_at: Some(now),
            updated_at: Some(now),
            request_count: 0,
            error_count: 0,
            ..Default::default()
        };
        pool.accounts.push(new_acc.clone());
        if pool.active_account_id.is_none() {
            pool.active_account_id = Some(new_acc.id.clone());
        }
        account = Some(new_acc);
        Ok(())
    })?;

    let mut acc = account.unwrap();
    if let Some((prober, _)) = get_quota_prober() {
        if let Ok(Some(q)) = prober(&mut acc) {
            acc.last_quota = Some(q);
            let _ = crate::auth::persist_account_fields(&acc, true, true, true);
        }
    }

    if let Err(e) = sync_active_agy_token_file().await {
        return Err(AccountsError::Message(format!(
            "authentication succeeded but native agy synchronization failed: {}",
            e
        )));
    }

    Ok(acc)
}

fn clear_validation_restriction_cas(
    target_acc: &crate::storage::Account,
    snap: &RestrictionSnapshot,
    new_quota: &crate::storage::QuotaState,
) -> Result<VerificationTransitionResult, AccountsError> {
    let cleared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cleared_tx = cleared.clone();
    execute_pool_transaction({
        let target_id = target_acc.id.clone();
        let target_token = target_acc.access_token.clone();
        let snap = snap.clone();
        let new_quota = new_quota.clone();
        move |pool| {
            if let Some(current) = pool.accounts.iter_mut().find(|a| a.id == target_id) {
                if snap.matches(current) {
                    current.status.clear();
                    current.validation_url = None;
                    current.rate_limited_until = None;
                    current.last_quota = Some(new_quota);
                    if !target_token.is_empty() {
                        current.access_token = target_token;
                    }
                    cleared_tx.store(true, std::sync::atomic::Ordering::SeqCst);
                } else {
                    cleared_tx.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(())
            } else {
                Err(AccountsError::Message(format!(
                    "account '{}' does not exist in pool",
                    target_id
                )))
            }
        }
    })?;
    Ok(VerificationTransitionResult {
        cleared: cleared.load(std::sync::atomic::Ordering::SeqCst),
    })
}

/// Persists a validation restriction that a quota probe cleared on `after`, unless the stored account
/// changed since `before` was read. Returns whether the restriction was cleared.
pub fn persist_cleared_restriction(
    before: &crate::storage::Account,
    after: &crate::storage::Account,
    quota: &crate::storage::QuotaState,
) -> Result<bool, AccountsError> {
    clear_validation_restriction_cas(after, &snapshot_account(before), quota).map(|r| r.cleared)
}

pub async fn verify_account(target: &str) -> Result<(crate::storage::Account, String), AccountsError> {
    let pool = crate::storage::load_pool()?;
    if pool.accounts.is_empty() {
        return Err(AccountsError::Message("account pool is empty".to_string()));
    }

    let target_acc = if !target.is_empty() {
        find_account_by_target(&pool.accounts, target).cloned()
    } else {
        pool.accounts
            .iter()
            .find(|a| a.status == crate::storage::STATUS_VALIDATION_REQUIRED)
            .cloned()
            .or_else(|| pool.accounts.first().cloned())
    };

    let mut target_acc = target_acc.ok_or_else(|| AccountsError::Message("account not found".to_string()))?;

    let was_validation_required = target_acc.status == crate::storage::STATUS_VALIDATION_REQUIRED;
    let snap = if was_validation_required {
        Some(snapshot_account(&target_acc))
    } else {
        None
    };

    if let Some((prober, _)) = get_quota_prober() {
        let orig_url = target_acc.validation_url.clone();
        let orig_rlu = target_acc.rate_limited_until;

        if was_validation_required {
            target_acc.status.clear();
        }

        let probe_res = prober(&mut target_acc);
        match probe_res {
            Err(_) | Ok(None) => {
                if was_validation_required {
                    target_acc.status = crate::storage::STATUS_VALIDATION_REQUIRED.to_string();
                    target_acc.validation_url = orig_url;
                    target_acc.rate_limited_until = orig_rlu;
                }
            }
            Ok(Some(q)) => {
                target_acc.last_quota = Some(q.clone());
                if was_validation_required && target_acc.status.is_empty() {
                    target_acc.status.clear();
                    target_acc.validation_url = None;
                    target_acc.rate_limited_until = None;

                    if let Some(hook) = {
                        let r = BEFORE_CLEAR_HOOK.read().unwrap_or_else(|e| e.into_inner());
                        r.clone()
                    } {
                        hook(&target_acc.id);
                    }

                    if is_account_fields_persister_replaced() {
                        let persister = {
                            let r = ACCOUNT_FIELDS_PERSISTER.read().unwrap_or_else(|e| e.into_inner());
                            r.clone()
                        };
                        if let Some(p_fn) = persister {
                            p_fn(&target_acc, true, true, true).map_err(|e| {
                                AccountsError::Message(format!("failed to persist account verification status: {}", e))
                            })?;
                        }
                    }

                    let _ = clear_validation_restriction_cas(&target_acc, snap.as_ref().unwrap(), &q).map_err(|e| {
                        AccountsError::Message(format!("failed to persist account verification status: {}", e))
                    })?;
                } else {
                    let persister = {
                        let r = ACCOUNT_FIELDS_PERSISTER.read().unwrap_or_else(|e| e.into_inner());
                        r.clone()
                    };
                    if let Some(p_fn) = persister {
                        p_fn(&target_acc, true, true, true).map_err(|e| {
                            AccountsError::Message(format!("failed to persist account verification status: {}", e))
                        })?;
                    } else {
                        crate::auth::persist_account_fields(&target_acc, true, true, true).map_err(|e| {
                            AccountsError::Message(format!("failed to persist account verification status: {}", e))
                        })?;
                    }
                }
            }
        }
    }

    let refreshed_pool = crate::storage::load_pool()?;
    let current_acc = refreshed_pool
        .accounts
        .iter()
        .find(|a| a.id == target_acc.id)
        .cloned()
        .unwrap_or(target_acc);

    if current_acc.status != crate::storage::STATUS_VALIDATION_REQUIRED {
        return Ok((current_acc, String::new()));
    }

    let validation_url = match current_acc.validation_url {
        Some(ref u) if !u.is_empty() => u.clone(),
        _ => {
            return Err(AccountsError::Message(
                "Google did not return a validation URL".to_string(),
            ));
        }
    };

    let mut v_url = validation_url;
    if !current_acc.email.is_empty() && !v_url.contains("&Email=") && !v_url.contains("&login_hint=") {
        let sep = if !v_url.contains('?') { "?" } else { "&" };
        let escaped: String = url::form_urlencoded::byte_serialize(current_acc.email.as_bytes()).collect();
        v_url = format!("{}{}Email={}", v_url, sep, escaped);
    }

    Ok((current_acc, v_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
            crate::config::reset_native_agy_dir();
        }
    }

    fn setup_isolated_test_dir() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let native_dir = dir.path().join("native_gemini");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&native_dir).unwrap();
        crate::config::configure_data_dir(&data_dir).unwrap();
        crate::config::set_native_agy_dir(&native_dir);
        TestEnv { _lock: lock, _dir: dir }
    }

    #[test]
    fn test_display_account_name() {
        let _env = setup_isolated_test_dir();

        // 1. Account with explicit Name
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Work Account".to_string(),
            email: "work@example.com".to_string(),
            ..Default::default()
        };
        assert_eq!(display_account_name(Some(&acc1), Some(false)), "Work Account");
        assert_eq!(
            display_account_name(Some(&acc1), Some(true)),
            "Work Account (work@example.com)"
        );

        // 2. Account with empty Name fallback to acc_N
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "  ".to_string(),
            email: "user2@gmail.com".to_string(),
            ..Default::default()
        };
        assert_eq!(display_account_name(Some(&acc2), Some(false)), "Account 2");
        assert_eq!(
            display_account_name(Some(&acc2), Some(true)),
            "Account 2 (user2@gmail.com)"
        );

        // 3. Fallback for non-standard ID
        let acc3 = crate::storage::Account {
            id: "custom_id".to_string(),
            name: "".to_string(),
            email: "custom@corp.com".to_string(),
            ..Default::default()
        };
        assert_eq!(display_account_name(Some(&acc3), Some(false)), "Account");
        assert_eq!(
            display_account_name(Some(&acc3), Some(true)),
            "Account (custom@corp.com)"
        );

        // 4. nil account
        assert_eq!(display_account_name(None, Some(false)), "Account");

        // 5. Env overrides
        unsafe {
            std::env::set_var("AGY_SHOW_EMAIL", "true");
        }
        assert_eq!(
            display_account_name(Some(&acc1), None),
            "Work Account (work@example.com)"
        );
        unsafe {
            std::env::remove_var("AGY_SHOW_EMAIL");
            std::env::set_var("AGY_PRIVACY", "false");
        }
        assert_eq!(
            display_account_name(Some(&acc1), None),
            "Work Account (work@example.com)"
        );
        unsafe {
            std::env::remove_var("AGY_PRIVACY");
        }
    }

    #[test]
    fn test_find_account_by_target() {
        let accs = vec![
            crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Alpha".to_string(),
                email: "alpha@corp.com".to_string(),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_2".to_string(),
                name: "Beta".to_string(),
                email: "beta@corp.com".to_string(),
                ..Default::default()
            },
        ];

        assert_eq!(find_account_by_target(&accs, "1").map(|a| a.id.as_str()), Some("acc_1"));
        assert_eq!(
            find_account_by_target(&accs, "acc_2").map(|a| a.name.as_str()),
            Some("Beta")
        );
        assert_eq!(
            find_account_by_target(&accs, "beta@corp.com").map(|a| a.id.as_str()),
            Some("acc_2")
        );
        assert_eq!(find_account_by_target(&accs, "nonexistent"), None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_write_and_sync_agy_token_file() {
        let _env = setup_isolated_test_dir();

        let expiry = crate::clock::now_unix() + 3600.0;
        let mut acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            email: "main@example.com".to_string(),
            access_token: "at_1".to_string(),
            refresh_token: "rf_1".to_string(),
            id_token: "id_1".to_string(),
            token_expiry: Some(expiry),
            ..Default::default()
        };

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc.clone()];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        write_agy_token_file(&acc).unwrap();

        let token_file = crate::config::agy_token_file();
        assert!(token_file.exists());

        let synced = sync_active_agy_token_file().await.unwrap();
        assert!(synced);

        // Near-expiry refresh triggers refresher
        acc.token_expiry = Some(crate::clock::now_unix() + 60.0);
        pool.accounts[0] = acc.clone();
        crate::storage::save_pool(&pool).unwrap();

        let refreshed_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rc = refreshed_called.clone();
        let _g = set_token_refresher_for_test(Some(Box::new(move |a| {
            rc.store(true, std::sync::atomic::Ordering::SeqCst);
            a.access_token = "at_refreshed".to_string();
            a.token_expiry = Some(crate::clock::now_unix() + 3600.0);
            Box::pin(async { Ok("at_refreshed".to_string()) })
        })));

        let synced = sync_active_agy_token_file().await.unwrap();
        assert!(synced);
        assert!(refreshed_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_sync_active_agy_token_file_refresh_failure_leaves_file_unchanged() {
        let _env = setup_isolated_test_dir();

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "initial-token".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 10.0), // needs refresh
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc.clone()];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();
        write_agy_token_file(&acc).unwrap();

        let _rg = set_token_refresher_for_test(Some(Box::new(|_| {
            Box::pin(async { Err(crate::auth::AuthError::Message("injected refresh error".to_string())) })
        })));

        let writer_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wc = writer_called.clone();
        let _wg = set_token_file_writer_for_test(Some(Box::new(move |_| {
            wc.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })));

        let res = sync_active_agy_token_file().await;
        assert!(res.is_err());
        assert!(!writer_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_sync_active_agy_token_file_writer_failure() {
        let _env = setup_isolated_test_dir();

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let _wg = set_token_file_writer_for_test(Some(Box::new(|_| {
            Err(AccountsError::Message("injected writer failure".to_string()))
        })));

        let res = sync_active_agy_token_file().await;
        assert!(res.is_err());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_switch_account_refresh_failure_keeps_previous_active() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            access_token: "".to_string(), // needs refresh
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let _rg = set_token_refresher_for_test(Some(Box::new(|_| {
            Box::pin(async { Err(crate::auth::AuthError::Message("refresh failed".to_string())) })
        })));

        let res = switch_account("acc_2").await;
        assert!(res.is_err());

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_switch_account_writer_failure_keeps_previous_active() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            access_token: "tok2".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let _wg = set_token_file_writer_for_test(Some(Box::new(|_| {
            Err(AccountsError::Message("injected writer failure".to_string()))
        })));

        let res = switch_account("acc_2").await;
        assert!(res.is_err());

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_remove_account_replacement_sync_failure_keeps_pool() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            access_token: "".to_string(), // replacement needs refresh
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let _rg = set_token_refresher_for_test(Some(Box::new(|_| {
            Box::pin(async { Err(crate::auth::AuthError::Message("refresh failed".to_string())) })
        })));

        let res = remove_account("acc_1").await;
        assert!(res.is_err());

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_remove_account() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            access_token: "tok2".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let rem = remove_account("acc_2").await.unwrap();
        assert_eq!(rem.id, "acc_2");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_switch_account() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "tok1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            access_token: "tok2".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc1, acc2];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let switched = switch_account("acc_2").await.unwrap();
        assert_eq!(switched.id, "acc_2");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_2"));
    }

    #[test]
    fn test_rename_account() {
        let _env = setup_isolated_test_dir();

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Old Name".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let updated = rename_account("acc_1", "New Name").unwrap();
        assert_eq!(updated.name, "New Name");

        assert!(rename_account("acc_1", "  ").is_err());
    }

    #[test]
    fn test_disable_and_enable_account() {
        let _env = setup_isolated_test_dir();

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let dis = disable_account("acc_1").unwrap();
        assert_eq!(dis.status, crate::storage::STATUS_DISABLED);

        let en = enable_account("acc_1").unwrap();
        assert!(en.status.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_import_current() {
        let _env = setup_isolated_test_dir();

        let mock_jwt = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ICJpbXBvcnRfdXNlckBnbWFpbC5jb20ifQ.";
        let payload = serde_json::json!({
            "token": {
                "access_token": "import-at",
                "refresh_token": "import-rf"
            },
            "id_token": mock_jwt
        });
        let token_file = crate::config::agy_token_file();
        std::fs::create_dir_all(token_file.parent().unwrap()).unwrap();
        std::fs::write(&token_file, serde_json::to_vec(&payload).unwrap()).unwrap();

        let acc = import_current().await.unwrap();
        assert_eq!(acc.email, "import_user@gmail.com");
        assert_eq!(acc.refresh_token, "import-rf");
        assert_eq!(acc.access_token, "import-at");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_login_flow() {
        let _env = setup_isolated_test_dir();

        let server = crate::testutil::TestServer::start(|req| {
            if req.form_value("code").as_deref() != Some("valid-test-code") {
                return (400, vec![], b"bad code".to_vec());
            }
            let mock_jwt = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ICJsb2dpbl91c2VyQGdtYWlsLmNvbSJ9.";
            let resp = serde_json::json!({
                "access_token": "login-at-123",
                "refresh_token": "login-rf-456",
                "id_token": mock_jwt,
                "expires_in": 3600
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });

        let opts = LoginOptions {
            token_endpoint: Some(server.url()),
            browser_opener: Some(Box::new(|_| Ok(()))),
            prompt_fn: Some(Box::new(|_| Ok("valid-test-code".to_string()))),
            port_range: Some([18085, 18135]),
            timeout: Some(std::time::Duration::from_secs(5)),
            auth_endpoint: None,
            stdout: None,
        };

        let acc = login(opts).await.unwrap();
        assert_eq!(acc.email, "login_user@gmail.com");
        assert_eq!(acc.access_token, "login-at-123");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_login_native_sync_failure_is_observable() {
        let _env = setup_isolated_test_dir();

        let _wg = set_token_file_writer_for_test(Some(Box::new(|_| {
            Err(AccountsError::Message("injected native writer failure".to_string()))
        })));

        let server = crate::testutil::TestServer::start(|_| {
            let resp = serde_json::json!({
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "id_token": "",
                "expires_in": 3600
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });

        let opts = LoginOptions {
            token_endpoint: Some(server.url()),
            browser_opener: Some(Box::new(|_| Ok(()))),
            prompt_fn: Some(Box::new(|_| Ok("code".to_string()))),
            port_range: Some([18600, 18650]),
            timeout: Some(std::time::Duration::from_secs(2)),
            auth_endpoint: None,
            stdout: None,
        };

        let err = login(opts).await.unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("native agy synchronization failed"));
        assert!(!err_str.contains("access-secret"));
        assert!(!err_str.contains("refresh-secret"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_clears_validation_required_on_success() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| {
                Ok(Some(crate::storage::QuotaState {
                    remaining_fraction: Some(0.9),
                    ..Default::default()
                }))
            })),
            None,
        );

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/verify".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, url) = verify_account("acc_1").await.unwrap();
        assert!(verified.status.is_empty());
        assert!(url.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_does_not_clear_auth_error() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| {
                Ok(Some(crate::storage::QuotaState {
                    remaining_fraction: Some(0.9),
                    ..Default::default()
                }))
            })),
            None,
        );

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_AUTH_ERROR.to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, _) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_AUTH_ERROR);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_preserves_restriction_when_validation_still_required() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|a| {
                a.status = crate::storage::STATUS_VALIDATION_REQUIRED.to_string();
                Ok(Some(crate::storage::QuotaState::default()))
            })),
            None,
        );

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, url) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_VALIDATION_REQUIRED);
        assert!(!url.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_preserves_restriction_on_probe_error() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| Err(AccountsError::Message("probe failed".to_string())))),
            None,
        );

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, _) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_VALIDATION_REQUIRED);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_persistence_failure_returns_error() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| Ok(Some(crate::storage::QuotaState::default())))),
            None,
        );
        let _pg = set_account_fields_persister(Some(Box::new(|_, _, _, _| {
            Err(AccountsError::Message("persist error".to_string()))
        })));

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let err = verify_account("acc_1").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to persist account verification status")
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_does_not_clear_newer_validation_restriction() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| Ok(Some(crate::storage::QuotaState::default())))),
            None,
        );
        let _bg = set_before_clear_hook(Some(Box::new(|id| {
            // Concurrent change updates validation URL
            let mut p = crate::storage::load_pool().unwrap();
            if let Some(a) = p.accounts.iter_mut().find(|a| a.id == id) {
                a.validation_url = Some("https://example.com/newer_val".to_string());
                crate::storage::save_pool(&p).unwrap();
            }
        })));

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val_initial".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, _) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_VALIDATION_REQUIRED);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_does_not_clear_concurrent_auth_error() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| Ok(Some(crate::storage::QuotaState::default())))),
            None,
        );
        let _bg = set_before_clear_hook(Some(Box::new(|id| {
            let mut p = crate::storage::load_pool().unwrap();
            if let Some(a) = p.accounts.iter_mut().find(|a| a.id == id) {
                a.status = crate::storage::STATUS_AUTH_ERROR.to_string();
                crate::storage::save_pool(&p).unwrap();
            }
        })));

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, _) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_AUTH_ERROR);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_does_not_clear_after_credential_generation_change() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|_| Ok(Some(crate::storage::QuotaState::default())))),
            None,
        );
        let _bg = set_before_clear_hook(Some(Box::new(|id| {
            let mut p = crate::storage::load_pool().unwrap();
            if let Some(a) = p.accounts.iter_mut().find(|a| a.id == id) {
                a.refresh_token = "new_refresh_token".to_string();
                crate::storage::save_pool(&p).unwrap();
            }
        })));

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            refresh_token: "old_refresh_token".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://example.com/val".to_string()),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (verified, _) = verify_account("acc_1").await.unwrap();
        assert_eq!(verified.status, crate::storage::STATUS_VALIDATION_REQUIRED);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_verify_account_allows_self_access_token_refresh() {
        let _env = setup_isolated_test_dir();

        let _g = set_quota_prober(
            Some(Box::new(|a| {
                a.access_token = "access_token_new".to_string();
                Ok(Some(crate::storage::QuotaState {
                    remaining_fraction: Some(0.85),
                    ..Default::default()
                }))
            })),
            None,
        );

        let url_a = "https://accounts.google.com/verify?id=val_A".to_string();
        let cooldown_a = crate::clock::now_unix() + 86400.0;
        let acc = crate::storage::Account {
            id: "acc_self_refresh".to_string(),
            email: "self_refresh@gmail.com".to_string(),
            access_token: "access_token_old".to_string(),
            refresh_token: "refresh_token_R".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some(url_a),
            rate_limited_until: Some(cooldown_a),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        crate::storage::save_pool(&pool).unwrap();

        let (res_acc, formatted_url) = verify_account("acc_self_refresh").await.unwrap();
        assert!(formatted_url.is_empty());
        assert!(res_acc.status.is_empty());
        assert!(res_acc.validation_url.is_none());
        assert!(res_acc.rate_limited_until.is_none());

        let stored_pool = crate::storage::load_pool().unwrap();
        let stored = stored_pool
            .accounts
            .iter()
            .find(|a| a.id == "acc_self_refresh")
            .unwrap();
        assert!(stored.status.is_empty());
        assert_eq!(stored.access_token, "access_token_new");
        assert_eq!(stored.refresh_token, "refresh_token_R");
    }

    #[test]
    fn test_native_agy_environment_independence() {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);

        let tmp1 = tempfile::tempdir().unwrap();
        let data_dir1 = tmp1.path().join("data");
        let native_dir1 = tmp1.path().join("native");
        std::fs::create_dir_all(&data_dir1).unwrap();
        std::fs::create_dir_all(&native_dir1).unwrap();
        crate::config::configure_data_dir(&data_dir1).unwrap();
        crate::config::set_native_agy_dir(&native_dir1);

        // 1. Absent token file
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err1 = rt.block_on(import_current()).unwrap_err();
        assert!(err1.to_string().contains("no existing token file found"));

        let acc = crate::storage::Account {
            id: "acc_indep".to_string(),
            email: "indep@example.com".to_string(),
            access_token: "at-indep".to_string(),
            refresh_token: "rf-indep".to_string(),
            id_token: "id-indep".to_string(),
            ..Default::default()
        };
        write_agy_token_file(&acc).unwrap();
        let tok_bytes1 = std::fs::read(crate::config::agy_token_file()).unwrap();

        // Condition 2
        let tmp2 = tempfile::tempdir().unwrap();
        let data_dir2 = tmp2.path().join("data");
        let native_dir2 = tmp2.path().join("native");
        std::fs::create_dir_all(&data_dir2).unwrap();
        std::fs::create_dir_all(&native_dir2).unwrap();
        crate::config::configure_data_dir(&data_dir2).unwrap();
        crate::config::set_native_agy_dir(&native_dir2);

        let err2 = rt.block_on(import_current()).unwrap_err();
        assert!(err2.to_string().contains("no existing token file found"));

        write_agy_token_file(&acc).unwrap();
        let tok_bytes2 = std::fs::read(crate::config::agy_token_file()).unwrap();

        let m1: serde_json::Value = serde_json::from_slice(&tok_bytes1).unwrap();
        let m2: serde_json::Value = serde_json::from_slice(&tok_bytes2).unwrap();
        assert_eq!(m1["auth_method"], m2["auth_method"]);

        crate::config::reset_data_dir();
        crate::config::reset_native_agy_dir();
        crate::config::set_test_mode(false);
        drop(lock);
    }

    #[test]
    fn test_home_override_regression() {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);

        let fake_home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let real_prod = crate::config::real_production_gemini_dir();
        if !real_prod.as_os_str().is_empty() {
            assert!(!real_prod.starts_with(fake_home.path()));
            let real_token = real_prod.join("antigravity-cli").join("antigravity-oauth-token");
            assert!(crate::config::assert_safe_write_path(&real_token).is_err());
        }

        let synthetic_gemini = fake_home.path().join(".gemini");
        crate::config::set_native_agy_dir(&synthetic_gemini);
        assert_eq!(crate::config::native_agy_dir(), synthetic_gemini);
        let expected_token = synthetic_gemini.join("antigravity-cli").join("antigravity-oauth-token");
        assert_eq!(crate::config::agy_token_file(), expected_token);

        crate::config::reset_native_agy_dir();
        unsafe {
            if let Some(h) = orig_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
        crate::config::set_test_mode(false);
        drop(lock);
    }

    #[test]
    fn test_symlink_to_production_rejected() {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);

        let real_prod = crate::config::real_production_gemini_dir();
        if real_prod.as_os_str().is_empty() {
            crate::config::set_test_mode(false);
            return;
        }

        let tmp_dir = tempfile::tempdir().unwrap();
        let real_token = real_prod.join("antigravity-cli").join("antigravity-oauth-token");
        let symlink_file = tmp_dir.path().join("symlink-token");
        if std::os::unix::fs::symlink(&real_token, &symlink_file).is_ok() {
            let write_err = crate::config::assert_safe_write_path(&symlink_file).unwrap_err();
            assert!(
                write_err
                    .to_string()
                    .contains("refusing write to protected production path")
            );

            let read_err = crate::config::assert_safe_read_path(&symlink_file).unwrap_err();
            assert!(
                read_err
                    .to_string()
                    .contains("refusing read from protected production path in test mode")
            );
        }

        crate::config::set_test_mode(false);
        drop(lock);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_remove_account_repairs_active() {
        let _env = setup_isolated_test_dir();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "first@example.com".to_string(),
            access_token: "token-acc-1".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            email: "second@example.com".to_string(),
            access_token: "token-acc-2".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let acc3 = crate::storage::Account {
            id: "acc_3".to_string(),
            email: "third@example.com".to_string(),
            access_token: "token-acc-3".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut p = crate::storage::Pool::new_empty();
        p.accounts = vec![acc1, acc2, acc3];
        p.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&p).unwrap();

        // 2. Remove middle account (acc_2)
        let rem = remove_account("acc_2").await.unwrap();
        assert_eq!(rem.id, "acc_2");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));

        // 3. Remove active account (acc_1)
        let rem = remove_account("acc_1").await.unwrap();
        assert_eq!(rem.id, "acc_1");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_3"));

        // 4. Remove final account (acc_3)
        let rem = remove_account("acc_3").await.unwrap();
        assert_eq!(rem.id, "acc_3");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 0);
        assert_eq!(loaded.active_account_id, None);
    }

    #[test]
    fn test_import_replace_atomicity() {
        let _env = setup_isolated_test_dir();

        let mut p = crate::storage::Pool::new_empty();
        p.accounts = vec![
            crate::storage::Account {
                id: "acc_1".to_string(),
                email: "keep1@example.com".to_string(),
                access_token: "tok1".to_string(),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_2".to_string(),
                email: "keep2@example.com".to_string(),
                access_token: "tok2".to_string(),
                ..Default::default()
            },
        ];
        p.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&p).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let bad_backup = tmp.path().join("bad_backup.json");
        std::fs::write(&bad_backup, b"NOT VALID JSON BACKUP").unwrap();

        let res = import_pool(bad_backup.to_str().unwrap(), "", true, false);
        assert!(res.is_err());

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].email, "keep1@example.com");
        assert_eq!(loaded.accounts[1].email, "keep2@example.com");
        assert_eq!(loaded.active_account_id.as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_login_status_output() {
        let _env = setup_isolated_test_dir();

        let server = crate::testutil::TestServer::start(|_| {
            let mock_jwt = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6ICJzdGF0dXNfdXNlckBnbWFpbC5jb20ifQ.";
            let resp = serde_json::json!({
                "access_token": "status-at-123",
                "refresh_token": "status-rf-456",
                "id_token": mock_jwt,
                "expires_in": 3600
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });

        // 1. Browser opener succeeds
        let stdout_success = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let opts_success = LoginOptions {
            token_endpoint: Some(server.url()),
            browser_opener: Some(Box::new(|_| Ok(()))),
            prompt_fn: Some(Box::new(|_| Ok("valid-code".to_string()))),
            port_range: Some([18140, 18190]),
            timeout: Some(std::time::Duration::from_secs(5)),
            auth_endpoint: None,
            stdout: Some(stdout_success.clone()),
        };

        let acc = login(opts_success).await.unwrap();
        assert_eq!(acc.email, "status_user@gmail.com");
        let out_str = String::from_utf8(stdout_success.lock().unwrap().clone()).unwrap();
        assert!(out_str.contains("Opening browser for Google authentication...\nWaiting for authorization..."));
        assert!(!out_str.contains("Could not open a browser automatically."));

        // 2. Browser opener fails
        let stdout_fail = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let opts_fail = LoginOptions {
            token_endpoint: Some(server.url()),
            browser_opener: Some(Box::new(|_| {
                Err(AccountsError::Message("headless failure".to_string()))
            })),
            prompt_fn: Some(Box::new(|_| Ok("valid-code".to_string()))),
            port_range: Some([18191, 18240]),
            timeout: Some(std::time::Duration::from_secs(5)),
            auth_endpoint: None,
            stdout: Some(stdout_fail.clone()),
        };

        let acc2 = login(opts_fail).await.unwrap();
        assert_eq!(acc2.email, "status_user@gmail.com");
        let out_fail_str = String::from_utf8(stdout_fail.lock().unwrap().clone()).unwrap();
        assert!(out_fail_str.contains(
            "Could not open a browser automatically.\nOpen this URL:\nhttps://accounts.google.com/o/oauth2/v2/auth?"
        ));
        assert!(!out_fail_str.contains("Opening browser for Google authentication..."));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_native_token_sync_production_mode_canonical_succeeds() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = crate::config::is_test_mode();
        crate::config::set_test_mode(false);

        let fake_home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let data_dir = fake_home.path().join(".local").join("share").join("agy-pool");
        std::fs::create_dir_all(&data_dir).unwrap();
        crate::config::configure_data_dir(&data_dir).unwrap();

        let expiry = crate::clock::now_unix() + 3600.0;
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            email: "main@example.com".to_string(),
            access_token: "prod-access-token-12345".to_string(),
            refresh_token: "prod-refresh-token-67890".to_string(),
            id_token: "header.payload.signature".to_string(),
            token_expiry: Some(expiry),
            ..Default::default()
        };

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc.clone()];
        pool.active_account_id = Some(acc.id.clone());
        crate::storage::save_pool(&pool).unwrap();

        // 1. WriteAgyTokenFile directly succeeds
        write_agy_token_file(&acc).unwrap();

        let expected_token_file = fake_home
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        let data = std::fs::read(&expected_token_file).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&expected_token_file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        #[derive(serde::Deserialize)]
        struct Parsed {
            token: ParsedToken,
        }
        #[derive(serde::Deserialize)]
        struct ParsedToken {
            access_token: String,
        }
        let parsed: Parsed = serde_json::from_slice(&data).unwrap();
        assert_eq!(parsed.token.access_token, "prod-access-token-12345");

        // 2. SyncActiveAgyTokenFile succeeds
        let synced = sync_active_agy_token_file().await.unwrap();
        assert!(synced);

        crate::config::reset_data_dir();
        unsafe {
            if let Some(h) = orig_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
        crate::config::set_test_mode(orig_test_mode);
    }

    #[test]
    fn test_native_token_sync_test_mode_real_production_rejected() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = crate::config::is_test_mode();
        crate::config::set_test_mode(true);

        let sandbox_dir = tempfile::tempdir().unwrap();
        let _sandbox_guard = crate::config::set_synthetic_sandbox_root(sandbox_dir.path());

        let fake_real_home = tempfile::tempdir().unwrap();
        let fake_real_gemini = fake_real_home.path().join(".gemini");
        let _real_prod_guard = crate::config::set_real_production_gemini_dir(&fake_real_gemini);

        crate::config::set_native_agy_dir(&fake_real_gemini);

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "secret-token".to_string(),
            ..Default::default()
        };

        let err = write_agy_token_file(&acc).unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("refusing write to protected production path in test mode")
                || err_str.contains("protected production path")
        );

        let real_token_file = fake_real_gemini.join("antigravity-cli").join("antigravity-oauth-token");
        assert!(!real_token_file.exists());

        crate::config::reset_native_agy_dir();
        crate::config::set_test_mode(orig_test_mode);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_native_token_sync_test_mode_synthetic_succeeds() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = crate::config::is_test_mode();
        crate::config::set_test_mode(true);

        let temp_dir = tempfile::tempdir().unwrap();
        let _sandbox_guard = crate::config::set_synthetic_sandbox_root(temp_dir.path());

        let data_dir = temp_dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        crate::config::configure_state_dir(&data_dir).unwrap();

        let native_dir = temp_dir.path().join("synthetic_gemini");
        crate::config::set_native_agy_dir(&native_dir);

        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "synced-token".to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc.clone()];
        pool.active_account_id = Some(acc.id.clone());
        crate::storage::save_pool(&pool).unwrap();

        write_agy_token_file(&acc).unwrap();

        let synced = sync_active_agy_token_file().await.unwrap();
        assert!(synced);

        crate::config::reset_data_dir();
        crate::config::reset_native_agy_dir();
        crate::config::set_test_mode(orig_test_mode);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_native_token_sync_privacy_on_failure() {
        let _env = setup_isolated_test_dir();

        let secret_token = "very-confidential-token-value-xyz";
        let secret_email = "confidential-user@secretcompany.com";
        let raw_oauth_body = format!("raw-provider-body-marker {} {}", secret_email, secret_token);

        let raw_body_clone = raw_oauth_body.clone();
        let server = crate::testutil::TestServer::start(move |_| {
            let resp = serde_json::json!({
                "error": "invalid_grant",
                "description": raw_body_clone
            });
            (
                400,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        });
        let _url_guard = crate::auth::set_token_url_for_test(&server.url());

        let past = crate::clock::now_unix() - 100.0;
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            email: secret_email.to_string(),
            access_token: secret_token.to_string(),
            refresh_token: "refresh-secret-123".to_string(),
            token_expiry: Some(past),
            ..Default::default()
        };

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![acc];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let err = sync_active_agy_token_file().await.unwrap_err();
        let err_str = err.to_string();
        assert!(!err_str.contains(secret_token), "leaked secret_token");
        assert!(!err_str.contains(secret_email), "leaked secret_email");
        assert!(!err_str.contains("raw-provider-body-marker"), "leaked raw_oauth_body");
    }

    #[tokio::test]
    async fn switch_account_prefers_exact_id() {
        let _env = setup_isolated_test_dir();
        let token = |t: &str| crate::storage::Account {
            access_token: t.to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        // acc_1 is *named* like acc_2's id, and acc_3 has a digit-only id that is also a valid list position.
        let mut a1 = token("tok1");
        a1.id = "acc_1".to_string();
        a1.name = "acc_2".to_string();
        let mut a2 = token("tok2");
        a2.id = "acc_2".to_string();
        let mut a3 = token("tok3");
        a3.id = "1".to_string();
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![a1, a2, a3];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        assert_eq!(switch_account("acc_2").await.unwrap().id, "acc_2");
        assert_eq!(switch_account("1").await.unwrap().id, "1");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.active_account_id.as_deref(), Some("1"));
    }

    #[test]
    fn persist_cleared_restriction_only_when_unchanged() {
        let _env = setup_isolated_test_dir();
        let restricted = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://accounts.google.com/v1".to_string()),
            rate_limited_until: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![restricted.clone()];
        crate::storage::save_pool(&pool).unwrap();

        let mut cleared = restricted.clone();
        cleared.status.clear();
        cleared.validation_url = None;
        cleared.rate_limited_until = None;
        let quota = crate::storage::QuotaState::default();

        // Someone re-restricted the account with a new link meanwhile: keep the stored state.
        crate::storage::pool_transaction(|p| {
            p.accounts[0].validation_url = Some("https://accounts.google.com/v2".to_string());
            Ok(())
        })
        .unwrap();
        assert!(!persist_cleared_restriction(&restricted, &cleared, &quota).unwrap());
        let stored = crate::storage::load_pool().unwrap().accounts[0].clone();
        assert_eq!(stored.status, crate::storage::STATUS_VALIDATION_REQUIRED);

        // Unchanged since `before`: the clear is persisted.
        assert!(persist_cleared_restriction(&stored, &cleared, &quota).unwrap());
        let stored = crate::storage::load_pool().unwrap().accounts[0].clone();
        assert!(stored.status.is_empty());
        assert_eq!(stored.validation_url, None);
        assert_eq!(stored.rate_limited_until, None);
    }
}
