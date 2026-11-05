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

fn default_server_listen() -> String {
    "127.0.0.1".to_string()
}

fn default_server_port() -> i64 {
    DEFAULT_PORT as i64
}

fn default_server_config() -> ServerConfig {
    ServerConfig {
        listen: default_server_listen(),
        port: default_server_port(),
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

fn default_logging_config() -> LoggingConfig {
    LoggingConfig {
        max_size_bytes: default_logging_max_size_bytes(),
        backup_count: default_logging_backup_count(),
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_server_listen")]
    pub listen: String,
    #[serde(default = "default_server_port")]
    pub port: i64,
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
}

pub fn default_static_config() -> StaticConfig {
    StaticConfig {
        version: 1,
        server: ServerConfig {
            listen: "127.0.0.1".to_string(),
            port: DEFAULT_PORT as i64,
        },
        scheduler: SchedulerConfig {
            strategy: DEFAULT_STRATEGY.to_string(),
        },
        native_agy: NativeAgyConfig { binary: None },
        logging: LoggingConfig {
            max_size_bytes: DEFAULT_MAX_LOG_BYTES,
            backup_count: DEFAULT_BACKUP_LOG_COUNT as i64,
        },
    }
}

static ACTIVE_STATIC_CONFIG: RwLock<Option<StaticConfig>> = RwLock::new(None);

pub fn get_static_config() -> StaticConfig {
    let read = ACTIVE_STATIC_CONFIG.read().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = &*read {
        cfg.clone()
    } else {
        drop(read);
        let mut write = ACTIVE_STATIC_CONFIG.write().unwrap_or_else(|e| e.into_inner());
        let cfg = default_static_config();
        *write = Some(cfg.clone());
        cfg
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
    let abs_path = make_absolute(expand_user(path));
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
        cfg.server.listen = "127.0.0.1".to_string();
    } else if cfg.server.listen != "localhost" && cfg.server.listen.parse::<std::net::IpAddr>().is_err() {
        return Err(ConfigError::Validation(format!(
            "invalid server listen address \"{}\"",
            cfg.server.listen
        )));
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
    if cfg.logging.backup_count < 0 {
        return Err(ConfigError::Validation(format!(
            "invalid logging backup_count {} (must be >= 0)",
            cfg.logging.backup_count
        )));
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

    Ok(cfg)
}

pub fn compute_config_file_hash(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let abs_path = make_absolute(expand_user(path));
    let data = std::fs::read(&abs_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

pub fn resolve_default_config_file() -> Result<PathBuf, ConfigError> {
    if let Ok(explicit) = std::env::var("AGY_CONFIG_FILE")
        && !explicit.is_empty()
    {
        let abs = make_absolute(expand_user(Path::new(&explicit)));
        if let Ok(meta) = std::fs::metadata(&abs)
            && !meta.is_dir()
        {
            return Ok(abs);
        }
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
        let example_path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../config.example.json"));
        let cfg = load_config_file(example_path).unwrap();

        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.server.listen, "127.0.0.1");
        assert_eq!(cfg.server.port, 8899);
        assert_eq!(cfg.scheduler.strategy, STRATEGY_MAX_QUOTA);
        assert!(cfg.native_agy.binary.is_none());
        assert_eq!(cfg.logging.max_size_bytes, DEFAULT_MAX_LOG_BYTES);
        assert_eq!(cfg.logging.backup_count as usize, DEFAULT_BACKUP_LOG_COUNT);
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
}
