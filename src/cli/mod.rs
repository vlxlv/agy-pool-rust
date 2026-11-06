pub mod accounts;
pub mod daemon_cmd;
pub mod eval;
pub mod forecast;
pub mod install;
pub mod order;
pub mod qrcode;
pub mod render;
pub mod run;
pub mod stats;
pub mod status;
pub mod term;
pub mod test_support;
pub mod top;
pub mod update;
pub mod width;

use std::io::Write;
use std::sync::Mutex;

pub struct Io<'a> {
    pub stdin: &'a mut dyn std::io::BufRead,
    pub stdout: &'a mut dyn std::io::Write,
    pub stderr: &'a mut dyn std::io::Write,
    /// fd behind stdout when it is the process's real stdout (Go's `w.(*os.File)`); None for buffers.
    pub stdout_fd: Option<i32>,
    /// fd behind stdin when it is the process's real stdin; None for buffers.
    pub stdin_fd: Option<i32>,
}

pub const CLR_RESET: &str = "\x1b[0m";
pub const CLR_BOLD: &str = "\x1b[1m";
pub const CLR_DIM: &str = "\x1b[2m";
pub const CLR_GREEN: &str = "\x1b[32m";
pub const CLR_YELLOW: &str = "\x1b[33m";
pub const CLR_BLUE: &str = "\x1b[34m";
pub const CLR_MAGENTA: &str = "\x1b[35m";
pub const CLR_CYAN: &str = "\x1b[36m";
pub const CLR_RED: &str = "\x1b[31m";

pub type ExecFn = fn(binary: &std::path::Path, argv: &[String], env: &[(String, String)]) -> std::io::Error;

#[cfg(test)]
fn default_exec(_binary: &std::path::Path, _argv: &[String], _env: &[(String, String)]) -> std::io::Error {
    panic!("exec_handler called in test without mock");
}

#[cfg(not(test))]
fn default_exec(binary: &std::path::Path, argv: &[String], env: &[(String, String)]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(binary);
    cmd.args(argv);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.exec()
}

static EXEC_HANDLER: Mutex<Option<ExecFn>> = Mutex::new(None);

pub fn set_exec_handler(f: Option<ExecFn>) {
    let mut lock = EXEC_HANDLER.lock().unwrap_or_else(|e| e.into_inner());
    *lock = f;
}

pub fn exec_handler() -> ExecFn {
    let lock = EXEC_HANDLER.lock().unwrap_or_else(|e| e.into_inner());
    lock.unwrap_or(default_exec)
}

pub type OpenerFn = fn(url: &str) -> Result<(), String>;

static LOGIN_OPENER: Mutex<Option<OpenerFn>> = Mutex::new(None);

pub fn set_login_opener(f: Option<OpenerFn>) {
    let mut lock = LOGIN_OPENER.lock().unwrap_or_else(|e| e.into_inner());
    *lock = f;
}

pub fn login_opener() -> Option<OpenerFn> {
    let lock = LOGIN_OPENER.lock().unwrap_or_else(|e| e.into_inner());
    *lock
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceFlags {
    pub config_file: String,
    pub data_dir: String,
    pub port: i64,
    pub rest: Vec<String>,
}

pub fn parse_instance_flags(args: &[String]) -> Result<InstanceFlags, String> {
    let mut config_file = String::new();
    let mut data_dir = String::new();
    let mut port: i64 = 0;
    let mut rest = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-c" || arg == "--config" {
            if i + 1 >= args.len() {
                return Err(format!("flag needs an argument: {arg}"));
            }
            config_file = args[i + 1].clone();
            i += 1;
        } else if let Some(stripped) = arg.strip_prefix("--config=") {
            config_file = stripped.to_string();
        } else if arg == "-D" || arg == "--directory" {
            if i + 1 >= args.len() {
                return Err(format!("flag needs an argument: {arg}"));
            }
            data_dir = args[i + 1].clone();
            i += 1;
        } else if let Some(stripped) = arg.strip_prefix("--directory=") {
            data_dir = stripped.to_string();
        } else if arg == "-p" || arg == "--port" {
            if i + 1 >= args.len() {
                return Err(format!("flag needs an argument: {arg}"));
            }
            let next = &args[i + 1];
            match next.parse::<i64>() {
                Ok(p) => {
                    port = p;
                    i += 1;
                }
                Err(e) => return Err(format!("invalid port \"{next}\": {e}")),
            }
        } else if let Some(stripped) = arg.strip_prefix("--port=") {
            match stripped.parse::<i64>() {
                Ok(p) => port = p,
                Err(_) => return Err(format!("invalid port: {arg}")),
            }
        } else {
            rest.push(arg.clone());
        }
        i += 1;
    }

    Ok(InstanceFlags {
        config_file,
        data_dir,
        port,
        rest,
    })
}

