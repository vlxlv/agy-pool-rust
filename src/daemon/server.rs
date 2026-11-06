use std::net::SocketAddr;
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

/// Checks whether a TCP connect to 127.0.0.1:port succeeds within 300ms.
pub fn is_port_listening(port: u16) -> bool {
    let port = if port == 0 { crate::config::DEFAULT_PORT } else { port };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Ok(stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        drop(stream);
        true
    } else {
        false
    }
}

/// Reports true if a valid agy-pool process is alive and its port is listening.
pub fn is_daemon_running(pid_file: Option<&Path>, port: u16) -> bool {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };
    let info = match super::pid::get_daemon_info(Some(&resolved)) {
        Some(i) if i.pid > 0 => i,
        _ => return false,
    };
    let port = if port == 0 {
        info.port.unwrap_or(crate::config::DEFAULT_PORT)
    } else {
        port
    };
    is_port_listening(port)
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

    let port = if port == 0 {
        info.port.unwrap_or(crate::config::DEFAULT_PORT)
    } else {
        port
    };

    if !is_port_listening(port) {
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
    // Rate-limited log rotation on proxied requests; without it the daemon log grows without bound.
    crate::proxy::set_log_rotator(|| {
        super::log::maybe_rotate_log();
    });

    // 2. Open TCP listener
    let addr_str = format!("{}:{}", ctx_inst.listen_host, port);
    let listener = match tokio::net::TcpListener::bind(&addr_str).await {
        Ok(l) => l,
        Err(e) => {
            return Err(DaemonError::ForeignPortOccupied(format!(
                "failed to listen on {addr_str}: {e}"
            )));
        }
    };

    let actual_port = listener.local_addr()?.port();
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
    let initial_stats = super::runtime::collect_runtime_stats(current_pid, Some(start_time));

    let pid_info = super::pid::DaemonInfo {
        pid: current_pid,
        version: Some(ctx_inst.version.clone()),
        script_mtime: Some(script_mtime),
        config_path: ctx_inst.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        config_hash: ctx_inst.config_hash.clone(),
        data_dir: Some(ctx_inst.data_dir.to_string_lossy().to_string()),
        listen_host: Some(ctx_inst.listen_host.clone()),
        listen: Some(ctx_inst.listen_host.clone()),
        port: Some(actual_port),
        runtime: Some(initial_stats.clone()),
    };

    super::pid::write_pid_file(&pid_file, pid_info)?;

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

    // 6. Signal readiness
    if let Some(tx) = opts.ready_tx.take() {
        let _ = tx.send(());
    }

    // 7. HTTP server serve loop with graceful shutdown
    let mut shutdown_rx = opts.shutdown_rx;
    let disable_signals = opts.disable_signals;

    let serve_loop = async {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(res) => res,
                Err(_) => break,
            };
            let _ = stream.set_nodelay(true);
            let io = hyper_util::rt::TokioIo::new(stream);
            let h = Arc::clone(&handler);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let h = Arc::clone(&h);
                    async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                });
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.pipeline_flush(true);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    };

    tokio::select! {
        _ = serve_loop => {}
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

    // Teardown
    let _ = sweep_stop_tx.send(());
    let _ = sweep_task.await;
    stats_writer.stop().await;
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
}
