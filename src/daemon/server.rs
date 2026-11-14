use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::DaemonError;

pub type QuotaRefresherFn = Arc<dyn Fn(&crate::storage::Account) + Send + Sync>;
pub type TokenPreRefresherFn = Arc<dyn Fn(&crate::storage::Account) -> Result<(), DaemonError> + Send + Sync>;

static QUOTA_REFRESHER: std::sync::RwLock<Option<QuotaRefresherFn>> = std::sync::RwLock::new(None);
static TOKEN_PRE_REFRESHER: std::sync::RwLock<Option<TokenPreRefresherFn>> = std::sync::RwLock::new(None);

pub struct QuotaRefresherGuard {
    _orig: Option<QuotaRefresherFn>,
}

impl Drop for QuotaRefresherGuard {
    fn drop(&mut self) {
        let mut w = QUOTA_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
        *w = self._orig.take();
    }
}

pub fn set_quota_refresher_for_test<F>(f: F) -> QuotaRefresherGuard
where
    F: Fn(&crate::storage::Account) + Send + Sync + 'static,
{
    let mut w = QUOTA_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
    let orig = w.take();
    *w = Some(Arc::new(f));
    QuotaRefresherGuard { _orig: orig }
}

pub struct TokenPreRefresherGuard {
    _orig: Option<TokenPreRefresherFn>,
}

impl Drop for TokenPreRefresherGuard {
    fn drop(&mut self) {
        let mut w = TOKEN_PRE_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
        *w = self._orig.take();
    }
}

pub fn set_token_pre_refresher_for_test<F>(f: F) -> TokenPreRefresherGuard
where
    F: Fn(&crate::storage::Account) -> Result<(), DaemonError> + Send + Sync + 'static,
{
    let mut w = TOKEN_PRE_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
    let orig = w.take();
    *w = Some(Arc::new(f));
    TokenPreRefresherGuard { _orig: orig }
}

pub struct ServerOptions {
    pub port: u16,
    pub ephemeral_port: bool,
    pub pid_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub handler: Option<Arc<crate::proxy::Handler>>,
    pub ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub bound_port_tx: Option<tokio::sync::oneshot::Sender<u16>>,
    pub quota_refresh_interval: Option<Duration>,
    pub disable_signals: bool,
    pub instance: Option<super::instance::InstanceContext>,
    pub shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// Maps wildcard bind addresses (`0.0.0.0` / `::`) to loopback for client connections.
pub fn connectable_socket_addr(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port()),
        IpAddr::V6(v6) if v6.is_unspecified() => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port()),
        _ => addr,
    }
}

/// Resolves all usable UP unicast IP addresses assigned to a Unix network interface.
#[cfg(unix)]
pub fn resolve_interface_ips(if_name: &str) -> Result<Vec<IpAddr>, DaemonError> {
    struct IfAddrsGuard(*mut libc::ifaddrs);
    impl Drop for IfAddrsGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { libc::freeifaddrs(self.0) };
            }
        }
    }

    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    let rc = unsafe { libc::getifaddrs(&mut addrs) };
    if rc != 0 {
        return Err(DaemonError::Io(std::io::Error::last_os_error()));
    }
    let _guard = IfAddrsGuard(addrs);

    let mut found_iface = false;
    let mut any_up = false;
    let mut ipv4s: Vec<IpAddr> = Vec::new();
    let mut ipv6s: Vec<IpAddr> = Vec::new();

    let mut cur = addrs;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;

        if ifa.ifa_name.is_null() {
            continue;
        }
        let name_bytes = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_bytes();
        if name_bytes != if_name.as_bytes() {
            continue;
        }
        found_iface = true;

        #[allow(clippy::unnecessary_cast)]
        let is_up = (ifa.ifa_flags as u32 & libc::IFF_UP as u32) != 0;
        if !is_up {
            continue;
        }
        any_up = true;

        if ifa.ifa_addr.is_null() {
            continue;
        }
        let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
        if family == libc::AF_INET {
            let sin = unsafe { std::ptr::read_unaligned(ifa.ifa_addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            if ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            let ip_addr = IpAddr::V4(ip);
            if !ipv4s.contains(&ip_addr) {
                ipv4s.push(ip_addr);
            }
        } else if family == libc::AF_INET6 {
            let sin6 = unsafe { std::ptr::read_unaligned(ifa.ifa_addr as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            // Skip link-local fe80::/10 because HTTP URLs/connect without interface scope_id fail or leak %zone.
            if ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local() {
                continue;
            }
            let ip_addr = IpAddr::V6(ip);
            if !ipv6s.contains(&ip_addr) {
                ipv6s.push(ip_addr);
            }
        }
    }

    if !found_iface {
        return Err(DaemonError::Message(format!(
            "listen interface \"{if_name}\" not found (hostnames are not supported; use an IP address or interface name)"
        )));
    }
    if !any_up {
        return Err(DaemonError::Message(format!("listen interface \"{if_name}\" is down")));
    }

    let mut ips = ipv4s;
    ips.extend(ipv6s);
    if ips.is_empty() {
        return Err(DaemonError::Message(format!(
            "listen interface \"{if_name}\" has no usable IP addresses"
        )));
    }
    Ok(ips)
}

#[cfg(not(unix))]
pub fn resolve_interface_ips(if_name: &str) -> Result<Vec<IpAddr>, DaemonError> {
    Err(DaemonError::Message(format!(
        "listening on network interface name \"{if_name}\" is only supported on Unix platforms"
    )))
}

/// Resolves configured listen targets (IP literals, `localhost`, or NIC names) into deduplicated SocketAddrs.
pub fn resolve_listen_targets(targets: &[String], port: u16) -> Result<Vec<SocketAddr>, DaemonError> {
    if targets.is_empty() {
        return Err(DaemonError::Config(crate::config::ConfigError::Validation(
            "invalid server listen address: list must not be empty".to_string(),
        )));
    }

    let mut resolved = Vec::new();
    let mut push_unique = |sa: SocketAddr| {
        if !resolved.contains(&sa) {
            resolved.push(sa);
        }
    };

    for target in targets {
        if !crate::config::is_valid_listen_target(target) {
            return Err(DaemonError::Config(crate::config::ConfigError::Validation(format!(
                "invalid server listen address \"{target}\""
            ))));
        }

        if let Ok(ip) = target.parse::<IpAddr>() {
            push_unique(SocketAddr::new(ip, port));
        } else if target.eq_ignore_ascii_case("localhost") {
            // Single address, matching what 0.3.x bound; list "::1" explicitly for IPv6 loopback.
            push_unique(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        } else {
            for ip in resolve_interface_ips(target)? {
                push_unique(SocketAddr::new(ip, port));
            }
        }
    }

    // Linux refuses a specific bind on a port held by 0.0.0.0/:: (and :: is dual-stack), so a wildcard must stand alone.
    if resolved.len() > 1 && resolved.iter().any(|a| a.ip().is_unspecified()) {
        return Err(DaemonError::Config(crate::config::ConfigError::Validation(
            "invalid server listen address: 0.0.0.0 or :: cannot be combined with other listen targets".to_string(),
        )));
    }

    Ok(resolved)
}

/// Picks the preferred client-connectable endpoint from bound/resolved socket addresses.
pub fn pick_primary_connect_addr(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    let candidates: Vec<SocketAddr> = addrs.iter().copied().map(connectable_socket_addr).collect();
    candidates
        .iter()
        .copied()
        .find(|a| a.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|a| matches!(a.ip(), IpAddr::V4(v4) if v4.is_loopback()))
        })
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|a| a.ip() == IpAddr::V6(Ipv6Addr::LOCALHOST))
        })
        .or_else(|| candidates.iter().copied().find(|a| a.is_ipv4()))
        .or_else(|| candidates.first().copied())
}

