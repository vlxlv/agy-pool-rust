use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

use super::ConfigError;

static REAL_HOST_USER_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static REAL_PRODUCTION_GEMINI_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
static FORBIDDEN_WRITE_DIRS: RwLock<Option<HashSet<PathBuf>>> = RwLock::new(None);
static SYNTHETIC_SANDBOX_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static NATIVE_AGY_CUSTOM_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);
static CONFIGURED_DATA_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
static TEST_MODE: RwLock<bool> = RwLock::new(false);

/// Lexically cleans a path without filesystem access, matching Go's filepath.Clean.
pub fn clean_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let path = path.as_ref();
    let mut is_abs = false;
    let mut parts: Vec<OsString> = Vec::new();

    for c in path.components() {
        match c {
            Component::Prefix(_) => {}
            Component::RootDir => {
                is_abs = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = parts.last() {
                    if last != ".." {
                        parts.pop();
                    } else if !is_abs {
                        parts.push(OsString::from(".."));
                    }
                } else if !is_abs {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(part) => {
                parts.push(part.to_os_string());
            }
        }
    }

    if is_abs {
        let mut out = PathBuf::from("/");
        for part in parts {
            out.push(part);
        }
        out
    } else if parts.is_empty() {
        PathBuf::from(".")
    } else {
        let mut out = PathBuf::new();
        for part in parts {
            out.push(part);
        }
        out
    }
}

/// Resolves relative path to absolute against the current working directory, then cleans it.
pub fn make_absolute<P: AsRef<Path>>(path: P) -> std::io::Result<PathBuf> {
    let p = path.as_ref();
    if p.is_absolute() {
        Ok(clean_path(p))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(clean_path(cwd.join(p)))
    }
}

/// Expands leading ~ or ~/ to the user home directory.
pub fn expand_user<P: AsRef<Path>>(path: P) -> PathBuf {
    let p = path.as_ref();
    let s = p.to_string_lossy();
    if s == "~" || s.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(real_host_user_home);
        if s == "~" { home } else { home.join(&s[2..]) }
    } else {
        p.to_path_buf()
    }
}

/// Detects the real OS user home directory independent of mutable $HOME.
fn detect_real_host_user_home() -> PathBuf {
    let home_from_passwd = {
        // SAFETY: getpwuid_r is re-entrant and called with local buffer.
        unsafe {
            let uid = libc::getuid();
            let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut buf = vec![0u8; 1024];
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            if libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            ) == 0
                && !result.is_null()
            {
                let dir = std::ffi::CStr::from_ptr((*result).pw_dir);
                dir.to_str().ok().map(PathBuf::from)
            } else {
                None
            }
        }
    };
    if let Some(h) = home_from_passwd
        && !h.as_os_str().is_empty()
    {
        if let Ok(eval) = std::fs::canonicalize(&h) {
            return clean_path(&eval);
        }
        return clean_path(&h);
    }
    if let Some(h) = std::env::var_os("HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            if let Ok(eval) = std::fs::canonicalize(&p) {
                return clean_path(&eval);
            }
            return clean_path(&p);
        }
    }
    PathBuf::from("/home/codex")
}

/// Returns the detected real OS home directory for the host user.
pub fn real_host_user_home() -> PathBuf {
    REAL_HOST_USER_HOME.get_or_init(detect_real_host_user_home).clone()
}

/// Detects the real production gemini directory.
fn detect_real_production_gemini_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("AGY_REAL_GEMINI_DIR")
        && !explicit.is_empty()
        && let Ok(abs) = make_absolute(expand_user(PathBuf::from(explicit)))
    {
        if let Ok(eval) = std::fs::canonicalize(&abs) {
            return clean_path(eval);
        }
        return clean_path(abs);
    }

    let real_home = real_host_user_home();
    if !real_home.as_os_str().is_empty() {
        let gemini_path = real_home.join(".gemini");
        if let Ok(eval) = std::fs::canonicalize(&gemini_path) {
            return clean_path(&eval);
        }
        return clean_path(&gemini_path);
    }

    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return clean_path(PathBuf::from(home).join(".gemini"));
    }
    PathBuf::from("/tmp/.gemini")
}

/// Returns the detected real production state directory.
pub fn real_production_gemini_dir() -> PathBuf {
    let guard = REAL_PRODUCTION_GEMINI_DIR.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref d) = *guard {
        return d.clone();
    }
    drop(guard);
    let mut write_guard = REAL_PRODUCTION_GEMINI_DIR.write().unwrap_or_else(|e| e.into_inner());
    let d = detect_real_production_gemini_dir();
    *write_guard = Some(d.clone());
    d
}

/// Guard restoring previous real production gemini directory and forbidden write dirs.
pub struct RealProductionGeminiDirGuard {
    orig_dir: PathBuf,
    orig_forbidden: HashSet<PathBuf>,
}

impl Drop for RealProductionGeminiDirGuard {
    fn drop(&mut self) {
        let mut dir_guard = REAL_PRODUCTION_GEMINI_DIR.write().unwrap_or_else(|e| e.into_inner());
        *dir_guard = Some(self.orig_dir.clone());
        let mut forbidden_guard = FORBIDDEN_WRITE_DIRS.write().unwrap_or_else(|e| e.into_inner());
        *forbidden_guard = Some(self.orig_forbidden.clone());
    }
}

