use std::path::PathBuf;

use crate::cli::{CLR_GREEN, CLR_RED, CLR_RESET, Io};
use crate::installer::{self, Options};

pub async fn handle_install(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut opts = Options::default();
    if let Ok(exe) = std::env::current_exe() {
        opts.source_binary = exe;
    }

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(
                io.stdout,
                "usage: agy-pool install [--user] [--system] [--prefix PREFIX] [--target DIR] [--bin-dir DIR] [--config-dir DIR] [--data-dir DIR] [--native-agy PATH] [--skip-rc]"
            );
            return 0;
        } else if arg == "--user" {
            opts.is_system = false;
        } else if arg == "--system" {
            opts.is_system = true;
        } else if arg == "--prefix" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool install: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.prefix = args[i + 1].clone();
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--prefix=") {
            opts.prefix = val.to_string();
        } else if arg == "--target" || arg == "--bin-dir" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool install: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.target_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--target=") {
            opts.target_dir = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--bin-dir=") {
            opts.target_dir = PathBuf::from(val);
        } else if arg == "--config-dir" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool install: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.config_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--config-dir=") {
            opts.config_dir = PathBuf::from(val);
        } else if arg == "--data-dir" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool install: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.data_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--data-dir=") {
            opts.data_dir = PathBuf::from(val);
        } else if arg == "--native-agy" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool install: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.native_agy_path = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--native-agy=") {
            opts.native_agy_path = PathBuf::from(val);
        } else if arg == "--skip-rc" {
            opts.skip_shell_rc = true;
        } else {
            let _ = writeln!(io.stderr, "agy-pool install: error: unrecognized argument: {arg}");
            return 2;
        }
        i += 1;
    }

    opts.stdout = Some(io.stdout);
    opts.stderr = Some(io.stderr);
    if let Err(err) = installer::install(opts) {
        let _ = writeln!(io.stderr, "{CLR_RED}[Error] Installation failed: {err}{CLR_RESET}");
        return 1;
    }
    let _ = writeln!(io.stdout, "{CLR_GREEN}✓ Successfully installed agy-pool{CLR_RESET}");
    0
}

pub async fn handle_repair(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut opts = Options::default();
    if let Ok(exe) = std::env::current_exe() {
        opts.source_binary = exe;
    }

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(
                io.stdout,
                "usage: agy-pool repair [--user] [--system] [--prefix PREFIX] [--target DIR] [--bin-dir DIR]"
            );
            return 0;
        } else if arg == "--user" {
            opts.is_system = false;
        } else if arg == "--system" {
            opts.is_system = true;
        } else if arg == "--prefix" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool repair: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.prefix = args[i + 1].clone();
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--prefix=") {
            opts.prefix = val.to_string();
        } else if arg == "--target" || arg == "--bin-dir" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool repair: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.target_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--target=") {
            opts.target_dir = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--bin-dir=") {
            opts.target_dir = PathBuf::from(val);
        } else {
            let _ = writeln!(io.stderr, "agy-pool repair: error: unrecognized argument: {arg}");
            return 2;
        }
        i += 1;
    }

    opts.stdout = Some(io.stdout);
    opts.stderr = Some(io.stderr);
    if let Err(err) = installer::repair(opts) {
        let _ = writeln!(io.stderr, "{CLR_RED}[Error] Repair failed: {err}{CLR_RESET}");
        return 1;
    }
    let _ = writeln!(
        io.stdout,
        "{CLR_GREEN}✓ Successfully repaired agy-pool integration{CLR_RESET}"
    );
    0
}

