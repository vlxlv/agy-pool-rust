use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

pub mod paths;
pub use paths::*;

pub static TEST_LOCK: Mutex<()> = Mutex::new(());

// Strategy constants
pub const STRATEGY_SMART: &str = "smart";
pub const STRATEGY_MAX_QUOTA: &str = "max_quota";
pub const DEFAULT_STRATEGY: &str = STRATEGY_SMART;

// Time window constants in seconds
pub const WINDOW_5H_SECS: f64 = 18000.0;
pub const WINDOW_7D_SECS: f64 = 604800.0;

// Quota freshness constants in seconds
pub const QUOTA_FRESH_MAX_AGE: f64 = 60.0;
pub const QUOTA_AGING_MAX_AGE: f64 = 300.0;

// Quota depletion threshold
pub const DEPLETED_THRESHOLD: f64 = 0.005;

// Default network port
pub const DEFAULT_PORT: u16 = 8899;

// Log defaults
pub const DEFAULT_MAX_LOG_BYTES: i64 = 5 * 1024 * 1024;
pub const DEFAULT_BACKUP_LOG_COUNT: usize = 1;
pub const DEFAULT_AUDIT_MAX_BYTES: i64 = 20 * 1024 * 1024;

// Quota refresh retry intervals
pub const QUOTA_REFRESH_BACKOFF: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(300),
];

