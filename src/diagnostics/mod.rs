#![allow(clippy::collapsible_if)]

pub mod effectiveness;
pub mod routing_audit;
pub mod routing_audit_eval;

pub use effectiveness::*;
pub use routing_audit::*;
pub use routing_audit_eval::*;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

// ANSI color codes matching Go reference
pub const CLR_RESET: &str = "\x1b[0m";
pub const CLR_BOLD: &str = "\x1b[1m";
pub const CLR_DIM: &str = "\x1b[2m";
pub const CLR_GREEN: &str = "\x1b[32m";
pub const CLR_YELLOW: &str = "\x1b[33m";
pub const CLR_BLUE: &str = "\x1b[34m";
pub const CLR_MAGENTA: &str = "\x1b[35m";
pub const CLR_CYAN: &str = "\x1b[36m";
pub const CLR_RED: &str = "\x1b[31m";

pub const SOURCE_ENV_AGY_BIN: &str = "AGY_BIN";
pub const SOURCE_CONFIG: &str = "config";
pub const SOURCE_MANAGED: &str = "managed native";
pub const SOURCE_PREFIX: &str = "PREFIX";
pub const SOURCE_SYSTEM: &str = "system path";
pub const SOURCE_USER_LOCAL: &str = "~/.local/bin";
pub const SOURCE_PATH_ENV: &str = "PATH";

pub const ERR_NATIVE_AGY_NOT_FOUND: &str =
    "Native agy was not found.\nInstall the original Antigravity CLI first, then rerun agy-pool installer.";

pub type AgyBinaryFinder = Arc<dyn Fn() -> String + Send + Sync>;
pub type AgyEntrypointFinder = Arc<dyn Fn() -> String + Send + Sync>;
pub type AgyVersionProvider = Arc<dyn Fn() -> String + Send + Sync>;
pub type BackendHostProvider = Arc<dyn Fn() -> String + Send + Sync>;
pub type TlsProber = Arc<dyn Fn(&str, Duration) -> Result<(), String> + Send + Sync>;

static AGY_BINARY_FINDER: LazyLock<RwLock<Option<AgyBinaryFinder>>> = LazyLock::new(|| RwLock::new(None));
static AGY_ENTRYPOINT_FINDER: LazyLock<RwLock<Option<AgyEntrypointFinder>>> = LazyLock::new(|| RwLock::new(None));
static AGY_VERSION_PROVIDER: LazyLock<RwLock<Option<AgyVersionProvider>>> = LazyLock::new(|| RwLock::new(None));
static BACKEND_HOST_PROVIDER: LazyLock<RwLock<Option<BackendHostProvider>>> = LazyLock::new(|| RwLock::new(None));
static TLS_PROBER: LazyLock<RwLock<Option<TlsProber>>> = LazyLock::new(|| RwLock::new(None));

pub fn set_agy_binary_finder(f: Option<AgyBinaryFinder>) {
    let mut w = AGY_BINARY_FINDER.write().unwrap();
    *w = f;
}

pub fn set_agy_entrypoint_finder(f: Option<AgyEntrypointFinder>) {
    let mut w = AGY_ENTRYPOINT_FINDER.write().unwrap();
    *w = f;
}

pub fn set_agy_version_provider(f: Option<AgyVersionProvider>) {
    let mut w = AGY_VERSION_PROVIDER.write().unwrap();
    *w = f;
}

pub fn set_backend_host_provider(f: Option<BackendHostProvider>) {
    let mut w = BACKEND_HOST_PROVIDER.write().unwrap();
    *w = f;
}

pub fn set_tls_prober(f: Option<TlsProber>) {
    let mut w = TLS_PROBER.write().unwrap();
    *w = f;
}

fn get_effective_user_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if crate::config::is_test_mode() {
        let is_empty_or_real = match &home {
            None => true,
            Some(h) => h.as_os_str().is_empty() || h == &crate::config::real_host_user_home(),
        };
        if is_empty_or_real {
            if let Some(sandbox) = crate::config::get_synthetic_sandbox_root() {
                return Some(sandbox.join("test_home"));
            }
        }
    }
    if let Some(h) = home {
        if !h.as_os_str().is_empty() {
            return Some(h);
        }
    }
    Some(crate::config::real_host_user_home())
}

fn get_self_paths() -> HashSet<PathBuf> {
    let mut paths = HashSet::new();

    // Current executable
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(real_exe) = std::fs::canonicalize(&exe) {
            paths.insert(real_exe);
        }
        paths.insert(exe);
    }

    // argv[0]
    if let Some(arg0) = std::env::args().next() {
        if !arg0.is_empty() {
            let p = PathBuf::from(arg0);
            if let Ok(real_arg) = std::fs::canonicalize(&p) {
                paths.insert(real_arg);
            }
            paths.insert(p);
        }
    }

    // Common agy-pool locations
    if let Some(home) = get_effective_user_home() {
        let p = home.join(".local/bin/agy-pool");
        if let Ok(real_p) = std::fs::canonicalize(&p) {
            paths.insert(real_p);
        }
        paths.insert(p);
    }

    let p_usr = PathBuf::from("/usr/local/bin/agy-pool");
    if let Ok(real_p) = std::fs::canonicalize(&p_usr) {
        paths.insert(real_p);
    }
    paths.insert(p_usr);

    if let Ok(prefix) = std::env::var("PREFIX") {
        if !prefix.is_empty() {
            let p_pre = PathBuf::from(&prefix).join("bin/agy-pool");
            if let Ok(real_p) = std::fs::canonicalize(&p_pre) {
                paths.insert(real_p);
            }
            paths.insert(p_pre);
        }
    }

    if let Ok(explicit) = std::env::var("AGY_SELF_PATH") {
        if !explicit.is_empty() {
            let p_exp = PathBuf::from(&explicit);
            if let Ok(real_exp) = std::fs::canonicalize(&p_exp) {
                paths.insert(real_exp);
            }
            paths.insert(p_exp);
        }
    }

    paths
}

/// Reports whether path is the agy-pool binary itself, an agy/agy-raw/agy-orig shim,
/// a hard link to one of them, or a symlink resolving to one.
/// The Go and Rust builds are both installed as `agy-pool`, so one name covers both.
pub fn is_shim_or_self(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return false;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    let cand_meta = std::fs::metadata(&abs);

    // 1. Check known self paths
    let self_paths = get_self_paths();
    if self_paths.contains(&abs) {
        return true;
    }
    let real_path = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
    if self_paths.contains(&real_path) {
        return true;
    }
    if let Some(name) = real_path.file_name().and_then(|s| s.to_str()) {
        if name == "agy-pool" {
            return true;
        }
    }

    // 2. Inode / SameFile check against running executable and known self paths
    if let Ok(cand_fi) = &cand_meta {
        use std::os::unix::fs::MetadataExt;
        if let Ok(exe) = std::env::current_exe() {
            if let Ok(exe_fi) = std::fs::metadata(&exe) {
                if cand_fi.dev() == exe_fi.dev() && cand_fi.ino() == exe_fi.ino() {
                    return true;
                }
            }
        }
        for sp in &self_paths {
            if let Ok(sp_fi) = std::fs::metadata(sp) {
                if cand_fi.dev() == sp_fi.dev() && cand_fi.ino() == sp_fi.ino() {
                    return true;
                }
            }
        }
    }

    // 3. Inspect file header for exact agy-pool shim signatures (narrow content fallback)
    if let Ok(mut f) = std::fs::File::open(&real_path) {
        use std::io::Read;
        let mut buf = [0u8; 1024];
        if let Ok(n) = f.read(&mut buf) {
            if n > 0 {
                let header = String::from_utf8_lossy(&buf[..n]);
                if header.contains("# agy-pool managed shim v1")
                    || header.contains("# >>> agy-pool integration >>>")
                    || header.contains("exec agy-pool run")
                    || header.contains("exec agy-pool raw")
                {
                    return true;
                }
            }
        }
    }

    false
}