pub fn resolve_context_for_cmd(args: &[String]) -> Result<(crate::daemon::InstanceContext, Vec<String>), String> {
    let flags = parse_instance_flags(args)?;
    let d_dir = if flags.data_dir.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&flags.data_dir))
    };
    let c_file = if flags.config_file.is_empty() {
        None
    } else {
        Some(std::path::Path::new(&flags.config_file))
    };
    let port = if flags.port > 0 { Some(flags.port as u16) } else { None };
    let ctx = crate::daemon::resolve_instance_context(d_dir, c_file, port, None).map_err(|e| e.to_string())?;
    crate::config::configure_data_dir(&ctx.data_dir).map_err(|e| e.to_string())?;
    Ok((ctx, flags.rest))
}

pub fn redact_user_url(raw: &str) -> (String, bool) {
    let mut u = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return (String::new(), false),
    };
    if u.scheme().is_empty() || u.host_str().unwrap_or("").is_empty() {
        return (String::new(), false);
    }

    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .map(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            match lower.as_str() {
                "email" | "login_hint" | "user" | "account" | "token" | "access_token" | "refresh_token"
                | "id_token" | "code" => (k.into_owned(), "REDACTED".to_string()),
                _ => (k.into_owned(), v.into_owned()),
            }
        })
        .collect();

    u.query_pairs_mut()
        .clear()
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    u.set_fragment(None);
    (u.to_string(), true)
}

fn print_help(out: &mut dyn Write) {
    let _ = write!(
        out,
        "usage: agy-pool [-h] [-v] {{login,add,import-current,list,ls,quota,strategy,strat,rename,doctor,check,health,remove,rm,switch,export,backup,import,restore,verify,disable,enable,move,mv,swap,sort,reorder,order,config,log,logs,start,stop,restart,status,stats,forecast,runway,predict,top,watch,monitor,update,version,run}} ...\n\n\
Antigravity Multi-Account Pool & Load Balancer for Termux & Linux\n\n\
Examples:\n\
  agy-pool login            Add/login a new Google account\n\
  agy-pool list [-e]        List accounts in rotation pool (-e to show email)\n\
  agy-pool quota [-e]       Display per-account quota progress bars (-e to show email)\n\
  agy-pool top [-e]         Interactive real-time quota & activity dashboard (-e to show email)\n\
  agy-pool stats [--json]   Track and evaluate algorithm routing and rotation effectiveness\n\
  agy-pool forecast         Predict quota endurance and runway based on usage\n\
  agy-pool status           Show gateway process and runtime summary\n\
  agy-pool update [--check] Update agy-pool to the latest GitHub release\n\
  agy-pool strategy         View or change load balancing strategy\n\
  agy-pool rename 1 \"Work\"  Set friendly label for an account\n\
  agy-pool move 2 top       Move account to a specific position\n\
  agy-pool swap 1 2         Swap positions of two accounts\n\
  agy-pool sort smart       Sort accounts by smart (renewal priority), quota, hits, name, email, or status\n\
  agy-pool reorder 3 1 2    Batch reorder accounts in custom order\n\
  agy-pool disable 4        Disable an account in rotation pool\n\
  agy-pool enable 4         Enable a previously disabled account\n\
  agy-pool doctor           Run comprehensive system diagnostic check\n\
  agy-pool switch auto      Switch CLI base account to the one with highest quota\n\
  agy-pool export           Export accounts pool to a backup JSON file\n\
  agy-pool import file.json Restore accounts pool from a backup file\n\
  agy-pool start            Start the load balancing reverse proxy daemon\n\
  agy-pool run              Launch agy with automated quota load balancing\n"
    );
}

