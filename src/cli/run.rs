use crate::cli::Io;
use crate::cli::{CLR_RED, CLR_RESET, CLR_YELLOW};

pub fn parse_run_args(args: &[String]) -> Result<(String, String, Vec<String>), String> {
    let dash_idx = args.iter().position(|a| a == "--");

    let (agy_pool_args, native_args) = if let Some(idx) = dash_idx {
        (&args[..idx], args[idx + 1..].to_vec())
    } else {
        let mut config_file = String::new();
        let mut data_dir = String::new();
        let mut native_args = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "-c" || arg == "--config" {
                if i + 1 >= args.len() {
                    return Err(format!("flag needs an argument: {arg}"));
                }
                config_file = args[i + 1].clone();
                i += 2;
            } else if let Some(val) = arg.strip_prefix("--config=") {
                config_file = val.to_string();
                i += 1;
            } else if arg == "-D" || arg == "--directory" {
                if i + 1 >= args.len() {
                    return Err(format!("flag needs an argument: {arg}"));
                }
                data_dir = args[i + 1].clone();
                i += 2;
            } else if let Some(val) = arg.strip_prefix("--directory=") {
                data_dir = val.to_string();
                i += 1;
            } else {
                native_args = args[i..].to_vec();
                break;
            }
        }
        return Ok((config_file, data_dir, native_args));
    };

    let mut config_file = String::new();
    let mut data_dir = String::new();
    let mut i = 0;
    while i < agy_pool_args.len() {
        let arg = &agy_pool_args[i];
        if arg == "-c" || arg == "--config" {
            if i + 1 >= agy_pool_args.len() {
                return Err(format!("flag needs an argument: {arg}"));
            }
            config_file = agy_pool_args[i + 1].clone();
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--config=") {
            config_file = val.to_string();
        } else if arg == "-D" || arg == "--directory" {
            if i + 1 >= agy_pool_args.len() {
                return Err(format!("flag needs an argument: {arg}"));
            }
            data_dir = agy_pool_args[i + 1].clone();
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--directory=") {
            data_dir = val.to_string();
        } else {
            return Err(format!("unknown option for run: {arg}"));
        }
        i += 1;
    }

    Ok((config_file, data_dir, native_args))
}

