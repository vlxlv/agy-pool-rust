use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_CYAN, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

fn is_relative_keyword(kw: &str) -> bool {
    let s = kw.trim().to_ascii_lowercase();
    match s.as_str() {
        "top" | "bottom" | "up" | "down" | "first" | "last" | "head" | "tail" | "begin" | "end" => true,
        _ => s.starts_with('+') || s.starts_with('-'),
    }
}

pub async fn move_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut show_email = crate::accounts::is_show_email_enabled(None);
    let mut positionals = Vec::new();

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = writeln!(
                    io.stdout,
                    "usage: agy-pool move [-h] [--show-email] [--hide-email] target destination\n"
                );
                let _ = writeln!(
                    io.stdout,
                    "Move an account to a new position (top, bottom, up, down, or index)\n"
                );
                let _ = writeln!(io.stdout, "positional arguments:");
                let _ = writeln!(io.stdout, "  target        Account index (1-based), ID, name, or email");
                let _ = writeln!(
                    io.stdout,
                    "  destination   Target position (1-based index, or top, bottom, up, down)"
                );
                return 0;
            }
            "-e" | "--show-email" | "--email" => {
                show_email = true;
            }
            "--hide-email" => {
                show_email = false;
            }
            _ => {
                if arg.starts_with('-') {
                    let _ = writeln!(io.stderr, "agy-pool move: error: unrecognized argument: {}", arg);
                    return 2;
                }
                positionals.push(arg.clone());
            }
        }
    }

    if positionals.len() < 2 {
        let _ = writeln!(io.stderr, "usage: agy-pool move [-h] target destination");
        let _ = writeln!(
            io.stderr,
            "agy-pool move: error: the following arguments are required: target, destination"
        );
        return 2;
    }

    let target = &positionals[0];
    let destination = &positionals[1];

    let res = match crate::accounts::move_account(target, destination) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };

    if res.is_same {
        let disp_name = crate::accounts::display_account_name(Some(&res.account), Some(show_email));
        let _ = writeln!(
            io.stdout,
            "{}Account {} is already at position [{}].{}",
            CLR_YELLOW,
            disp_name,
            res.dest_index + 1,
            CLR_RESET
        );
        return 0;
    }

    let disp_name = crate::accounts::display_account_name(Some(&res.account), Some(show_email));
    let _ = writeln!(
        io.stdout,
        "{}✓ Moved {}{}{}{} from position [{}] to [{}].{}",
        CLR_GREEN,
        CLR_BOLD,
        disp_name,
        CLR_RESET,
        CLR_GREEN,
        res.orig_index + 1,
        res.dest_index + 1,
        CLR_RESET
    );
    let _ = crate::accounts::print_account_order_preview(
        io.stdout,
        &res.accounts,
        &[res.dest_index],
        res.active_id.as_deref(),
        Some(show_email),
    );
    0
}

pub async fn swap_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut show_email = crate::accounts::is_show_email_enabled(None);
    let mut positionals = Vec::new();

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = writeln!(
                    io.stdout,
                    "usage: agy-pool swap [-h] [--show-email] [--hide-email] target1 target2\n"
                );
                let _ = writeln!(io.stdout, "Swap the positions of two accounts in the pool\n");
                let _ = writeln!(io.stdout, "positional arguments:");
                let _ = writeln!(
                    io.stdout,
                    "  target1       First account index (1-based), ID, name, or email"
                );
                let _ = writeln!(
                    io.stdout,
                    "  target2       Second account index (1-based), ID, name, or email"
                );
                return 0;
            }
            "-e" | "--show-email" | "--email" => {
                show_email = true;
            }
            "--hide-email" => {
                show_email = false;
            }
            _ => {
                if arg.starts_with('-') {
                    let _ = writeln!(io.stderr, "agy-pool swap: error: unrecognized argument: {}", arg);
                    return 2;
                }
                positionals.push(arg.clone());
            }
        }
    }

    if positionals.len() < 2 {
        let _ = writeln!(io.stderr, "usage: agy-pool swap [-h] target1 target2");
        let _ = writeln!(
            io.stderr,
            "agy-pool swap: error: the following arguments are required: target1, target2"
        );
        return 2;
    }

    let target1 = &positionals[0];
    let target2 = &positionals[1];

    let res = match crate::accounts::swap_accounts(target1, target2) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };

    if res.is_same {
        let disp_name = crate::accounts::display_account_name(Some(&res.account1), Some(show_email));
        let _ = writeln!(
            io.stdout,
            "{}Both targets refer to the same account: [{}] {}.{}",
            CLR_YELLOW,
            res.index1 + 1,
            disp_name,
            CLR_RESET
        );
        return 0;
    }

    let name1 = crate::accounts::display_account_name(Some(&res.account1), Some(show_email));
    let name2 = crate::accounts::display_account_name(Some(&res.account2), Some(show_email));
    let _ = writeln!(
        io.stdout,
        "{}✓ Swapped [{}] {}{}{} ⇄ [{}] {}{}{}",
        CLR_GREEN,
        res.index1 + 1,
        CLR_BOLD,
        name1,
        CLR_RESET,
        res.index2 + 1,
        CLR_BOLD,
        name2,
        CLR_RESET
    );
    let _ = crate::accounts::print_account_order_preview(
        io.stdout,
        &res.accounts,
        &[res.index1, res.index2],
        res.active_id.as_deref(),
        Some(show_email),
    );
    0
}