pub fn migrate_legacy_strategy(s: &str) -> String {
    if s == "least_used" || s == "round_robin" {
        STRATEGY_SMART.to_string()
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Message(String),
    Validation(String),
    Guard(String),
    Io(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Message(msg)
            | ConfigError::Validation(msg)
            | ConfigError::Guard(msg)
            | ConfigError::Io(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<String> for ConfigError {
    fn from(s: String) -> Self {
        ConfigError::Message(s)
    }
}

impl From<&str> for ConfigError {
    fn from(s: &str) -> Self {
        ConfigError::Message(s.to_string())
    }
}

fn default_server_listen() -> Vec<String> {
    vec!["127.0.0.1".to_string()]
}

fn deserialize_server_listen<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a listen address/interface string or list of strings")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v.is_empty() {
                Ok(default_server_listen())
            } else {
                Ok(vec![v.to_owned()])
            }
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v.is_empty() {
                Ok(default_server_listen())
            } else {
                Ok(vec![v])
            }
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                items.push(item);
            }
            Ok(items)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn serialize_server_listen<S>(listen: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if listen.len() == 1 {
        serializer.serialize_str(&listen[0])
    } else {
        use serde::Serialize;
        listen.serialize(serializer)
    }
}

pub fn is_valid_listen_target(target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    if target.eq_ignore_ascii_case("localhost") || target.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // Linux IPv4 alias labels look like "eth0:1"; any other ':' is a malformed IP or host:port.
    if let Some((base, label)) = target.split_once(':') {
        return target.len() <= 15
            && !base.eq_ignore_ascii_case("localhost")
            && base.parse::<std::net::IpAddr>().is_err()
            && !label.is_empty()
            && label.bytes().all(|b| b.is_ascii_alphanumeric())
            && is_valid_listen_target(base);
    }
    // Reject malformed IP-like values (e.g. 999.999.999.999, bare port) instead of treating as interface names.
    if target.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return false;
    }
    // POSIX/Linux IFNAMSIZ is 16 bytes including NUL terminator.
    if target.len() > 15 || target.starts_with('.') || target.ends_with('.') || target == ".." {
        return false;
    }
    target.bytes().all(|b| {
        !b.is_ascii_control()
            && !b.is_ascii_whitespace()
            && !matches!(b, b'/' | b'\\' | b'@' | b'[' | b']' | b'?' | b'#' | b',' | b'*')
    })
}

fn default_server_port() -> i64 {
    DEFAULT_PORT as i64
}

fn default_server_config() -> ServerConfig {
    ServerConfig {
        listen: default_server_listen(),
        port: default_server_port(),
        keep_alive_secs: 0,
    }
}

fn default_scheduler_strategy_for_load() -> String {
    STRATEGY_MAX_QUOTA.to_string()
}

fn default_scheduler_config_for_load() -> SchedulerConfig {
    SchedulerConfig {
        strategy: default_scheduler_strategy_for_load(),
    }
}

fn default_logging_max_size_bytes() -> i64 {
    DEFAULT_MAX_LOG_BYTES
}

fn default_logging_backup_count() -> i64 {
    DEFAULT_BACKUP_LOG_COUNT as i64
}

fn default_logging_audit_max_size_bytes() -> i64 {
    DEFAULT_AUDIT_MAX_BYTES
}

fn default_logging_config() -> LoggingConfig {
    LoggingConfig {
        max_size_bytes: default_logging_max_size_bytes(),
        backup_count: default_logging_backup_count(),
        audit_max_size_bytes: default_logging_audit_max_size_bytes(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticConfig {
    #[serde(default)]
    pub version: i64,
    #[serde(default = "default_server_config")]
    pub server: ServerConfig,
    #[serde(default = "default_scheduler_config_for_load")]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub native_agy: NativeAgyConfig,
    #[serde(default = "default_logging_config")]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub model_mapping: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub dashboard: DashboardConfig,
}

pub const ENV_DASHBOARD_TOKEN: &str = "AGY_POOL_DASHBOARD_TOKEN";
pub const MIN_DASHBOARD_TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Lets dashboard mutations run without a token. Only for networks where every peer is trusted.
    #[serde(default)]
    pub allow_unauthenticated_control: bool,
}

/// Browsers can only send visible ASCII in a header, and short tokens are guessable.
pub fn check_dashboard_token(token: &str) -> Result<(), String> {
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("token must contain only visible ASCII characters (no spaces)".to_string());
    }
    if token.len() < MIN_DASHBOARD_TOKEN_LEN {
        return Err(format!(
            "token must be at least {MIN_DASHBOARD_TOKEN_LEN} characters (generate one with `openssl rand -hex 32`)"
        ));
    }
    Ok(())
}

/// The env var wins over config.json; empty values count as unset.
pub fn resolve_dashboard_token(env_value: Option<String>, cfg: &DashboardConfig) -> Option<String> {
    env_value
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.auth_token.clone().filter(|s| !s.is_empty()))
}

pub fn dashboard_token(cfg: &DashboardConfig) -> Option<String> {
    resolve_dashboard_token(std::env::var(ENV_DASHBOARD_TOKEN).ok(), cfg)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(
        default = "default_server_listen",
        deserialize_with = "deserialize_server_listen",
        serialize_with = "serialize_server_listen"
    )]
    pub listen: Vec<String>,
    #[serde(default = "default_server_port")]
    pub port: i64,
    /// Seconds before a slow SSE request gets an early 200 and heartbeats; 0 = off.
    /// Off by default: once 200 is sent, upstream 429/503 can only arrive as an in-stream error event.
    #[serde(default)]
    pub keep_alive_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    #[serde(default = "default_scheduler_strategy_for_load")]
    pub strategy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NativeAgyConfig {
    #[serde(default)]
    pub binary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default = "default_logging_max_size_bytes")]
    pub max_size_bytes: i64,
    #[serde(default = "default_logging_backup_count")]
    pub backup_count: i64,
    #[serde(default = "default_logging_audit_max_size_bytes")]
    pub audit_max_size_bytes: i64,
}

pub fn default_static_config() -> StaticConfig {
    StaticConfig {
        version: 1,
        server: ServerConfig {
            listen: default_server_listen(),
            port: DEFAULT_PORT as i64,
            keep_alive_secs: 0,
        },
        scheduler: SchedulerConfig {
            strategy: DEFAULT_STRATEGY.to_string(),
        },
        native_agy: NativeAgyConfig { binary: None },
        logging: LoggingConfig {
            max_size_bytes: DEFAULT_MAX_LOG_BYTES,
            backup_count: DEFAULT_BACKUP_LOG_COUNT as i64,
            audit_max_size_bytes: DEFAULT_AUDIT_MAX_BYTES,
        },
        model_mapping: std::collections::HashMap::new(),
        dashboard: DashboardConfig::default(),
    }
}