pub async fn run_agy_with_lb(io: &mut Io<'_>, extra: &[String]) -> i32 {
    let (config_file, data_dir, native_args) = match parse_run_args(extra) {
        Ok(res) => res,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] agy-pool run: {}{}", CLR_RED, e, CLR_RESET);
            return 2;
        }
    };

    let d_dir = if data_dir.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&data_dir))
    };
    let c_file = if config_file.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&config_file))
    };

    let ctx = match crate::daemon::resolve_instance_context(d_dir, c_file, None, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    if let Err(e) = crate::config::configure_data_dir(&ctx.data_dir) {
        let _ = writeln!(
            io.stderr,
            "{}[Error] Invalid data directory {}: {}{}",
            CLR_RED,
            ctx.data_dir.display(),
            e,
            CLR_RESET
        );
        return 1;
    }

    let (status, info, msg) = crate::daemon::check_instance_status(&ctx);
    match status {
        crate::daemon::InstanceStatus::RunningSameInstance => {}
        crate::daemon::InstanceStatus::OutdatedBinary => {
            let _ = writeln!(
                io.stderr,
                "{}[agy-pool] Outdated daemon detected; reloading...{}",
                CLR_YELLOW, CLR_RESET
            );
            // As Go: empty options; start/restart fill them from the instance context.
            let opts = crate::daemon::LaunchOptions::default();
            if let Err(e) = crate::daemon::restart_instance(&ctx, opts) {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Failed to reload daemon: {}{}",
                    CLR_RED, e, CLR_RESET
                );
                return 1;
            }
        }
        crate::daemon::InstanceStatus::ConfigMismatch => {
            let _ = writeln!(io.stderr, "{}[agy-pool] Notice: {}{}", CLR_YELLOW, msg, CLR_RESET);
            let occupied_cfg = info.as_ref().and_then(|i| i.config_path.as_deref());
            let current_cfg = ctx.config_path.as_ref().map(|p| p.to_str().unwrap_or(""));
            if occupied_cfg != current_cfg {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Port {} is occupied by another daemon with config {:?} (requested {:?}){}",
                    CLR_RED,
                    ctx.listen_port,
                    occupied_cfg.unwrap_or(""),
                    current_cfg.unwrap_or(""),
                    CLR_RESET
                );
                return 1;
            }
        }
        crate::daemon::InstanceStatus::PortOccupiedForeign
        | crate::daemon::InstanceStatus::PortMismatch
        | crate::daemon::InstanceStatus::ForeignInstance => {
            if !crate::config::is_test_mode() {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Port {} or data directory is occupied by another process: {}{}",
                    CLR_RED, ctx.listen_port, msg, CLR_RESET
                );
                return 1;
            }
        }
        // As Go: a daemon with legacy metadata is running, so leave it alone.
        crate::daemon::InstanceStatus::LegacyPID => {}
        crate::daemon::InstanceStatus::Stopped | crate::daemon::InstanceStatus::StalePID => {
            // As Go: empty options; start/restart fill them from the instance context.
            let opts = crate::daemon::LaunchOptions::default();
            if let Err(e) = crate::daemon::start_instance(&ctx, opts)
                && !crate::config::is_test_mode()
            {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Failed to start gateway daemon: {}{}",
                    CLR_RED, e, CLR_RESET
                );
                return 1;
            }
        }
    }

    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k != "CLOUD_CODE_URL").collect();
    let gateway_url = format!("http://{}:{}", ctx.listen_host, ctx.listen_port);
    env.push(("CLOUD_CODE_URL".to_string(), gateway_url));

    if let Err(e) = crate::accounts::sync_active_agy_token_file().await {
        let _ = writeln!(
            io.stderr,
            "{}[Error] Failed to synchronize native agy credentials: {}{}",
            CLR_RED, e, CLR_RESET
        );
        return 1;
    }

    let (agy_path, _) = match crate::diagnostics::resolve_native_agy_binary() {
        Ok(res) => res,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not locate native 'agy' executable: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let resolved_args = crate::conversation::resolve_continue_arg(&native_args, &mut io.stderr);
    let mut cmd = vec![agy_path.to_string_lossy().to_string()];
    cmd.extend(resolved_args);

    let exec = crate::cli::exec_handler();
    let err = exec(&agy_path, &cmd, &env);
    if err.raw_os_error() == Some(0) {
        return 0;
    }
    let _ = writeln!(
        io.stderr,
        "{}[Error] Failed to execute agy: {}{}",
        CLR_RED, err, CLR_RESET
    );
    1
}