pub fn validate_native_candidate(cand: impl AsRef<Path>) -> Result<(), String> {
    let cand_path = cand.as_ref();
    if cand_path.as_os_str().is_empty() || cand_path.to_string_lossy().trim().is_empty() {
        return Err("empty path".to_string());
    }
    let abs = if cand_path.is_absolute() {
        cand_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(cand_path))
            .unwrap_or_else(|_| cand_path.to_path_buf())
    };

    let meta = std::fs::metadata(&abs).map_err(|e| format!("file not found or inaccessible: {}", e))?;
    if meta.is_dir() {
        return Err("candidate path is a directory".to_string());
    }
    use std::os::unix::fs::PermissionsExt;
    if (meta.permissions().mode() & 0o111) == 0 {
        return Err("candidate file is not executable".to_string());
    }
    if is_shim_or_self(&abs) {
        return Err("candidate is agy-pool binary, hard link, or managed shim (recursion prevented)".to_string());
    }

    Ok(())
}

fn get_managed_native_candidates() -> Vec<PathBuf> {
    let mut managed_candidates = Vec::new();
    if let Ok(explicit) = std::env::var("AGY_MANAGED_NATIVE_PATH") {
        if !explicit.is_empty() {
            managed_candidates.push(PathBuf::from(explicit));
        }
    }
    if let Ok(prefix) = std::env::var("PREFIX") {
        if !prefix.is_empty() {
            managed_candidates.push(PathBuf::from(prefix).join("libexec/agy-pool/agy-native"));
        }
    }
    if crate::config::is_test_mode() {
        if let Some(sandbox) = crate::config::get_synthetic_sandbox_root() {
            managed_candidates.push(sandbox.join("test_home/.local/libexec/agy-pool/agy-native"));
            managed_candidates.push(sandbox.join("test_system/usr/local/libexec/agy-pool/agy-native"));
        }
    }
    if let Some(home) = get_effective_user_home() {
        managed_candidates.push(home.join(".local/libexec/agy-pool/agy-native"));
    }
    managed_candidates.push(PathBuf::from("/usr/local/libexec/agy-pool/agy-native"));
    managed_candidates
}

pub fn resolve_native_agy_binary() -> Result<(PathBuf, String), String> {
    if let Some(ref finder) = *AGY_BINARY_FINDER.read().unwrap() {
        let p = finder();
        if !p.is_empty() {
            return Ok((PathBuf::from(p), "mock".to_string()));
        }
        return Err(ERR_NATIVE_AGY_NOT_FOUND.to_string());
    }

    // 1. AGY_BIN environment override (highest priority)
    if let Ok(explicit) = std::env::var("AGY_BIN") {
        if !explicit.is_empty() {
            if let Err(err) = validate_native_candidate(&explicit) {
                return Err(format!("explicit AGY_BIN override {:?} is invalid: {}", explicit, err));
            }
            let clean = if Path::new(&explicit).is_absolute() {
                PathBuf::from(explicit)
            } else {
                std::env::current_dir().unwrap_or_default().join(explicit)
            };
            return Ok((clean, SOURCE_ENV_AGY_BIN.to_string()));
        }
    }

    // 2. Configured native_agy.binary
    let cfg = crate::config::get_static_config();
    if let Some(cand) = &cfg.native_agy.binary {
        if !cand.is_empty() {
            if let Err(err) = validate_native_candidate(cand) {
                return Err(format!(
                    "explicit config native_agy.binary {:?} is invalid: {}",
                    cand, err
                ));
            }
            let clean = if Path::new(cand).is_absolute() {
                PathBuf::from(cand)
            } else {
                std::env::current_dir().unwrap_or_default().join(cand)
            };
            return Ok((clean, SOURCE_CONFIG.to_string()));
        }
    }

    // 3. Managed native binary
    for cand in get_managed_native_candidates() {
        if cand.as_os_str().is_empty() {
            continue;
        }
        let clean = if cand.is_absolute() {
            cand
        } else {
            std::env::current_dir().unwrap_or_default().join(cand)
        };
        if validate_native_candidate(&clean).is_ok() {
            return Ok((clean, SOURCE_MANAGED.to_string()));
        }
    }

    // Fallback discovery in documented order
    struct FallbackCandidate {
        path: PathBuf,
        source: &'static str,
    }
    let mut fallbacks = Vec::new();

    if let Ok(prefix) = std::env::var("PREFIX") {
        if !prefix.is_empty() {
            fallbacks.push(FallbackCandidate {
                path: PathBuf::from(prefix).join("bin/agy"),
                source: SOURCE_PREFIX,
            });
        }
    }

    fallbacks.push(FallbackCandidate {
        path: PathBuf::from("/data/data/com.termux/files/usr/bin/agy"),
        source: SOURCE_SYSTEM,
    });
    fallbacks.push(FallbackCandidate {
        path: PathBuf::from("/usr/local/bin/agy"),
        source: SOURCE_SYSTEM,
    });
    fallbacks.push(FallbackCandidate {
        path: PathBuf::from("/usr/bin/agy"),
        source: SOURCE_SYSTEM,
    });

    if let Some(home) = get_effective_user_home() {
        fallbacks.push(FallbackCandidate {
            path: home.join(".local/bin/agy"),
            source: SOURCE_USER_LOCAL,
        });
    }

    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            if !dir.as_os_str().is_empty() {
                fallbacks.push(FallbackCandidate {
                    path: dir.join("agy"),
                    source: SOURCE_PATH_ENV,
                });
            }
        }
    }

    let mut seen_paths = HashSet::new();

    for cand in fallbacks {
        if cand.path.as_os_str().is_empty() {
            continue;
        }
        let clean = if cand.path.is_absolute() {
            cand.path
        } else {
            std::env::current_dir().unwrap_or_default().join(cand.path)
        };
        let real_path = std::fs::canonicalize(&clean).unwrap_or_else(|_| clean.clone());
        if seen_paths.contains(&clean) || seen_paths.contains(&real_path) {
            continue;
        }
        seen_paths.insert(clean.clone());
        seen_paths.insert(real_path);

        if validate_native_candidate(&clean).is_err() {
            continue;
        }

        return Ok((clean, cand.source.to_string()));
    }

    Err(ERR_NATIVE_AGY_NOT_FOUND.to_string())
}

pub fn find_real_agy_binary() -> Option<PathBuf> {
    if let Some(ref finder) = *AGY_BINARY_FINDER.read().unwrap() {
        let p = finder();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
        return None;
    }

    resolve_native_agy_binary().ok().map(|(p, _)| p)
}

pub fn installed_agy_version() -> String {
    if let Some(ref provider) = *AGY_VERSION_PROVIDER.read().unwrap() {
        return provider();
    }

    if let Some(agy_bin) = find_real_agy_binary()
        && let Some(v) = agy_version_output(&agy_bin, Duration::from_secs(2))
    {
        return v;
    }

    "1.2.2".to_string()
}

/// Runs `<bin> --version`, killing it after `timeout` like Go's exec.CommandContext, so a hung agy cannot hang doctor.
fn agy_version_output(bin: &Path, timeout: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(Some(_)) => return None,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let v = out.trim().to_string();
    (!v.is_empty() && v.contains('.')).then_some(v)
}

pub fn get_installed_agy_version() -> String {
    installed_agy_version()
}

fn default_tls_probe(host: &str, timeout: Duration) -> Result<(), String> {
    if crate::config::is_test_mode() {
        return Err("network probe disabled in test mode".to_string());
    }

    tls_probe(host, 443, timeout)
}

/// Completes a verified TLS handshake like Go's tls.DialWithDialer; a bare TCP connect would pass behind a
/// TLS-intercepting proxy or with a broken trust store.
fn tls_probe(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    use tokio_rustls::rustls;
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve error: {e}"))?
        .next()
        .ok_or_else(|| format!("no address found for {host}"))?;
    let mut tcp =
        std::net::TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connection failed: {e}"))?;
    tcp.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let mut conn = rustls::ClientConnection::new(crate::upstream::client_config(), name).map_err(|e| e.to_string())?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)
            .map_err(|e| format!("tls handshake failed: {e}"))?;
    }
    Ok(())
}

pub fn lease_fallback_rate_high(fallbacks: u64, decisions: u64) -> bool {
    fallbacks * 100 > decisions
}