/// Overrides the detected real production directory (for testing safety boundaries).
pub fn set_real_production_gemini_dir(dir: &Path) -> RealProductionGeminiDirGuard {
    let clean = clean_path(dir);
    let mut dir_guard = REAL_PRODUCTION_GEMINI_DIR.write().unwrap_or_else(|e| e.into_inner());
    let orig_dir = dir_guard.clone().unwrap_or_else(detect_real_production_gemini_dir);
    *dir_guard = Some(clean.clone());

    let mut forbidden_guard = FORBIDDEN_WRITE_DIRS.write().unwrap_or_else(|e| e.into_inner());
    let mut current_set = forbidden_guard.clone().unwrap_or_else(init_forbidden_paths);
    let orig_forbidden = current_set.clone();

    current_set.insert(clean.clone());
    if let Ok(eval) = std::fs::canonicalize(&clean) {
        current_set.insert(clean_path(&eval));
    }
    *forbidden_guard = Some(current_set);

    RealProductionGeminiDirGuard {
        orig_dir,
        orig_forbidden,
    }
}

fn init_forbidden_paths() -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    let real_prod = real_production_gemini_dir();
    let real_home = real_host_user_home();
    let paths = vec![
        real_prod,
        real_home.join(".gemini"),
        real_home.join(".local").join("bin"),
        real_home.join(".config").join("agy-pool"),
        real_home.join(".local").join("share").join("agy-pool"),
        real_home.join(".bashrc"),
        real_home.join(".zshrc"),
        real_home.join(".profile"),
        real_home.join(".bash_profile"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/etc/agy-pool"),
        PathBuf::from("/var/lib/agy-pool"),
    ];

    for p in paths {
        if p.as_os_str().is_empty() {
            continue;
        }
        let clean = clean_path(&p);
        set.insert(clean.clone());
        if let Ok(eval) = std::fs::canonicalize(&clean) {
            set.insert(clean_path(&eval));
        }
    }
    set
}

/// Reports whether the runtime is in test mode.
pub fn is_test_mode() -> bool {
    let tm = *TEST_MODE.read().unwrap_or_else(|e| e.into_inner());
    tm || std::env::var("AGY_TEST_MODE").map(|v| v == "1").unwrap_or(false)
}

/// Sets or unsets test mode.
pub fn set_test_mode(enabled: bool) {
    let mut tm = TEST_MODE.write().unwrap_or_else(|e| e.into_inner());
    *tm = enabled;
    // SAFETY: caller holds TEST_LOCK in tests when mutating environment variables.
    unsafe {
        if enabled {
            std::env::set_var("AGY_TEST_MODE", "1");
        } else {
            std::env::remove_var("AGY_TEST_MODE");
        }
    }
}

/// Guard restoring synthetic sandbox root and environment variable.
pub struct SyntheticSandboxGuard {
    orig_root: Option<PathBuf>,
    orig_env: Option<String>,
}

impl Drop for SyntheticSandboxGuard {
    fn drop(&mut self) {
        let mut root_guard = SYNTHETIC_SANDBOX_ROOT.write().unwrap_or_else(|e| e.into_inner());
        *root_guard = self.orig_root.clone();
        // SAFETY: caller holds TEST_LOCK in tests when mutating environment variables.
        unsafe {
            if let Some(ref env_val) = self.orig_env {
                std::env::set_var("AGY_SANDBOX_DIR", env_val);
            } else {
                std::env::remove_var("AGY_SANDBOX_DIR");
            }
        }
    }
}

/// Sets the active test sandbox root, returning a guard that restores previous state on drop.
pub fn set_synthetic_sandbox_root(root: &Path) -> SyntheticSandboxGuard {
    let mut root_guard = SYNTHETIC_SANDBOX_ROOT.write().unwrap_or_else(|e| e.into_inner());
    let orig_root = root_guard.clone();
    let orig_env = std::env::var("AGY_SANDBOX_DIR").ok();

    if !root.as_os_str().is_empty() {
        let clean = match make_absolute(root) {
            Ok(abs) => {
                let eval = std::fs::canonicalize(&abs).unwrap_or(abs);
                clean_path(eval)
            }
            Err(_) => clean_path(root),
        };
        *root_guard = Some(clean.clone());
        // SAFETY: caller holds TEST_LOCK in tests when mutating environment variables.
        unsafe {
            std::env::set_var("AGY_SANDBOX_DIR", &clean);
        }
    } else {
        *root_guard = None;
        // SAFETY: caller holds TEST_LOCK in tests when mutating environment variables.
        unsafe {
            std::env::remove_var("AGY_SANDBOX_DIR");
        }
    }

    SyntheticSandboxGuard { orig_root, orig_env }
}

/// Returns the active test sandbox root.
pub fn get_synthetic_sandbox_root() -> Option<PathBuf> {
    let root_guard = SYNTHETIC_SANDBOX_ROOT.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref r) = *root_guard
        && !r.as_os_str().is_empty()
    {
        return Some(r.clone());
    }
    std::env::var_os("AGY_SANDBOX_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Sets an explicit root directory for native agy state.
pub fn set_native_agy_dir(root: &Path) {
    let mut custom = NATIVE_AGY_CUSTOM_ROOT.write().unwrap_or_else(|e| e.into_inner());
    if root.as_os_str().is_empty() {
        *custom = None;
    } else {
        *custom = Some(root.to_path_buf());
    }
}

/// Resets native agy path resolution to defaults.
pub fn reset_native_agy_dir() {
    set_native_agy_dir(Path::new(""));
}

/// Returns the active root directory for native agy state.
/// Test builds only: panics if `path` lies inside live agy-pool state on this host, whatever the
/// test-mode flag says. Tests can flip that flag and reset the data dir; on 2026-11-06 a racing
/// test did, and another test's save_pool overwrote the live gateway's state.db.
#[cfg(test)]
pub(crate) fn deny_live_path(path: PathBuf) -> PathBuf {
    let home = real_host_user_home();
    let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| clean_path(&path));
    for live in [
        ".local/share/agy-pool",
        ".config/agy-pool",
        ".gemini",
        ".local/bin",
        ".local/libexec/agy-pool",
    ] {
        let root = home.join(live);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        if resolved.starts_with(&root) || path.starts_with(home.join(live)) {
            panic!(
                "test resolved a live path {} (under {}); configure a temp dir first",
                path.display(),
                root.display()
            );
        }
    }
    path
}