pub fn format_socket_addrs(addrs: &[SocketAddr]) -> String {
    addrs.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

pub fn resolve_context_socket_addrs(ctx: &super::instance::InstanceContext) -> Result<Vec<SocketAddr>, DaemonError> {
    match &ctx.resolved_listen {
        // Re-apply the port: callers such as restart's stop_ctx override listen_port after resolution.
        Some(Ok(addrs)) => Ok(addrs.iter().map(|a| SocketAddr::new(a.ip(), ctx.listen_port)).collect()),
        Some(Err(e)) => Err(DaemonError::Message(e.clone())),
        None => resolve_listen_targets(&ctx.effective_listen_targets(), ctx.listen_port),
    }
}

pub fn resolve_info_socket_addrs(info: &super::pid::DaemonInfo, port_override: u16) -> Vec<SocketAddr> {
    let port = if port_override > 0 {
        port_override
    } else {
        info.port.unwrap_or(crate::config::DEFAULT_PORT)
    };

    if !info.listen_addrs.is_empty() {
        let mut addrs = Vec::new();
        for raw in &info.listen_addrs {
            if let Ok(mut sa) = raw.parse::<SocketAddr>() {
                if port_override > 0 {
                    sa.set_port(port_override);
                }
                if !addrs.contains(&sa) {
                    addrs.push(sa);
                }
            }
        }
        if !addrs.is_empty() {
            return addrs;
        }
    }

    if let Some(host) = info
        .listen_host
        .as_deref()
        .or(info.listen.as_deref())
        .filter(|h| !h.is_empty())
        && let Ok(addrs) = resolve_listen_targets(&[host.to_string()], port)
        && !addrs.is_empty()
    {
        return addrs;
    }

    vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))]
}

/// Checks whether a TCP connect to `addr` succeeds within 300ms (mapping wildcard addrs to loopback).
pub fn is_addr_listening(addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    let connect_addr = connectable_socket_addr(addr);
    std::net::TcpStream::connect_timeout(&connect_addr, Duration::from_millis(300)).is_ok()
}

pub fn any_addr_listening(addrs: &[SocketAddr]) -> bool {
    addrs.iter().copied().any(is_addr_listening)
}

pub fn all_addrs_listening(addrs: &[SocketAddr]) -> bool {
    !addrs.is_empty() && addrs.iter().copied().all(is_addr_listening)
}