pub async fn main(args: &[String], io: &mut Io<'_>) -> i32 {
    if std::env::var("AGY_POOL_DAEMON_CHILD").as_deref() == Ok("1") {
        return crate::daemon::run_daemon_child(&mut io.stderr);
    }

    if args.is_empty() {
        return accounts::list_cmd(io, &[]).await;
    }

    let cmd = &args[0];

    match cmd.as_str() {
        "-v" | "--version" | "version" => {
            let _ = writeln!(io.stdout, "agy-pool {}", crate::VERSION);
            0
        }
        "-h" | "--help" | "help" => {
            print_help(io.stdout);
            0
        }
        "run" => run::run_agy_with_lb(io, &args[1..]).await,
        "raw" | "orig" | "direct" => run::run_agy_direct(io, &args[1..]).await,
        "doctor" | "check" | "health" => {
            if !crate::diagnostics::run_doctor(&mut io.stdout) {
                1
            } else {
                0
            }
        }
        "login" | "add" => accounts::login_cmd(io).await,
        "import-current" => match crate::accounts::import_current().await {
            Ok(acc) => {
                if acc.id.is_empty() {
                    let _ = writeln!(
                        io.stderr,
                        "{}[Error] Current native credentials were not imported.{}",
                        CLR_RED, CLR_RESET
                    );
                    1
                } else {
                    let _ = writeln!(
                        io.stdout,
                        "{}✓ Imported current native credentials into the pool.{}",
                        CLR_GREEN, CLR_RESET
                    );
                    0
                }
            }
            Err(e) => {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Could not import current native credentials: {}{}",
                    CLR_RED, e, CLR_RESET
                );
                1
            }
        },
        "list" | "ls" => accounts::list_cmd(io, &args[1..]).await,
        "quota" => accounts::quota_cmd(io, &args[1..]).await,
        "top" | "watch" | "monitor" => top::top_cmd(io, &args[1..]).await,
        "strategy" | "strat" => {
            let target = if args.len() > 1 { &args[1] } else { "" };
            if !status::manage_strategy(io, target).await {
                1
            } else {
                0
            }
        }
        "rename" => {
            if args.len() < 3 {
                let _ = writeln!(
                    io.stderr,
                    "usage: agy-pool rename [-h] target name\nagy-pool rename: error: the following arguments are required: target, name"
                );
                return 2;
            }
            if !accounts::rename_account(io, &args[1], &args[2]).await {
                1
            } else {
                0
            }
        }
        "remove" | "rm" => {
            if args.len() < 2 {
                let _ = writeln!(
                    io.stderr,
                    "usage: agy-pool remove [-h] target\nagy-pool remove: error: the following arguments are required: target"
                );
                return 2;
            }
            if !accounts::remove_account(io, &args[1]).await {
                1
            } else {
                0
            }
        }
        "disable" => {
            if args.len() < 2 {
                let _ = writeln!(
                    io.stderr,
                    "usage: agy-pool disable [-h] target\nagy-pool disable: error: the following arguments are required: target"
                );
                return 2;
            }
            if !accounts::disable_account(io, &args[1]).await {
                1
            } else {
                0
            }
        }
        "enable" => {
            if args.len() < 2 {
                let _ = writeln!(
                    io.stderr,
                    "usage: agy-pool enable [-h] target\nagy-pool enable: error: the following arguments are required: target"
                );
                return 2;
            }
            if !accounts::enable_account(io, &args[1]).await {
                1
            } else {
                0
            }
        }
        "move" | "mv" => order::move_cmd(io, &args[1..]).await,
        "swap" => order::swap_cmd(io, &args[1..]).await,
        "sort" => order::sort_cmd(io, &args[1..]).await,
        "reorder" | "order" => order::reorder_cmd(io, &args[1..]).await,
        "config" => order::config_cmd(io, &args[1..]).await,
        "switch" => {
            let target = if args.len() > 1 { &args[1] } else { "auto" };
            if !accounts::switch_account(io, target).await {
                1
            } else {
                0
            }
        }
        "export" | "backup" => accounts::handle_export(io, &args[1..]).await,
        "import" | "restore" => accounts::handle_import(io, &args[1..]).await,
        "verify" => {
            let target = if args.len() > 1 { &args[1] } else { "" };
            let (acc, verification_url) = match crate::accounts::verify_account(target).await {
                Ok(res) => res,
                Err(e) => {
                    let _ = writeln!(
                        io.stderr,
                        "{}[Error] Verification could not be completed: {}{}",
                        CLR_RED, e, CLR_RESET
                    );
                    return 1;
                }
            };
            let label = crate::accounts::display_account_name(Some(&acc), None);
            if verification_url.is_empty() {
                let _ = writeln!(
                    io.stdout,
                    "{}🎉 Verification Successful! Account is Ready!{}",
                    CLR_GREEN, CLR_RESET
                );
                return 0;
            }
            let _ = writeln!(io.stdout, "Verification required for {label}.");
            let _ = writeln!(io.stdout, "Open the verification URL shown below:");
            let (safe_url, ok) = redact_user_url(&verification_url);
            if ok {
                let _ = writeln!(io.stdout, "{safe_url}");
                if let Some(opener) = login_opener() {
                    let _ = opener(&verification_url);
                } else {
                    let _ = crate::accounts::default_browser_opener(&verification_url);
                }
                let is_tty = io.stdout_fd.map(term::is_terminal).unwrap_or(false);
                let tw = render::terminal_width(io.stdout_fd);
                qrcode::render_terminal_qr_code(io.stdout, &verification_url, is_tty, tw);
                let _ = writeln!(io.stdout, "\nInstructions:");
                let _ = writeln!(
                    io.stdout,
                    "  1. Complete Google verification in your browser or scan the QR code with your phone."
                );
                let _ = writeln!(
                    io.stdout,
                    "  2. Wait until redirected to 'auth_success_gemini' (Verification Successful)."
                );
                let _ = writeln!(io.stdout, "  3. Re-run 'agy-pool verify' to confirm.");

                let stdin_is_tty = io.stdin_fd.map(term::is_terminal).unwrap_or(false);
                if stdin_is_tty {
                    let _ = write!(
                        io.stdout,
                        "\nPress [Enter] here once verification is completed in browser..."
                    );
                    let _ = io.stdout.flush();
                    let mut line = String::new();
                    if io.stdin.read_line(&mut line).is_ok() {
                        let _ = writeln!(io.stdout, "\nRe-probing status for {label}...");
                        if let Ok((recheck_acc, _)) = crate::accounts::verify_account(target).await {
                            if recheck_acc.status != crate::storage::STATUS_VALIDATION_REQUIRED {
                                let _ = writeln!(
                                    io.stdout,
                                    "{}🎉 Verification Successful! Account is Ready!{}",
                                    CLR_GREEN, CLR_RESET
                                );
                            } else {
                                let _ = writeln!(
                                    io.stdout,
                                    "Account still appears restricted. If you just completed verification, wait a few moments and re-run 'agy-pool verify {target}'."
                                );
                            }
                        } else {
                            let _ = writeln!(
                                io.stdout,
                                "Account still appears restricted. If you just completed verification, wait a few moments and re-run 'agy-pool verify {target}'."
                            );
                        }
                    }
                }
            } else {
                let _ = writeln!(io.stdout, "Re-run login/re-authentication for this account.");
            }
            0
        }
        "log" | "logs" => daemon_cmd::handle_logs(io, &args[1..]).await,
        "start" => daemon_cmd::start_daemon_cmd(io, &args[1..]).await,
        "stop" => daemon_cmd::stop_daemon_cmd(io, &args[1..]).await,
        "restart" => daemon_cmd::restart_daemon_cmd(io, &args[1..]).await,
        "status" => status::status_cmd(io, &args[1..]).await,
        "stats" | "audit" | "effectiveness" => stats::stats_cmd(io, &args[1..]).await,
        "forecast" | "runway" | "predict" => {
            if let Err(e) = forecast::forecast_cmd(io, &args[1..]).await {
                let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
                1
            } else {
                0
            }
        }
        "migrate-legacy" => accounts::handle_migrate_legacy(io, &args[1..]).await,
        "install" => install::handle_install(io, &args[1..]).await,
        "repair" => install::handle_repair(io, &args[1..]).await,
        "update" | "upgrade" => update::update_cmd(io, &args[1..]).await,
        "uninstall" => install::handle_uninstall(io, &args[1..]).await,
        _ => {
            let _ = writeln!(
                io.stderr,
                "usage: agy-pool [-h] [-v] {{login,add,import-current,list,ls,quota,strategy,strat,rename,doctor,check,health,remove,rm,switch,export,backup,import,restore,verify,disable,enable,move,mv,swap,sort,reorder,order,config,log,logs,start,stop,restart,status,stats,forecast,runway,predict,top,watch,monitor,update,version,run}} ..."
            );
            let _ = writeln!(io.stderr, "agy-pool: error: argument command: invalid choice: '{cmd}'");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_version_and_help() {
        let _env = crate::cli::test_support::cli_test_env();

        for v_flag in [&["--version"][..], &["-v"][..], &["version"][..]] {
            let (code, stdout, _stderr) = crate::cli::test_support::run_cli(v_flag, "").await;
            assert_eq!(code, 0, "{v_flag:?} expected exit 0, got {code}");
            assert!(
                stdout.starts_with("agy-pool "),
                "{v_flag:?} expected 'agy-pool <ver>', got {stdout:?}"
            );
        }

        for h_flag in [&["--help"][..], &["-h"][..], &["help"][..]] {
            let (code, stdout, _stderr) = crate::cli::test_support::run_cli(h_flag, "").await;
            assert_eq!(code, 0, "{h_flag:?} expected exit 0, got {code}");
            assert!(
                stdout.contains("usage: agy-pool"),
                "{h_flag:?} expected usage text, got {stdout:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_unknown_command_and_invalid_choice() {
        let _env = crate::cli::test_support::cli_test_env();

        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["nonexistent_command"], "").await;
        assert_eq!(code, 2, "expected exit code 2 on unknown command, got {code}");
        assert!(
            stderr.contains("invalid choice: 'nonexistent_command'"),
            "expected invalid choice error on stderr, got: {stderr}"
        );
    }

    #[tokio::test]
    async fn test_doctor_command() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::diagnostics::set_agy_binary_finder(Some(std::sync::Arc::new(|| "/bin/true".to_string())));
        crate::diagnostics::set_tls_prober(Some(std::sync::Arc::new(|_, _| Ok(()))));

        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["doctor"], "").await;
        assert_eq!(code, 0, "doctor expected 0, got {code}");
        assert!(
            stdout.contains("Antigravity System Doctor"),
            "expected doctor header, got: {stdout}"
        );
    }

    #[tokio::test]
    async fn test_doctor_cmd_behavior_unchanged() {
        let _env = crate::cli::test_support::cli_test_env();
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["doctor"], "").await;
        assert_eq!(code, 0, "expected exit 0 on doctor, got {code}");
        assert!(
            stdout.contains("Antigravity System Doctor"),
            "doctor output missing header, got: {stdout}"
        );
        assert!(
            stdout.contains("Native Binary:"),
            "doctor output missing Native Binary check, got: {stdout}"
        );
    }

    #[tokio::test]
    async fn test_verify_cmd_success_clears_restriction() {
        let _env = crate::cli::test_support::cli_test_env();

        let _g = crate::accounts::set_quota_prober(
            Some(Box::new(|_| {
                Ok(Some(crate::storage::QuotaState {
                    remaining_fraction: Some(0.85),
                    ..Default::default()
                }))
            })),
            None,
        );

        let v_url = "https://accounts.google.com/verify?id=test_url".to_string();
        let cooldown = crate::clock::now_unix() + 86400.0;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Primary".to_string(),
            email: "primary@example.com".to_string(),
            ..Default::default()
        };
        let acc = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Backup".to_string(),
            email: "backup@example.com".to_string(),
            status: "validation_required".to_string(),
            validation_url: Some(v_url),
            rate_limited_until: Some(cooldown),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            accounts: vec![acc1, acc.clone()],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["verify", "2"], "").await;
        assert_eq!(code, 0, "verify expected exit 0, got {code}, stderr: {stderr}");
        assert!(
            stdout.contains("🎉 Verification Successful! Account is Ready!"),
            "expected success message, got: {stdout:?}"
        );
        assert!(
            !stdout.contains("backup@example.com"),
            "email leaked in output: {stdout:?}"
        );

        let stored_pool = crate::storage::load_pool().unwrap();
        let stored = stored_pool.accounts.iter().find(|a| a.id == acc.id).unwrap();
        assert_eq!(stored.status, "", "persisted status should be empty");
        assert!(
            stored.validation_url.is_none(),
            "persisted validation_url should be None"
        );
        assert!(
            stored.rate_limited_until.is_none(),
            "persisted rate_limited_until should be None"
        );
    }

    #[tokio::test]
    async fn test_verify_cmd_persistence_failure_exits_non_zero() {
        let _env = crate::cli::test_support::cli_test_env();

        let _g = crate::accounts::set_quota_prober(
            Some(Box::new(|_| {
                Ok(Some(crate::storage::QuotaState {
                    remaining_fraction: Some(0.85),
                    ..Default::default()
                }))
            })),
            None,
        );

        let _pg = crate::accounts::set_account_fields_persister(Some(Box::new(|_, _, _, _| {
            Err(crate::accounts::AccountsError::Message(
                "injected storage persistence error".to_string(),
            ))
        })));

        let v_url = "https://accounts.google.com/verify?id=test_url".to_string();
        let cooldown = crate::clock::now_unix() + 86400.0;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Primary".to_string(),
            email: "primary@example.com".to_string(),
            ..Default::default()
        };
        let acc = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Backup".to_string(),
            email: "backup@example.com".to_string(),
            status: "validation_required".to_string(),
            validation_url: Some(v_url),
            rate_limited_until: Some(cooldown),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            accounts: vec![acc1, acc],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["verify", "2"], "").await;
        assert_ne!(code, 0, "expected non-zero exit on persistence failure, got 0");
        assert!(
            !stdout.contains("Verification is not required"),
            "unexpected success message on persistence failure: {stdout:?}"
        );
        assert!(
            stderr.contains("Verification could not be completed"),
            "expected error message on stderr, got: {stderr:?}"
        );
        assert!(
            stderr.contains("injected storage persistence error"),
            "expected root cause error on stderr, got: {stderr:?}"
        );
    }

    #[test]
    fn test_redact_user_url() {
        let (got, ok) = redact_user_url("https://example.test/path?Email=a%40b.test&foo=bar&token=abc#fragment");
        assert!(ok, "expected ok=true");
        assert!(!got.contains("a@b.test"), "leaked email: {got}");
        assert!(!got.contains("abc"), "leaked token: {got}");
        assert!(!got.contains("fragment"), "leaked fragment: {got}");
        assert!(got.contains("foo=bar"), "missing foo=bar: {got}");
        assert!(got.contains("REDACTED"), "missing REDACTED: {got}");
    }

    #[tokio::test]
    async fn test_cp1d_verify_redacts_sensitive_url() {
        let _env = crate::cli::test_support::cli_test_env();
        let url = "https://example.test/verify?Email=user@example.com&login_hint=user@example.com&foo=bar&code=secret"
            .to_string();
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                email: "user@example.com".to_string(),
                status: "validation_required".to_string(),
                validation_url: Some(url),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["verify", "1"], "").await;
        assert_eq!(code, 0, "verify exit code = {code}, stderr = {stderr:?}");
        assert!(
            stdout.contains("Verification required for Account 1."),
            "missing safe account label: {stdout:?}"
        );
        assert!(
            !stdout.contains("user@example.com") && !stdout.contains("secret"),
            "sensitive URL data leaked: {stdout:?}"
        );
        assert!(
            stdout.contains("foo=bar") && stdout.contains("REDACTED"),
            "URL was not safely rendered: {stdout:?}"
        );
    }

    #[tokio::test]
    async fn test_cp1d_required_command_failures() {
        let _env = crate::cli::test_support::cli_test_env();
        let cases: &[(&str, &[&str])] = &[
            ("import-current", &["import-current"]),
            ("import", &["import", "/does/not/exist.json"]),
            ("verify", &["verify", "user@example.com"]),
            ("quota refresh", &["quota", "--refresh"]),
        ];

        for (name, args) in cases {
            let (code, stdout, stderr) = crate::cli::test_support::run_cli(args, "").await;
            assert_ne!(
                code, 0,
                "case {name}: expected failure, stdout={stdout:?} stderr={stderr:?}"
            );
            if *name != "quota refresh" {
                assert!(stdout.is_empty(), "case {name}: unexpected success output: {stdout:?}");
            }
            assert!(!stderr.is_empty(), "case {name}: expected safe error on stderr");
        }
    }

    #[test]
    fn test_entrypoint_provider_and_exec_default_are_safe_in_tests() {
        let _env = crate::cli::test_support::cli_test_env();
        assert_eq!(crate::daemon::entrypoint(), std::path::PathBuf::from("/bin/true"));
        let handler = exec_handler();
        let res = std::panic::catch_unwind(|| handler(std::path::Path::new("/bin/echo"), &["hello".to_string()], &[]));
        assert!(res.is_err(), "default exec_handler must panic in tests");
    }
}