#[cfg(not(test))]
#[inline]
pub(crate) fn deny_live_path(path: PathBuf) -> PathBuf {
    path
}

pub fn native_agy_dir() -> PathBuf {
    deny_live_path(native_agy_dir_unchecked())
}

fn native_agy_dir_unchecked() -> PathBuf {
    let custom = NATIVE_AGY_CUSTOM_ROOT.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref root) = *custom {
        return root.clone();
    }
    drop(custom);
    if let Ok(env_dir) = std::env::var("AGY_GEMINI_DIR")
        && !env_dir.is_empty()
    {
        return PathBuf::from(env_dir);
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(real_host_user_home);
    home.join(".gemini")
}

pub fn agy_cli_dir() -> PathBuf {
    native_agy_dir().join("antigravity-cli")
}

pub fn agy_token_file() -> PathBuf {
    agy_cli_dir().join("antigravity-oauth-token")
}

pub fn conversation_db_file() -> PathBuf {
    agy_cli_dir().join("conversation_summaries.db")
}

pub fn presence_dir() -> PathBuf {
    agy_cli_dir().join("presence")
}

pub fn presence_lock_file(cid: &str) -> PathBuf {
    presence_dir().join(format!("{}.lock", cid))
}

/// Resolves symlink components along path as far as possible, even if non-existent.
pub fn resolve_path_symlinks(path: &Path) -> PathBuf {
    let clean = clean_path(path);
    if let Ok(eval) = std::fs::canonicalize(&clean) {
        return clean_path(&eval);
    }

    let mut curr = if clean.is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(".")
    };

    for c in clean.components() {
        match c {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => continue,
            Component::ParentDir => {
                curr.pop();
            }
            Component::Normal(part) => {
                let mut next = curr.join(part);
                for _ in 0..16 {
                    let meta = match std::fs::symlink_metadata(&next) {
                        Ok(m) => m,
                        Err(_) => break,
                    };
                    if !meta.file_type().is_symlink() {
                        break;
                    }
                    let target = match std::fs::read_link(&next) {
                        Ok(t) => t,
                        Err(_) => break,
                    };
                    let resolved_target = if target.is_absolute() {
                        target
                    } else {
                        match next.parent() {
                            Some(p) => p.join(target),
                            None => target,
                        }
                    };
                    next = clean_path(&resolved_target);
                }
                curr = next;
            }
        }
    }
    clean_path(&curr)
}

/// Reports whether path falls within a protected production state directory.
pub fn is_protected_production_dir(path: &Path) -> bool {
    let clean = clean_path(path);

    if let Some(sandbox) = get_synthetic_sandbox_root()
        && !sandbox.as_os_str().is_empty()
        && clean.starts_with(&sandbox)
    {
        return false;
    }

    let real_prod = real_production_gemini_dir();
    if !real_prod.as_os_str().is_empty() && clean.starts_with(&real_prod) {
        return true;
    }

    if is_test_mode() {
        let forbidden_guard = FORBIDDEN_WRITE_DIRS.read().unwrap_or_else(|e| e.into_inner());
        let forbidden_dirs = match forbidden_guard.as_ref() {
            Some(set) => set.clone(),
            None => {
                drop(forbidden_guard);
                let mut write_guard = FORBIDDEN_WRITE_DIRS.write().unwrap_or_else(|e| e.into_inner());
                let set = init_forbidden_paths();
                *write_guard = Some(set.clone());
                set
            }
        };
        for forbidden in &forbidden_dirs {
            if clean.starts_with(forbidden) {
                return true;
            }
        }
    }

    false
}

/// Guards against destructive operations targeting real production and real host locations in test mode.
pub fn assert_safe_destructive_path(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let abs = match make_absolute(expand_user(path)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] failed to resolve destructive path \"{}\": {}",
                path.display(),
                err
            )));
        }
    };
    let clean_abs = clean_path(&abs);

    // Check 1: nominal path
    if is_protected_production_dir(&clean_abs) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing write to protected production path: {}",
            clean_abs.display()
        )));
    }

    // Check 2: enclosing directory resolved
    if let Some(parent) = clean_abs.parent()
        && let Ok(parent_eval) = std::fs::canonicalize(parent)
    {
        let clean_parent = clean_path(&parent_eval);
        let resolved_parent_path = clean_parent.join(clean_abs.file_name().unwrap_or_default());
        if is_protected_production_dir(&clean_parent) || is_protected_production_dir(&resolved_parent_path) {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] refusing write to protected production path: {}",
                resolved_parent_path.display()
            )));
        }
    }

    // Check 3: follow symlink hops
    let mut curr = clean_abs.clone();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&curr) {
            Ok(m) => m,
            Err(_) => break,
        };
        if !meta.file_type().is_symlink() {
            break;
        }
        let target = match std::fs::read_link(&curr) {
            Ok(t) => t,
            Err(_) => break,
        };
        let resolved_target = if target.is_absolute() {
            target
        } else {
            match curr.parent() {
                Some(p) => p.join(target),
                None => target,
            }
        };
        let clean_target = clean_path(&resolved_target);
        if is_protected_production_dir(&clean_target) {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] refusing write to protected production path: {} (via {})",
                clean_target.display(),
                curr.display()
            )));
        }
        if let Some(parent) = clean_target.parent()
            && let Ok(target_parent_eval) = std::fs::canonicalize(parent)
        {
            let clean_target_parent = clean_path(&target_parent_eval);
            if is_protected_production_dir(&clean_target_parent) {
                return Err(ConfigError::Guard(format!(
                    "[FAIL-CLOSED GUARD] refusing write to protected production path: {} (via {})",
                    clean_target_parent.display(),
                    curr.display()
                )));
            }
        }
        curr = clean_target;
    }

    // Check 4: fully evaluated target
    if let Ok(eval) = std::fs::canonicalize(&clean_abs) {
        let clean_eval = clean_path(&eval);
        if is_protected_production_dir(&clean_eval) {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] refusing write to protected production path: {}",
                clean_eval.display()
            )));
        }
    }

    // Check 5: structural symlink component resolution (handles non-existent target files/dirs)
    let resolved_all = resolve_path_symlinks(&clean_abs);
    if is_protected_production_dir(&resolved_all) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing write to protected production path: {}",
            resolved_all.display()
        )));
    }

    Ok(())
}