/// Checks whether a TCP connect to 127.0.0.1:port succeeds within 300ms.
pub fn is_port_listening(port: u16) -> bool {
    let port = if port == 0 { crate::config::DEFAULT_PORT } else { port };
    is_addr_listening(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

/// Reports true if a valid agy-pool process is alive and its configured addresses are listening.
pub fn is_daemon_running(pid_file: Option<&Path>, port: u16) -> bool {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };
    let info = match super::pid::get_daemon_info(Some(&resolved)) {
        Some(i) if i.pid > 0 => i,
        _ => return false,
    };
    let addrs = resolve_info_socket_addrs(&info, port);
    any_addr_listening(&addrs)
}

/// Checks if the running daemon has an older version or older binary mtime.
pub fn is_daemon_outdated(pid_file: Option<&Path>, port: u16) -> bool {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };

    let info = match super::pid::get_daemon_info(Some(&resolved)) {
        Some(i) => i,
        None => return false,
    };

    let addrs = resolve_info_socket_addrs(&info, port);
    if !any_addr_listening(&addrs) {
        return false;
    }

    if info.version.is_none() || info.version.as_deref() != Some(crate::VERSION) {
        return true;
    }

    let curr_exe = super::pid::get_entrypoint_path();
    if let Ok(meta) = std::fs::metadata(&curr_exe)
        && let Ok(mod_time) = meta.modified()
        && let Ok(dur) = mod_time.duration_since(std::time::UNIX_EPOCH)
    {
        let curr_mtime = dur.as_secs() as i64;
        if let Some(script_mtime) = info.script_mtime
            && script_mtime > 0
            && curr_mtime > script_mtime
        {
            return true;
        }
    }

    false
}

