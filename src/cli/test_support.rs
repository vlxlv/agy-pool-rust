#![cfg(test)]
use std::path::PathBuf;

pub struct CliTestEnv {
    pub dir: tempfile::TempDir,
    pub state_dir: PathBuf,
    pub test_home: PathBuf,
    _sandbox: crate::config::SyntheticSandboxGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for CliTestEnv {
    fn drop(&mut self) {
        // Runs before the fields drop, i.e. while _lock is still held.
        crate::installer::set_test_installer_roots(None);
        crate::daemon::set_entrypoint_provider(None);
        crate::cli::set_exec_handler(None);
        crate::cli::set_login_opener(None);
        crate::config::set_native_agy_dir(std::path::Path::new(""));
        crate::observability::reset_in_flight();
        crate::proxy::model_cooldown::reset_model_cooldowns();
        crate::proxy::affinity::affinity_manager().clear();
        crate::proxy::clear_all_verification_confirmation_claims();
        crate::config::reset_data_dir();
        crate::diagnostics::set_agy_binary_finder(None);
        crate::diagnostics::set_agy_version_provider(None);
        crate::diagnostics::set_tls_prober(None);
    }
}

/// Port of Go's setupCLITestEnv. Hold the returned value for the whole test.
pub fn cli_test_env() -> CliTestEnv {
    let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::config::set_test_mode(true);
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join(".gemini");
    std::fs::create_dir_all(&state_dir).unwrap();
    let sandbox = crate::config::set_synthetic_sandbox_root(dir.path());
    crate::config::set_native_agy_dir(&state_dir);
    crate::config::configure_state_dir(&state_dir).unwrap();
    let test_home = dir.path().join("test_home");
    for sub in [
        ".local/bin",
        ".config/agy-pool",
        ".local/share/agy-pool",
        ".local/libexec/agy-pool",
    ] {
        std::fs::create_dir_all(test_home.join(sub)).unwrap();
    }
    crate::installer::set_test_installer_roots(Some(crate::installer::InstallerRoots {
        user_home: test_home.clone(),
        bin_dir: test_home.join(".local/bin"),
        config_dir: test_home.join(".config/agy-pool"),
        data_dir: test_home.join(".local/share/agy-pool"),
        libexec_dir: test_home.join(".local/libexec/agy-pool"),
        system_root: dir.path().join("test_system"),
    }));
    crate::daemon::set_entrypoint_provider(Some(|| PathBuf::from("/bin/true")));
    // Go's verify calls the real browser opener; tests get a no-op (login tests install their own).
    crate::cli::set_login_opener(Some(|_| Ok(())));
    crate::observability::reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    crate::proxy::affinity::affinity_manager().clear();
    CliTestEnv {
        dir,
        state_dir,
        test_home,
        _sandbox: sandbox,
        _lock: lock,
    }
}

/// Runs the dispatcher with in-memory streams; returns (exit code, stdout, stderr).
pub async fn run_cli(args: &[&str], stdin: &str) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = {
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        crate::cli::main(&args, &mut io).await
    };
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}