/// Ensures a target path never targets real production state.
pub fn assert_safe_write_path(path: &Path) -> Result<(), ConfigError> {
    assert_safe_destructive_path(path)
}

/// Validates that path is safe for native token credential operations.
pub fn assert_safe_native_token_write(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Guard(
            "[FAIL-CLOSED GUARD] native token write path is empty".to_string(),
        ));
    }

    let abs = match make_absolute(expand_user(path)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] failed to resolve native token write path \"{}\": {}",
                path.display(),
                err
            )));
        }
    };
    let clean_target = clean_path(&abs);

    let mut resolved_target = resolve_path_symlinks(&clean_target);
    if resolved_target.as_os_str().is_empty() {
        resolved_target = clean_target.clone();
    }

    if is_test_mode() {
        let real_prod = real_production_gemini_dir();
        if !real_prod.as_os_str().is_empty() {
            let clean_real_prod = clean_path(&real_prod);
            if clean_target.starts_with(&clean_real_prod) || resolved_target.starts_with(&clean_real_prod) {
                return Err(ConfigError::Guard(format!(
                    "[FAIL-CLOSED GUARD] refusing write to protected production path in test mode: {}",
                    clean_target.display()
                )));
            }
        }

        let real_home = real_host_user_home();
        if !real_home.as_os_str().is_empty() {
            let real_home_gemini = clean_path(real_home.join(".gemini"));
            if clean_target.starts_with(&real_home_gemini) || resolved_target.starts_with(&real_home_gemini) {
                return Err(ConfigError::Guard(format!(
                    "[FAIL-CLOSED GUARD] refusing write to protected production path in test mode: {}",
                    clean_target.display()
                )));
            }
        }

        let forbidden_guard = FORBIDDEN_WRITE_DIRS.read().unwrap_or_else(|e| e.into_inner());
        let forbidden_dirs = match forbidden_guard.as_ref() {
            Some(set) => set.clone(),
            None => {
                drop(forbidden_guard);
                let mut write_guard = FORBIDDEN_WRITE_DIRS.write().unwrap_or_else(|e| e.into_inner());
                let set = init_forbidden_paths();
                *write_guard = Some(set.clone());
                set
            }
        };

        for forbidden in &forbidden_dirs {
            if clean_target.starts_with(forbidden) || resolved_target.starts_with(forbidden) {
                return Err(ConfigError::Guard(format!(
                    "[FAIL-CLOSED GUARD] refusing write to protected production path in test mode: {}",
                    clean_target.display()
                )));
            }
        }

        let sandbox = get_synthetic_sandbox_root();
        let in_sandbox = sandbox
            .as_ref()
            .map(|s| !s.as_os_str().is_empty() && clean_target.starts_with(s))
            .unwrap_or(false);

        let native_dir = native_agy_dir();
        let in_native_dir = !native_dir.as_os_str().is_empty() && clean_target.starts_with(&native_dir);

        if !in_sandbox && !in_native_dir {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] native token write in test mode must target synthetic sandbox or isolated native dir: {}",
                clean_target.display()
            )));
        }
    }

    let canonical_token_file = agy_token_file();
    if canonical_token_file.as_os_str().is_empty() {
        return Err(ConfigError::Guard(
            "[FAIL-CLOSED GUARD] canonical native token file is unconfigured".to_string(),
        ));
    }
    let canonical_abs = match make_absolute(expand_user(&canonical_token_file)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] failed to resolve canonical native token path \"{}\": {}",
                canonical_token_file.display(),
                err
            )));
        }
    };
    let clean_canonical = clean_path(&canonical_abs);
    let clean_lock = PathBuf::from(format!("{}.lock", clean_canonical.display()));
    let canonical_dir = clean_canonical.parent().unwrap_or(Path::new(".")).to_path_buf();

    let mut is_canonical = false;
    if clean_target == clean_canonical || clean_target == clean_lock {
        is_canonical = true;
    } else if clean_target.parent() == Some(canonical_dir.as_path()) {
        let base = clean_target.file_name().unwrap_or_default().to_string_lossy();
        let canonical_base = clean_canonical.file_name().unwrap_or_default().to_string_lossy();
        let prefix = format!("{}.", canonical_base);
        if base.starts_with(&prefix) && !base.contains("..") {
            is_canonical = true;
        }
    }

    if !is_canonical {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing native token write to non-canonical path: {}",
            clean_target.display()
        )));
    }

    let mut resolved_canonical_dir = resolve_path_symlinks(&canonical_dir);
    if resolved_canonical_dir.as_os_str().is_empty() {
        resolved_canonical_dir = canonical_dir;
    }

    if resolved_target != resolved_canonical_dir && !resolved_target.starts_with(&resolved_canonical_dir) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing native token write through symlink escaping canonical directory: {} -> {}",
            clean_target.display(),
            resolved_target.display()
        )));
    }

    for sys_prefix in ["/usr", "/bin", "/sbin", "/etc", "/var"] {
        let sys_path = Path::new(sys_prefix);
        if resolved_target.starts_with(sys_path) {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] refusing native token write redirecting to system directory: {}",
                resolved_target.display()
            )));
        }
    }

    Ok(())
}