pub async fn run_agy_direct(io: &mut Io<'_>, extra: &[String]) -> i32 {
    let native_args = if let Some(idx) = extra.iter().position(|a| a == "--") {
        extra[idx + 1..].to_vec()
    } else {
        extra.to_vec()
    };

    let env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k != "CLOUD_CODE_URL").collect();

    let (agy_path, _) = match crate::diagnostics::resolve_native_agy_binary() {
        Ok(res) => res,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not locate native 'agy' executable: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let resolved_args = crate::conversation::resolve_continue_arg(&native_args, &mut io.stderr);
    let mut cmd = vec![agy_path.to_string_lossy().to_string()];
    cmd.extend(resolved_args);

    let exec = crate::cli::exec_handler();
    let err = exec(&agy_path, &cmd, &env);
    if err.raw_os_error() == Some(0) {
        return 0;
    }
    let _ = writeln!(
        io.stderr,
        "{}[Error] Failed to execute agy: {}{}",
        CLR_RED, err, CLR_RESET
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::{cli_test_env, run_cli};
    use std::sync::Arc;
    use std::sync::Mutex;

    struct CapturedExec {
        binary: std::path::PathBuf,
        argv: Vec<String>,
        env: Vec<(String, String)>,
    }

    static CAPTURED_EXEC: Mutex<Option<CapturedExec>> = Mutex::new(None);

    fn recording_exec(binary: &std::path::Path, argv: &[String], env: &[(String, String)]) -> std::io::Error {
        let mut lock = CAPTURED_EXEC.lock().unwrap();
        *lock = Some(CapturedExec {
            binary: binary.to_path_buf(),
            argv: argv.to_vec(),
            env: env.to_vec(),
        });
        std::io::Error::from_raw_os_error(0)
    }

    struct EnvVarGuard {
        key: &'static str,
        orig: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
            let orig = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, val);
            }
            Self { key, orig }
        }
        fn remove(key: &'static str) -> Self {
            let orig = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, orig }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.orig {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[tokio::test]
    async fn test_run_wrapper_and_agy_direct() {
        let _env = cli_test_env();

        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(|| "/fake/bin/agy".to_string())));
        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        // 1. Test "run"
        let (code, _, _) = run_cli(&["run", "--some-flag", "val"], "").await;
        assert_eq!(code, 0, "run expected 0, got {code}");

        let captured = CAPTURED_EXEC
            .lock()
            .unwrap()
            .take()
            .expect("ExecHandler was not called");
        assert_eq!(captured.binary, std::path::PathBuf::from("/fake/bin/agy"));
        assert!(captured.argv.len() >= 3 && captured.argv[1] == "--some-flag" && captured.argv[2] == "val");

        let has_cloud_code_url = captured
            .env
            .iter()
            .any(|(k, v)| k == "CLOUD_CODE_URL" && v.starts_with("http://127.0.0.1:8899"));
        assert!(has_cloud_code_url, "expected CLOUD_CODE_URL in env for run");

        // 2. Test "raw"
        *CAPTURED_EXEC.lock().unwrap() = None;
        let (code, _, _) = run_cli(&["raw", "--direct-flag"], "").await;
        assert_eq!(code, 0, "raw expected 0, got {code}");

        let captured_raw = CAPTURED_EXEC
            .lock()
            .unwrap()
            .take()
            .expect("ExecHandler was not called for raw");
        for (k, _) in &captured_raw.env {
            assert_ne!(k, "CLOUD_CODE_URL", "raw mode must NOT set CLOUD_CODE_URL");
        }
    }

    #[test]
    fn test_parse_run_args() {
        let _env = cli_test_env();
        let (cfg, data, native) = parse_run_args(&[
            "-c".to_string(),
            "cfg.json".to_string(),
            "-D".to_string(),
            "data".to_string(),
            "--".to_string(),
            "-c".to_string(),
            "foo".to_string(),
            "-D".to_string(),
            "bar".to_string(),
            "--future-option".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg, "cfg.json");
        assert_eq!(data, "data");
        assert_eq!(native, vec!["-c", "foo", "-D", "bar", "--future-option"]);

        let (cfg, data, native) = parse_run_args(&[
            "-c".to_string(),
            "/etc/agy-pool/config.json".to_string(),
            "-D".to_string(),
            "/var/lib/agy-pool".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg, "/etc/agy-pool/config.json");
        assert_eq!(data, "/var/lib/agy-pool");
        assert!(native.is_empty());

        let (cfg, data, native) = parse_run_args(&[
            "-c".to_string(),
            "~/.config/agy-pool/config.json".to_string(),
            "-D".to_string(),
            "~/.local/share/agy-pool".to_string(),
            "--".to_string(),
            "--conversation=abc123".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg, "~/.config/agy-pool/config.json");
        assert_eq!(data, "~/.local/share/agy-pool");
        assert_eq!(native, vec!["--conversation=abc123"]);

        assert!(parse_run_args(&["-c".to_string()]).is_err());
        assert!(parse_run_args(&["-D".to_string()]).is_err());
        assert!(
            parse_run_args(&[
                "-c".to_string(),
                "cfg.json".to_string(),
                "--unknown-flag".to_string(),
                "--".to_string(),
                "native".to_string(),
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn test_run_argument_scoping_and_delimiter() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let cfg_file = tmp_dir.join("config.json");
        let cfg_content = r#"{"version": 1, "server": {"listen": "127.0.0.1", "port": 8899}, "scheduler": {"strategy": "max_quota"}, "native_agy": {"binary": null}, "logging": {"max_size_bytes": 5242880, "backup_count": 1}}"#;
        std::fs::write(&cfg_file, cfg_content).unwrap();

        let data_dir = tmp_dir.join("data");

        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(|| "/fake/bin/agy".to_string())));
        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(
            &[
                "run",
                "-c",
                &cfg_file.to_string_lossy(),
                "-D",
                &data_dir.to_string_lossy(),
                "--",
                "-c",
                "foo",
                "-D",
                "bar",
                "--future-option",
            ],
            "",
        )
        .await;

        assert_eq!(code, 0, "run expected 0, got {code}. stderr: {stderr}");
        let captured = CAPTURED_EXEC.lock().unwrap().take().unwrap();
        assert_eq!(captured.binary, std::path::PathBuf::from("/fake/bin/agy"));

        let expected_argv = vec!["/fake/bin/agy", "-c", "foo", "-D", "bar", "--future-option"];
        assert_eq!(captured.argv, expected_argv);

        assert_eq!(crate::config::data_dir(), data_dir);
    }

    #[tokio::test]
    async fn test_run_top_level_options_rejected() {
        let _env = cli_test_env();

        let (code, _, stderr) = run_cli(&["-c", "/etc/config.json", "list"], "").await;
        assert_eq!(code, 2, "expected exit code 2 for top-level -c, got {code}");
        assert!(
            stderr.contains("invalid choice: '-c'"),
            "expected invalid choice error for -c, got: {stderr}"
        );

        let (code, _, stderr) = run_cli(&["-D", "/var/lib/data", "status"], "").await;
        assert_eq!(code, 2, "expected exit code 2 for top-level -D, got {code}");
        assert!(
            stderr.contains("invalid choice: '-D'"),
            "expected invalid choice error for -D, got: {stderr}"
        );
    }

    #[tokio::test]
    async fn test_run_bare_continue_resolves_conversation() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let cli_dir = crate::config::agy_cli_dir();
        std::fs::create_dir_all(&cli_dir).unwrap();
        let db_path = cli_dir.join("conversation_summaries.db");

        if let Ok(conn) = rusqlite::Connection::open(&db_path) {
            let _ = conn.execute(
                "CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, workspace_uris TEXT, last_modified_time INTEGER);",
                [],
            );
            let _ = conn.execute(
                "INSERT INTO conversation_summaries VALUES ('conv_12345678', 'Test Session', ?1, 5000);",
                rusqlite::params![format!(r#"["file://{}"]"#, tmp_dir.display())],
            );
        }

        let cfg_file = tmp_dir.join("config.json");
        let data_dir = tmp_dir.join("state_run");
        std::fs::write(&cfg_file, r#"{"version": 1, "server": {"port": 9199}}"#).unwrap();

        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(|| "/fake/bin/agy".to_string())));
        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(
            &[
                "run",
                "-c",
                &cfg_file.to_string_lossy(),
                "-D",
                &data_dir.to_string_lossy(),
                "--",
                "-c",
            ],
            "",
        )
        .await;

        assert_eq!(code, 0, "run expected 0, got {code}. stderr: {stderr}");
        let captured = CAPTURED_EXEC.lock().unwrap().take().unwrap();
        assert_eq!(captured.binary, std::path::PathBuf::from("/fake/bin/agy"));
        assert!(!captured.argv.is_empty());
        assert_eq!(crate::config::data_dir(), data_dir);
    }

    #[test]
    fn test_find_real_agy_binary_excludes_shims() {
        let _env = cli_test_env();

        crate::diagnostics::set_agy_binary_finder(None);

        let temp_dir = tempfile::tempdir().unwrap();
        let shim_dir = temp_dir.path().join("shim_bin");
        let real_dir = temp_dir.path().join("real_bin");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        let shim_file = shim_dir.join("agy");
        let shim_content = "#!/bin/sh\n# agy: agy-pool integration shim\nexec agy-pool run \"$@\"\n";
        std::fs::write(&shim_file, shim_content).unwrap();

        let real_file = real_dir.join("agy");
        let real_content = "#!/bin/sh\necho \"real native agy v1.0.0\"\n";
        std::fs::write(&real_file, real_content).unwrap();

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_file, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&real_file, std::fs::Permissions::from_mode(0o755)).unwrap();

        let new_path = format!("{}:{}", shim_dir.display(), real_dir.display());
        let _p_guard = EnvVarGuard::set("PATH", &new_path);
        let _h_guard = EnvVarGuard::set("HOME", temp_dir.path());
        let _prefix_guard = EnvVarGuard::remove("PREFIX");
        let _bin_guard = EnvVarGuard::remove("AGY_BIN");

        let found = crate::diagnostics::find_real_agy_binary();

        assert_eq!(
            found,
            Some(real_file.clone()),
            "expected FindRealAgyBinary to skip shim {} and find real binary {}, got: {:?}",
            shim_file.display(),
            real_file.display(),
            found
        );
    }

    #[tokio::test]
    async fn test_run_agy_with_lb_argument_preservation() {
        let _env = cli_test_env();

        let tmp_dir = tempfile::tempdir().unwrap();
        let data_dir = tmp_dir.path().join("data");
        let cfg_file = tmp_dir.path().join("config.json");
        std::fs::write(
            &cfg_file,
            r#"{"version":1,"server":{"listen":"127.0.0.1","port":9999}}"#,
        )
        .unwrap();

        let fake_agy = tmp_dir.path().join("agy");
        std::fs::write(&fake_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_agy_str = fake_agy.to_string_lossy().to_string();
        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(move || fake_agy_str.clone())));

        let cfg_str = cfg_file.to_string_lossy().to_string();
        let data_str = data_dir.to_string_lossy().to_string();
        let fake_agy_path_str = fake_agy.to_string_lossy().to_string();

        let test_cases = [
            (
                "flag -c",
                vec![
                    "run".to_string(),
                    "-c".to_string(),
                    cfg_str.clone(),
                    "-D".to_string(),
                    data_str.clone(),
                    "--".to_string(),
                    "-c".to_string(),
                ],
                vec![fake_agy_path_str.clone(), "-c".to_string()],
            ),
            (
                "conversation flag",
                vec![
                    "run".to_string(),
                    "-c".to_string(),
                    cfg_str.clone(),
                    "-D".to_string(),
                    data_str.clone(),
                    "--".to_string(),
                    "--conversation=conv-1234".to_string(),
                ],
                vec![fake_agy_path_str.clone(), "--conversation=conv-1234".to_string()],
            ),
            (
                "prompt flag",
                vec![
                    "run".to_string(),
                    "-c".to_string(),
                    cfg_str.clone(),
                    "-D".to_string(),
                    data_str.clone(),
                    "--".to_string(),
                    "-p".to_string(),
                    "hello".to_string(),
                ],
                vec![fake_agy_path_str.clone(), "-p".to_string(), "hello".to_string()],
            ),
        ];

        for (name, args, expected_args) in test_cases {
            *CAPTURED_EXEC.lock().unwrap() = None;
            crate::cli::set_exec_handler(Some(recording_exec));

            let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let (code, _, stderr) = run_cli(&str_args, "").await;
            assert_eq!(code, 0, "case {name}: expected code 0, got {code} (stderr: {stderr})");

            let captured = CAPTURED_EXEC.lock().unwrap().take().unwrap();
            assert_eq!(captured.binary, fake_agy, "case {name}: binary mismatch");
            assert_eq!(captured.argv, expected_args, "case {name}: argv mismatch");
        }
    }

    #[tokio::test]
    async fn test_run_agy_with_lb_resolves_managed_native_binary() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let fake_home = tmp_dir.join("user_home");
        let libexec_dir = fake_home.join(".local/libexec/agy-pool");
        std::fs::create_dir_all(&libexec_dir).unwrap();
        let managed_agy = libexec_dir.join("agy-native");
        std::fs::write(&managed_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&managed_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _h_guard = EnvVarGuard::set("HOME", &fake_home);
        crate::diagnostics::set_agy_binary_finder(None);

        let cfg_file = fake_home.join("config.json");
        let data_dir = fake_home.join("data");
        std::fs::write(
            &cfg_file,
            r#"{"version":1,"server":{"listen":"127.0.0.1","port":9876}}"#,
        )
        .unwrap();

        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(
            &[
                "run",
                "-c",
                &cfg_file.to_string_lossy(),
                "-D",
                &data_dir.to_string_lossy(),
                "--",
                "--dangerously-skip-permissions",
                "-p",
                "hello",
            ],
            "",
        )
        .await;

        assert_eq!(code, 0, "expected code 0, got {code} (stderr: {stderr})");
        let captured = CAPTURED_EXEC.lock().unwrap().take().unwrap();
        assert_eq!(captured.binary, managed_agy);

        let has_cloud_code_url = captured
            .env
            .iter()
            .any(|(k, v)| k == "CLOUD_CODE_URL" && v.starts_with("http://127.0.0.1:9876"));
        assert!(
            has_cloud_code_url,
            "expected CLOUD_CODE_URL to be injected in env, got: {:?}",
            captured.env
        );

        let expected_args = vec![
            managed_agy.to_string_lossy().to_string(),
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];
        assert_eq!(captured.argv, expected_args);
    }

    #[tokio::test]
    async fn test_run_agy_direct_resolves_managed_native_binary() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let fake_home = tmp_dir.join("user_home");
        let libexec_dir = fake_home.join(".local/libexec/agy-pool");
        std::fs::create_dir_all(&libexec_dir).unwrap();
        let managed_agy = libexec_dir.join("agy-native");
        std::fs::write(&managed_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&managed_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _h_guard = EnvVarGuard::set("HOME", &fake_home);
        let _url_guard = EnvVarGuard::set("CLOUD_CODE_URL", "http://127.0.0.1:9999");
        crate::diagnostics::set_agy_binary_finder(None);

        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(&["raw", "--", "--mode", "accept-edits"], "").await;

        assert_eq!(code, 0, "expected code 0, got {code} (stderr: {stderr})");
        let captured = CAPTURED_EXEC.lock().unwrap().take().unwrap();
        assert_eq!(captured.binary, managed_agy);

        for (k, _) in &captured.env {
            assert_ne!(
                k, "CLOUD_CODE_URL",
                "expected CLOUD_CODE_URL to be stripped in raw bypass"
            );
        }

        let expected_args = vec![
            managed_agy.to_string_lossy().to_string(),
            "--mode".to_string(),
            "accept-edits".to_string(),
        ];
        assert_eq!(captured.argv, expected_args);
    }

    #[tokio::test]
    async fn test_run_agy_with_lb_valid_sync_executes_agy() {
        let _env = cli_test_env();

        let mut pool = crate::storage::Pool::new_empty();
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            email: "user@example.com".to_string(),
            access_token: "valid-access-token".to_string(),
            ..Default::default()
        };
        pool.accounts = vec![acc];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        let tmp_dir = tempfile::tempdir().unwrap();
        let fake_agy = tmp_dir.path().join("agy");
        std::fs::write(&fake_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_agy_str = fake_agy.to_string_lossy().to_string();
        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(move || fake_agy_str.clone())));

        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(&["run", "--", "-p", "hello"], "").await;
        assert_eq!(code, 0, "expected code 0 on valid sync, got {code} (stderr: {stderr})");
        assert!(
            CAPTURED_EXEC.lock().unwrap().is_some(),
            "expected ExecHandler to be called on valid sync"
        );

        let token_file = crate::config::agy_token_file();
        assert!(
            token_file.exists(),
            "expected token file {} to exist after sync",
            token_file.display()
        );
    }

    #[tokio::test]
    async fn test_run_agy_with_lb_sync_failure_aborts() {
        let _env = cli_test_env();

        let secret_token = "confidential-token-secret-999";
        let secret_email = "super-confidential@secret-corp.internal";
        let mut pool = crate::storage::Pool::new_empty();
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            email: secret_email.to_string(),
            access_token: secret_token.to_string(),
            ..Default::default()
        };
        pool.accounts = vec![acc];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        // Make token path a directory so writing fails
        std::fs::create_dir_all(crate::config::agy_token_file()).unwrap();

        let tmp_dir = tempfile::tempdir().unwrap();
        let fake_agy = tmp_dir.path().join("agy");
        std::fs::write(&fake_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_agy_str = fake_agy.to_string_lossy().to_string();
        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(move || fake_agy_str.clone())));

        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(&["run", "--", "-p", "hello"], "").await;
        assert_ne!(code, 0, "expected non-zero exit code on sync failure, got 0");
        assert!(
            CAPTURED_EXEC.lock().unwrap().is_none(),
            "CRITICAL: ExecHandler was called despite sync failure!"
        );

        assert!(
            stderr.contains("Failed to synchronize native agy credentials"),
            "expected sync failure error in stderr, got: {stderr}"
        );
        assert!(
            !stderr.contains(secret_token),
            "token secret leaked in stderr: {stderr}"
        );
        assert!(!stderr.contains(secret_email), "email leaked in stderr: {stderr}");
    }

    #[tokio::test]
    async fn test_cp1d_run_sync_failure_does_not_exec() {
        let _env = cli_test_env();

        let mut pool = crate::storage::Pool::new_empty();
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            access_token: "access-secret".to_string(),
            ..Default::default()
        };
        pool.accounts = vec![acc];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        std::fs::create_dir_all(crate::config::agy_token_file()).unwrap();

        let tmp_dir = tempfile::tempdir().unwrap();
        let fake_agy = tmp_dir.path().join("agy");
        std::fs::write(&fake_agy, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_agy_str = fake_agy.to_string_lossy().to_string();
        crate::diagnostics::set_agy_binary_finder(Some(Arc::new(move || fake_agy_str.clone())));

        *CAPTURED_EXEC.lock().unwrap() = None;
        crate::cli::set_exec_handler(Some(recording_exec));

        let (code, _, stderr) = run_cli(&["run", "--", "-p", "hello"], "").await;
        assert_ne!(code, 0);
        assert!(CAPTURED_EXEC.lock().unwrap().is_none());
        assert!(!stderr.contains("access-secret"));
    }
}