pub async fn sort_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut show_email = crate::accounts::is_show_email_enabled(None);
    let mut reverse = false;
    let mut refresh = false;
    let mut by = "quota".to_string();
    let mut by_specified = false;

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = writeln!(
                    io.stdout,
                    "usage: agy-pool sort [-h] [-r] [--refresh] [--show-email] [--hide-email] [by]\n"
                );
                let _ = writeln!(
                    io.stdout,
                    "Sort account pool by criteria (smart, quota, hits, name, email, status, id, created)\n"
                );
                let _ = writeln!(io.stdout, "positional arguments:");
                let _ = writeln!(
                    io.stdout,
                    "  by            Sort criterion (default: quota, options: smart, quota, hits, name, email, status, id, created)\n"
                );
                let _ = writeln!(io.stdout, "options:");
                let _ = writeln!(io.stdout, "  -r, --reverse Reverse the sort order");
                let _ = writeln!(io.stdout, "  --refresh     Refresh quotas before sorting");
                return 0;
            }
            "-r" | "--reverse" => {
                reverse = true;
            }
            "--refresh" => {
                refresh = true;
            }
            "-e" | "--show-email" | "--email" => {
                show_email = true;
            }
            "--hide-email" => {
                show_email = false;
            }
            _ => {
                if arg.starts_with('-') {
                    let _ = writeln!(io.stderr, "agy-pool sort: error: unrecognized argument: {}", arg);
                    return 2;
                }
                if !by_specified {
                    by = arg.clone();
                    by_specified = true;
                }
            }
        }
    }

    let res = match crate::accounts::sort_accounts(&by, reverse, refresh) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };

    let is_desc = if res.criterion == "smart"
        || res.criterion == "quota"
        || res.criterion == "hits"
        || res.criterion == "usage"
    {
        !reverse
    } else {
        reverse
    };
    let order_desc = if is_desc { "descending" } else { "ascending" };
    let rev_tag = if reverse { " (reversed)" } else { "" };

    let _ = writeln!(
        io.stdout,
        "{}✓ Sorted account pool by {}{}{}{} ({}{}).{}",
        CLR_GREEN, CLR_BOLD, res.criterion, CLR_RESET, CLR_GREEN, order_desc, rev_tag, CLR_RESET
    );
    let _ = crate::accounts::print_account_order_preview(
        io.stdout,
        &res.accounts,
        &[],
        res.active_id.as_deref(),
        Some(show_email),
    );
    0
}