/// Loads the config at the given path or the default config file if available, falling back to defaults.
pub fn load_static_config(path: Option<&Path>) -> StaticConfig {
    let resolved = match path {
        Some(p) => Some(p.to_path_buf()),
        None => resolve_default_config_file().ok().filter(|p| !p.as_os_str().is_empty()),
    };
    resolved
        .filter(|p| p.exists())
        .and_then(|p| load_config_file(&p).ok())
        .unwrap_or_else(default_static_config)
}

/// Loads logging configuration from the given path or default config file if available, falling back to defaults.
pub fn load_logging_config(path: Option<&Path>) -> LoggingConfig {
    load_static_config(path).logging
}

static ACTIVE_STATIC_CONFIG: RwLock<Option<StaticConfig>> = RwLock::new(None);

pub fn get_static_config() -> StaticConfig {
    let read = ACTIVE_STATIC_CONFIG.read().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = &*read {
        cfg.clone()
    } else {
        drop(read);
        // Load outside the lock. Tests skip the host's config file so they stay hermetic.
        let loaded = if is_test_mode() {
            default_static_config()
        } else {
            load_static_config(None)
        };
        let mut write = ACTIVE_STATIC_CONFIG.write().unwrap_or_else(|e| e.into_inner());
        write.get_or_insert(loaded).clone()
    }
}

pub fn set_static_config(cfg: StaticConfig) {
    let mut write = ACTIVE_STATIC_CONFIG.write().unwrap_or_else(|e| e.into_inner());
    *write = Some(cfg);
}

const FORBIDDEN_KEYS: &[&str] = &[
    "directory",
    "data_dir",
    "state_dir",
    "accounts",
    "tokens",
    "quota",
    "active_account_id",
    "round_robin cursor",
    "round_robin_last_account_id",
    "PID",
    "pid",
    "runtime counters",
    "runtime",
    "cooldown",
    "working directory",
    "working_directory",
    "cwd",
];

pub fn load_config_file(path: &Path) -> Result<StaticConfig, ConfigError> {
    load_config_file_with_hash(path).map(|(cfg, _)| cfg)
}