/// Ensures test code does not accidentally read real production state.
pub fn assert_safe_read_path(path: &Path) -> Result<(), ConfigError> {
    if !is_test_mode() || path.as_os_str().is_empty() {
        return Ok(());
    }
    let abs = match make_absolute(expand_user(path)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] failed to resolve read path \"{}\": {}",
                path.display(),
                err
            )));
        }
    };
    let clean_abs = clean_path(&abs);

    if is_protected_production_dir(&clean_abs) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing read from protected production path in test mode: {}",
            clean_abs.display()
        )));
    }

    if let Ok(eval) = std::fs::canonicalize(&clean_abs) {
        let clean_eval = clean_path(&eval);
        if is_protected_production_dir(&clean_eval) {
            return Err(ConfigError::Guard(format!(
                "[FAIL-CLOSED GUARD] refusing read from protected production path in test mode: {}",
                clean_eval.display()
            )));
        }
    }

    let resolved_all = resolve_path_symlinks(&clean_abs);
    if is_protected_production_dir(&resolved_all) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing read from protected production path in test mode: {}",
            resolved_all.display()
        )));
    }

    Ok(())
}

/// Sets the writable data directory with fail-closed safety checks.
pub fn configure_data_dir(dir: &Path) -> Result<(), ConfigError> {
    let abs = match make_absolute(expand_user(dir)) {
        Ok(a) => a,
        Err(err) => {
            return Err(ConfigError::Guard(format!(
                "failed to resolve data directory \"{}\": {}",
                dir.display(),
                err
            )));
        }
    };
    let real_target = std::fs::canonicalize(&abs).unwrap_or(abs);
    let real_target = clean_path(&real_target);

    if is_protected_production_dir(&real_target) {
        return Err(ConfigError::Guard(format!(
            "[FAIL-CLOSED GUARD] refusing to use Python production state ({}) during Go development",
            real_target.display()
        )));
    }
    // Before the locks dir is created below: a live path must not even get a directory made.
    let real_target = deny_live_path(real_target);

    let mut configured = CONFIGURED_DATA_DIR.write().unwrap_or_else(|e| e.into_inner());
    *configured = Some(real_target.clone());

    let locks_dir = real_target.join("locks");
    let _ = std::fs::create_dir_all(&locks_dir);

    Ok(())
}

/// Resets the configured data and state directory.
pub fn reset_data_dir() {
    let mut configured = CONFIGURED_DATA_DIR.write().unwrap_or_else(|e| e.into_inner());
    *configured = None;
}

/// Sets the default data directory.
pub fn configure_default_data_dir() -> Result<(), ConfigError> {
    if let Ok(env_dir) = std::env::var("AGY_DATA_DIR")
        && !env_dir.is_empty()
    {
        return configure_data_dir(Path::new(&env_dir));
    }
    if let Ok(env_dir) = std::env::var("AGY_GEMINI_DIR")
        && !env_dir.is_empty()
    {
        return configure_data_dir(Path::new(&env_dir));
    }

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(real_host_user_home);
    let default_dir = home.join(".local").join("share").join("agy-pool");
    configure_data_dir(&default_dir)
}

/// Returns the active writable data directory.
pub fn data_dir() -> PathBuf {
    deny_live_path(data_dir_unchecked())
}

fn data_dir_unchecked() -> PathBuf {
    let configured = CONFIGURED_DATA_DIR.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref d) = *configured {
        return d.clone();
    }
    drop(configured);

    let _ = configure_default_data_dir();

    let configured = CONFIGURED_DATA_DIR.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref d) = *configured {
        return d.clone();
    }
    PathBuf::new()
}

pub fn state_dir() -> PathBuf {
    data_dir()
}

pub fn configure_state_dir(dir: &Path) -> Result<(), ConfigError> {
    configure_data_dir(dir)
}

pub fn state_db_file() -> PathBuf {
    data_dir().join("state.db")
}

pub fn accounts_file() -> PathBuf {
    let dir = data_dir();
    let cand1 = dir.join("accounts.json");
    if cand1.exists() {
        return cand1;
    }
    let cand2 = dir.join("agy-pool-accounts.json");
    if cand2.exists() {
        return cand2;
    }
    cand1
}

pub fn runtime_file() -> PathBuf {
    data_dir().join("runtime.json")
}

pub fn quota_file() -> PathBuf {
    data_dir().join("quota.json")
}

pub fn pid_file() -> PathBuf {
    data_dir().join("agy-pool.pid")
}

pub fn log_file() -> PathBuf {
    data_dir().join("agy-pool.log")
}

pub fn locks_dir() -> PathBuf {
    data_dir().join("locks")
}

pub fn lock_file() -> PathBuf {
    let dir = data_dir();
    let cand1 = dir.join("locks").join("accounts.lock");
    if cand1.exists() {
        return cand1;
    }
    let cand2 = dir.join("agy-pool-accounts.json.lock");
    if cand2.exists() {
        return cand2;
    }
    cand1
}