pub async fn handle_uninstall(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut opts = Options::default();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(
                io.stdout,
                "usage: agy-pool uninstall [--user] [--system] [--prefix PREFIX] [--target DIR] [--bin-dir DIR]"
            );
            return 0;
        } else if arg == "--user" {
            opts.is_system = false;
        } else if arg == "--system" {
            opts.is_system = true;
        } else if arg == "--prefix" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool uninstall: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.prefix = args[i + 1].clone();
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--prefix=") {
            opts.prefix = val.to_string();
        } else if arg == "--target" || arg == "--bin-dir" {
            if i + 1 >= args.len() {
                let _ = writeln!(io.stderr, "agy-pool uninstall: error: flag needs an argument: {arg}");
                return 2;
            }
            opts.target_dir = PathBuf::from(&args[i + 1]);
            i += 1;
        } else if let Some(val) = arg.strip_prefix("--target=") {
            opts.target_dir = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--bin-dir=") {
            opts.target_dir = PathBuf::from(val);
        } else if arg == "--clean-rc" || arg == "--clean-legacy-rc" {
            opts.clean_legacy_rc = true;
        } else {
            let _ = writeln!(io.stderr, "agy-pool uninstall: error: unrecognized argument: {arg}");
            return 2;
        }
        i += 1;
    }

    opts.stdout = Some(io.stdout);
    opts.stderr = Some(io.stderr);
    if let Err(err) = installer::uninstall(opts) {
        let _ = writeln!(io.stderr, "{CLR_RED}[Error] Uninstallation failed: {err}{CLR_RESET}");
        return 1;
    }
    let _ = writeln!(io.stdout, "{CLR_GREEN}✓ Successfully uninstalled agy-pool{CLR_RESET}");
    0
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn test_install_command_precondition_failure() {
        let env = crate::cli::test_support::cli_test_env();
        let target_dir = env.dir.path().join("bin");
        let non_existent_agy = env.dir.path().join("no_such_agy");

        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(
            &[
                "install",
                "--target",
                target_dir.to_str().unwrap(),
                "--native-agy",
                non_existent_agy.to_str().unwrap(),
                "--skip-rc",
            ],
            "",
        )
        .await;

        assert_eq!(
            code, 1,
            "expected exit code 1 on missing native agy, got {code}. stderr: {stderr}"
        );
        assert!(
            stderr.contains("Native agy was not found"),
            "expected error message about native agy, got: {stderr}"
        );
    }

    #[tokio::test]
    async fn test_install_and_uninstall_command_user_mode() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let target_bin = temp_dir.join("usr_bin");
        let config_dir = temp_dir.join("cfg");
        let data_dir = temp_dir.join("data");
        let real_agy_dir = temp_dir.join("real_bin");
        fs::create_dir_all(&real_agy_dir).unwrap();

        let mock_native_agy = real_agy_dir.join("agy");
        fs::write(&mock_native_agy, "#!/bin/sh\necho \"native agy\"\n").unwrap();
        fs::set_permissions(&mock_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        // 1. Install
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(
            &[
                "install",
                "--user",
                "--target",
                target_bin.to_str().unwrap(),
                "--native-agy",
                mock_native_agy.to_str().unwrap(),
                "--config-dir",
                config_dir.to_str().unwrap(),
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--skip-rc",
            ],
            "",
        )
        .await;

        assert_eq!(code, 0, "install failed with exit code {code}, stderr: {stderr}");
        assert!(
            stdout.contains("Successfully installed agy-pool"),
            "expected success message in stdout, got: {stdout}"
        );

        // Verify installed files
        let shim_path = target_bin.join("agy");
        let shim_bytes = fs::read(&shim_path).expect("failed to read installed shim");
        let shim_str = String::from_utf8_lossy(&shim_bytes);
        assert!(
            shim_str.contains("agy-pool run"),
            "expected shim to run agy-pool, got: {shim_str}"
        );

        let raw_path = target_bin.join("agy-raw");
        assert!(raw_path.exists(), "agy-raw was not created");

        let cfg_path = config_dir.join("config.json");
        assert!(cfg_path.exists(), "config.json was not created");

        let locks_dir = data_dir.join("locks");
        assert!(locks_dir.is_dir(), "data locks directory was not created");

        // 2. Uninstall
        let (code, stdout, stderr) =
            crate::cli::test_support::run_cli(&["uninstall", "--target", target_bin.to_str().unwrap()], "").await;

        assert_eq!(code, 0, "uninstall failed with exit code {code}, stderr: {stderr}");
        assert!(
            stdout.contains("Successfully uninstalled agy-pool"),
            "expected success message in stdout, got: {stdout}"
        );

        // Shim in target_bin should be removed
        assert!(
            !shim_path.exists(),
            "shim at {} was not removed during uninstall",
            shim_path.display()
        );

        // Real native agy must still exist!
        assert!(
            mock_native_agy.exists(),
            "CRITICAL: real native agy was deleted by uninstall!"
        );

        // Config and data dir must remain intact
        assert!(
            cfg_path.exists(),
            "user config.json was unexpectedly deleted by uninstall"
        );
        assert!(
            locks_dir.exists(),
            "user data locks dir was unexpectedly deleted by uninstall"
        );
    }

    #[tokio::test]
    async fn test_install_and_uninstall_invalid_args() {
        let _env = crate::cli::test_support::cli_test_env();

        // Missing flag argument
        let (code, _, _) = crate::cli::test_support::run_cli(&["install", "--prefix"], "").await;
        assert_eq!(
            code, 2,
            "expected code 2 for install --prefix without value, got {code}"
        );

        // Unknown argument
        let (code, _, _) = crate::cli::test_support::run_cli(&["install", "--unknown-flag"], "").await;
        assert_eq!(code, 2, "expected code 2 for install --unknown-flag, got {code}");

        // Help flag
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["install", "--help"], "").await;
        assert_eq!(code, 0, "expected code 0 for install --help, got {code}");
        assert!(
            stdout.contains("usage: agy-pool install"),
            "expected usage in help output, got: {stdout}"
        );

        // Uninstall missing flag argument
        let (code, _, _) = crate::cli::test_support::run_cli(&["uninstall", "--target"], "").await;
        assert_eq!(
            code, 2,
            "expected code 2 for uninstall --target without value, got {code}"
        );

        // Uninstall unknown argument
        let (code, _, _) = crate::cli::test_support::run_cli(&["uninstall", "--bogus"], "").await;
        assert_eq!(code, 2, "expected code 2 for uninstall --bogus, got {code}");

        // Uninstall help flag
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["uninstall", "-h"], "").await;
        assert_eq!(code, 0, "expected code 0 for uninstall -h, got {code}");
        assert!(
            stdout.contains("usage: agy-pool uninstall"),
            "expected usage in help output, got: {stdout}"
        );
    }
}