/// Like `load_config_file`, plus the SHA-256 of the exact bytes parsed, so the two cannot disagree.
pub fn load_config_file_with_hash(path: &Path) -> Result<(StaticConfig, String), ConfigError> {
    use sha2::Digest;
    let abs_path = match make_absolute(expand_user(path)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Validation(format!(
                "invalid config file path \"{}\": {}",
                path.display(),
                err
            )));
        }
    };
    let data = match std::fs::read(&abs_path) {
        Ok(d) => d,
        Err(e) => {
            return Err(ConfigError::Io(format!(
                "failed to read config file \"{}\": {}",
                abs_path.display(),
                e
            )));
        }
    };

    // 1. Check for forbidden mutable/state keys with descriptive errors
    let raw: serde_json::Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(e) => {
            return Err(ConfigError::Validation(format!(
                "malformed JSON in config file \"{}\": {}",
                abs_path.display(),
                e
            )));
        }
    };

    if let Some(obj) = raw.as_object() {
        for &key in FORBIDDEN_KEYS {
            if obj.contains_key(key) {
                return Err(ConfigError::Validation(format!(
                    "config error: forbidden key \"{}\" must not be in static config file (mutable state belongs in -D DIR)",
                    key
                )));
            }
        }
    }

    // 2. Strict decoding rejecting unknown fields
    let mut cfg: StaticConfig = match serde_json::from_slice(&data) {
        Ok(c) => c,
        Err(e) => {
            return Err(ConfigError::Validation(format!(
                "config error in \"{}\": {}",
                abs_path.display(),
                e
            )));
        }
    };

    // 3. Schema & value validation
    if cfg.version != 1 {
        return Err(ConfigError::Validation(format!(
            "unsupported config version {} (expected version 1)",
            cfg.version
        )));
    }
    if cfg.server.port <= 0 || cfg.server.port > 65535 {
        return Err(ConfigError::Validation(format!(
            "invalid server port {} (must be between 1 and 65535)",
            cfg.server.port
        )));
    }
    if cfg.server.listen.is_empty() {
        return Err(ConfigError::Validation(
            "invalid server listen address: list must not be empty".to_string(),
        ));
    }
    for target in &cfg.server.listen {
        if !is_valid_listen_target(target) {
            return Err(ConfigError::Validation(format!(
                "invalid server listen address \"{}\"",
                target
            )));
        }
    }

    cfg.scheduler.strategy = migrate_legacy_strategy(&cfg.scheduler.strategy);
    match cfg.scheduler.strategy.as_str() {
        STRATEGY_SMART | STRATEGY_MAX_QUOTA => {}
        _ => {
            return Err(ConfigError::Validation(format!(
                "invalid scheduler strategy \"{}\"",
                cfg.scheduler.strategy
            )));
        }
    }

    if cfg.logging.max_size_bytes <= 0 {
        return Err(ConfigError::Validation(format!(
            "invalid logging max_size_bytes {} (must be > 0)",
            cfg.logging.max_size_bytes
        )));
    }
    if cfg.logging.audit_max_size_bytes <= 0 {
        return Err(ConfigError::Validation(format!(
            "invalid logging audit_max_size_bytes {} (must be > 0)",
            cfg.logging.audit_max_size_bytes
        )));
    }
    if cfg.logging.backup_count < 0 {
        return Err(ConfigError::Validation(format!(
            "invalid logging backup_count {} (must be >= 0)",
            cfg.logging.backup_count
        )));
    }

    if let Some(token) = cfg.dashboard.auth_token.as_deref()
        && !token.is_empty()
        && let Err(e) = check_dashboard_token(token)
    {
        return Err(ConfigError::Validation(format!("invalid dashboard.auth_token: {e}")));
    }

    // 4. Resolve relative native_agy.binary relative to config file directory
    if let Some(bin_path) = &cfg.native_agy.binary
        && !bin_path.is_empty()
    {
        let p = Path::new(bin_path);
        if !p.is_absolute() {
            let parent_dir = abs_path.parent().unwrap_or(Path::new("."));
            let resolved = clean_path(parent_dir.join(p));
            cfg.native_agy.binary = Some(resolved.to_string_lossy().into_owned());
        }
    }

    Ok((cfg, hex::encode(sha2::Sha256::digest(&data))))
}

pub fn compute_config_file_hash(path: &Path) -> Result<String, ConfigError> {
    use sha2::{Digest, Sha256};
    let abs_path = match make_absolute(expand_user(path)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Io(format!("failed to resolve config path: {}", err)));
        }
    };
    let data = match std::fs::read(&abs_path) {
        Ok(d) => d,
        Err(err) => {
            return Err(ConfigError::Io(format!(
                "failed to read config file for hashing: {}",
                err
            )));
        }
    };
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

