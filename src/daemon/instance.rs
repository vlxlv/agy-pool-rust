use std::path::{Path, PathBuf};

use super::DaemonError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    Exact,
    ConfigMismatch,
    PortMismatch,
    VersionMismatch,
    Foreign,
    Stale,
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceStatus {
    RunningSameInstance,
    Stopped,
    StalePID,
    ForeignInstance,
    ConfigMismatch,
    PortMismatch,
    PortOccupiedForeign,
    OutdatedBinary,
    LegacyPID,
}

impl std::fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceStatus::RunningSameInstance => write!(f, "RUNNING_SAME_INSTANCE"),
            InstanceStatus::Stopped => write!(f, "STOPPED"),
            InstanceStatus::StalePID => write!(f, "STALE_PID"),
            InstanceStatus::ForeignInstance => write!(f, "FOREIGN_INSTANCE"),
            InstanceStatus::ConfigMismatch => write!(f, "CONFIG_MISMATCH"),
            InstanceStatus::PortMismatch => write!(f, "PORT_MISMATCH"),
            InstanceStatus::PortOccupiedForeign => write!(f, "PORT_OCCUPIED_FOREIGN"),
            InstanceStatus::OutdatedBinary => write!(f, "OUTDATED_BINARY"),
            InstanceStatus::LegacyPID => write!(f, "LEGACY_PID"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct InstanceContext {
    pub data_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listen_targets: Vec<String>,
    /// listen_targets resolved once at context creation (Err holds the message); None means resolve on demand.
    #[serde(skip)]
    pub resolved_listen: Option<Result<Vec<std::net::SocketAddr>, String>>,
    pub listen_port: u16,
    pub binary_path: PathBuf,
    pub version: String,
}

impl InstanceContext {
    pub fn effective_listen_targets(&self) -> Vec<String> {
        if !self.listen_targets.is_empty() {
            self.listen_targets.clone()
        } else {
            vec!["127.0.0.1".to_string()]
        }
    }

    pub fn primary_connect_addr(&self, info: Option<&super::pid::DaemonInfo>) -> std::net::SocketAddr {
        let port = if self.listen_port > 0 {
            self.listen_port
        } else {
            info.and_then(|i| i.port).unwrap_or(crate::config::DEFAULT_PORT)
        };
        if let Some(i) = info
            && !i.listen_addrs.is_empty()
        {
            let addrs = super::server::resolve_info_socket_addrs(i, port);
            if let Some(primary) = super::server::pick_primary_connect_addr(&addrs) {
                return primary;
            }
        }
        if let Ok(addrs) = super::server::resolve_context_socket_addrs(self)
            && let Some(primary) = super::server::pick_primary_connect_addr(&addrs)
        {
            return std::net::SocketAddr::new(primary.ip(), port);
        }
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    pub fn gateway_url(&self, info: Option<&super::pid::DaemonInfo>) -> String {
        format!("http://{}", self.primary_connect_addr(info))
    }

    pub fn display_listen(&self, info: Option<&super::pid::DaemonInfo>) -> String {
        if let Some(i) = info
            && !i.listen_addrs.is_empty()
        {
            return i.listen_addrs.join(", ");
        }
        if let Ok(addrs) = super::server::resolve_context_socket_addrs(self)
            && !addrs.is_empty()
        {
            return super::server::format_socket_addrs(&addrs);
        }
        let targets = self.effective_listen_targets();
        targets
            .into_iter()
            .map(|t| {
                if let Ok(ip) = t.parse::<std::net::IpAddr>() {
                    std::net::SocketAddr::new(ip, self.listen_port).to_string()
                } else {
                    format!("{t}:{}", self.listen_port)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.data_dir.join("agy-pool.pid")
    }

    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("agy-pool.log")
    }

    pub fn audit_file(&self) -> PathBuf {
        self.data_dir.join("audit-events.jsonl")
    }

    pub fn state_db(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.data_dir.join("locks")
    }

    pub fn lock_file(&self, name: &str) -> PathBuf {
        self.data_dir.join("locks").join(name)
    }

    pub fn runtime_file(&self) -> PathBuf {
        runtime_file_in(&self.data_dir)
    }

    pub fn get_runtime_stats(&self, expected_pid: i32) -> Result<super::runtime::RuntimeStats, DaemonError> {
        let stats_err = match super::runtime::read_runtime_stats(&self.runtime_file()) {
            Ok(stats) => {
                if expected_pid > 0 && stats.pid != expected_pid {
                    return Err(DaemonError::RuntimeStatsMismatch(format!(
                        "stats PID {} != expected {}",
                        stats.pid, expected_pid
                    )));
                }
                let now = jiff::Timestamp::now().as_second();
                let updated = stats.updated_at.as_second();
                if (now - updated).abs() > super::runtime::STALE_RUNTIME_THRESHOLD.as_secs() as i64 {
                    return Err(DaemonError::RuntimeStatsStale(format!(
                        "last update {}s ago",
                        (now - updated).abs()
                    )));
                }
                return Ok(stats);
            }
            Err(e) => e,
        };

        // Fallback to DaemonInfo in PID file if available
        if let Ok(info) = super::pid::read_pid_file(&self.pid_file())
            && let Some(rt) = info.runtime
        {
            if expected_pid > 0 && rt.pid != expected_pid {
                return Err(DaemonError::RuntimeStatsMismatch(format!(
                    "pid info stats PID {} != expected {}",
                    rt.pid, expected_pid
                )));
            }
            return Ok(rt);
        }

        Err(stats_err)
    }
}

pub fn runtime_file_in(data_dir: &Path) -> PathBuf {
    data_dir.join("agy-pool-runtime.json")
}

/// CanonicalizeDir resolves symlinks, relative components, and user home directory for a directory path.
pub fn canonicalize_dir(dir: &Path) -> Result<PathBuf, DaemonError> {
    if dir.as_os_str().is_empty() {
        return Err(DaemonError::Message("empty directory path".to_string()));
    }
    let expanded = crate::config::expand_user(dir);
    let abs = crate::config::make_absolute(&expanded)
        .map_err(|e| DaemonError::Message(format!("failed to resolve absolute path: {e}")))?;
    let cleaned = crate::config::clean_path(&abs);

    if cleaned.exists()
        && let Ok(real) = cleaned.canonicalize()
    {
        return Ok(crate::config::clean_path(&real));
    }

    // Evaluate symlinks on deepest existing ancestor
    let mut curr = cleaned.clone();
    let mut rest = Vec::new();
    while !curr.exists() {
        if let Some(base) = curr.file_name() {
            rest.push(base.to_os_string());
        }
        if let Some(parent) = curr.parent() {
            if parent == curr {
                break;
            }
            curr = parent.to_path_buf();
        } else {
            break;
        }
    }

    if let Ok(real) = curr.canonicalize() {
        let mut out = real;
        for part in rest.into_iter().rev() {
            out.push(part);
        }
        return Ok(crate::config::clean_path(&out));
    }

    Ok(cleaned)
}

/// CanonicalizeFile resolves symlinks, relative components, and user home directory for a file path.
pub fn canonicalize_file(file: &Path) -> Result<PathBuf, DaemonError> {
    if file.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    let expanded = crate::config::expand_user(file);
    let abs = crate::config::make_absolute(&expanded)
        .map_err(|e| DaemonError::Message(format!("failed to resolve absolute path: {e}")))?;
    let cleaned = crate::config::clean_path(&abs);

    if cleaned.exists()
        && let Ok(real) = cleaned.canonicalize()
    {
        return Ok(crate::config::clean_path(&real));
    }
    Ok(cleaned)
}

/// Resolves an explicit, immutable InstanceContext for the given data directory and config.
pub fn resolve_instance_context(
    data_dir: Option<&Path>,
    config_path: Option<&Path>,
    port: Option<u16>,
    listen_host: Option<&str>,
) -> Result<InstanceContext, DaemonError> {
    let raw_data_dir = match data_dir {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => crate::config::data_dir(),
    };

    let can_data_dir = canonicalize_dir(&raw_data_dir)?;

    if crate::config::is_protected_production_dir(&can_data_dir) {
        return Err(DaemonError::Message(format!(
            "[FAIL-CLOSED GUARD] refusing to use Python production state ({}) during Go development",
            can_data_dir.display()
        )));
    }

    let mut can_config_path = None;
    let mut cfg_hash = None;
    let mut static_cfg = None;

    let target_cfg = if let Some(p) = config_path {
        if !p.as_os_str().is_empty() {
            Some(p.to_path_buf())
        } else {
            None
        }
    } else {
        crate::config::resolve_default_config_file()
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
    };

    // Stay lenient on a broken config (as before) so stop/status keep working while the user fixes it.
    if let Some(cfg_p) = target_cfg
        && let Ok(can) = canonicalize_file(&cfg_p)
        && can.exists()
    {
        cfg_hash = crate::config::compute_config_file_hash(&can).ok();
        static_cfg = crate::config::load_config_file(&can).ok();
        can_config_path = Some(can);
    }

    let mut final_port = port.unwrap_or(0);
    let explicit_listen = listen_host.unwrap_or("").trim();

    if final_port == 0
        && let Ok(p_str) = std::env::var(super::lifecycle::ENV_DAEMON_PORT)
        && let Ok(p) = p_str.parse::<u16>()
    {
        final_port = p;
    }

    let mut final_listen_targets: Vec<String> = if !explicit_listen.is_empty() {
        vec![explicit_listen.to_string()]
    } else {
        Vec::new()
    };

    if let Some(sc) = &static_cfg {
        if final_port == 0 && sc.server.port > 0 {
            final_port = sc.server.port as u16;
        }
        if final_listen_targets.is_empty() && !sc.server.listen.is_empty() {
            final_listen_targets = sc.server.listen.clone();
        }
    }

    if final_port == 0 {
        final_port = crate::config::DEFAULT_PORT;
    }
    if final_listen_targets.is_empty() {
        final_listen_targets = vec!["127.0.0.1".to_string()];
    }

    for target in &final_listen_targets {
        if !crate::config::is_valid_listen_target(target) {
            return Err(DaemonError::Config(crate::config::ConfigError::Validation(format!(
                "invalid server listen address \"{target}\""
            ))));
        }
    }

    let resolved_listen =
        super::server::resolve_listen_targets(&final_listen_targets, final_port).map_err(|e| e.to_string());

    Ok(InstanceContext {
        data_dir: can_data_dir,
        config_path: can_config_path,
        config_hash: cfg_hash,
        listen_targets: final_listen_targets,
        resolved_listen: Some(resolved_listen),
        listen_port: final_port,
        binary_path: super::pid::entrypoint(),
        version: crate::VERSION.to_string(),
    })
}

pub fn is_process_owned_by_data_dir(pid: i32, data_dir: &Path) -> bool {
    if pid <= 0 || data_dir.as_os_str().is_empty() {
        return false;
    }
    let data_dir_str = data_dir.to_string_lossy();

    // Inspect /proc/<pid>/cmdline
    if let Ok(data) = std::fs::read(format!("/proc/{}/cmdline", pid)) {
        let cmd = String::from_utf8_lossy(&data);
        if cmd.contains(data_dir_str.as_ref()) {
            return true;
        }
    }

    // Inspect /proc/<pid>/environ
    if let Ok(data) = std::fs::read(format!("/proc/{}/environ", pid)) {
        let env = String::from_utf8_lossy(&data);
        let marker = format!("AGY_DATA_DIR={}", data_dir_str);
        if env.contains(&marker) {
            return true;
        }
    }

    false
}

/// MatchesInstance checks whether a DaemonInfo matches the given InstanceContext.
pub fn matches_instance(info: &super::pid::DaemonInfo, ctx: &InstanceContext) -> MatchResult {
    if info.pid <= 0 {
        return MatchResult::Stale;
    }
    if !super::pid::is_process_alive(info.pid) {
        return MatchResult::Stale;
    }
    if !super::pid::is_agy_pool_process(info.pid) {
        return MatchResult::Foreign;
    }

    // 1. Data directory check
    if let Some(info_dir) = &info.data_dir {
        if !info_dir.is_empty() {
            let can_info = canonicalize_dir(Path::new(info_dir)).unwrap_or_else(|_| PathBuf::from(info_dir));
            if can_info != ctx.data_dir {
                return MatchResult::Foreign;
            }
        }
    } else {
        // Legacy metadata: check if process is owned by this dataDir
        if !is_process_owned_by_data_dir(info.pid, &ctx.data_dir) {
            return MatchResult::Legacy;
        }
    }

    // 2. Port check
    if let Some(port) = info.port
        && port > 0
        && ctx.listen_port > 0
        && port != ctx.listen_port
    {
        return MatchResult::PortMismatch;
    }

    // 3. Config path check
    if let (Some(info_cfg), Some(ctx_cfg)) = (&info.config_path, &ctx.config_path) {
        let can_info_cfg = canonicalize_file(Path::new(info_cfg)).unwrap_or_else(|_| PathBuf::from(info_cfg));
        if can_info_cfg != *ctx_cfg {
            return MatchResult::ConfigMismatch;
        }
        // 4. Config hash check (in-place modification)
        if let (Some(info_hash), Some(ctx_hash)) = (&info.config_hash, &ctx.config_hash)
            && info_hash != ctx_hash
        {
            return MatchResult::ConfigMismatch;
        }
    } else if info.config_path.is_some() != ctx.config_path.is_some() {
        return MatchResult::ConfigMismatch;
    }

    // 5. Version check
    if let Some(info_ver) = &info.version
        && !info_ver.is_empty()
        && !ctx.version.is_empty()
        && info_ver != &ctx.version
    {
        return MatchResult::VersionMismatch;
    }

    MatchResult::Exact
}

/// Checks the status of the daemon for the given InstanceContext and specific PID file path.
pub fn check_status_with_file(
    ctx: &InstanceContext,
    pid_file: &Path,
) -> (InstanceStatus, Option<super::pid::DaemonInfo>, String) {
    let resolved_addrs_res = super::server::resolve_context_socket_addrs(ctx);
    let any_target_listening = |res: &Result<Vec<std::net::SocketAddr>, DaemonError>| -> bool {
        res.as_ref()
            .map(|addrs| super::server::any_addr_listening(addrs))
            .unwrap_or(false)
    };

    let info = match super::pid::read_pid_file(pid_file) {
        Ok(info) => info,
        Err(DaemonError::PIDNotFound) => {
            if any_target_listening(&resolved_addrs_res) {
                return (
                    InstanceStatus::PortOccupiedForeign,
                    None,
                    format!(
                        "{} occupied by another process, no PID file for this instance",
                        ctx.display_listen(None)
                    ),
                );
            }
            return (InstanceStatus::Stopped, None, "instance stopped".to_string());
        }
        Err(err) => {
            if any_target_listening(&resolved_addrs_res) {
                return (
                    InstanceStatus::PortOccupiedForeign,
                    None,
                    format!("malformed PID file and {} occupied", ctx.display_listen(None)),
                );
            }
            return (InstanceStatus::StalePID, None, format!("malformed PID file: {err}"));
        }
    };

    if !super::pid::is_process_alive(info.pid) {
        if any_target_listening(&resolved_addrs_res) {
            return (
                InstanceStatus::PortOccupiedForeign,
                Some(info.clone()),
                format!(
                    "daemon PID {} dead but {} occupied by another process",
                    info.pid,
                    ctx.display_listen(None)
                ),
            );
        }
        return (
            InstanceStatus::StalePID,
            Some(info.clone()),
            format!("stale PID {} (process dead)", info.pid),
        );
    }

    if !super::pid::is_agy_pool_process(info.pid) {
        return (
            InstanceStatus::ForeignInstance,
            Some(info.clone()),
            format!("PID {} is not an agy-pool process (PID reused)", info.pid),
        );
    }

    let res = matches_instance(&info, ctx);
    match res {
        MatchResult::Foreign => (
            InstanceStatus::ForeignInstance,
            Some(info.clone()),
            format!(
                "daemon running (PID {}) belongs to a different data directory {:?}",
                info.pid, info.data_dir
            ),
        ),
        MatchResult::PortMismatch => (
            InstanceStatus::PortMismatch,
            Some(info.clone()),
            format!(
                "daemon running (PID {}) on port {}, requested {}",
                info.pid,
                info.port.unwrap_or(0),
                ctx.listen_port
            ),
        ),
        MatchResult::ConfigMismatch => {
            let msg = if info.config_path.as_deref() != ctx.config_path.as_ref().map(|p| p.to_string_lossy()).as_deref()
            {
                format!(
                    "daemon running (PID {}) with config {:?}, requested {:?}",
                    info.pid, info.config_path, ctx.config_path
                )
            } else {
                format!(
                    "config file {:?} modified since daemon start (hash mismatch)",
                    ctx.config_path
                )
            };
            (InstanceStatus::ConfigMismatch, Some(info), msg)
        }
        MatchResult::VersionMismatch => (
            InstanceStatus::OutdatedBinary,
            Some(info.clone()),
            format!(
                "daemon running (PID {}) with version {:?}, current binary is {}",
                info.pid, info.version, ctx.version
            ),
        ),
        MatchResult::Legacy => (
            InstanceStatus::LegacyPID,
            Some(info.clone()),
            format!("daemon running (PID {}) has legacy unverified metadata", info.pid),
        ),
        MatchResult::Exact => {
            // Probe what the daemon actually bound; one interface going down must not mark a serving daemon stale.
            let addrs = super::server::resolve_info_socket_addrs(&info, ctx.listen_port);
            if !super::server::any_addr_listening(&addrs) {
                return (
                    InstanceStatus::StalePID,
                    Some(info.clone()),
                    format!(
                        "daemon PID {} is alive but none of its endpoints ({}) are listening",
                        info.pid,
                        ctx.display_listen(Some(&info))
                    ),
                );
            }
            if super::server::is_daemon_outdated(Some(pid_file), ctx.listen_port) {
                return (
                    InstanceStatus::OutdatedBinary,
                    Some(info.clone()),
                    format!("daemon running (PID {}) has outdated binary code", info.pid),
                );
            }
            (
                InstanceStatus::RunningSameInstance,
                Some(info.clone()),
                format!(
                    "daemon running (PID {}) on {}",
                    info.pid,
                    ctx.display_listen(Some(&info))
                ),
            )
        }
        MatchResult::Stale => (InstanceStatus::StalePID, Some(info), "stale PID".to_string()),
    }
}

pub fn check_instance_status(ctx: &InstanceContext) -> (InstanceStatus, Option<super::pid::DaemonInfo>, String) {
    check_status_with_file(ctx, &ctx.pid_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_dir() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let real_dir = tmp_dir.join("real-state");
        std::fs::create_dir_all(&real_dir).unwrap();

        let symlink_dir = tmp_dir.join("link-state");
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).unwrap();

        let can_real = canonicalize_dir(&real_dir).unwrap();
        let can_link = canonicalize_dir(&symlink_dir).unwrap();
        assert_eq!(
            can_link, can_real,
            "symlink canonical dir should match real canonical dir"
        );

        let sub_link = symlink_dir.join("sub").join("child");
        let can_sub_link = canonicalize_dir(&sub_link).unwrap();
        let expected_sub = can_real.join("sub").join("child");
        assert_eq!(can_sub_link, expected_sub);
    }

    #[test]
    fn test_resolve_instance_context() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let data_dir = tmp_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let cfg_path = tmp_dir.join("config.json");
        let cfg_content = r#"{"version": 1, "server": {"port": 9199, "listen": "127.0.0.1"}}"#;
        std::fs::write(&cfg_path, cfg_content.as_bytes()).unwrap();

        let ctx = resolve_instance_context(Some(&data_dir), Some(&cfg_path), None, None).unwrap();

        let can_data_dir = canonicalize_dir(&data_dir).unwrap();
        assert_eq!(ctx.data_dir, can_data_dir);
        let can_cfg_path = canonicalize_file(&cfg_path).unwrap();
        assert_eq!(ctx.config_path, Some(can_cfg_path));
        assert!(ctx.config_hash.is_some());
        assert_eq!(ctx.listen_port, 9199);
        assert_eq!(ctx.listen_targets, vec!["127.0.0.1".to_string()]);
        assert_eq!(ctx.pid_file(), can_data_dir.join("agy-pool.pid"));
        assert_eq!(ctx.log_file(), can_data_dir.join("agy-pool.log"));
        assert_eq!(ctx.state_db(), can_data_dir.join("state.db"));
    }

    #[test]
    fn test_matches_instance() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let data_dir_a = tmp_dir.join("data-a");
        let data_dir_b = tmp_dir.join("data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let cfg_a = tmp_dir.join("cfg-a.json");
        let cfg_b = tmp_dir.join("cfg-b.json");
        std::fs::write(&cfg_a, br#"{"version": 1, "server":{"port":9101}}"#).unwrap();
        std::fs::write(&cfg_b, br#"{"version": 1, "server":{"port":9102}}"#).unwrap();

        let ctx_a = resolve_instance_context(Some(&data_dir_a), Some(&cfg_a), Some(9101), Some("127.0.0.1")).unwrap();

        let curr_pid = std::process::id() as i32;

        // 1. Exact match
        let info_exact = super::super::pid::DaemonInfo {
            pid: curr_pid,
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            port: Some(ctx_a.listen_port),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(matches_instance(&info_exact, &ctx_a), MatchResult::Exact);

        // 2. Foreign data dir
        let info_foreign_dir = super::super::pid::DaemonInfo {
            pid: curr_pid,
            data_dir: Some(data_dir_b.to_string_lossy().to_string()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            port: Some(ctx_a.listen_port),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(matches_instance(&info_foreign_dir, &ctx_a), MatchResult::Foreign);

        // 3. Port mismatch
        let info_port_mismatch = super::super::pid::DaemonInfo {
            pid: curr_pid,
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            port: Some(9102),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(matches_instance(&info_port_mismatch, &ctx_a), MatchResult::PortMismatch);

        // 4. Config path mismatch
        let can_cfg_b = canonicalize_file(&cfg_b).unwrap();
        let info_cfg_mismatch = super::super::pid::DaemonInfo {
            pid: curr_pid,
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            config_path: Some(can_cfg_b.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            port: Some(ctx_a.listen_port),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(
            matches_instance(&info_cfg_mismatch, &ctx_a),
            MatchResult::ConfigMismatch
        );

        // 5. Config hash mismatch (modified in place)
        let info_hash_mismatch = super::super::pid::DaemonInfo {
            pid: curr_pid,
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: Some("different-hash".to_string()),
            port: Some(ctx_a.listen_port),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(
            matches_instance(&info_hash_mismatch, &ctx_a),
            MatchResult::ConfigMismatch
        );

        // 6. Dead PID
        let info_dead = super::super::pid::DaemonInfo {
            pid: 999999999,
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            port: Some(ctx_a.listen_port),
            listen_host: Some(ctx_a.primary_connect_addr(None).ip().to_string()),
            version: Some(ctx_a.version.clone()),
            ..Default::default()
        };
        assert_eq!(matches_instance(&info_dead, &ctx_a), MatchResult::Stale);
    }

    #[test]
    fn test_check_instance_status() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let data_dir = tmp_dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let cfg_path = tmp_dir.join("config.json");
        std::fs::write(&cfg_path, br#"{"version": 1, "server":{"port":9201}}"#).unwrap();

        let ctx = resolve_instance_context(Some(&data_dir), Some(&cfg_path), Some(9201), Some("127.0.0.1")).unwrap();

        // 1. Initial status: stopped
        let (status, info, _) = check_instance_status(&ctx);
        assert_eq!(status, InstanceStatus::Stopped);
        assert!(info.is_none());

        // 2. Stale PID file
        let stale_info = super::super::pid::DaemonInfo {
            pid: 999999999,
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            port: Some(9201),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), stale_info).unwrap();

        let (status, _, _) = check_instance_status(&ctx);
        assert_eq!(status, InstanceStatus::StalePID);

        // 3. Foreign DataDir in PID file
        let foreign_info = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            data_dir: Some(tmp_dir.join("foreign-data").to_string_lossy().to_string()),
            port: Some(9201),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), foreign_info).unwrap();

        let (status, _, _) = check_instance_status(&ctx);
        assert_eq!(status, InstanceStatus::ForeignInstance);

        // 4. Config mismatch (hash changed)
        let modified_hash_info = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: Some("old-sha256-hash".to_string()),
            port: Some(9201),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), modified_hash_info).unwrap();

        let (status, _, _) = check_instance_status(&ctx);
        assert_eq!(status, InstanceStatus::ConfigMismatch);

        // 5. Port mismatch (same data dir, different port)
        let port_mismatch_info = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx.config_hash.clone(),
            port: Some(9999),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), port_mismatch_info).unwrap();

        let (status, _, _) = check_instance_status(&ctx);
        assert_eq!(status, InstanceStatus::PortMismatch);
    }
}