pub async fn reorder_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut show_email = crate::accounts::is_show_email_enabled(None);
    let mut sequence = Vec::new();

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = writeln!(
                    io.stdout,
                    "usage: agy-pool reorder [-h] [--show-email] [--hide-email] [sequence ...]\n"
                );
                let _ = writeln!(io.stdout, "Batch reorder accounts or display ordering guide\n");
                let _ = writeln!(io.stdout, "positional arguments:");
                let _ = writeln!(
                    io.stdout,
                    "  sequence      Desired sequence of account indices / names (or leave empty for guide)"
                );
                return 0;
            }
            "-e" | "--show-email" | "--email" => {
                show_email = true;
            }
            "--hide-email" => {
                show_email = false;
            }
            _ => {
                if arg.starts_with('-') {
                    let _ = writeln!(io.stderr, "agy-pool reorder: error: unrecognized argument: {}", arg);
                    return 2;
                }
                let cleaned = arg.replace(',', " ");
                for part in cleaned.split_whitespace() {
                    sequence.push(part.to_string());
                }
            }
        }
    }

    if sequence.is_empty() {
        let pool = crate::storage::load_pool().unwrap_or_else(|_| crate::storage::Pool::new_empty());
        if pool.accounts.is_empty() {
            let _ = writeln!(io.stderr, "{}[Error] Account pool is empty.{}", CLR_RED, CLR_RESET);
            return 1;
        }
        let _ = writeln!(io.stdout, "{}Current Account Pool Order:{}", CLR_CYAN, CLR_RESET);
        let _ = crate::accounts::print_account_order_preview(
            io.stdout,
            &pool.accounts,
            &[],
            pool.active_account_id.as_deref(),
            Some(show_email),
        );
        let _ = writeln!(io.stdout, "{}Usage:{}", CLR_BOLD, CLR_RESET);
        let _ = writeln!(
            io.stdout,
            "  agy-pool move <account> <position|top|bottom|up|down>  (Move single account)"
        );
        let _ = writeln!(
            io.stdout,
            "  agy-pool swap <account1> <account2>                   (Swap two accounts)"
        );
        let _ = writeln!(
            io.stdout,
            "  agy-pool sort [smart|quota|hits|name|email|status|id] [-r] (Sort pool by criteria)"
        );
        let _ = writeln!(
            io.stdout,
            "  agy-pool reorder <pos1> <pos2> ... <posN>             (Custom batch reordering)"
        );
        return 0;
    }

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(io.stderr, "{}[Error] Account pool is empty.{}", CLR_RED, CLR_RESET);
            return 1;
        }
    };
    if pool.accounts.is_empty() {
        let _ = writeln!(io.stderr, "{}[Error] Account pool is empty.{}", CLR_RED, CLR_RESET);
        return 1;
    }

    if sequence.len() == 2 && (pool.accounts.len() != 2 || is_relative_keyword(&sequence[1])) {
        return Box::pin(move_cmd(io, &[sequence[0].clone(), sequence[1].clone()])).await;
    }

    let seq_refs: Vec<&str> = sequence.iter().map(|s| s.as_str()).collect();
    let res = match crate::accounts::reorder_accounts(&seq_refs) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };

    let _ = writeln!(
        io.stdout,
        "{}✓ Reordered all {} accounts successfully.{}",
        CLR_GREEN,
        res.accounts.len(),
        CLR_RESET
    );
    let _ = crate::accounts::print_account_order_preview(
        io.stdout,
        &res.accounts,
        &[],
        res.active_id.as_deref(),
        Some(show_email),
    );
    0
}