pub fn specific_lock_file(name: &str) -> PathBuf {
    locks_dir().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuse_production_state() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prod_dir = real_production_gemini_dir();
        if prod_dir.as_os_str().is_empty() {
            return;
        }

        assert!(
            configure_state_dir(&prod_dir).is_err(),
            "expected error when configuring production dir {}, got nil",
            prod_dir.display()
        );

        let sub_path = prod_dir.join("agy-pool-accounts.json");
        assert!(
            assert_safe_write_path(&sub_path).is_err(),
            "expected error when asserting safe write to {}, got nil",
            sub_path.display()
        );
    }

    #[test]
    fn allow_dev_and_temp_dirs() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            configure_state_dir(tmp.path()).is_ok(),
            "expected success configuring temp dir"
        );

        let accounts = accounts_file();
        let expected_canonical = tmp.path().join("accounts.json");
        let expected_legacy = tmp.path().join("agy-pool-accounts.json");
        assert!(
            accounts == expected_canonical || accounts == expected_legacy,
            "expected {} or {}, got {}",
            expected_canonical.display(),
            expected_legacy.display(),
            accounts.display()
        );

        assert!(
            assert_safe_write_path(&accounts).is_ok(),
            "expected safe write path in temp dir"
        );
        reset_data_dir();
    }

    #[test]
    fn data_directory_layout() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        assert!(configure_data_dir(tmp.path()).is_ok(), "failed to configure data dir");

        assert_eq!(accounts_file(), tmp.path().join("accounts.json"));
        assert_eq!(runtime_file(), tmp.path().join("runtime.json"));
        assert_eq!(quota_file(), tmp.path().join("quota.json"));
        assert_eq!(pid_file(), tmp.path().join("agy-pool.pid"));
        assert_eq!(log_file(), tmp.path().join("agy-pool.log"));
        assert_eq!(locks_dir(), tmp.path().join("locks"));
        assert_eq!(lock_file(), tmp.path().join("locks").join("accounts.lock"));
        assert_eq!(
            specific_lock_file("pid.lock"),
            tmp.path().join("locks").join("pid.lock")
        );

        let legacy_file = tmp.path().join("agy-pool-accounts.json");
        std::fs::write(&legacy_file, "{}").unwrap();
        assert_eq!(
            accounts_file(),
            legacy_file,
            "expected fallback to legacy accounts file"
        );

        reset_data_dir();
    }

    #[test]
    fn native_agy_resolver_and_read_path_guard() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let synthetic_root = tmp.path().join("synthetic_gemini");
        set_native_agy_dir(&synthetic_root);

        assert_eq!(native_agy_dir(), synthetic_root);
        let expected_cli_dir = synthetic_root.join("antigravity-cli");
        assert_eq!(agy_cli_dir(), expected_cli_dir);
        let expected_token = expected_cli_dir.join("antigravity-oauth-token");
        assert_eq!(agy_token_file(), expected_token);
        let expected_db = expected_cli_dir.join("conversation_summaries.db");
        assert_eq!(conversation_db_file(), expected_db);
        let expected_presence = expected_cli_dir.join("presence");
        assert_eq!(presence_dir(), expected_presence);
        let expected_lock = expected_presence.join("test_cid.lock");
        assert_eq!(presence_lock_file("test_cid"), expected_lock);

        let orig_mode = is_test_mode();
        set_test_mode(true);

        assert!(
            assert_safe_read_path(&expected_token).is_ok(),
            "expected read of synthetic token to be safe"
        );

        let prod_dir = real_production_gemini_dir();
        if !prod_dir.as_os_str().is_empty() {
            let prod_token = prod_dir.join("antigravity-cli").join("antigravity-oauth-token");
            let err = assert_safe_read_path(&prod_token);
            assert!(
                err.is_err(),
                "expected assert_safe_read_path to reject production token in test mode"
            );
            assert!(
                err.unwrap_err()
                    .to_string()
                    .contains("refusing read from protected production path in test mode"),
                "unexpected error message"
            );
        }

        reset_native_agy_dir();
        set_test_mode(orig_mode);
    }

    #[test]
    fn destructive_guard_protects_real_host_paths_in_test_mode() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_mode = is_test_mode();
        set_test_mode(true);

        let real_home = real_host_user_home();
        assert!(!real_home.as_os_str().is_empty(), "real host user home is empty");

        let real_local_bin_agy_pool = real_home.join(".local").join("bin").join("agy-pool");
        assert!(
            assert_safe_destructive_path(&real_local_bin_agy_pool).is_err(),
            "expected error protecting real local bin agy-pool"
        );

        let real_gemini_dir = real_home.join(".gemini");
        assert!(
            assert_safe_destructive_path(&real_gemini_dir).is_err(),
            "expected error protecting real ~/.gemini"
        );

        let real_bashrc = real_home.join(".bashrc");
        assert!(
            assert_safe_destructive_path(&real_bashrc).is_err(),
            "expected error protecting real ~/.bashrc"
        );

        for sys_path in ["/usr/local/bin", "/usr/bin", "/etc/agy-pool", "/var/lib/agy-pool"] {
            assert!(
                assert_safe_destructive_path(Path::new(sys_path)).is_err(),
                "expected error protecting system path {}",
                sys_path
            );
        }

        set_test_mode(orig_mode);
    }

    #[test]
    fn destructive_guard_symlink_resolution() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_mode = is_test_mode();
        set_test_mode(true);

        let temp_dir = tempfile::tempdir().unwrap();
        let _reset_sandbox = set_synthetic_sandbox_root(temp_dir.path());

        let real_home = real_host_user_home();
        let real_local_bin = real_home.join(".local").join("bin");

        let symlink_to_real_local_bin = temp_dir.path().join("fake_local_bin");
        std::os::unix::fs::symlink(&real_local_bin, &symlink_to_real_local_bin).unwrap();

        let target_inside_symlink = symlink_to_real_local_bin.join("agy-pool");
        assert!(
            assert_safe_destructive_path(&target_inside_symlink).is_err(),
            "expected error for path traversing symlink to real ~/.local/bin"
        );

        let symlink_to_file = temp_dir.path().join("symlink_to_entrypoint");
        let _ = std::os::unix::fs::symlink(real_local_bin.join("agy-pool"), &symlink_to_file);
        assert!(
            assert_safe_destructive_path(&symlink_to_file).is_err(),
            "expected error for symlink pointing to real entrypoint"
        );

        set_test_mode(orig_mode);
    }

    #[test]
    fn destructive_guard_home_override_cannot_redirect_safety_classification() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_mode = is_test_mode();
        set_test_mode(true);

        let orig_home = std::env::var_os("HOME");
        let temp_dir = tempfile::tempdir().unwrap();
        // SAFETY: test holds TEST_LOCK
        unsafe {
            std::env::set_var("HOME", temp_dir.path());
        }

        let real_home = real_host_user_home();
        let real_local_bin = real_home.join(".local").join("bin").join("agy-pool");

        let res = assert_safe_destructive_path(&real_local_bin);
        assert!(
            res.is_err(),
            "expected assert_safe_destructive_path to protect real home {} even when HOME={}",
            real_local_bin.display(),
            temp_dir.path().display()
        );

        // SAFETY: test holds TEST_LOCK
        unsafe {
            match orig_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        set_test_mode(orig_mode);
    }

    #[test]
    fn destructive_guard_real_home_used_only_for_protection() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_mode = is_test_mode();
        set_test_mode(true);

        let real_home = real_host_user_home();
        assert!(!real_home.as_os_str().is_empty(), "real host user home is empty");

        let temp_dir = tempfile::tempdir().unwrap();
        let _reset_sandbox = set_synthetic_sandbox_root(temp_dir.path());

        let safe_path = temp_dir.path().join("safe").join("path");
        assert!(
            assert_safe_destructive_path(&safe_path).is_ok(),
            "expected safe sandbox path to pass"
        );

        set_test_mode(orig_mode);
    }

    #[test]
    fn assert_safe_native_token_write_production_mode_canonical_succeeds() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = is_test_mode();
        set_test_mode(false);

        let fake_home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var_os("HOME");
        // SAFETY: test holds TEST_LOCK
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let canonical_token = agy_token_file();
        let expected_prefix = fake_home.path().join(".gemini").join("antigravity-cli");
        assert!(
            canonical_token.starts_with(&expected_prefix),
            "expected canonical token file under {}, got {}",
            expected_prefix.display(),
            canonical_token.display()
        );

        // 1. Canonical token file itself
        assert!(
            assert_safe_native_token_write(&canonical_token).is_ok(),
            "expected canonical token file to be allowed in production mode"
        );

        // 2. Canonical lock file
        let lock_file = PathBuf::from(format!("{}.lock", canonical_token.display()));
        assert!(
            assert_safe_native_token_write(&lock_file).is_ok(),
            "expected canonical lock file to be allowed in production mode"
        );

        // 3. Canonical atomic staging file
        let staging_file = canonical_token
            .parent()
            .unwrap()
            .join("antigravity-oauth-token.tmp12345");
        assert!(
            assert_safe_native_token_write(&staging_file).is_ok(),
            "expected canonical staging file to be allowed in production mode"
        );

        // SAFETY: test holds TEST_LOCK
        unsafe {
            match orig_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        set_test_mode(orig_test_mode);
    }

    #[test]
    fn assert_safe_native_token_write_test_mode_real_production_rejected() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = is_test_mode();
        set_test_mode(true);

        let sandbox_dir = tempfile::tempdir().unwrap();
        let _reset_sandbox = set_synthetic_sandbox_root(sandbox_dir.path());

        let fake_real_home = tempfile::tempdir().unwrap();
        let fake_real_gemini = fake_real_home.path().join(".gemini");
        let _reset_real_prod = set_real_production_gemini_dir(&fake_real_gemini);

        let target_real_token = fake_real_gemini.join("antigravity-cli").join("antigravity-oauth-token");

        let err = assert_safe_native_token_write(&target_real_token);
        assert!(
            err.is_err(),
            "expected write to real-production-like path {} to fail closed in test mode",
            target_real_token.display()
        );
        let err_msg = err.unwrap_err().to_string();
        assert!(
            err_msg.contains("refusing write to protected production path in test mode")
                || err_msg.contains("protected production path"),
            "expected fail-closed error message, got: {}",
            err_msg
        );

        set_test_mode(orig_test_mode);
    }

    #[test]
    fn assert_safe_native_token_write_test_mode_synthetic_token_succeeds() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = is_test_mode();
        set_test_mode(true);

        let temp_dir = tempfile::tempdir().unwrap();
        let _reset_sandbox = set_synthetic_sandbox_root(temp_dir.path());

        let synthetic_gemini = temp_dir.path().join("synthetic_gemini");
        set_native_agy_dir(&synthetic_gemini);

        let canonical_token = agy_token_file();
        assert!(
            canonical_token.starts_with(temp_dir.path()),
            "expected canonical token under {}, got {}",
            temp_dir.path().display(),
            canonical_token.display()
        );

        // 1. Canonical token file in sandbox
        assert!(
            assert_safe_native_token_write(&canonical_token).is_ok(),
            "expected synthetic token path in sandbox to succeed in test mode"
        );

        // 2. Lock file in sandbox
        let lock_file = PathBuf::from(format!("{}.lock", canonical_token.display()));
        assert!(
            assert_safe_native_token_write(&lock_file).is_ok(),
            "expected synthetic lock path in sandbox to succeed in test mode"
        );

        // 3. Staging file in sandbox
        let staging_file = canonical_token.parent().unwrap().join("antigravity-oauth-token.12345");
        assert!(
            assert_safe_native_token_write(&staging_file).is_ok(),
            "expected synthetic staging path in sandbox to succeed in test mode"
        );

        reset_native_agy_dir();
        set_test_mode(orig_test_mode);
    }

    #[test]
    fn assert_safe_native_token_write_production_mode_arbitrary_paths_rejected() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = is_test_mode();
        set_test_mode(false);

        let fake_home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var_os("HOME");
        // SAFETY: test holds TEST_LOCK
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let canonical_token = agy_token_file();
        let canonical_dir = canonical_token.parent().unwrap();

        let arbitrary_paths = [
            PathBuf::from("/tmp/random-token"),
            PathBuf::from("/tmp/antigravity-oauth-token"),
            canonical_dir.join("other_token.json"),
            canonical_dir.join("conversation_summaries.db"),
            canonical_dir.join("arbitrary_file.txt"),
            fake_home.path().join(".bashrc"),
            fake_home.path().join("notes.txt"),
            PathBuf::from(""),
        ];

        for p in &arbitrary_paths {
            assert!(
                assert_safe_native_token_write(p).is_err(),
                "expected arbitrary path {:?} to be rejected in production mode",
                p
            );
        }

        // SAFETY: test holds TEST_LOCK
        unsafe {
            match orig_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        set_test_mode(orig_test_mode);
    }

    #[test]
    fn assert_safe_native_token_write_symlink_and_traversal_protection() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_test_mode = is_test_mode();
        set_test_mode(false);

        let fake_home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var_os("HOME");
        // SAFETY: test holds TEST_LOCK
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let canonical_token = agy_token_file();
        let canonical_dir = canonical_token.parent().unwrap();
        std::fs::create_dir_all(canonical_dir).unwrap();

        // 1. Path traversal attempting to escape canonical directory
        let traversal_path = canonical_dir.join("..").join("other-token");
        assert!(
            assert_safe_native_token_write(&traversal_path).is_err(),
            "expected traversal path {:?} to be rejected",
            traversal_path
        );

        // 2. Symlink attempting to redirect canonical token to sensitive file outside canonical dir
        let sensitive_target = fake_home.path().join("sensitive_file.txt");
        std::fs::write(&sensitive_target, "secret").unwrap();

        let symlink_path = canonical_dir.join("antigravity-oauth-token");
        std::os::unix::fs::symlink(&sensitive_target, &symlink_path).unwrap();

        let res = assert_safe_native_token_write(&symlink_path);
        assert!(
            res.is_err(),
            "expected symlink redirecting out of canonical dir to {:?} to be rejected",
            sensitive_target
        );
        let _ = std::fs::remove_file(&symlink_path);

        // 3. Symlink to system directory
        let sys_symlink = canonical_dir.join("antigravity-oauth-token.system");
        std::os::unix::fs::symlink("/etc/passwd", &sys_symlink).unwrap();
        assert!(
            assert_safe_native_token_write(&sys_symlink).is_err(),
            "expected symlink to system directory to be rejected"
        );
        let _ = std::fs::remove_file(&sys_symlink);

        // SAFETY: test holds TEST_LOCK
        unsafe {
            match orig_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        set_test_mode(orig_test_mode);
    }

    #[test]
    fn deny_live_path_covers_every_live_root() {
        let home = real_host_user_home();
        for live in [
            ".local/share/agy-pool",
            ".config/agy-pool",
            ".gemini",
            ".local/bin",
            ".local/libexec/agy-pool",
        ] {
            let p = home.join(live).join("agy-native");
            assert!(
                std::panic::catch_unwind(|| deny_live_path(p.clone())).is_err(),
                "{} must be denied",
                p.display()
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(deny_live_path(tmp.path().join("x")), tmp.path().join("x"));
    }

    #[test]
    fn live_paths_panic_even_with_test_mode_off() {
        // Reproduces the 2026-11-06 incident's state: test mode off and no configured data dir,
        // so the default would be the live ~/.local/share/agy-pool.
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let was_test_mode = is_test_mode();
        set_test_mode(false);
        reset_data_dir();
        let data = std::panic::catch_unwind(data_dir);
        let native = std::panic::catch_unwind(native_agy_dir);
        set_test_mode(was_test_mode);
        assert!(
            data.is_err(),
            "data_dir() must panic instead of returning the live data dir"
        );
        assert!(
            native.is_err(),
            "native_agy_dir() must panic instead of returning the live ~/.gemini"
        );

        let tmp = tempfile::tempdir().unwrap();
        configure_data_dir(tmp.path()).unwrap();
        assert!(data_dir().starts_with(std::fs::canonicalize(tmp.path()).unwrap()));
        reset_data_dir();
    }

    #[test]
    fn refuses_live_data_dir_in_test_mode() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_test_mode(true);
        let live = real_host_user_home().join(".local/share/agy-pool");
        assert!(
            configure_data_dir(&live).is_err(),
            "test mode must refuse the live data dir"
        );
        assert!(assert_safe_write_path(&live.join("state.db")).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(configure_data_dir(tmp.path()).is_ok());
        reset_data_dir();
    }

    struct CwdRestoreGuard {
        orig_cwd: PathBuf,
    }

    impl Drop for CwdRestoreGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.orig_cwd);
        }
    }

    #[test]
    fn guards_fail_closed_when_cwd_is_gone() {
        let _g = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig_mode = is_test_mode();
        set_test_mode(true);

        let orig_cwd = std::env::current_dir().expect("must get current dir");
        let _guard = CwdRestoreGuard { orig_cwd };

        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        std::env::set_current_dir(&tmp_path).expect("must chdir to tempdir");
        drop(tmp);
        let _ = std::fs::remove_dir_all(&tmp_path);

        let destructive_res = assert_safe_destructive_path(Path::new("relative/file"));
        assert!(destructive_res.is_err(), "expected error when cwd is deleted");
        let destructive_err = destructive_res.unwrap_err().to_string();
        assert!(
            destructive_err.contains("[FAIL-CLOSED GUARD] failed to resolve"),
            "destructive error message must contain '[FAIL-CLOSED GUARD] failed to resolve', got: {}",
            destructive_err
        );

        let read_res = assert_safe_read_path(Path::new("relative/file"));
        assert!(read_res.is_err(), "expected error when cwd is deleted");
        let read_err = read_res.unwrap_err().to_string();
        assert!(
            read_err.contains("[FAIL-CLOSED GUARD] failed to resolve"),
            "read error message must contain '[FAIL-CLOSED GUARD] failed to resolve', got: {}",
            read_err
        );

        set_test_mode(orig_mode);
    }
}