/// Runs the agy-pool reverse proxy server in the foreground.
pub async fn run_foreground(mut opts: ServerOptions) -> Result<(), DaemonError> {
    let ctx_inst = match opts.instance.take() {
        Some(ctx) => ctx,
        None => {
            let data_dir = opts
                .pid_file
                .as_ref()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(crate::config::data_dir);
            super::instance::resolve_instance_context(Some(&data_dir), None, Some(opts.port), None)?
        }
    };

    let pid_file = opts.pid_file.take().unwrap_or_else(|| ctx_inst.pid_file());
    crate::config::assert_safe_write_path(&pid_file)?;

    let mut port = opts.port;
    if port == 0 && !opts.ephemeral_port {
        port = ctx_inst.listen_port;
    }

    // Fail-closed test safety guard: never bind 8899 or 8901 in test mode!
    if crate::config::is_test_mode() && (port == crate::config::DEFAULT_PORT || port == 8899 || port == 8901) {
        return Err(DaemonError::Message(format!(
            "[FAIL-CLOSED TEST GUARD] refusing to bind production port {} during test mode",
            port
        )));
    }

    let handler = opts
        .handler
        .take()
        .unwrap_or_else(|| Arc::new(crate::proxy::Handler::new()));

    let refresh_interval = opts.quota_refresh_interval.unwrap_or(Duration::from_secs(180));

    // 1. Acquire exclusive non-blocking PID lock
    let pid_lock = super::pid::try_acquire_pid_lock(&pid_file)?;

    // Restore per-model pauses and session affinities
    crate::proxy::load_model_cooldowns();
    crate::proxy::load_affinities();
    let log_cfg = crate::config::load_logging_config(ctx_inst.config_path.as_deref());
    super::log::set_rotate_limits(log_cfg.max_size_bytes, log_cfg.backup_count as i32);
    // Rate-limited log rotation on proxied requests; without it the daemon log grows without bound.
    crate::proxy::set_log_rotator(super::log::daemon_rotate_log);

    // Initialize dual-track logging
    let log_level = std::env::var("AGY_LOG_LEVEL")
        .ok()
        .map(|s| crate::observability::LogLevel::parse_lossy(&s))
        .unwrap_or(crate::observability::LogLevel::Info);
    let log_target = opts.log_file.clone().unwrap_or_else(|| ctx_inst.log_file());
    let _logger_handle = if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_target)
    {
        Some(crate::observability::init_async_logger_with_writer(
            Box::new(file),
            log_level,
        ))
    } else {
        None
    };
    let audit_target = ctx_inst.audit_file();
    let _audit_handle = crate::observability::init_audit_worker(
        audit_target,
        log_cfg.audit_max_size_bytes as u64,
        log_cfg.backup_count as i32,
    );

    // 2. Open TCP listener(s)
    let targets = ctx_inst.effective_listen_targets();
    let resolved_addrs = resolve_listen_targets(&targets, port)?;
    if resolved_addrs.is_empty() {
        return Err(DaemonError::Message("no listen socket addresses resolved".to_string()));
    }

    let mut listeners: Vec<tokio::net::TcpListener> = Vec::with_capacity(resolved_addrs.len());
    let mut actual_port = port;
    for (idx, mut addr) in resolved_addrs.into_iter().enumerate() {
        if idx > 0 && actual_port > 0 {
            addr.set_port(actual_port);
        }
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                drop(listeners);
                return Err(DaemonError::ForeignPortOccupied(format!(
                    "failed to listen on {addr}: {e}"
                )));
            }
        };
        if idx == 0 {
            actual_port = listener.local_addr()?.port();
        }
        listeners.push(listener);
    }

    let bound_addrs: Vec<SocketAddr> = listeners
        .iter()
        .map(|l| l.local_addr())
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(tx) = opts.bound_port_tx.take() {
        let _ = tx.send(actual_port);
    }

    // 3. Write PID metadata atomically with 0600 mode
    let script_mtime = if let Ok(meta) = std::fs::metadata(&ctx_inst.binary_path) {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(|| jiff::Timestamp::now().as_second())
    } else {
        jiff::Timestamp::now().as_second()
    };

    let current_pid = std::process::id() as i32;
    let start_time = jiff::Timestamp::now();
    let primary_host = pick_primary_connect_addr(&bound_addrs)
        .map(|sa| sa.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let listen_addrs: Vec<String> = bound_addrs.iter().map(ToString::to_string).collect();
    let runtime_listen = format_socket_addrs(&bound_addrs);
    crate::dashboard::dto::set_runtime_meta(runtime_listen, start_time);
    let initial_stats = super::runtime::collect_runtime_stats(current_pid, Some(start_time));

    let pid_info = super::pid::DaemonInfo {
        pid: current_pid,
        version: Some(ctx_inst.version.clone()),
        script_mtime: Some(script_mtime),
        config_path: ctx_inst.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        config_hash: ctx_inst.config_hash.clone(),
        data_dir: Some(ctx_inst.data_dir.to_string_lossy().to_string()),
        listen_host: Some(primary_host.clone()),
        listen: Some(primary_host),
        listen_addrs,
        port: Some(actual_port),
        runtime: Some(initial_stats.clone()),
    };

    super::pid::write_pid_file(&pid_file, pid_info)?;

    crate::dashboard::control::set_config_context(Some(crate::dashboard::control::ConfigContext {
        config_path: ctx_inst.config_path.clone(),
        config_hash: ctx_inst.config_hash.clone(),
        pid_file: Some(pid_file.clone()),
        startup: crate::config::get_static_config(),
        restart_required: Vec::new(),
    }));

    let runtime_file = ctx_inst.runtime_file();
    let _ = super::runtime::write_runtime_stats(&runtime_file, &initial_stats);

    // 4. Background quota refresher & proactive token refresh
    let (sweep_stop_tx, mut sweep_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let sweep_task = tokio::spawn(async move {
        let sweep = || {
            if let Ok(pool) = crate::storage::load_pool() {
                let now = jiff::Timestamp::now().as_second() as f64;
                for acc in &pool.accounts {
                    if acc.status == crate::storage::STATUS_DISABLED {
                        continue;
                    }
                    if crate::quota::refresh_needed(acc, now) {
                        let custom_refresher = QUOTA_REFRESHER.read().unwrap().clone();
                        if let Some(r) = custom_refresher {
                            r(acc);
                        }
                    }
                    if !acc.refresh_token.is_empty()
                        && !crate::scheduler::is_restricted(acc)
                        && crate::auth::token_expiring_soon(Some(acc), Duration::from_secs(600))
                    {
                        let custom_pre = TOKEN_PRE_REFRESHER.read().unwrap().clone();
                        if let Some(p) = custom_pre {
                            let _ = p(acc);
                        }
                    }
                }
            }
            if let Ok(db) = crate::storage::StateDb::global() {
                let _ = db.checkpoint_passive();
            }
            crate::proxy::save_affinities();
        };

        sweep();
        let mut ticker = tokio::time::interval(refresh_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut sweep_stop_rx => break,
                _ = ticker.tick() => sweep(),
            }
        }
    });

    // 5. Periodic runtime stats writer with bounded coalescing
    let mut stats_writer = super::runtime::RuntimeStatsWriter::start(
        current_pid,
        start_time,
        runtime_file.clone(),
        super::runtime::RUNTIME_STATS_FLUSH_THROTTLE,
        Duration::from_secs(1),
    );
    let trigger_tx = stats_writer.trigger_tx.clone();
    crate::observability::set_on_in_flight_change(Some(Box::new(move || {
        let _ = trigger_tx.try_send(());
    })));

    // 5b. Token usage: write the in-memory hourly counters to state.db off the request path.
    let token_flush_task = tokio::spawn(async {
        let mut ticker = tokio::time::interval(crate::storage::tokens::FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let _ = tokio::task::spawn_blocking(crate::storage::tokens::flush_token_usage_logged).await;
        }
    });

    // 6. Signal readiness
    if let Some(tx) = opts.ready_tx.take() {
        let _ = tx.send(());
    }

    // 7. HTTP server serve loop with graceful shutdown
    let mut shutdown_rx = opts.shutdown_rx;
    let disable_signals = opts.disable_signals;

    let mut accept_tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let h = Arc::clone(&handler);
        accept_tasks.spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(res) => res,
                    Err(_) => break,
                };
                let _ = stream.set_nodelay(true);
                let io = hyper_util::rt::TokioIo::new(stream);
                let h = Arc::clone(&h);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                        req.extensions_mut().insert(crate::dashboard::PeerAddr(peer));
                        let h = Arc::clone(&h);
                        async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.pipeline_flush(true);
                    let _ = builder.serve_connection(io, service).await;
                });
            }
        });
    }

    tokio::select! {
        // Any accept loop ending means an accept error: exit so the supervisor restarts us (as the single-listener loop did).
        _ = accept_tasks.join_next() => {}
        _ = async {
            if let Some(rx) = shutdown_rx.as_mut() {
                let _ = rx.await;
            } else {
                std::future::pending::<()>().await
            }
        } => {}
        _ = async {
            if !disable_signals {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{signal, SignalKind};
                    if let (Ok(mut sigterm), Ok(mut sigint)) = (
                        signal(SignalKind::terminate()),
                        signal(SignalKind::interrupt()),
                    ) {
                        tokio::select! {
                            _ = sigterm.recv() => {},
                            _ = sigint.recv() => {},
                        }
                    } else {
                        let _ = tokio::signal::ctrl_c().await;
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                }
            } else {
                std::future::pending::<()>().await
            }
        } => {}
    }

    // Abort accept loops first so all TcpListeners drop before teardown finishes.
    accept_tasks.abort_all();
    while accept_tasks.join_next().await.is_some() {}

    // Teardown
    let _ = sweep_stop_tx.send(());
    let _ = sweep_task.await;
    stats_writer.stop().await;
    token_flush_task.abort();
    let _ = token_flush_task.await;
    // shortcut: connections are not drained, so streams cut by shutdown record after this flush and their
    // partial usage is lost; drain connections first if restarts mid-stream become common.
    let _ = tokio::task::spawn_blocking(crate::storage::tokens::flush_token_usage_logged).await;
    crate::observability::clear_on_in_flight_change();
    crate::proxy::save_affinities();

    let _ = super::pid::remove_pid_file_if_owned(&pid_file, current_pid);
    let _ = std::fs::remove_file(&runtime_file);
    drop(pid_lock);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_foreground_lifecycle_and_graceful_shutdown() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("foreground.pid");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (bound_port_tx, bound_port_rx) = tokio::sync::oneshot::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let pf = pid_file.clone();
        let server_handle = tokio::spawn(async move {
            run_foreground(ServerOptions {
                port: 0,
                ephemeral_port: true,
                pid_file: Some(pf),
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx),
                bound_port_tx: Some(bound_port_tx),
                quota_refresh_interval: Some(Duration::from_secs(3600)),
                disable_signals: true,
                instance: None,
                shutdown_rx: Some(shutdown_rx),
            })
            .await
        });

        ready_rx.await.unwrap();
        let bound_port = bound_port_rx.await.unwrap();
        assert!(bound_port > 0);

        // 1. Verify port is listening
        assert!(is_port_listening(bound_port));
        assert!(
            crate::proxy::log_rotator_installed(),
            "run_foreground must wire log rotation, as Go does"
        );

        // 2. Verify PID file was written
        let info = super::super::pid::read_pid_file(&pid_file).unwrap();
        assert_eq!(info.pid, std::process::id() as i32);

        // 2b. Verify runtime stats file was written
        let runtime_file = env.temp_dir.path().join("agy-pool-runtime.json");
        let stats = super::super::runtime::read_runtime_stats(&runtime_file).unwrap();
        assert_eq!(stats.pid, std::process::id() as i32);
        assert!(stats.alloc_bytes > 0);

        // 3. Verify concurrent start on same PID file fails
        let conc_res = run_foreground(ServerOptions {
            port: 0,
            ephemeral_port: true,
            pid_file: Some(pid_file.clone()),
            log_file: None,
            handler: None,
            ready_tx: None,
            bound_port_tx: None,
            quota_refresh_interval: None,
            disable_signals: true,
            instance: None,
            shutdown_rx: None,
        })
        .await;
        assert!(matches!(conc_res, Err(DaemonError::AlreadyRunning)));

        // 4. Trigger graceful shutdown
        let _ = shutdown_tx.send(());
        let res = server_handle.await.unwrap();
        assert!(res.is_ok());

        // 5. Verify listener is closed
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!is_port_listening(bound_port));

        // 6. Verify PID file and runtime stats file were cleaned up
        assert!(!pid_file.exists());
        assert!(!runtime_file.exists());
    }

    #[tokio::test]
    async fn test_run_foreground_refuses_production_port_in_test_mode() {
        let env = crate::daemon::test_env();
        let res = run_foreground(ServerOptions {
            port: 8899,
            ephemeral_port: false,
            pid_file: Some(env.temp_dir.path().join("prod.pid")),
            log_file: None,
            handler: None,
            ready_tx: None,
            bound_port_tx: None,
            quota_refresh_interval: None,
            disable_signals: true,
            instance: None,
            shutdown_rx: None,
        })
        .await;

        assert!(res.is_err());
        let err_msg = format!("{}", res.unwrap_err());
        assert!(err_msg.contains("[FAIL-CLOSED TEST GUARD]"));
    }

    #[tokio::test]
    async fn test_is_daemon_outdated() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("outdated.pid");

        // If port not listening -> false
        assert!(!is_daemon_outdated(Some(&pid_file), 12345));

        // Start dummy server
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (bound_port_tx, bound_port_rx) = tokio::sync::oneshot::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let pf = pid_file.clone();
        let _srv = tokio::spawn(async move {
            let _ = run_foreground(ServerOptions {
                port: 0,
                ephemeral_port: true,
                pid_file: Some(pf),
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx),
                bound_port_tx: Some(bound_port_tx),
                quota_refresh_interval: None,
                disable_signals: true,
                instance: None,
                shutdown_rx: Some(shutdown_rx),
            })
            .await;
        });

        ready_rx.await.unwrap();
        let port = bound_port_rx.await.unwrap();

        // Current daemon not outdated
        assert!(!is_daemon_outdated(Some(&pid_file), port));

        // Overwrite PID file with an older version string
        let _ = super::super::pid::write_pid_file(
            &pid_file,
            super::super::pid::DaemonInfo {
                pid: std::process::id() as i32,
                version: Some("0.0.1-old".to_string()),
                script_mtime: Some(jiff::Timestamp::now().as_second()),
                ..Default::default()
            },
        );
        assert!(is_daemon_outdated(Some(&pid_file), port));

        // Overwrite PID file with an older script_mtime
        let curr_exe = super::super::pid::get_entrypoint_path();
        if let Ok(meta) = std::fs::metadata(&curr_exe)
            && let Ok(mod_time) = meta.modified()
            && let Ok(dur) = mod_time.duration_since(std::time::UNIX_EPOCH)
        {
            let _ = super::super::pid::write_pid_file(
                &pid_file,
                super::super::pid::DaemonInfo {
                    pid: std::process::id() as i32,
                    version: Some(crate::VERSION.to_string()),
                    script_mtime: Some(dur.as_secs() as i64 - 10),
                    ..Default::default()
                },
            );
            assert!(is_daemon_outdated(Some(&pid_file), port));
        }

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_daemon_starts_quota_refresher() {
        let env = crate::daemon::test_env();

        // Seed pool with stale account
        let old_time = jiff::Timestamp::now().as_second() - 500;
        let frac = 0.5;
        let acc = crate::storage::Account {
            id: "acc-stale-refresh".to_string(),
            email: "stale@example.com".to_string(),
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                updated_at: Some(old_time),
                ..Default::default()
            }),
            ..Default::default()
        };
        crate::storage::save_pool(&crate::storage::Pool {
            accounts: vec![acc],
            strategy: "max_quota".to_string(),
            ..Default::default()
        })
        .unwrap();

        let (refreshed_tx, mut refreshed_rx) = tokio::sync::mpsc::channel::<String>(1);
        let _g = set_quota_refresher_for_test(move |a| {
            let _ = refreshed_tx.try_send(a.id.clone());
        });

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let pid_file = env.temp_dir.path().join("refresher.pid");

        let srv = tokio::spawn(async move {
            let _ = run_foreground(ServerOptions {
                port: 0,
                ephemeral_port: true,
                pid_file: Some(pid_file),
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx),
                bound_port_tx: None,
                quota_refresh_interval: Some(Duration::from_millis(20)),
                disable_signals: true,
                instance: None,
                shutdown_rx: Some(shutdown_rx),
            })
            .await;
        });

        ready_rx.await.unwrap();

        let id = tokio::time::timeout(Duration::from_secs(2), refreshed_rx.recv())
            .await
            .expect("timeout waiting for QuotaRefresher")
            .unwrap();
        assert_eq!(id, "acc-stale-refresh");

        let _ = shutdown_tx.send(());
        let _ = srv.await;
    }

    #[tokio::test]
    async fn test_daemon_proactive_token_refresh() {
        let env = crate::daemon::test_env();

        let soon_exp = (jiff::Timestamp::now().as_second() + 180) as f64;
        let acc = crate::storage::Account {
            id: "acc-token-refresh".to_string(),
            email: "expiring@example.com".to_string(),
            access_token: "old-token".to_string(),
            refresh_token: "refresh-secret".to_string(),
            token_expiry: Some(soon_exp),
            ..Default::default()
        };
        let restricted = crate::storage::Account {
            id: "acc-token-restricted".to_string(),
            email: "restricted@example.com".to_string(),
            access_token: "old-token".to_string(),
            refresh_token: "refresh-secret".to_string(),
            token_expiry: Some(soon_exp),
            status: crate::storage::STATUS_AUTH_ERROR.to_string(),
            ..Default::default()
        };

        crate::storage::save_pool(&crate::storage::Pool {
            accounts: vec![acc, restricted],
            strategy: "max_quota".to_string(),
            ..Default::default()
        })
        .unwrap();

        let (refreshed_tx, mut refreshed_rx) = tokio::sync::mpsc::channel::<String>(1);
        let _g = set_token_pre_refresher_for_test(move |a| {
            if a.id == "acc-token-restricted" {
                panic!("restricted account should be skipped");
            }
            let _ = refreshed_tx.try_send(a.id.clone());
            Ok(())
        });

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let pid_file = env.temp_dir.path().join("pre_refresh.pid");

        let srv = tokio::spawn(async move {
            let _ = run_foreground(ServerOptions {
                port: 0,
                ephemeral_port: true,
                pid_file: Some(pid_file),
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx),
                bound_port_tx: None,
                quota_refresh_interval: Some(Duration::from_millis(20)),
                disable_signals: true,
                instance: None,
                shutdown_rx: Some(shutdown_rx),
            })
            .await;
        });

        ready_rx.await.unwrap();

        let id = tokio::time::timeout(Duration::from_secs(2), refreshed_rx.recv())
            .await
            .expect("timeout waiting for TokenPreRefresher")
            .unwrap();
        assert_eq!(id, "acc-token-refresh");

        let _ = shutdown_tx.send(());
        let _ = srv.await;
    }

    fn find_dual_free_port() -> u16 {
        for _ in 0..20 {
            let l1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l1.local_addr().unwrap().port();
            if port != crate::config::DEFAULT_PORT
                && port != 8899
                && port != 8901
                && let Ok(l2) = std::net::TcpListener::bind(("127.0.0.2", port))
            {
                drop(l2);
                drop(l1);
                return port;
            }
        }
        panic!("could not find a free port bindable on both 127.0.0.1 and 127.0.0.2");
    }

    #[test]
    fn test_resolve_listen_targets_interfaces_and_fail_closed() {
        #[cfg(target_os = "linux")]
        {
            let lo_addrs = resolve_listen_targets(&["lo".to_string()], 8890).unwrap();
            assert!(
                lo_addrs.contains(&SocketAddr::from(([127, 0, 0, 1], 8890))),
                "lo should resolve 127.0.0.1:8890, got {lo_addrs:?}"
            );
            assert!(
                !lo_addrs.iter().any(|a| a.ip().is_unspecified()),
                "lo must not contain unspecified address: {lo_addrs:?}"
            );
            assert!(
                !lo_addrs
                    .iter()
                    .any(|a| matches!(a.ip(), IpAddr::V6(v6) if v6.is_unicast_link_local())),
                "lo must skip IPv6 link-local addresses: {lo_addrs:?}"
            );

            let dedup = resolve_listen_targets(&["lo".to_string(), "127.0.0.1".to_string()], 8890).unwrap();
            assert_eq!(dedup, lo_addrs, "duplicate 127.0.0.1 must be deduplicated");
        }

        for combo in [["0.0.0.0", "::"], ["0.0.0.0", "127.0.0.1"], ["::", "localhost"]] {
            let targets: Vec<String> = combo.iter().map(|s| s.to_string()).collect();
            let err = resolve_listen_targets(&targets, 8890).unwrap_err();
            assert!(err.to_string().contains("cannot be combined"), "{combo:?}: {err}");
        }
        assert_eq!(
            resolve_listen_targets(&["localhost".to_string()], 8890).unwrap(),
            vec![SocketAddr::from(([127, 0, 0, 1], 8890))]
        );

        let err = resolve_listen_targets(&["agy_no_such0".to_string()], 8890).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "missing interface must fail closed with clear error, got: {err}"
        );

        let multi = resolve_listen_targets(&["127.0.0.1".to_string(), "::1".to_string()], 8890).unwrap();
        assert_eq!(
            multi,
            vec![
                SocketAddr::from(([127, 0, 0, 1], 8890)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, 8890))
            ]
        );
        assert_eq!(
            pick_primary_connect_addr(&multi),
            Some(SocketAddr::from(([127, 0, 0, 1], 8890)))
        );
        assert_eq!(
            pick_primary_connect_addr(&[SocketAddr::from(([0, 0, 0, 0], 8890))]),
            Some(SocketAddr::from(([127, 0, 0, 1], 8890)))
        );
    }

    #[tokio::test]
    async fn test_run_foreground_multi_listen_and_non_loopback_liveness() {
        let env = crate::daemon::test_env();
        let data_dir = env.temp_dir.path().join("multi-listen-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let cfg_path = env.temp_dir.path().join("multi-listen.json");
        let port = find_dual_free_port();

        std::fs::write(
            &cfg_path,
            format!(r#"{{"version": 1, "server": {{"listen": ["127.0.0.1", "127.0.0.2"], "port": {port}}}}}"#),
        )
        .unwrap();

        let ctx = super::super::instance::resolve_instance_context(Some(&data_dir), Some(&cfg_path), Some(port), None)
            .unwrap();
        assert_eq!(
            ctx.listen_targets,
            vec!["127.0.0.1".to_string(), "127.0.0.2".to_string()]
        );

        let pid_file = ctx.pid_file();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (bound_port_tx, bound_port_rx) = tokio::sync::oneshot::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let srv_ctx = ctx.clone();
        let srv = tokio::spawn(async move {
            run_foreground(ServerOptions {
                port,
                ephemeral_port: false,
                pid_file: None,
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx),
                bound_port_tx: Some(bound_port_tx),
                quota_refresh_interval: Some(Duration::from_secs(3600)),
                disable_signals: true,
                instance: Some(srv_ctx),
                shutdown_rx: Some(shutdown_rx),
            })
            .await
        });

        ready_rx.await.unwrap();
        assert_eq!(bound_port_rx.await.unwrap(), port);

        for host in ["127.0.0.1", "127.0.0.2"] {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(format!("{host}:{port}")).await.unwrap();
            stream
                .write_all(
                    format!("GET /healthz HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut buf = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut buf))
                .await
                .expect("timeout reading /healthz response");
            assert!(
                buf.starts_with("HTTP/1.1 200") && buf.contains("{\"status\":\"ok\"}"),
                "expected 200 /healthz on {host}:{port}, got: {buf}"
            );

            // Dashboard API must accept each bound IP as Host/Origin and report every endpoint.
            let mut stream = tokio::net::TcpStream::connect(format!("{host}:{port}")).await.unwrap();
            stream
                .write_all(
                    format!(
                        "GET /api/v1/status HTTP/1.1\r\nHost: {host}:{port}\r\nOrigin: http://{host}:{port}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut buf = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut buf))
                .await
                .expect("timeout reading /api/v1/status response");
            assert!(
                buf.starts_with("HTTP/1.1 200")
                    && buf.contains(&format!("\"listen\":\"127.0.0.1:{port}, 127.0.0.2:{port}\"")),
                "expected 200 /api/v1/status listing both endpoints on {host}:{port}, got: {buf}"
            );
        }

        let info = super::super::pid::read_pid_file(&pid_file).unwrap();
        assert_eq!(
            info.listen_addrs,
            vec![format!("127.0.0.1:{port}"), format!("127.0.0.2:{port}")]
        );
        assert_eq!(
            crate::dashboard::dto::get_runtime_meta().0,
            format!("127.0.0.1:{port}, 127.0.0.2:{port}")
        );
        assert!(is_daemon_running(Some(&pid_file), port));
        let (status, _, _) = super::super::instance::check_instance_status(&ctx);
        assert_eq!(status, super::super::instance::InstanceStatus::RunningSameInstance);

        let _ = shutdown_tx.send(());
        srv.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!is_addr_listening(SocketAddr::from(([127, 0, 0, 1], port))));
        assert!(!is_addr_listening(SocketAddr::from(([127, 0, 0, 2], port))));

        // Now verify single non-127.0.0.1 listen address works when 127.0.0.1:port is unbound.
        let port2 = find_dual_free_port();
        let data_dir2 = env.temp_dir.path().join("only-127-0-0-2");
        std::fs::create_dir_all(&data_dir2).unwrap();
        let cfg2 = env.temp_dir.path().join("only-127-0-0-2.json");
        std::fs::write(
            &cfg2,
            format!(r#"{{"version": 1, "server": {{"listen": "127.0.0.2", "port": {port2}}}}}"#),
        )
        .unwrap();
        let ctx2 =
            super::super::instance::resolve_instance_context(Some(&data_dir2), Some(&cfg2), Some(port2), None).unwrap();
        let pid_file2 = ctx2.pid_file();

        let (ready_tx2, ready_rx2) = tokio::sync::oneshot::channel::<()>();
        let (shutdown_tx2, shutdown_rx2) = tokio::sync::oneshot::channel::<()>();
        let srv_ctx2 = ctx2.clone();
        let srv2 = tokio::spawn(async move {
            run_foreground(ServerOptions {
                port: port2,
                ephemeral_port: false,
                pid_file: None,
                log_file: None,
                handler: None,
                ready_tx: Some(ready_tx2),
                bound_port_tx: None,
                quota_refresh_interval: Some(Duration::from_secs(3600)),
                disable_signals: true,
                instance: Some(srv_ctx2),
                shutdown_rx: Some(shutdown_rx2),
            })
            .await
        });

        ready_rx2.await.unwrap();
        assert!(
            !is_port_listening(port2),
            "127.0.0.1:{port2} must NOT be bound when listen is 127.0.0.2"
        );
        assert!(is_daemon_running(Some(&pid_file2), port2));
        assert!(!is_daemon_outdated(Some(&pid_file2), port2));
        let (status2, info2, _) = super::super::instance::check_instance_status(&ctx2);
        assert_eq!(status2, super::super::instance::InstanceStatus::RunningSameInstance);
        assert_eq!(ctx2.gateway_url(info2.as_ref()), format!("http://127.0.0.2:{port2}"));

        let _ = shutdown_tx2.send(());
        srv2.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_run_foreground_partial_bind_failure_rolls_back() {
        let env = crate::daemon::test_env();
        let data_dir = env.temp_dir.path().join("partial-bind");
        std::fs::create_dir_all(&data_dir).unwrap();
        let port = find_dual_free_port();

        // Occupy only the second address (127.0.0.2); 127.0.0.1 stays free.
        let occupied = std::net::TcpListener::bind(("127.0.0.2", port)).unwrap();

        let cfg_path = env.temp_dir.path().join("partial.json");
        std::fs::write(
            &cfg_path,
            format!(r#"{{"version": 1, "server": {{"listen": ["127.0.0.1", "127.0.0.2"], "port": {port}}}}}"#),
        )
        .unwrap();
        let ctx = super::super::instance::resolve_instance_context(Some(&data_dir), Some(&cfg_path), Some(port), None)
            .unwrap();
        let pid_file = ctx.pid_file();

        let res = run_foreground(ServerOptions {
            port,
            ephemeral_port: false,
            pid_file: Some(pid_file.clone()),
            log_file: None,
            handler: None,
            ready_tx: None,
            bound_port_tx: None,
            quota_refresh_interval: None,
            disable_signals: true,
            instance: Some(ctx),
            shutdown_rx: None,
        })
        .await;

        assert!(matches!(res, Err(DaemonError::ForeignPortOccupied(_))));
        assert!(
            !is_addr_listening(SocketAddr::from(([127, 0, 0, 1], port))),
            "partial bind failure must close already-bound 127.0.0.1 socket"
        );
        assert!(!pid_file.exists(), "failed bind must not write PID file");
        drop(occupied);
    }
}