pub async fn config_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut positionals = Vec::new();
    for a in args {
        if a == "-h" || a == "--help" {
            let _ = writeln!(io.stdout, "usage: agy-pool config [-h] [key] [value]\n");
            let _ = writeln!(
                io.stdout,
                "View or update pool configuration settings (e.g. show_email, strategy)"
            );
            return 0;
        }
        if !a.starts_with('-') {
            positionals.push(a.clone());
        }
    }

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not load the account pool: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    if positionals.is_empty() {
        let strat = if !pool.strategy.is_empty() {
            pool.strategy.as_str()
        } else {
            crate::config::DEFAULT_STRATEGY
        };
        let show_email_eff = crate::accounts::is_show_email_enabled(Some(&pool));
        let cfg_str = match pool.show_email {
            Some(b) => b.to_string(),
            None => "nil".to_string(),
        };

        let _ = writeln!(io.stdout, "\n{}Pool Configuration Settings:{}", CLR_BOLD, CLR_RESET);
        let _ = writeln!(
            io.stdout,
            "  • {}strategy{}   : {}{}{}",
            CLR_BOLD, CLR_RESET, CLR_GREEN, strat, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "  • {}show_email{} : {}{}{} (configured: {})",
            CLR_BOLD, CLR_RESET, CLR_CYAN, show_email_eff, CLR_RESET, cfg_str
        );
        let _ = writeln!(
            io.stdout,
            "\nTo change a setting: {}agy-pool config <key> <value>{}",
            CLR_BOLD, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "Example: {}agy-pool config show_email true{}\n",
            CLR_BOLD, CLR_RESET
        );
        return 0;
    }

    let key = positionals[0].trim().replace('-', "_").to_ascii_lowercase();
    let val = if positionals.len() > 1 {
        Some(positionals[1].trim())
    } else {
        None
    };

    match key.as_str() {
        "show_email" | "email" => {
            let Some(val_str) = val else {
                let _ = writeln!(
                    io.stdout,
                    "show_email: {}",
                    crate::accounts::is_show_email_enabled(Some(&pool))
                );
                return 0;
            };
            let lower = val_str.to_ascii_lowercase();
            let b_val = matches!(lower.as_str(), "1" | "true" | "yes" | "on" | "enable");
            let err = crate::storage::pool_transaction(|p| {
                p.show_email = Some(b_val);
                Ok(())
            });
            if let Err(e) = err {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Failed to update config: {}{}",
                    CLR_RED, e, CLR_RESET
                );
                return 1;
            }
            let status_tag = if b_val {
                format!("{}enabled (true){}", CLR_GREEN, CLR_RESET)
            } else {
                format!("{}disabled (false){}", CLR_YELLOW, CLR_RESET)
            };
            let _ = writeln!(
                io.stdout,
                "{}✓ Configuration updated: {}{}show_email{} = {}",
                CLR_GREEN, CLR_BOLD, CLR_RESET, CLR_GREEN, status_tag
            );
            0
        }
        "strategy" | "strat" => {
            let Some(val_str) = val else {
                let strat = if !pool.strategy.is_empty() {
                    pool.strategy.as_str()
                } else {
                    crate::config::DEFAULT_STRATEGY
                };
                let _ = writeln!(io.stdout, "strategy: {}", strat);
                return 0;
            };
            if !crate::cli::status::manage_strategy(io, val_str).await {
                1
            } else {
                0
            }
        }
        _ => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Unknown configuration key '{}'. Supported keys: show_email, strategy{}",
                CLR_RED, positionals[0], CLR_RESET
            );
            1
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {

    fn create_test_cli_order_pool() {
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Alpha".to_string(),
            email: "alpha@test.com".to_string(),
            request_count: 10,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.9),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.9),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Beta".to_string(),
            email: "beta@test.com".to_string(),
            request_count: 50,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.7),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.7),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc3 = crate::storage::Account {
            id: "acc_3".to_string(),
            name: "Gamma".to_string(),
            email: "gamma@test.com".to_string(),
            request_count: 30,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.5),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(0.5),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut p = crate::storage::Pool::new_empty();
        p.accounts = vec![acc1, acc2, acc3];
        p.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&p).unwrap();
    }

    #[tokio::test]
    async fn test_move_cmd() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // --help
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["move", "--help"], "").await;
        assert_eq!(code, 0, "expected help output");
        assert!(stdout.contains("usage: agy-pool move"), "out={}", stdout);

        // Missing args
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["move", "acc_1"], "").await;
        assert_eq!(code, 2, "expected code 2 for missing destination");
        assert!(stderr.contains("required: target, destination"), "err={}", stderr);

        // Valid move: acc_1 to bottom
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["move", "acc_1", "bottom"], "").await;
        assert_eq!(code, 0, "expected code 0, err={}", stderr);
        assert!(
            stdout.contains("Moved") && stdout.contains("Alpha"),
            "unexpected stdout: {}",
            stdout
        );

        // Move to same position
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["move", "acc_1", "3"], "").await;
        assert_eq!(code, 0, "expected same position message");
        assert!(stdout.contains("already at position"), "out={}", stdout);

        // Unknown account
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["move", "unknown", "top"], "").await;
        assert_eq!(code, 1, "expected code 1");
        assert!(stderr.contains("not found"), "err={}", stderr);
    }

    #[tokio::test]
    async fn test_swap_cmd() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // --help
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["swap", "--help"], "").await;
        assert_eq!(code, 0, "expected help output");
        assert!(stdout.contains("usage: agy-pool swap"), "out={}", stdout);

        // Missing args
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["swap", "acc_1"], "").await;
        assert_eq!(code, 2, "expected code 2");
        assert!(stderr.contains("required: target1, target2"), "err={}", stderr);

        // Swap 1 and 2
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["swap", "1", "2"], "").await;
        assert_eq!(code, 0, "expected success, err={}", stderr);
        assert!(stdout.contains("Swapped"), "out={}", stdout);

        // Swap same account
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["swap", "1", "1"], "").await;
        assert_eq!(code, 0, "expected same account message");
        assert!(
            stdout.contains("Both targets refer to the same account"),
            "out={}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_sort_cmd() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // --help
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["sort", "--help"], "").await;
        assert_eq!(code, 0, "expected help output");
        assert!(stdout.contains("usage: agy-pool sort"), "out={}", stdout);

        // Sort by quota
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["sort", "quota"], "").await;
        assert_eq!(code, 0, "expected sort success, err={}", stderr);
        assert!(
            stdout.contains("Sorted account pool by") && stdout.contains("quota"),
            "out={}",
            stdout
        );

        // Reverse sort by hits
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["sort", "hits", "-r"], "").await;
        assert_eq!(code, 0, "expected reverse sort success, err={}", stderr);
        assert!(
            stdout.contains("Sorted account pool by") && stdout.contains("hits"),
            "out={}",
            stdout
        );

        // Sort by smart
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["sort", "smart"], "").await;
        assert_eq!(code, 0, "expected smart sort success, err={}", stderr);
        assert!(
            stdout.contains("Sorted account pool by") && stdout.contains("smart"),
            "out={}",
            stdout
        );

        // Invalid criterion
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["sort", "nonexistent"], "").await;
        assert_eq!(code, 1, "expected unknown criterion error");
        assert!(stderr.contains("unknown sort criterion"), "err={}", stderr);
    }

    #[tokio::test]
    async fn test_reorder_cmd() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // No args: show guide and preview
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["reorder"], "").await;
        assert_eq!(code, 0, "expected guide output");
        assert!(
            stdout.contains("Current Account Pool Order") && stdout.contains("Usage:"),
            "out={}",
            stdout
        );

        // 2 args relative: delegates to MoveCmd
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["reorder", "1", "bottom"], "").await;
        assert_eq!(code, 0, "expected delegation to move, err={}", stderr);
        assert!(stdout.contains("Moved"), "out={}", stdout);

        // Exact sequence
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["reorder", "3", "2", "1"], "").await;
        assert_eq!(code, 0, "expected reorder success, err={}", stderr);
        assert!(
            stdout.contains("Reordered all 3 accounts successfully"),
            "out={}",
            stdout
        );

        // Mismatched sequence length (1 argument for a 3-account pool)
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["reorder", "1"], "").await;
        assert_eq!(code, 1, "expected count mismatch error");
        assert!(stderr.contains("does not match"), "err={}", stderr);
    }

    #[tokio::test]
    async fn test_main_order_subcommands() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // "mv" alias
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["mv", "acc_1", "bottom"], "").await;
        assert_eq!(code, 0, "mv failed: err={}", stderr);

        // "move"
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["move", "acc_1", "top"], "").await;
        assert_eq!(code, 0, "move failed: err={}", stderr);

        // "swap"
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["swap", "1", "2"], "").await;
        assert_eq!(code, 0, "swap failed: err={}", stderr);

        // "sort"
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["sort", "quota"], "").await;
        assert_eq!(code, 0, "sort failed: err={}", stderr);

        // "order" alias
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["order"], "").await;
        assert_eq!(code, 0, "order guide failed");
        assert!(stdout.contains("Current Account Pool Order"), "out={}", stdout);

        // "reorder"
        let (code, _stdout, stderr) = crate::cli::test_support::run_cli(&["reorder", "3", "2", "1"], "").await;
        assert_eq!(code, 0, "reorder failed: err={}", stderr);
    }

    #[tokio::test]
    async fn test_config_cmd() {
        let _env = crate::cli::test_support::cli_test_env();
        create_test_cli_order_pool();

        // View config (no args)
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["config"], "").await;
        assert_eq!(code, 0, "expected config view");
        assert!(stdout.contains("Pool Configuration Settings"), "out={}", stdout);

        // Set show_email true
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["config", "show_email", "true"], "").await;
        assert_eq!(code, 0, "expected show_email enabled");
        assert!(stdout.contains("enabled (true)"), "out={}", stdout);

        // Query show_email
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["config", "show_email"], "").await;
        assert_eq!(code, 0, "expected show_email query true");
        assert!(stdout.contains("show_email: true"), "out={}", stdout);

        // Set show_email false
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["config", "show_email", "false"], "").await;
        assert_eq!(code, 0, "expected show_email disabled");
        assert!(stdout.contains("disabled (false)"), "out={}", stdout);

        // Main dispatch "config"
        let (code, stdout, _stderr) = crate::cli::test_support::run_cli(&["config"], "").await;
        assert_eq!(code, 0, "main config failed");
        assert!(stdout.contains("Pool Configuration Settings"), "out={}", stdout);
    }
}