pub fn run_doctor(out: &mut impl Write) -> bool {
    let sep = "=".repeat(68);
    let sub_sep = "-".repeat(68);

    let _ = writeln!(out, "\n{}", sep);
    let title = format!("Antigravity System Doctor (agy-pool v{})", crate::VERSION);
    let padding = (68 + title.len()) / 2;
    let _ = writeln!(out, "{}{:width$}{}", CLR_BOLD, title, CLR_RESET, width = padding);
    let _ = writeln!(out, "{}\n", sep);

    let mut issues_found = 0;
    let mut warnings_found = 0;

    // 1. Rust Runtime Environment & Concurrency
    let runtime_ver = format!("rust {}", crate::VERSION);
    let _ = writeln!(
        out,
        "  {}✓{} Rust Runtime: {}{}{} ({} {})",
        CLR_GREEN,
        CLR_RESET,
        CLR_BOLD,
        runtime_ver,
        CLR_RESET,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(
        out,
        "  {}✓{} POSIX Concurrency: flock multi-process file locking available",
        CLR_GREEN, CLR_RESET
    );

    // 2. Native Antigravity Executable (agy)
    let res = resolve_native_agy_binary();
    let real_agy = match &res {
        Ok((p, src)) => {
            let agy_ver = installed_agy_version();
            let _ = writeln!(
                out,
                "  {}✓{} Native Binary: {}{}{} (v{}, source: {})",
                CLR_GREEN,
                CLR_RESET,
                CLR_BOLD,
                p.display(),
                CLR_RESET,
                agy_ver,
                src
            );
            p.display().to_string()
        }
        Err(err) => {
            let _ = writeln!(out, "  {}⚠{} Native Binary: {}", CLR_YELLOW, CLR_RESET, err);
            let _ = writeln!(
                out,
                "    {}↳ Set AGY_BIN=/path/to/agy if installed in custom location.{}",
                CLR_DIM, CLR_RESET
            );
            warnings_found += 1;
            String::new()
        }
    };

    // 2b. Native Shim Integration
    let finder_hook_active = AGY_BINARY_FINDER.read().unwrap().is_some();
    let ep_hook = AGY_ENTRYPOINT_FINDER.read().unwrap().clone();

    if finder_hook_active {
        let _ = writeln!(
            out,
            "  {}✓{} Native Integration: Mock integration active ({})",
            CLR_GREEN, CLR_RESET, real_agy
        );
    } else {
        let home = get_effective_user_home();
        let mut entrypoint = String::new();

        if let Some(ref ep_fn) = ep_hook {
            entrypoint = ep_fn();
        } else if let Ok(ep) = std::env::var("AGY_ENTRYPOINT_PATH") {
            if !ep.is_empty() {
                entrypoint = ep;
            }
        }

        if entrypoint.is_empty() {
            if let Some(ref h) = home {
                let cand = h.join(".local/bin/agy");
                if std::fs::symlink_metadata(&cand).is_ok() {
                    entrypoint = cand.display().to_string();
                }
            }
            if entrypoint.is_empty() && crate::config::is_test_mode() {
                if let Some(sandbox) = crate::config::get_synthetic_sandbox_root() {
                    let cand = sandbox.join("test_home/.local/bin/agy");
                    if std::fs::symlink_metadata(&cand).is_ok() {
                        entrypoint = cand.display().to_string();
                    }
                }
            }
            if entrypoint.is_empty() && !crate::config::is_test_mode() {
                if let Ok(path_env) = std::env::var("PATH") {
                    for dir in std::env::split_paths(&path_env) {
                        let cand = dir.join("agy");
                        if cand.is_file() {
                            entrypoint = cand.display().to_string();
                            break;
                        }
                    }
                }
            }
        }

        if entrypoint.is_empty() {
            let _ = writeln!(
                out,
                "  {}○{} Native Integration: No agy entrypoint found (run 'agy-pool install' to set up)",
                CLR_BLUE, CLR_RESET
            );
        } else if !is_shim_or_self(&entrypoint) {
            let _ = writeln!(
                out,
                "  {}✖{} Native Integration: {} is a native binary bypassing agy-pool",
                CLR_RED, CLR_RESET, entrypoint
            );
            let _ = writeln!(
                out,
                "    {}↳ Run 'agy-pool repair' (or 'agy-pool install') to restore the managed agy shim.{}",
                CLR_DIM, CLR_RESET
            );
            issues_found += 1;
        } else {
            let mut managed_path: Option<PathBuf> = None;
            for cand in get_managed_native_candidates() {
                if !cand.as_os_str().is_empty() && std::fs::symlink_metadata(&cand).is_ok() {
                    managed_path = Some(cand);
                    break;
                }
            }

            let mut broken = false;
            if let Some(mp) = &managed_path {
                if is_shim_or_self(mp) {
                    let _ = writeln!(
                        out,
                        "  {}✖{} Native Integration: Managed native {} resolves to shim/self",
                        CLR_RED,
                        CLR_RESET,
                        mp.display()
                    );
                    issues_found += 1;
                    broken = true;
                } else if let Err(err) = validate_native_candidate(mp) {
                    let _ = writeln!(
                        out,
                        "  {}✖{} Native Integration: Managed native {} is not executable or invalid: {}",
                        CLR_RED,
                        CLR_RESET,
                        mp.display(),
                        err
                    );
                    issues_found += 1;
                    broken = true;
                }
            } else {
                let _ = writeln!(
                    out,
                    "  {}✖{} Native Integration: {} is a managed shim but managed native agy is missing",
                    CLR_RED, CLR_RESET, entrypoint
                );
                let _ = writeln!(
                    out,
                    "    {}↳ Run 'agy-pool repair' (or 'agy-pool install') to restore managed native agy.{}",
                    CLR_DIM, CLR_RESET
                );
                issues_found += 1;
                broken = true;
            }

            if res.is_err() {
                let _ = writeln!(
                    out,
                    "  {}✖{} Native Integration: {} is a managed shim but no valid native agy can be resolved",
                    CLR_RED, CLR_RESET, entrypoint
                );
                let _ = writeln!(
                    out,
                    "    {}↳ Run 'agy-pool repair' (or 'agy-pool install') or verify native agy installation.{}",
                    CLR_DIM, CLR_RESET
                );
                issues_found += 1;
            } else if !broken {
                let _ = writeln!(
                    out,
                    "  {}✓{} Native Integration: {} (managed shim -> {})",
                    CLR_GREEN, CLR_RESET, entrypoint, real_agy
                );
            }
        }
    }

    // 3. Gateway Proxy Daemon
    let info = crate::daemon::get_daemon_info(None);
    let default_port = crate::config::DEFAULT_PORT;
    let listening = crate::daemon::is_port_listening(default_port);

    if let Some(info) = info
        && listening
    {
        if crate::daemon::is_daemon_outdated(None, default_port) {
            let _ = writeln!(
                out,
                "  {}⚠{} Gateway Daemon: RUNNING (PID {}) with {}outdated disk code{}",
                CLR_YELLOW, CLR_RESET, info.pid, CLR_YELLOW, CLR_RESET
            );
            let _ = writeln!(
                out,
                "    {}↳ Run 'agy-pool restart' to reload daemon with latest code.{}",
                CLR_DIM, CLR_RESET
            );
            warnings_found += 1;
        } else {
            let ver_tag = match &info.version {
                Some(v) if !v.is_empty() => format!(" [v{}]", v),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "  {}✓{} Gateway Daemon: RUNNING (PID {}{}) on port {}",
                CLR_GREEN, CLR_RESET, info.pid, ver_tag, default_port
            );
        }

        let ctx = crate::daemon::InstanceContext {
            data_dir: crate::config::data_dir(),
            config_path: None,
            config_hash: None,
            listen_host: "127.0.0.1".to_string(),
            listen_port: default_port,
            binary_path: PathBuf::new(),
            version: crate::VERSION.to_string(),
        };
        if let Ok(stats) = ctx.get_runtime_stats(info.pid) {
            if lease_fallback_rate_high(stats.snapshot.lease_fallbacks, stats.snapshot.routing_decisions) {
                let _ = writeln!(
                    out,
                    "  {}⚠{} Lease Fallbacks: {} of {} routing decisions could not record an in-flight lease",
                    CLR_YELLOW, CLR_RESET, stats.snapshot.lease_fallbacks, stats.snapshot.routing_decisions
                );
                let _ = writeln!(
                    out,
                    "    {}↳ state.db writes are slow or locked (disk I/O, network mount); cross-process balancing is degraded.{}",
                    CLR_DIM, CLR_RESET
                );
                warnings_found += 1;
            }
        }
    } else if listening {
        let _ = writeln!(
            out,
            "  {}⚠{} Gateway Daemon: Port {} is in use by another process",
            CLR_YELLOW, CLR_RESET, default_port
        );
        warnings_found += 1;
    } else {
        let _ = writeln!(
            out,
            "  {}○{} Gateway Daemon: STOPPED (will auto-launch on 'agy' run)",
            CLR_BLUE, CLR_RESET
        );
    }

    // 4. Storage & Account Pool Health
    let _ = crate::storage::ensure_dirs();
    let state_db_file = crate::config::state_db_file();
    let pool_file = crate::config::accounts_file();

    let db_meta = std::fs::metadata(&state_db_file);
    let pool_meta = std::fs::metadata(&pool_file);

    if db_meta.is_ok() || pool_meta.is_ok() {
        let mut pool: Option<crate::storage::Pool> = None;
        let mut load_err: Option<String> = None;
        let fi_meta;
        let storage_desc: String;

        if let Ok(m) = db_meta {
            fi_meta = m;
            storage_desc = format!(
                "state.db: schema v{}, readable",
                crate::storage::STATE_DB_SCHEMA_VERSION
            );
            match crate::storage::StateDb::open(&state_db_file) {
                Ok(sdb) => {
                    match sdb.view(crate::storage::load_pool_from_tx) {
                        Ok(p) => pool = Some(p),
                        Err(e) => load_err = Some(e.to_string()),
                    }
                    let _ = sdb.close();
                }
                Err(e) => load_err = Some(e.to_string()),
            }
        } else {
            fi_meta = pool_meta.unwrap();
            storage_desc = "accounts.json: legacy".to_string();
            match crate::storage::read_pool_unlocked(&pool_file) {
                Ok(p) => pool = Some(p),
                Err(e) => load_err = Some(e.to_string()),
            }
        }

        if let Some(err) = load_err {
            let _ = writeln!(
                out,
                "  {}✖{} Account Pool: Failed to read pool file: {}",
                CLR_RED, CLR_RESET, err
            );
            issues_found += 1;
        } else if let Some(pool) = pool {
            let mut ready_cnt = 0;
            let mut exhausted_cnt = 0;
            let mut cooldown_cnt = 0;
            let mut restricted_cnt = 0;
            let now_ts = jiff::Timestamp::now().as_second();

            for a in &pool.accounts {
                let rem_fraction = a
                    .last_quota
                    .as_ref()
                    .and_then(|lq| lq.remaining_fraction)
                    .unwrap_or(1.0);
                let is_cooling = a.rate_limited_until.is_some_and(|rl| rl > now_ts as f64);

                if a.status == crate::storage::STATUS_VALIDATION_REQUIRED
                    || a.status == crate::storage::STATUS_VERIFICATION_PENDING
                    || a.status == crate::storage::STATUS_AUTH_ERROR
                    || a.status == crate::storage::STATUS_DISABLED
                {
                    restricted_cnt += 1;
                } else if is_cooling {
                    cooldown_cnt += 1;
                } else if rem_fraction <= crate::config::DEPLETED_THRESHOLD {
                    exhausted_cnt += 1;
                } else {
                    ready_cnt += 1;
                }
            }

            use std::os::unix::fs::PermissionsExt;
            let perm = fi_meta.permissions().mode() & 0o777;
            let mut perm_str = format!("0o{:03o}", perm);
            let perm_ok = perm == 0o600;
            if !perm_ok {
                perm_str = format!("{}{}{} (expected 0o600){}", CLR_YELLOW, perm_str, CLR_RESET, "");
                warnings_found += 1;
            }

            let mut active_str = String::new();
            if let Some(active_id) = &pool.active_account_id {
                if !active_id.is_empty() {
                    if let Some(a) = pool.accounts.iter().find(|acc| acc.id == *active_id) {
                        active_str = format!(
                            " | CLI Base: {}",
                            crate::accounts::display_account_name(Some(a), Some(false))
                        );
                    }
                }
            }

            let strat = if !pool.strategy.is_empty() {
                &pool.strategy
            } else {
                crate::config::STRATEGY_MAX_QUOTA
            };

            let _ = writeln!(
                out,
                "  {}✓{} Account Pool: {} account(s) configured ({}, {}){}",
                CLR_GREEN,
                CLR_RESET,
                pool.accounts.len(),
                storage_desc,
                perm_str,
                active_str
            );
            let _ = writeln!(
                out,
                "    • Status: {}{} Ready{}, {}{} Cooldown{}, {}{} Exhausted{}, {}{} Restricted{}",
                CLR_GREEN,
                ready_cnt,
                CLR_RESET,
                CLR_YELLOW,
                cooldown_cnt,
                CLR_RESET,
                CLR_DIM,
                exhausted_cnt,
                CLR_RESET,
                CLR_RED,
                restricted_cnt,
                CLR_RESET
            );
            let _ = writeln!(out, "    • Strategy: {}{}{}", CLR_BOLD, strat, CLR_RESET);

            if restricted_cnt > 0 {
                let _ = writeln!(
                    out,
                    "    {}↳ Some accounts require verification or re-auth. Run 'agy-pool verify'.{}",
                    CLR_YELLOW, CLR_RESET
                );
                warnings_found += 1;
            }
            if pool.accounts.is_empty() {
                let _ = writeln!(
                    out,
                    "    {}↳ Pool is empty. Run 'agy-pool login' or 'agy-pool import-current'.{}",
                    CLR_YELLOW, CLR_RESET
                );
                warnings_found += 1;
            }

            let mut not_renewed = Vec::new();
            for a in &pool.accounts {
                if !crate::scheduler::is_restricted(a) && crate::quota::gemini_quota_not_renewed(a) {
                    not_renewed.push(crate::accounts::display_account_name(Some(a), Some(false)));
                }
            }
            if !not_renewed.is_empty() {
                let _ = writeln!(
                    out,
                    "  {}⚠{} Gemini Quota: last refresh did not renew a Gemini window for {}",
                    CLR_YELLOW,
                    CLR_RESET,
                    not_renewed.join(", ")
                );
                let _ = writeln!(
                    out,
                    "    {}↳ Upstream may have changed its quota response; check 'agy-pool logs' for QUOTA WARN.{}",
                    CLR_DIM, CLR_RESET
                );
                warnings_found += 1;
            }

            let native_state = crate::accounts::inspect_native_agy_identity(Some(&pool))
                .unwrap_or(crate::accounts::NativeIdentityState::Unknown);
            match native_state {
                crate::accounts::NativeIdentityState::Match => {
                    let active_name = pool
                        .active_account_id
                        .as_ref()
                        .and_then(|id| pool.accounts.iter().find(|a| a.id == *id))
                        .map(|a| crate::accounts::display_account_name(Some(a), Some(false)))
                        .unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "  {}✓{} Native Identity: Matches CLI Base ({})",
                        CLR_GREEN, CLR_RESET, active_name
                    );
                }
                crate::accounts::NativeIdentityState::MismatchKnown => {
                    let _ = writeln!(
                        out,
                        "  {}✖{} Native Identity: Mismatch with CLI Base",
                        CLR_RED, CLR_RESET
                    );
                    let _ = writeln!(
                        out,
                        "    {}↳ Re-run switch or login for the CLI Base credentials.{}",
                        CLR_DIM, CLR_RESET
                    );
                    warnings_found += 1;
                }
                crate::accounts::NativeIdentityState::MismatchUnknown => {
                    let _ = writeln!(
                        out,
                        "  {}✖{} Native Identity: Does not match a known pool account",
                        CLR_RED, CLR_RESET
                    );
                    let _ = writeln!(
                        out,
                        "    {}↳ Re-run switch or login for the CLI Base credentials.{}",
                        CLR_DIM, CLR_RESET
                    );
                    warnings_found += 1;
                }
                crate::accounts::NativeIdentityState::Missing => {
                    let _ = writeln!(
                        out,
                        "  {}✖{} Native Identity: Native credential file missing",
                        CLR_RED, CLR_RESET
                    );
                    warnings_found += 1;
                }
                crate::accounts::NativeIdentityState::Unknown => {
                    let _ = writeln!(
                        out,
                        "  {}?{} Native Identity: Unable to determine safely",
                        CLR_YELLOW, CLR_RESET
                    );
                    warnings_found += 1;
                }
                crate::accounts::NativeIdentityState::NoActive => {
                    let _ = writeln!(
                        out,
                        "  {}?{} Native Identity: No CLI Base account configured",
                        CLR_BLUE, CLR_RESET
                    );
                }
            }
        }
    } else {
        let _ = writeln!(
            out,
            "  {}○{} Account Pool: No account pool file found yet ({})",
            CLR_YELLOW,
            CLR_RESET,
            state_db_file.display()
        );
        let _ = writeln!(
            out,
            "    {}↳ Run 'agy-pool import-current' or 'agy-pool login' to initialize.{}",
            CLR_DIM, CLR_RESET
        );
        warnings_found += 1;
    }

    // 5. Upstream Google Cloud Code Connectivity
    let backend_host = BACKEND_HOST_PROVIDER
        .read()
        .unwrap()
        .as_ref()
        .map(|f| f())
        .unwrap_or_else(|| "cloudaicompanion.googleapis.com".to_string());

    let tls_res = if let Some(ref probe_fn) = *TLS_PROBER.read().unwrap() {
        probe_fn(&backend_host, Duration::from_secs(5))
    } else {
        default_tls_probe(&backend_host, Duration::from_secs(5))
    };

    match tls_res {
        Ok(()) => {
            let _ = writeln!(
                out,
                "  {}✓{} Cloud Code API: Reachable via TLS ({}:443)",
                CLR_GREEN, CLR_RESET, backend_host
            );
        }
        Err(err) => {
            let _ = writeln!(
                out,
                "  {}⚠{} Cloud Code API: Connection to {} failed: {}",
                CLR_YELLOW, CLR_RESET, backend_host, err
            );
            warnings_found += 1;
        }
    }

    // 6. Session Database
    let db_path = crate::config::conversation_db_file();
    if db_path.exists() {
        match rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => {
                let count_res: Result<i64, _> =
                    conn.query_row("SELECT COUNT(*) FROM conversation_summaries", [], |row| row.get(0));
                match count_res {
                    Ok(count) => {
                        let _ = writeln!(
                            out,
                            "  {}✓{} Session Continuity: Database active ({} conversation(s) recorded)",
                            CLR_GREEN, CLR_RESET, count
                        );
                    }
                    Err(err) => {
                        let _ = writeln!(
                            out,
                            "  {}⚠{} Session Continuity: Database query warning: {}",
                            CLR_YELLOW, CLR_RESET, err
                        );
                        warnings_found += 1;
                    }
                }
            }
            Err(err) => {
                let _ = writeln!(
                    out,
                    "  {}⚠{} Session Continuity: Database query warning: {}",
                    CLR_YELLOW, CLR_RESET, err
                );
                warnings_found += 1;
            }
        }
    } else {
        let _ = writeln!(
            out,
            "  {}○{} Session Continuity: Database not yet created (created on first agy session)",
            CLR_DIM, CLR_RESET
        );
    }

    // 7. Gateway Log File
    let log_file = crate::config::log_file();
    if let Ok(meta) = std::fs::metadata(&log_file) {
        let _ = writeln!(
            out,
            "  {}✓{} Gateway Log: {} ({}, cap: {})",
            CLR_GREEN,
            CLR_RESET,
            log_file.display(),
            crate::daemon::format_size(meta.len() as i64),
            crate::daemon::format_size(crate::config::load_logging_config(None).max_size_bytes)
        );
    }

    // Summary Footer
    let _ = writeln!(out, "\n{}", sub_sep);
    if issues_found == 0 && warnings_found == 0 {
        let _ = writeln!(
            out,
            " {}{}All systems nominal! You are ready to use 'agy'.{}",
            CLR_GREEN, CLR_BOLD, CLR_RESET
        );
    } else if issues_found == 0 {
        let _ = writeln!(
            out,
            " {}{}System operational with {} warning(s). Check details above.{}",
            CLR_YELLOW, CLR_BOLD, warnings_found, CLR_RESET
        );
    } else {
        let _ = writeln!(
            out,
            " {}{}System has {} error(s) and {} warning(s). Please fix issues above.{}",
            CLR_RED, CLR_BOLD, issues_found, warnings_found, CLR_RESET
        );
    }
    let _ = writeln!(out, "{}\n", sep);

    issues_found == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Account, Pool};
    use std::sync::MutexGuard;

    struct DoctorTestEnv {
        _lock: MutexGuard<'static, ()>,
        temp_dir: tempfile::TempDir,
        orig_test_mode: bool,
        orig_gemini_dir: Option<String>,
        orig_home: Option<String>,
        orig_path: Option<String>,
        orig_prefix: Option<String>,
        orig_agy_bin: Option<String>,
        orig_self_path: Option<String>,
        orig_entrypoint_path: Option<String>,
        orig_managed_native_path: Option<String>,
        orig_cfg: crate::config::StaticConfig,
    }

    fn restore_env(key: &str, val: Option<&str>) {
        if let Some(v) = val {
            unsafe { std::env::set_var(key, v) };
        } else {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for DoctorTestEnv {
        fn drop(&mut self) {
            set_agy_binary_finder(None);
            set_agy_entrypoint_finder(None);
            set_agy_version_provider(None);
            set_backend_host_provider(None);
            set_tls_prober(None);

            crate::config::set_static_config(self.orig_cfg.clone());
            crate::config::reset_data_dir();
            crate::config::set_test_mode(self.orig_test_mode);

            restore_env("AGY_GEMINI_DIR", self.orig_gemini_dir.as_deref());
            restore_env("HOME", self.orig_home.as_deref());
            restore_env("PATH", self.orig_path.as_deref());
            restore_env("PREFIX", self.orig_prefix.as_deref());
            restore_env("AGY_BIN", self.orig_agy_bin.as_deref());
            restore_env("AGY_SELF_PATH", self.orig_self_path.as_deref());
            restore_env("AGY_ENTRYPOINT_PATH", self.orig_entrypoint_path.as_deref());
            restore_env("AGY_MANAGED_NATIVE_PATH", self.orig_managed_native_path.as_deref());
        }
    }

    fn setup_doctor_test_env() -> DoctorTestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = crate::config::is_test_mode();
        let orig_gemini_dir = std::env::var("AGY_GEMINI_DIR").ok();
        let orig_home = std::env::var("HOME").ok();
        let orig_path = std::env::var("PATH").ok();
        let orig_prefix = std::env::var("PREFIX").ok();
        let orig_agy_bin = std::env::var("AGY_BIN").ok();
        let orig_self_path = std::env::var("AGY_SELF_PATH").ok();
        let orig_entrypoint_path = std::env::var("AGY_ENTRYPOINT_PATH").ok();
        let orig_managed_native_path = std::env::var("AGY_MANAGED_NATIVE_PATH").ok();
        let orig_cfg = crate::config::get_static_config().clone();

        let temp_dir = tempfile::tempdir().unwrap();
        crate::config::set_test_mode(true);

        let state_dir = temp_dir.path().join(".gemini");
        std::fs::create_dir_all(&state_dir).unwrap();
        unsafe { std::env::set_var("AGY_GEMINI_DIR", &state_dir) };
        let _ = crate::config::configure_state_dir(&state_dir);
        let _ = crate::config::configure_data_dir(temp_dir.path());

        set_agy_binary_finder(None);
        set_agy_entrypoint_finder(None);
        set_agy_version_provider(None);
        set_backend_host_provider(None);
        set_tls_prober(None);

        DoctorTestEnv {
            _lock: lock,
            temp_dir,
            orig_test_mode,
            orig_gemini_dir,
            orig_home,
            orig_path,
            orig_prefix,
            orig_agy_bin,
            orig_self_path,
            orig_entrypoint_path,
            orig_managed_native_path,
            orig_cfg,
        }
    }

    #[test]
    fn test_binary_discovery_and_recursion_protection() {
        let env = setup_doctor_test_env();

        let bin_dir = env.temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let self_bin = bin_dir.join("agy-pool");
        std::fs::write(&self_bin, b"#!/bin/sh\necho agy-pool\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let real_agy = bin_dir.join("real-agy");
        std::fs::write(&real_agy, b"#!/bin/sh\necho '1.2.3'\n").unwrap();
        std::fs::set_permissions(&real_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let symlink_to_self = bin_dir.join("agy");
        std::os::unix::fs::symlink(&self_bin, &symlink_to_self).unwrap();

        // 1. AGY_BIN points to self -> recursion protection skips it
        unsafe {
            std::env::set_var("AGY_BIN", &symlink_to_self);
            std::env::set_var("AGY_SELF_PATH", &self_bin);
        }

        let found = find_real_agy_binary();
        assert!(
            found.as_ref() != Some(&symlink_to_self) && found.as_ref() != Some(&self_bin),
            "expected self to be skipped by recursion protection, got {:?}",
            found
        );

        // 2. AGY_BIN points to real binary -> returns it
        unsafe { std::env::set_var("AGY_BIN", &real_agy) };
        let found = find_real_agy_binary();
        assert_eq!(found.as_ref(), Some(&real_agy));

        // 3. Test installed_agy_version
        let v = installed_agy_version();
        assert_eq!(v, "1.2.3");
        drop(env);
    }

    #[test]
    fn test_run_doctor_native_identity_states() {
        let cases = [
            ("matching", "match@example.com", "Matches CLI Base"),
            ("known mismatch", "other@example.com", "Mismatch with CLI Base"),
            ("unknown identity", "", "Unable to determine safely"),
        ];

        for (name, email, want) in cases {
            let env = setup_doctor_test_env();
            set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
            set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

            let active_id = "acc_1".to_string();
            let pool = Pool {
                active_account_id: Some(active_id),
                accounts: vec![
                    Account {
                        id: "acc_1".to_string(),
                        name: "Main".to_string(),
                        email: "match@example.com".to_string(),
                        status: "active".to_string(),
                        ..Default::default()
                    },
                    Account {
                        id: "acc_2".to_string(),
                        name: "Other".to_string(),
                        email: "other@example.com".to_string(),
                        status: "active".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            };
            crate::storage::save_pool(&pool).unwrap();

            let claims = if !email.is_empty() {
                let payload = serde_json::json!({ "email": email });
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                    payload.to_string().as_bytes(),
                );
                format!("eyJhbGciOiJub25lIn0.{}.", b64)
            } else {
                String::new()
            };

            let data = serde_json::json!({ "id_token": claims });
            let token_file = crate::config::agy_token_file();
            std::fs::create_dir_all(token_file.parent().unwrap()).unwrap();
            std::fs::write(&token_file, data.to_string().as_bytes()).unwrap();

            let mut out = Vec::new();
            let ok = run_doctor(&mut out);
            let text = String::from_utf8_lossy(&out).to_string();
            assert!(ok, "doctor failed for {}: {}", name, text);
            assert!(
                text.contains(want),
                "missing {:?} in output for {}: {}",
                want,
                name,
                text
            );
            assert!(!text.contains("@example.com"), "email leaked for {}: {}", name, text);
            drop(env);
        }
    }

    #[test]
    fn test_run_doctor_native_identity_missing_and_malformed() {
        for malformed in [false, true] {
            let env = setup_doctor_test_env();
            set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
            set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

            let active_id = "acc_1".to_string();
            let pool = Pool {
                active_account_id: Some(active_id),
                accounts: vec![Account {
                    id: "acc_1".to_string(),
                    name: "Main".to_string(),
                    email: "user@example.com".to_string(),
                    status: "active".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            crate::storage::save_pool(&pool).unwrap();

            let want = if malformed {
                let token_file = crate::config::agy_token_file();
                std::fs::create_dir_all(token_file.parent().unwrap()).unwrap();
                std::fs::write(&token_file, b"raw-provider-body-marker user@example.com").unwrap();
                "Unable to determine safely"
            } else {
                "Native credential file missing"
            };

            let mut out = Vec::new();
            run_doctor(&mut out);
            let text = String::from_utf8_lossy(&out).to_string();
            assert!(text.contains(want), "expected {:?}, got:\n{}", want, text);
            assert!(
                !text.contains("raw-provider-body-marker"),
                "leaked token marker in output:\n{}",
                text
            );
            assert!(!text.contains("user@example.com"), "leaked email in output:\n{}", text);
            drop(env);
        }
    }

    #[test]
    fn test_run_doctor_empty_environment() {
        let env = setup_doctor_test_env();
        set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
        set_agy_version_provider(Some(Arc::new(|| "1.2.2".to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        assert!(ok, "expected run_doctor to succeed on empty environment");

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("Antigravity System Doctor"), "missing title");
        assert!(text.contains("Runtime:"), "missing Runtime in doctor output");
        assert!(
            text.contains("POSIX Concurrency:"),
            "missing Concurrency in doctor output"
        );
        assert!(
            text.contains("No account pool file found yet"),
            "expected missing pool message"
        );
        assert!(
            text.contains("Cloud Code API: Reachable via TLS"),
            "expected TLS reachable message"
        );
        drop(env);
    }

    #[test]
    fn test_run_doctor_populated_pool_and_privacy() {
        let env = setup_doctor_test_env();
        set_agy_binary_finder(Some(Arc::new(|| "/usr/local/bin/agy".to_string())));
        set_agy_version_provider(Some(Arc::new(|| "1.2.2".to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        let secret_email = "secret_user_email_12345@gmail.com";
        let now = jiff::Timestamp::now().as_second();
        let active_id = "acc_1".to_string();

        let acc1 = Account {
            id: "acc_1".to_string(),
            email: secret_email.to_string(),
            name: "Work Account".to_string(),
            status: "active".to_string(),
            created_at: Some(now),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_2".to_string(),
            email: "restricted_person@example.com".to_string(),
            status: "validation_required".to_string(),
            created_at: Some(now),
            ..Default::default()
        };

        let pool = Pool {
            version: 1,
            active_account_id: Some(active_id),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1, acc2],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        // Create session DB
        let db_path = crate::config::conversation_db_file();
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE conversation_summaries (conversation_id TEXT);", [])
            .unwrap();
        conn.execute("INSERT INTO conversation_summaries VALUES ('c1'), ('c2'), ('c3');", [])
            .unwrap();
        conn.close().unwrap();

        // Create log file
        let log_path = crate::config::log_file();
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, b"2026-09-16 test log entry\n").unwrap();

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();

        assert!(
            ok,
            "expected run_doctor to return true (warnings only), got false. Output:\n{}",
            text
        );
        assert!(
            !text.contains(secret_email),
            "PRIVACY VIOLATION: sensitive email found in doctor output!"
        );
        assert!(
            !text.contains("restricted_person@example.com"),
            "PRIVACY VIOLATION: restricted email found!"
        );
        assert!(
            text.contains("CLI Base: Work Account"),
            "expected active account display name 'Work Account'"
        );
        assert!(text.contains("Session Continuity: Database active (3 conversation(s) recorded)"));
        assert!(text.contains("Gateway Log:"));
        drop(env);
    }

    #[test]
    fn test_run_doctor_fatal_error_on_corrupted_pool() {
        let env = setup_doctor_test_env();
        set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        let pool_file = crate::config::accounts_file();
        std::fs::create_dir_all(pool_file.parent().unwrap()).unwrap();
        std::fs::write(&pool_file, b"{corrupt json data").unwrap();

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        assert!(!ok, "expected run_doctor to return false on corrupted pool file");

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("Account Pool: Failed to read pool file"));
        drop(env);
    }

    #[test]
    fn test_run_doctor_tls_failure_warning() {
        let env = setup_doctor_test_env();
        set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Err("network unreachable".to_string()))));

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        assert!(ok, "expected run_doctor to return true (warning only) on TLS failure");

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            text.contains("Cloud Code API: Connection to cloudaicompanion.googleapis.com failed: network unreachable"),
            "expected TLS warning message, got:\n{}",
            text
        );
        drop(env);
    }

    #[test]
    fn test_run_doctor_state_db_inspection_and_corruption() {
        let env = setup_doctor_test_env();
        set_agy_binary_finder(Some(Arc::new(|| "/bin/true".to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        // 1. Healthy state.db
        let secret_email = "hidden_doctor_user@example.com";
        let secret_token = "ya29.sensitive_secret_token_12345";
        let now = jiff::Timestamp::now().as_second();
        let pool = Pool {
            version: 1,
            accounts: vec![Account {
                id: "acc_db1".to_string(),
                name: "Test SQLite User".to_string(),
                email: secret_email.to_string(),
                access_token: secret_token.to_string(),
                status: "active".to_string(),
                created_at: Some(now),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        assert!(ok, "expected run_doctor to succeed on valid state.db");
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("state.db: schema v1, readable"));
        assert!(text.contains("1 account(s) configured"));
        assert!(!text.contains(secret_email));
        assert!(!text.contains(secret_token));

        // 2. Corrupt state.db (0-byte file)
        let state_db_file = crate::config::state_db_file();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&state_db_file)
            .unwrap();
        drop(f);

        out.clear();
        let ok = run_doctor(&mut out);
        assert!(!ok, "expected run_doctor to fail on 0-byte corrupt state.db");
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("Failed to read pool file"));
        drop(env);
    }

    #[test]
    fn test_discovery_precedence_and_fallbacks() {
        let env = setup_doctor_test_env();

        let temp_path = env.temp_dir.path();
        use std::os::unix::fs::PermissionsExt;

        let prefix_dir = temp_path.join("prefix");
        let prefix_bin = prefix_dir.join("bin");
        std::fs::create_dir_all(&prefix_bin).unwrap();
        let prefix_agy = prefix_bin.join("agy");
        std::fs::write(&prefix_agy, b"#!/bin/sh\necho prefix\n").unwrap();
        std::fs::set_permissions(&prefix_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let user_dir = temp_path.join("userhome");
        let user_bin = user_dir.join(".local/bin");
        std::fs::create_dir_all(&user_bin).unwrap();
        let user_agy = user_bin.join("agy");
        std::fs::write(&user_agy, b"#!/bin/sh\necho user\n").unwrap();
        std::fs::set_permissions(&user_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_dir = temp_path.join("pathbin");
        std::fs::create_dir_all(&path_dir).unwrap();
        let path_agy = path_dir.join("agy");
        std::fs::write(&path_agy, b"#!/bin/sh\necho path\n").unwrap();
        std::fs::set_permissions(&path_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let override_dir = temp_path.join("override");
        std::fs::create_dir_all(&override_dir).unwrap();
        let override_agy = override_dir.join("custom-agy");
        std::fs::write(&override_agy, b"#!/bin/sh\necho override\n").unwrap();
        std::fs::set_permissions(&override_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config_agy = override_dir.join("config-agy");
        std::fs::write(&config_agy, b"#!/bin/sh\necho config\n").unwrap();
        std::fs::set_permissions(&config_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        unsafe {
            std::env::set_var("HOME", &user_dir);
            std::env::set_var("PATH", &path_dir);
            std::env::set_var("PREFIX", &prefix_dir);
        }

        // A. When AGY_BIN is set, it wins with SOURCE_ENV_AGY_BIN
        unsafe { std::env::set_var("AGY_BIN", &override_agy) };
        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, override_agy);
        assert_eq!(src, SOURCE_ENV_AGY_BIN);

        // B. When AGY_BIN unset, config native_agy.binary wins
        unsafe { std::env::remove_var("AGY_BIN") };
        let mut mock_cfg = env.orig_cfg.clone();
        mock_cfg.native_agy.binary = Some(config_agy.display().to_string());
        crate::config::set_static_config(mock_cfg.clone());

        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, config_agy);
        assert_eq!(src, SOURCE_CONFIG);

        // C. When config is empty, PREFIX wins over user home and PATH
        mock_cfg.native_agy.binary = None;
        crate::config::set_static_config(mock_cfg.clone());
        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, prefix_agy);
        assert_eq!(src, SOURCE_PREFIX);

        // D. When PREFIX unset, ~/.local/bin/agy wins over PATH
        unsafe { std::env::remove_var("PREFIX") };
        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, user_agy);
        assert_eq!(src, SOURCE_USER_LOCAL);

        // E. When user home agy is a managed shim, it is skipped and PATH candidate wins
        let shim_content = b"#!/bin/sh\n# agy-pool managed shim v1\nexec agy-pool run -- \"$@\"\n";
        std::fs::write(&user_agy, shim_content).unwrap();
        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, path_agy);
        assert_eq!(src, SOURCE_PATH_ENV);

        drop(env);
    }

    #[test]
    fn test_discovery_explicit_override_failure() {
        let env = setup_doctor_test_env();

        // 1. Missing AGY_BIN fails clearly without silent fallback
        unsafe { std::env::set_var("AGY_BIN", env.temp_dir.path().join("nonexistent")) };
        let err = resolve_native_agy_binary().unwrap_err();
        assert!(
            err.contains("explicit AGY_BIN override"),
            "expected explicit AGY_BIN error, got: {}",
            err
        );

        // 2. Non-executable AGY_BIN fails clearly
        let non_exec = env.temp_dir.path().join("non_exec");
        std::fs::write(&non_exec, b"echo hi").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        unsafe { std::env::set_var("AGY_BIN", &non_exec) };
        assert!(resolve_native_agy_binary().is_err());

        // 3. Invalid config native_agy.binary fails clearly
        unsafe { std::env::remove_var("AGY_BIN") };
        let bad_path = env.temp_dir.path().join("bad_cfg_bin");
        let mut mock_cfg = env.orig_cfg.clone();
        mock_cfg.native_agy.binary = Some(bad_path.display().to_string());
        crate::config::set_static_config(mock_cfg.clone());

        let err = resolve_native_agy_binary().unwrap_err();
        assert!(
            err.contains("explicit config native_agy.binary"),
            "expected explicit config error message, got: {}",
            err
        );
        drop(env);
    }

    #[test]
    fn test_discovery_hard_link_protection() {
        let env = setup_doctor_test_env();

        let bin_dir = env.temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let self_bin = bin_dir.join("agy-pool");
        std::fs::write(&self_bin, b"#!/bin/sh\necho self\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("AGY_SELF_PATH", &self_bin) };

        let hard_link_agy = bin_dir.join("some-other-name-agy");
        if std::fs::hard_link(&self_bin, &hard_link_agy).is_err() {
            return; // skip if filesystem does not support hard links
        }

        let err = validate_native_candidate(&hard_link_agy).unwrap_err();
        assert!(
            err.contains("recursion prevented"),
            "expected recursion prevented error, got: {}",
            err
        );
        drop(env);
    }

    #[test]
    fn test_discovery_symlink_chain_protection() {
        let env = setup_doctor_test_env();

        let bin_dir = env.temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let self_bin = bin_dir.join("agy-pool");
        std::fs::write(&self_bin, b"#!/bin/sh\necho self\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("AGY_SELF_PATH", &self_bin) };

        let link3 = bin_dir.join("link3");
        let link2 = bin_dir.join("link2");
        let link1 = bin_dir.join("link1");
        std::os::unix::fs::symlink(&self_bin, &link3).unwrap();
        std::os::unix::fs::symlink(&link3, &link2).unwrap();
        std::os::unix::fs::symlink(&link2, &link1).unwrap();

        let err = validate_native_candidate(&link1);
        assert!(err.is_err(), "expected symlink chain resolving to self to be rejected");
        drop(env);
    }

    #[test]
    fn test_discovery_legitimate_script_content() {
        let env = setup_doctor_test_env();

        let bin_dir = env.temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let script_path = bin_dir.join("real_custom_agy.sh");
        let script_body = b"#!/usr/bin/env bash\n# This script interacts with python and agy-pool in some tests.\n# It is a legitimate wrapper around upstream antigravity.\necho \"native agy 2.5.0\"\n";
        std::fs::write(&script_path, script_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let res = validate_native_candidate(&script_path);
        assert!(res.is_ok(), "legitimate script was falsely rejected: {:?}", res);
        drop(env);
    }

    #[test]
    fn test_discovery_managed_native_precedence() {
        let env = setup_doctor_test_env();
        let user_dir = env.temp_dir.path().join("user");
        let managed_dir = user_dir.join(".local/libexec/agy-pool");
        std::fs::create_dir_all(&managed_dir).unwrap();
        let managed_agy = managed_dir.join("agy-native");
        std::fs::write(&managed_agy, b"#!/bin/sh\necho managed\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&managed_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_dir = env.temp_dir.path().join("path");
        std::fs::create_dir_all(&path_dir).unwrap();
        let path_agy = path_dir.join("agy");
        std::fs::write(&path_agy, b"#!/bin/sh\necho path\n").unwrap();
        std::fs::set_permissions(&path_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        unsafe {
            std::env::set_var("HOME", &user_dir);
            std::env::set_var("PATH", &path_dir);
        }

        // 1. Managed native wins over PATH
        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, managed_agy);
        assert_eq!(src, SOURCE_MANAGED);

        // 2. Config native_agy.binary wins over managed native
        let override_dir = env.temp_dir.path().join("override");
        std::fs::create_dir_all(&override_dir).unwrap();
        let config_agy = override_dir.join("config-agy");
        std::fs::write(&config_agy, b"#!/bin/sh\necho config\n").unwrap();
        std::fs::set_permissions(&config_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut mock_cfg = env.orig_cfg.clone();
        mock_cfg.native_agy.binary = Some(config_agy.display().to_string());
        crate::config::set_static_config(mock_cfg.clone());

        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, config_agy);
        assert_eq!(src, SOURCE_CONFIG);

        // 3. AGY_BIN wins over config and managed native
        let override_agy = override_dir.join("env-agy");
        std::fs::write(&override_agy, b"#!/bin/sh\necho env\n").unwrap();
        std::fs::set_permissions(&override_agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("AGY_BIN", &override_agy) };

        let (p, src) = resolve_native_agy_binary().unwrap();
        assert_eq!(p, override_agy);
        assert_eq!(src, SOURCE_ENV_AGY_BIN);

        drop(env);
    }

    #[test]
    fn test_doctor_native_integration_diagnostics() {
        let env = setup_doctor_test_env();
        let user_dir = env.temp_dir.path().join("user");
        let bin_dir = user_dir.join(".local/bin");
        let libexec_dir = user_dir.join(".local/libexec/agy-pool");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&libexec_dir).unwrap();

        let entry_agy = bin_dir.join("agy");
        let managed_native = libexec_dir.join("agy-native");

        let empty_bin = env.temp_dir.path().join("empty_bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        unsafe {
            std::env::set_var("PATH", &empty_bin);
            std::env::set_var("HOME", &user_dir);
        }

        let entry_agy_clone = entry_agy.clone();
        set_agy_entrypoint_finder(Some(Arc::new(move || entry_agy_clone.display().to_string())));
        set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        let shim_content = b"#!/bin/sh\n# agy-pool managed shim v1\nexec agy-pool run -- \"$@\"\n";
        use std::os::unix::fs::PermissionsExt;

        // Case 1: Healthy layout
        std::fs::write(&entry_agy, shim_content).unwrap();
        std::fs::set_permissions(&entry_agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&managed_native, b"#!/bin/sh\necho '1.2.3'\n").unwrap();
        std::fs::set_permissions(&managed_native, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut out = Vec::new();
        let ok = run_doctor(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            ok,
            "expected doctor to succeed on healthy layout, got false. Output:\n{}",
            text
        );
        assert!(
            text.contains("managed shim ->"),
            "expected healthy managed shim message, got:\n{}",
            text
        );

        // Case 2: Broken layout - entrypoint is a native binary bypassing agy-pool
        std::fs::write(&entry_agy, b"#!/bin/sh\necho 'native bypass binary'\n").unwrap();
        out.clear();
        let ok = run_doctor(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            !ok,
            "expected doctor to fail when entrypoint is native binary bypassing agy-pool"
        );
        assert!(
            text.contains("is a native binary bypassing agy-pool"),
            "expected native binary bypass warning, got:\n{}",
            text
        );

        // Case 3: Broken layout - entrypoint is managed shim, but managed native is missing
        std::fs::write(&entry_agy, shim_content).unwrap();
        let _ = std::fs::remove_file(&managed_native);
        out.clear();
        let ok = run_doctor(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            !ok,
            "expected doctor to fail when managed shim exists but native binary missing"
        );
        assert!(
            text.contains("managed native agy is missing") || text.contains("no valid native agy can be resolved"),
            "expected missing native agy warning, got:\n{}",
            text
        );

        // Case 4: Broken layout - managed native resolves to shim or self
        std::fs::write(&managed_native, shim_content).unwrap();
        std::fs::set_permissions(&managed_native, std::fs::Permissions::from_mode(0o755)).unwrap();
        out.clear();
        let ok = run_doctor(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(!ok, "expected doctor to fail when managed native resolves to shim/self");
        assert!(
            text.contains("resolves to shim/self"),
            "expected shim/self warning, got:\n{}",
            text
        );

        drop(env);
    }

    #[test]
    fn test_lease_fallback_rate_high() {
        let cases = [
            (0, 0, false),
            (0, 5000, false),
            (1, 100, false), // exactly 1% is tolerated
            (2, 100, true),
            (11, 1000, true),
        ];

        for (fallbacks, decisions, want) in cases {
            assert_eq!(
                lease_fallback_rate_high(fallbacks, decisions),
                want,
                "lease_fallback_rate_high({}, {}) = {}, want {}",
                fallbacks,
                decisions,
                !want,
                want
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tls_probe_requires_a_real_handshake() {
        use tokio::io::AsyncWriteExt;
        use tokio_rustls::rustls;
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        crate::upstream::add_test_root_certificate(certified.cert.der().clone());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let server_config =
            rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key)
                .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_port = tls_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = tls_listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(sock).await;
                });
            }
        });
        let plain_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let plain_port = plain_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = plain_listener.accept().await {
                let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });

        let ok = tokio::task::spawn_blocking(move || tls_probe("127.0.0.1", tls_port, Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(ok, Ok(()));
        let plain = tokio::task::spawn_blocking(move || tls_probe("127.0.0.1", plain_port, Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(
            plain.is_err(),
            "a listener that accepts TCP but does not speak TLS must fail the probe"
        );
    }

    #[test]
    fn agy_version_output_times_out_and_requires_success() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let good = script("good", "echo 1.4.0");
        let hung = script("hung", "sleep 30");
        let failing = script("failing", "echo 1.4.0; exit 3");

        assert_eq!(
            agy_version_output(&good, Duration::from_secs(5)),
            Some("1.4.0".to_string())
        );
        let start = std::time::Instant::now();
        assert_eq!(agy_version_output(&hung, Duration::from_millis(300)), None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hung agy must be killed at the timeout"
        );
        assert_eq!(agy_version_output(&failing, Duration::from_secs(5)), None);
    }

    #[test]
    fn is_shim_or_self_matches_go_name_rule() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("agy-pool-go");
        std::fs::write(&other, "#!/bin/sh\nexec /opt/agy/bin/agy \"$@\"\n").unwrap();
        assert!(!is_shim_or_self(&other), "Go only treats the name agy-pool as self");
        let named = dir.path().join("agy-pool");
        std::fs::write(&named, "binary").unwrap();
        assert!(is_shim_or_self(&named));
    }
}