pub fn resolve_default_config_file() -> Result<PathBuf, ConfigError> {
    if let Ok(explicit) = std::env::var("AGY_CONFIG_FILE")
        && !explicit.is_empty()
        && let Ok(abs) = make_absolute(expand_user(Path::new(&explicit)))
        && let Ok(meta) = std::fs::metadata(&abs)
        && !meta.is_dir()
    {
        return Ok(abs);
    }

    if is_test_mode() {
        return Ok(PathBuf::new());
    }

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(real_host_user_home);

    if !home.as_os_str().is_empty() {
        let user_cfg = home.join(".config").join("agy-pool").join("config.json");
        if let Ok(meta) = std::fs::metadata(&user_cfg)
            && !meta.is_dir()
        {
            return Ok(user_cfg);
        }
    }

    let sys_cfg = PathBuf::from("/etc/agy-pool/config.json");
    if let Ok(meta) = std::fs::metadata(&sys_cfg)
        && !meta.is_dir()
    {
        return Ok(sys_cfg);
    }

    Ok(PathBuf::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(WINDOW_5H_SECS, 18000.0);
        assert_eq!(WINDOW_7D_SECS, 604800.0);
        assert_eq!(QUOTA_FRESH_MAX_AGE, 60.0);
        assert_eq!(QUOTA_AGING_MAX_AGE, 300.0);
        assert_eq!(DEPLETED_THRESHOLD, 0.005);
        assert_eq!(STRATEGY_SMART, "smart");
        assert_eq!(STRATEGY_MAX_QUOTA, "max_quota");
    }

    #[test]
    fn static_config_load_and_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let valid_cfg = tmp.path().join("config.json");
        let content = r#"{
  "version": 1,
  "server": {
    "listen": "127.0.0.1",
    "port": 8899
  },
  "scheduler": {
    "strategy": "max_quota"
  },
  "native_agy": {
    "binary": "/usr/bin/agy"
  },
  "logging": {
    "max_size_bytes": 10485760,
    "backup_count": 3
  }
}"#;
        std::fs::write(&valid_cfg, content).unwrap();

        let cfg = load_config_file(&valid_cfg).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.server.port, 8899);
        assert_eq!(cfg.scheduler.strategy, "max_quota");
        assert_eq!(cfg.native_agy.binary.as_deref(), Some("/usr/bin/agy"));
        assert_eq!(cfg.logging.max_size_bytes, 10485760);
        assert_eq!(cfg.logging.backup_count, 3);
    }

    #[test]
    fn static_config_forbidden_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let forbidden_keys = ["directory", "data_dir", "state_dir", "accounts", "runtime", "quota"];

        for key in forbidden_keys {
            let cfg_path = tmp.path().join(format!("{}.json", key));
            let content = format!(r#"{{"version": 1, "{}": "/some/path"}}"#, key);
            std::fs::write(&cfg_path, content).unwrap();
            let res = load_config_file(&cfg_path);
            assert!(
                res.is_err(),
                "expected error loading config with forbidden key {:?}, got Ok",
                key
            );
        }
    }

    #[test]
    fn strict_config_parsing_errors() {
        let tmp = tempfile::tempdir().unwrap();

        let cases = [
            (
                "unknown_top_level_field",
                r#"{"version": 1, "sever": {"port": 9000}}"#,
                "unknown field",
            ),
            (
                "unknown_nested_server_field",
                r#"{"version": 1, "server": {"listen": "127.0.0.1", "port": 8899, "extra": 1}}"#,
                "unknown field",
            ),
            ("future_version_2", r#"{"version": 2}"#, "unsupported config version 2"),
            (
                "omitted_version",
                r#"{"server": {"port": 8899}}"#,
                "unsupported config version 0",
            ),
            (
                "invalid_port_high",
                r#"{"version": 1, "server": {"port": 70000}}"#,
                "invalid server port 70000",
            ),
            (
                "invalid_port_zero",
                r#"{"version": 1, "server": {"port": 0}}"#,
                "invalid server port 0",
            ),
            (
                "invalid_listen_ip",
                r#"{"version": 1, "server": {"listen": "999.999.999.999", "port": 8899}}"#,
                "invalid server listen address",
            ),
            (
                "invalid_scheduler_strategy",
                r#"{"version": 1, "scheduler": {"strategy": "fastest"}}"#,
                "invalid scheduler strategy",
            ),
            (
                "invalid_logging_size",
                r#"{"version": 1, "logging": {"max_size_bytes": 0}}"#,
                "invalid logging max_size_bytes",
            ),
            (
                "invalid_logging_audit_size",
                r#"{"version": 1, "logging": {"audit_max_size_bytes": 0}}"#,
                "invalid logging audit_max_size_bytes",
            ),
            (
                "invalid_logging_backup_negative",
                r#"{"version": 1, "logging": {"backup_count": -1}}"#,
                "invalid logging backup_count",
            ),
            ("malformed_json", r#"{"version": 1, server: bad"#, "malformed JSON"),
        ];

        for (name, json_str, want_err) in cases {
            let p = tmp.path().join(format!("{}.json", name));
            std::fs::write(&p, json_str).unwrap();
            let res = load_config_file(&p);
            assert!(
                res.is_err(),
                "case {}: expected error containing {:?}, got Ok",
                name,
                want_err
            );
            let err_msg = res.unwrap_err().to_string();
            assert!(
                err_msg.contains(want_err),
                "case {}: expected error containing {:?}, got: {:?}",
                name,
                want_err,
                err_msg
            );
        }
    }

    #[test]
    fn relative_native_agy_binary_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join("sub").join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();

        let cfg_path = cfg_dir.join("config.json");
        let content = r#"{
  "version": 1,
  "native_agy": {
    "binary": "../bin/native-agy"
  }
}"#;
        std::fs::write(&cfg_path, content).unwrap();

        let cfg = load_config_file(&cfg_path).unwrap();
        assert!(cfg.native_agy.binary.is_some(), "expected resolved binary, got None");
        let expected_path = clean_path(tmp.path().join("sub").join("bin").join("native-agy"));
        assert_eq!(cfg.native_agy.binary.as_deref(), Some(expected_path.to_str().unwrap()));
    }

    #[test]
    fn compute_config_file_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(&p, br#"{"version": 1}"#).unwrap();

        let hash = super::compute_config_file_hash(&p).unwrap();
        assert_eq!(hash.len(), 64, "expected 64-char hex hash, got {}", hash);
    }

    #[test]
    fn config_example_json_validation() {
        let example_path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.json"));
        let cfg = load_config_file(example_path).unwrap();

        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.server.listen, vec!["127.0.0.1".to_string()]);
        assert_eq!(cfg.server.port, 8899);
        assert_eq!(cfg.scheduler.strategy, STRATEGY_MAX_QUOTA);
        assert!(cfg.native_agy.binary.is_none());
        assert_eq!(cfg.logging.max_size_bytes, DEFAULT_MAX_LOG_BYTES);
        assert_eq!(cfg.logging.backup_count as usize, DEFAULT_BACKUP_LOG_COUNT);
        assert_eq!(cfg.logging.audit_max_size_bytes, DEFAULT_AUDIT_MAX_BYTES);
    }

    #[test]
    fn test_config_logging_audit_max_size_bytes_default_when_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config_omitted.json");
        std::fs::write(&p, r#"{"version": 1, "logging": {"max_size_bytes": 1000}}"#).unwrap();
        let cfg = load_config_file(&p).unwrap();
        assert_eq!(cfg.logging.audit_max_size_bytes, DEFAULT_AUDIT_MAX_BYTES);
    }

    #[test]
    fn config_deprecated_strategies_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        for strat in ["least_used", "round_robin"] {
            let cfg_path = tmp.path().join(format!("config_{}.json", strat));
            let content = format!(
                r#"{{"version":1,"server":{{"listen":"127.0.0.1","port":8899}},"scheduler":{{"strategy":"{}"}}}}"#,
                strat
            );
            std::fs::write(&cfg_path, content).unwrap();
            let cfg = load_config_file(&cfg_path).unwrap();
            assert_eq!(cfg.scheduler.strategy, STRATEGY_SMART);
        }
    }

    #[test]
    fn load_static_config_reads_file_and_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good.json");
        std::fs::write(
            &good,
            r#"{"version":1,"model_mapping":{"a":"b"},"server":{"keep_alive_secs":7}}"#,
        )
        .unwrap();
        let cfg = load_static_config(Some(&good));
        assert_eq!(cfg.model_mapping.get("a").map(String::as_str), Some("b"));
        assert_eq!(cfg.server.keep_alive_secs, 7);

        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, r#"{"version":1,"no_such_key":true}"#).unwrap();
        assert_eq!(load_static_config(Some(&bad)), default_static_config());
        assert_eq!(
            load_static_config(Some(&tmp.path().join("missing.json"))),
            default_static_config()
        );
    }

    #[test]
    fn server_listen_string_array_and_validation() {
        let tmp = tempfile::tempdir().unwrap();

        let valid_cases = [
            (
                "empty_string_defaults",
                r#"{"version":1,"server":{"listen":""}}"#,
                vec!["127.0.0.1"],
            ),
            (
                "single_ip",
                r#"{"version":1,"server":{"listen":"127.0.0.1"}}"#,
                vec!["127.0.0.1"],
            ),
            (
                "single_iface",
                r#"{"version":1,"server":{"listen":"tailscale0"}}"#,
                vec!["tailscale0"],
            ),
            (
                "multi_targets",
                r#"{"version":1,"server":{"listen":["lo","tailscale0","eth0.100","::1"]}}"#,
                vec!["lo", "tailscale0", "eth0.100", "::1"],
            ),
            (
                "iface_alias_label",
                r#"{"version":1,"server":{"listen":"eth0:1"}}"#,
                vec!["eth0:1"],
            ),
        ];

        for (name, json_str, expected) in valid_cases {
            let p = tmp.path().join(format!("{name}.json"));
            std::fs::write(&p, json_str).unwrap();
            let cfg = load_config_file(&p).unwrap();
            assert_eq!(
                cfg.server.listen,
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "case {name}"
            );
        }

        let invalid_cases = [
            ("empty_array", r#"{"version":1,"server":{"listen":[]}}"#),
            ("array_with_empty", r#"{"version":1,"server":{"listen":["lo",""]}}"#),
            (
                "malformed_ip",
                r#"{"version":1,"server":{"listen":["999.999.999.999"]}}"#,
            ),
            (
                "host_with_port",
                r#"{"version":1,"server":{"listen":"127.0.0.1:8899"}}"#,
            ),
            ("iface_with_slash", r#"{"version":1,"server":{"listen":"bad/iface"}}"#),
            (
                "localhost_with_port",
                r#"{"version":1,"server":{"listen":"localhost:8899"}}"#,
            ),
            (
                "iface_too_long",
                r#"{"version":1,"server":{"listen":"too_long_iface_16"}}"#,
            ),
            ("wrong_type_number", r#"{"version":1,"server":{"listen":123}}"#),
        ];

        for (name, json_str) in invalid_cases {
            let p = tmp.path().join(format!("bad_{name}.json"));
            std::fs::write(&p, json_str).unwrap();
            let res = load_config_file(&p);
            assert!(res.is_err(), "case {name} expected validation error, got Ok({res:?})");
        }
    }

    #[test]
    fn dashboard_config_parses_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dash.json");
        std::fs::write(
            &p,
            r#"{"version":1,"dashboard":{"auth_token":"0123456789abcdef0123456789abcdef","allow_unauthenticated_control":true}}"#,
        )
        .unwrap();
        let cfg = load_config_file(&p).unwrap();
        assert_eq!(
            cfg.dashboard.auth_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(cfg.dashboard.allow_unauthenticated_control);

        std::fs::write(&p, r#"{"version":1}"#).unwrap();
        assert_eq!(load_config_file(&p).unwrap().dashboard, DashboardConfig::default());
        assert_eq!(default_static_config().dashboard, DashboardConfig::default());

        std::fs::write(&p, r#"{"version":1,"dashboard":{"token":"x"}}"#).unwrap();
        assert!(load_config_file(&p).is_err(), "unknown dashboard keys must be rejected");
    }

    #[test]
    fn dashboard_token_rules() {
        assert!(check_dashboard_token("0123456789abcdef0123456789abcdef").is_ok());
        assert!(check_dashboard_token("short").is_err());
        assert!(check_dashboard_token(" 0123456789abcdef0123456789abcdef").is_err());
        assert!(check_dashboard_token("0123456789abcdef0123456789abcdé").is_err());

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dash.json");
        std::fs::write(&p, r#"{"version":1,"dashboard":{"auth_token":"too-short"}}"#).unwrap();
        let err = load_config_file(&p).unwrap_err().to_string();
        assert!(err.contains("dashboard.auth_token"), "error should name the key: {err}");

        // An empty token means "no token", not an error.
        std::fs::write(&p, r#"{"version":1,"dashboard":{"auth_token":""}}"#).unwrap();
        assert!(load_config_file(&p).is_ok());
    }

    #[test]
    fn dashboard_token_env_overrides_config() {
        let cfg = DashboardConfig {
            auth_token: Some("from-config".to_string()),
            allow_unauthenticated_control: false,
        };
        assert_eq!(
            resolve_dashboard_token(Some("from-env".to_string()), &cfg).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            resolve_dashboard_token(Some(String::new()), &cfg).as_deref(),
            Some("from-config")
        );
        assert_eq!(resolve_dashboard_token(None, &cfg).as_deref(), Some("from-config"));
        let empty = DashboardConfig {
            auth_token: Some(String::new()),
            allow_unauthenticated_control: false,
        };
        assert_eq!(resolve_dashboard_token(None, &empty), None);
    }
}
