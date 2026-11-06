use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::cli::Io;
use crate::cli::render::{
    choose_bar_width, extract_reset_times, gated_bar, get_account_state, terminal_width, truncate_runes, weekly_blocked,
};
use crate::cli::width::strip_ansi;
use crate::cli::{CLR_BOLD, CLR_CYAN, CLR_DIM, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

pub async fn list_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (_, rem) = match crate::cli::resolve_context_for_cmd(args) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let mut target = "";
    let mut show_email = false;
    for a in &rem {
        if a == "-e" || a == "--email" || a == "--show-email" {
            show_email = true;
        } else if !a.starts_with('-') && target.is_empty() {
            target = a.as_str();
        }
    }

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(io.stdout, "\n{}No accounts in pool yet.{}", CLR_YELLOW, CLR_RESET);
            let _ = writeln!(
                io.stdout,
                "Run {}agy-pool login{} or {}agy-pool import-current{} to add accounts.\n",
                CLR_BOLD, CLR_RESET, CLR_BOLD, CLR_RESET
            );
            return 0;
        }
    };
    if pool.accounts.is_empty() {
        let _ = writeln!(io.stdout, "\n{}No accounts in pool yet.{}", CLR_YELLOW, CLR_RESET);
        let _ = writeln!(
            io.stdout,
            "Run {}agy-pool login{} or {}agy-pool import-current{} to add accounts.\n",
            CLR_BOLD, CLR_RESET, CLR_BOLD, CLR_RESET
        );
        return 0;
    }

    let acc_list: Vec<&crate::storage::Account> = if !target.is_empty() {
        if let Ok(idx) = target.parse::<usize>() {
            if idx >= 1 && idx <= pool.accounts.len() {
                vec![&pool.accounts[idx - 1]]
            } else {
                let _ = writeln!(
                    io.stdout,
                    "{}[Error] The selected account was not found.{}",
                    CLR_RED, CLR_RESET
                );
                return 0;
            }
        } else {
            let found = pool
                .accounts
                .iter()
                .find(|a| a.id == target || a.email == target || a.name == target);
            if let Some(a) = found {
                vec![a]
            } else {
                let _ = writeln!(
                    io.stdout,
                    "{}[Error] The selected account was not found.{}",
                    CLR_RED, CLR_RESET
                );
                return 0;
            }
        }
    } else {
        pool.accounts.iter().collect()
    };

    let now_ts = crate::clock::now_unix() as i64;
    let active_id = pool.active_account_id.as_deref().unwrap_or("");

    let mut rows: Vec<Vec<String>> = Vec::new();
    if show_email {
        rows.push(vec![
            "#".to_string(),
            "ACCOUNT ID".to_string(),
            "NAME".to_string(),
            "EMAIL".to_string(),
            "STATUS".to_string(),
            "HITS".to_string(),
            "5H QUOTA".to_string(),
            "WEEKLY QUOTA".to_string(),
            "LAST USED".to_string(),
        ]);
    } else {
        rows.push(vec![
            "#".to_string(),
            "ACCOUNT ID".to_string(),
            "NAME".to_string(),
            "STATUS".to_string(),
            "HITS".to_string(),
            "5H QUOTA".to_string(),
            "WEEKLY QUOTA".to_string(),
            "LAST USED".to_string(),
        ]);
    }

    for (i, a) in acc_list.iter().enumerate() {
        let is_active = a.id == active_id;
        let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(a.last_quota.as_ref());
        let is_cooling = a.rate_limited_until.map(|t| t > now_ts as f64).unwrap_or(false);
        let is_exhausted = q5_frac.map(|f| f <= crate::config::DEPLETED_THRESHOLD).unwrap_or(false)
            || gw_frac.map(|f| f <= crate::config::DEPLETED_THRESHOLD).unwrap_or(false);

        let mut status_str = "Ready";
        if a.status == crate::storage::STATUS_DISABLED {
            if is_active {
                status_str = "CLI Base (Disabled)";
            } else {
                status_str = "Disabled";
            }
        } else if a.status == crate::storage::STATUS_VALIDATION_REQUIRED {
            status_str = "⚠ Verify Required";
        } else if a.status == crate::storage::STATUS_VERIFICATION_PENDING {
            status_str = "Verification Pending";
        } else if a.status == crate::storage::STATUS_AUTH_ERROR {
            status_str = "✖ Auth Error";
        } else if is_cooling {
            if is_active {
                status_str = "CLI Base (Cooldown)";
            } else {
                status_str = "Cooldown";
            }
        } else if is_exhausted {
            if is_active {
                status_str = "CLI Base (Exhausted)";
            } else {
                status_str = "Exhausted";
            }
        } else if crate::scheduler::is_reserve_candidate(a, now_ts as f64) {
            if is_active {
                status_str = "CLI Base (Protected)";
            } else {
                status_str = "Protected";
            }
        } else if is_active {
            status_str = "CLI Base";
        }

        let hits = a.get_hits();
        let q5_str = if let Some(frac) = q5_frac
            && a.status != crate::storage::STATUS_DISABLED
        {
            format!("{:.1}%", frac * 100.0)
        } else {
            "--".to_string()
        };
        let gw_str = if let Some(frac) = gw_frac
            && a.status != crate::storage::STATUS_DISABLED
        {
            format!("{:.1}%", frac * 100.0)
        } else {
            "--".to_string()
        };
        let last_used_str = format_last_used(a.last_used_at, now_ts);
        let disp_name = crate::accounts::display_account_name(Some(a), None);

        if show_email {
            let email_str = if a.email.is_empty() {
                "--".to_string()
            } else {
                a.email.clone()
            };
            rows.push(vec![
                (i + 1).to_string(),
                a.id.clone(),
                disp_name,
                email_str,
                status_str.to_string(),
                hits.to_string(),
                q5_str,
                gw_str,
                last_used_str,
            ]);
        } else {
            rows.push(vec![
                (i + 1).to_string(),
                a.id.clone(),
                disp_name,
                status_str.to_string(),
                hits.to_string(),
                q5_str,
                gw_str,
                last_used_str,
            ]);
        }
    }

    let table_text = format_tabwriter_table(&rows);
    let _ = write!(io.stdout, "{}", table_text);

    let strat = if pool.strategy.is_empty() {
        crate::config::STRATEGY_MAX_QUOTA
    } else {
        &pool.strategy
    };
    let _ = writeln!(
        io.stdout,
        "\nStrategy: {} | CLI Base: {} | Total: {} account(s)",
        strat,
        active_id,
        pool.accounts.len()
    );
    0
}

fn format_tabwriter_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; col_count];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.chars().count();
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    let mut out = String::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.chars().count();
            out.push_str(cell);
            if i + 1 < row.len() {
                let pad = widths[i] + 2 - w;
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    }
    out
}

pub async fn quota_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    for a in args {
        if a == "-w" || a == "--watch" {
            return crate::cli::top::top_cmd(io, args).await;
        }
    }

    let (ctx, rem) = match crate::cli::resolve_context_for_cmd(args) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    if rem.first().map(|s| s.as_str()) == Some("--refresh") {
        let _ = writeln!(io.stderr, "usage: agy-pool quota [account]");
        let _ = writeln!(io.stderr, "agy-pool quota: error: unrecognized argument '--refresh'");
        return 2;
    }

    let mut target = "";
    let mut show_email = false;
    for a in &rem {
        if a == "-e" || a == "--email" || a == "--show-email" {
            show_email = true;
        } else if !a.starts_with('-') && target.is_empty() {
            target = a.as_str();
        }
    }

    refresh_all_quotas(std::time::Duration::from_secs(10)).await;
    list_accounts_with_port(target, io, show_email, Some(ctx.listen_port));
    0
}

pub fn format_last_used(ts: Option<i64>, now: i64) -> String {
    let t = match ts {
        Some(t) if t > 0 => t,
        _ => return "never".to_string(),
    };
    let diff = now - t;
    if diff < 0 {
        return "just now".to_string();
    }
    if diff < 60 {
        return format!("{diff}s ago");
    }
    if diff < 3600 {
        return format!("{}m ago", diff / 60);
    }
    if diff < 86400 {
        return format!("{}h ago", diff / 3600);
    }
    format!("{}d ago", diff / 86400)
}

pub async fn refresh_all_quotas(timeout: std::time::Duration) {
    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => return,
    };
    let now = crate::clock::now_unix();
    let mut handles = Vec::new();
    for acc in pool.accounts {
        if acc.status == crate::storage::STATUS_DISABLED || !crate::quota::refresh_needed(&acc, now) {
            continue;
        }
        let mut acc_clone = acc.clone();
        handles.push(tokio::spawn(async move {
            let _ = tokio::time::timeout(timeout, crate::quota::query_quota(&mut acc_clone)).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

pub fn resolve_next_candidate_id(pool: &crate::storage::Pool, now_ts: i64) -> String {
    let rf_in = crate::daemon::runtime_file_in(&crate::config::data_dir());
    let rf = crate::config::runtime_file();
    if rf_in.exists() && rf_in != rf {
        let _ = std::fs::copy(&rf_in, &rf);
    } else if !rf_in.exists() && rf.exists() && rf_in != rf {
        let _ = std::fs::remove_file(&rf);
    }
    crate::daemon::next_candidate_id(Some(pool), now_ts as f64)
}

pub fn resolve_configured_port(explicit_port: Option<u16>) -> u16 {
    if let Some(p) = explicit_port
        && p > 0
    {
        return p;
    }
    if let Ok(p_str) = std::env::var(crate::daemon::ENV_DAEMON_PORT)
        && let Ok(p) = p_str.parse::<u16>()
        && p > 0
    {
        return p;
    }
    if let Ok(cfg_path) = crate::config::resolve_default_config_file()
        && !cfg_path.as_os_str().is_empty()
        && cfg_path.exists()
        && let Ok(cfg) = crate::config::load_config_file(&cfg_path)
        && cfg.server.port > 0
    {
        return cfg.server.port as u16;
    }
    let sc = crate::config::get_static_config();
    if sc.server.port > 0 {
        return sc.server.port as u16;
    }
    crate::config::DEFAULT_PORT
}

pub fn list_accounts(target: &str, io: &mut Io<'_>, show_email: bool) {
    list_accounts_with_port(target, io, show_email, None);
}

pub fn list_accounts_with_port(target: &str, io: &mut Io<'_>, show_email: bool, explicit_port: Option<u16>) {
    let width = terminal_width(io.stdout_fd);
    list_accounts_with_width_and_port(target, width, io, show_email, explicit_port);
}

pub fn list_accounts_with_width(target: &str, width: usize, io: &mut Io<'_>, show_email: bool) {
    list_accounts_with_width_and_port(target, width, io, show_email, None);
}

pub fn list_accounts_with_width_and_port(
    target: &str,
    mut width: usize,
    io: &mut Io<'_>,
    show_email: bool,
    explicit_port: Option<u16>,
) {
    if width == 0 {
        width = 100;
    }

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(io.stdout, "\n{}No accounts in pool yet.{}", CLR_YELLOW, CLR_RESET);
            let _ = writeln!(
                io.stdout,
                "Run {}agy-pool login{} or {}agy-pool import-current{} to add accounts.\n",
                CLR_BOLD, CLR_RESET, CLR_BOLD, CLR_RESET
            );
            return;
        }
    };
    if pool.accounts.is_empty() {
        let _ = writeln!(io.stdout, "\n{}No accounts in pool yet.{}", CLR_YELLOW, CLR_RESET);
        let _ = writeln!(
            io.stdout,
            "Run {}agy-pool login{} or {}agy-pool import-current{} to add accounts.\n",
            CLR_BOLD, CLR_RESET, CLR_BOLD, CLR_RESET
        );
        return;
    }

    let acc_list: Vec<&crate::storage::Account> = if !target.is_empty() {
        if let Ok(idx) = target.parse::<usize>() {
            if idx >= 1 && idx <= pool.accounts.len() {
                vec![&pool.accounts[idx - 1]]
            } else {
                let _ = writeln!(
                    io.stdout,
                    "{}[Error] The selected account was not found.{}",
                    CLR_RED, CLR_RESET
                );
                return;
            }
        } else {
            let found = pool
                .accounts
                .iter()
                .find(|a| a.id == target || a.email == target || a.name == target);
            if let Some(a) = found {
                vec![a]
            } else {
                let _ = writeln!(
                    io.stdout,
                    "{}[Error] The selected account was not found.{}",
                    CLR_RED, CLR_RESET
                );
                return;
            }
        }
    } else {
        pool.accounts.iter().collect()
    };

    let port = resolve_configured_port(explicit_port);
    let now_ts = crate::clock::now_unix() as i64;

    if width >= 60 {
        render_wide_quota_dashboard(&acc_list, &pool, port, now_ts, width, show_email, io);
        return;
    }

    if width >= 40 {
        render_mobile_quota_dashboard(&acc_list, &pool, port, now_ts, width, show_email, io);
        return;
    }

    render_very_narrow_quota_dashboard(&acc_list, &pool, now_ts, width, show_email, io);
}

#[allow(clippy::too_many_arguments)]
fn render_wide_quota_dashboard(
    acc_list: &[&crate::storage::Account],
    pool: &crate::storage::Pool,
    port: u16,
    now_ts: i64,
    width: usize,
    show_email: bool,
    io: &mut Io<'_>,
) {
    let daemon_running = crate::daemon::is_port_listening(port);
    let gateway_status = if daemon_running {
        format!("{}RUNNING (127.0.0.1:{}){}", CLR_GREEN, port, CLR_RESET)
    } else {
        format!("{}STOPPED{}", CLR_DIM, CLR_RESET)
    };
    let mut border_width = 68;
    if width < 68 {
        border_width = width;
    }
    let sep = "=".repeat(border_width);
    let sub_sep = "-".repeat(border_width);

    let _ = writeln!(io.stdout, "\n{sep}");
    let title = format!("Antigravity Multi-Account Pool v{}", crate::VERSION);
    let pad = (68 + title.len()) / 2;
    let lead_spaces = pad.saturating_sub(title.len());
    let _ = writeln!(
        io.stdout,
        "{}{}{}{}",
        CLR_BOLD,
        " ".repeat(lead_spaces),
        title,
        CLR_RESET
    );
    let _ = writeln!(io.stdout, "{sep}");

    let next_id = resolve_next_candidate_id(pool, now_ts);
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (state, state_clr) = get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();
        let age_str = crate::quota::format_quota_age(acc, now_ts as f64);

        let right = if acc.last_quota.is_some() {
            if border_width >= 68 {
                format!("hits {hits} · quota age {age_str}")
            } else {
                format!("hits {hits} · age {age_str}")
            }
        } else {
            format!("hits {hits}")
        };
        let right_len = right.len();
        let target_col = if border_width < 68 {
            border_width.saturating_sub(4)
        } else {
            64
        };

        let (next_tag, next_tag_visible) = if !next_id.is_empty() && acc.id == next_id {
            (format!(" {}{}[Next ⚡]{}", CLR_BOLD, CLR_CYAN, CLR_RESET), " [Next ⚡]")
        } else {
            (String::new(), "")
        };

        let suffix_len = " · ".chars().count() + state.chars().count() + next_tag_visible.chars().count();
        let max_left_len = target_col.saturating_sub(2).saturating_sub(right_len).saturating_sub(2);
        if disp_name.chars().count() + suffix_len > max_left_len {
            let avail = max_left_len.saturating_sub(suffix_len);
            if avail > 3 {
                disp_name = truncate_runes(&disp_name, avail);
            }
        }

        let left_visible = 2 + disp_name.chars().count() + suffix_len;
        let mut spaces = target_col.saturating_sub(left_visible).saturating_sub(right_len);
        if spaces < 2 {
            spaces = 2;
        }

        let _ = writeln!(
            io.stdout,
            "  {}{}{} · {}{}{}{}{}{}",
            CLR_BOLD,
            disp_name,
            CLR_RESET,
            state_clr,
            state,
            CLR_RESET,
            next_tag,
            " ".repeat(spaces),
            right
        );

        if show_email && !acc.email.is_empty() {
            let _ = writeln!(io.stdout, "  {}Email: {}{}", CLR_DIM, acc.email, CLR_RESET);
        }

        if acc.status == crate::storage::STATUS_DISABLED {
            let _ = writeln!(
                io.stdout,
                "  {}Disabled (Excluded from pool routing){}",
                CLR_DIM, CLR_RESET
            );
        } else if acc.status == crate::storage::STATUS_VALIDATION_REQUIRED {
            let _ = writeln!(
                io.stdout,
                "  {}Action Required (Verify needed){}",
                CLR_YELLOW, CLR_RESET
            );
            let _ = writeln!(io.stdout, "  Run: agy-pool verify {}", i + 1);
        } else if acc.status == crate::storage::STATUS_VERIFICATION_PENDING {
            let _ = writeln!(
                io.stdout,
                "  {}Waiting for generation confirmation{}",
                CLR_CYAN, CLR_RESET
            );
        } else if acc.status == crate::storage::STATUS_AUTH_ERROR {
            let _ = writeln!(io.stdout, "  {}Auth Failure (Re-authenticate){}", CLR_RED, CLR_RESET);
        } else {
            if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
                let cap_state = crate::quota::compute_capacity_state(acc, now_ts as f64);
                let mut reason = "Weekly quota low".to_string();
                if cap_state.q5_known
                    && cap_state
                        .q5
                        .is_some_and(|q| q <= crate::scheduler::FIVE_HOUR_RESERVE_FLOOR)
                {
                    reason = "5H quota <= 15%".to_string();
                } else if cap_state.q7_known && cap_state.q7.is_some() {
                    reason = format!(
                        "Weekly quota <= {:.1}%",
                        crate::scheduler::dynamic_weekly_reserve_floor(cap_state.r7) * 100.0
                    );
                }
                if width >= 74 {
                    let _ = writeln!(
                        io.stdout,
                        "  {}[Reserve Protection: {}, held back from pool routing]{}",
                        CLR_YELLOW, reason, CLR_RESET
                    );
                } else {
                    let _ = writeln!(
                        io.stdout,
                        "  {}[Reserve Protection: {}]{}",
                        CLR_YELLOW, reason, CLR_RESET
                    );
                }
            }

            let q = acc.last_quota.as_ref();
            let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(q);
            let wk_blocked = weekly_blocked(gw_frac);
            let (reset5, reset7) = extract_reset_times(acc, q, now_ts);

            if let Some(frac) = q5_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let g5_pct = format!("{:>4}", format!("{pct_val}%"));
                let g5_bar = gated_bar(wk_blocked, &crate::quota::render_block_progress_bar(Some(frac), 20));
                let g5_reset = crate::quota::format_compact_remaining_time(reset5);
                let r5_str = if g5_reset == "ready" || g5_reset == "N/A" {
                    g5_reset
                } else {
                    format!("resets in {g5_reset}")
                };
                let _ = writeln!(io.stdout, "  5H  {g5_bar} {g5_pct}   {r5_str}");
            } else {
                let _ = writeln!(io.stdout, "  5H -- unknown");
            }

            if let Some(frac) = gw_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let gw_pct = format!("{:>4}", format!("{pct_val}%"));
                let gw_bar = crate::quota::render_block_progress_bar(Some(frac), 20);
                let mut gw_reset = crate::quota::format_compact_remaining_time(reset7);
                if wk_blocked {
                    gw_reset.push_str(" · all models blocked");
                }
                let r7_str = if gw_reset == "ready" || gw_reset == "N/A" {
                    gw_reset
                } else {
                    format!("resets in {gw_reset}")
                };
                let _ = writeln!(io.stdout, "  WK  {gw_bar} {gw_pct}   {r7_str}");
            } else {
                let _ = writeln!(io.stdout, "  {}", crate::quota::weekly_unknown_label(acc));
            }

            if let Some(q) = q
                && let Some(tp5) = &q.third_party_5h
                && let Some(frac) = tp5.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_bar = gated_bar(wk_blocked, &crate::quota::render_block_progress_bar(Some(frac), 20));
                let tp_reset = crate::quota::format_compact_remaining_time(tp5.reset_time.as_ref());
                let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                    tp_reset
                } else {
                    format!("resets in {tp_reset}")
                };
                let _ = writeln!(io.stdout, "  3P  {tp_bar} {tp_pct}   {r_tp_str}");
            }
            if let Some(q) = q
                && let Some(tpw) = &q.third_party_weekly
                && let Some(frac) = tpw.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_bar = gated_bar(wk_blocked, &crate::quota::render_block_progress_bar(Some(frac), 20));
                let tp_reset = crate::quota::format_compact_remaining_time(tpw.reset_time.as_ref());
                let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                    tp_reset
                } else {
                    format!("resets in {tp_reset}")
                };
                let _ = writeln!(io.stdout, "  3W  {tp_bar} {tp_pct}   {r_tp_str}");
            }
        }

        if i < acc_list.len() - 1 {
            let _ = writeln!(io.stdout);
        }
    }

    let strat = if pool.strategy.is_empty() {
        crate::config::DEFAULT_STRATEGY
    } else {
        &pool.strategy
    };

    let _ = writeln!(io.stdout, "{sub_sep}");
    let _ = writeln!(
        io.stdout,
        " {}[CLI Base]{} {}Base account for CLI/auth metadata{}",
        CLR_GREEN, CLR_RESET, CLR_DIM, CLR_RESET
    );
    let _ = writeln!(
        io.stdout,
        " {}[Ready]{}    {}Available in rotation pool{}",
        CLR_CYAN, CLR_RESET, CLR_DIM, CLR_RESET
    );
    let _ = writeln!(
        io.stdout,
        " {}[Cooldown]{} {}Rate Limited{}      {}[Exhausted] Quota Depleted{}",
        CLR_YELLOW, CLR_RESET, CLR_DIM, CLR_RESET, CLR_DIM, CLR_RESET
    );
    if width >= 64 {
        let _ = writeln!(
            io.stdout,
            " {}[Protected]{} {}Low Quota Reserve{}   {}[Disabled] Excluded from Pool{}",
            CLR_YELLOW, CLR_RESET, CLR_DIM, CLR_RESET, CLR_DIM, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            " Strategy: {}{}{} | Gateway Proxy: {}",
            CLR_BOLD, strat, CLR_RESET, gateway_status
        );
    } else {
        let _ = writeln!(
            io.stdout,
            " {}[Protected]{} {}Low Quota Reserve{}  {}[Disabled] Excluded{}",
            CLR_YELLOW, CLR_RESET, CLR_DIM, CLR_RESET, CLR_DIM, CLR_RESET
        );
        let short_status = if daemon_running {
            format!("{}RUNNING (:{port}){}", CLR_GREEN, CLR_RESET)
        } else {
            format!("{}STOPPED{}", CLR_DIM, CLR_RESET)
        };
        let _ = writeln!(
            io.stdout,
            " Strategy: {}{}{} | Proxy: {}",
            CLR_BOLD, strat, CLR_RESET, short_status
        );
    }
    let mut out_ref: &mut dyn Write = io.stdout;
    crate::forecast::print_runway_summary(&mut out_ref, pool, now_ts as f64, Some(width));
    let _ = writeln!(io.stdout, "{sep}\n");
}

#[allow(clippy::too_many_arguments)]
fn render_mobile_quota_dashboard(
    acc_list: &[&crate::storage::Account],
    pool: &crate::storage::Pool,
    port: u16,
    now_ts: i64,
    width: usize,
    show_email: bool,
    io: &mut Io<'_>,
) {
    let daemon_running = crate::daemon::is_port_listening(port);
    let _ = writeln!(io.stdout);
    let next_id = resolve_next_candidate_id(pool, now_ts);
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (state, state_clr) = get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();

        let (next_tag, next_tag_visible) = if !next_id.is_empty() && acc.id == next_id {
            (format!(" {}{}[Next ⚡]{}", CLR_BOLD, CLR_CYAN, CLR_RESET), " [Next ⚡]")
        } else {
            (String::new(), "")
        };

        let suffix = format!(" · {state}{next_tag_visible}");
        let suffix_len = suffix.chars().count();
        if disp_name.chars().count() + suffix_len > width {
            let avail = width.saturating_sub(suffix_len);
            if avail > 3 {
                disp_name = truncate_runes(&disp_name, avail);
            }
        }

        let _ = writeln!(
            io.stdout,
            "{}{}{} · {}{}{}{}",
            CLR_BOLD, disp_name, CLR_RESET, state_clr, state, CLR_RESET, next_tag
        );

        if show_email && !acc.email.is_empty() {
            let _ = writeln!(io.stdout, "{}Email: {}{}", CLR_DIM, acc.email, CLR_RESET);
        }

        if acc.status == crate::storage::STATUS_DISABLED {
            let _ = writeln!(
                io.stdout,
                "{}Disabled (Excluded from pool routing){}",
                CLR_DIM, CLR_RESET
            );
        } else if acc.status == crate::storage::STATUS_VALIDATION_REQUIRED {
            let _ = writeln!(io.stdout, "{}Action Required (Verify needed){}", CLR_YELLOW, CLR_RESET);
            let _ = writeln!(io.stdout, "Run: agy-pool verify {}", i + 1);
        } else if acc.status == crate::storage::STATUS_VERIFICATION_PENDING {
            let _ = writeln!(
                io.stdout,
                "{}Waiting for generation confirmation{}",
                CLR_CYAN, CLR_RESET
            );
        } else if acc.status == crate::storage::STATUS_AUTH_ERROR {
            let _ = writeln!(io.stdout, "{}Auth Failure (Re-authenticate){}", CLR_RED, CLR_RESET);
        } else {
            if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
                let _ = writeln!(io.stdout, "{}[Reserve Protection: Low Quota]{}", CLR_YELLOW, CLR_RESET);
            }

            let q = acc.last_quota.as_ref();
            let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(q);
            let wk_blocked = weekly_blocked(gw_frac);
            let (reset5, reset7) = extract_reset_times(acc, q, now_ts);

            if let Some(frac) = q5_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let g5_pct = format!("{:>4}", format!("{pct_val}%"));
                let g5_reset = crate::quota::format_compact_remaining_time(reset5);
                let non_bar5 = 10 + g5_reset.len();
                let bar_w5 = choose_bar_width(width, non_bar5, 10, 4);
                if bar_w5 > 0 {
                    let g5_bar = gated_bar(
                        wk_blocked,
                        &crate::quota::render_block_progress_bar(Some(frac), bar_w5 as i32),
                    );
                    let _ = writeln!(io.stdout, "5H {g5_pct} {g5_bar}  {g5_reset}");
                } else {
                    let _ = writeln!(io.stdout, "5H {g5_pct}  {g5_reset}");
                }
            } else {
                let _ = writeln!(io.stdout, "5H -- unknown");
            }

            if let Some(frac) = gw_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let gw_pct = format!("{:>4}", format!("{pct_val}%"));
                let mut gw_reset = crate::quota::format_compact_remaining_time(reset7);
                if wk_blocked {
                    gw_reset.push_str(" · all models blocked");
                }
                let non_bar7 = 10 + gw_reset.len();
                let bar_w7 = choose_bar_width(width, non_bar7, 10, 4);
                if bar_w7 > 0 {
                    let gw_bar = crate::quota::render_block_progress_bar(Some(frac), bar_w7 as i32);
                    let _ = writeln!(io.stdout, "WK {gw_pct} {gw_bar}  {gw_reset}");
                } else {
                    let _ = writeln!(io.stdout, "WK {gw_pct}  {gw_reset}");
                }
            } else {
                let _ = writeln!(io.stdout, "{}", crate::quota::weekly_unknown_label(acc));
            }

            if let Some(q) = q
                && let Some(tp5) = &q.third_party_5h
                && let Some(frac) = tp5.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_reset = crate::quota::format_compact_remaining_time(tp5.reset_time.as_ref());
                let non_bar_tp = 10 + tp_reset.len();
                let bar_w_tp = choose_bar_width(width, non_bar_tp, 10, 4);
                if bar_w_tp > 0 {
                    let tp_bar = gated_bar(
                        wk_blocked,
                        &crate::quota::render_block_progress_bar(Some(frac), bar_w_tp as i32),
                    );
                    let _ = writeln!(io.stdout, "3P {tp_pct} {tp_bar}  {tp_reset}");
                } else {
                    let _ = writeln!(io.stdout, "3P {tp_pct}  {tp_reset}");
                }
            }
            if let Some(q) = q
                && let Some(tpw) = &q.third_party_weekly
                && let Some(frac) = tpw.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_reset = crate::quota::format_compact_remaining_time(tpw.reset_time.as_ref());
                let non_bar_tp = 10 + tp_reset.len();
                let bar_w_tp = choose_bar_width(width, non_bar_tp, 10, 4);
                if bar_w_tp > 0 {
                    let tp_bar = gated_bar(
                        wk_blocked,
                        &crate::quota::render_block_progress_bar(Some(frac), bar_w_tp as i32),
                    );
                    let _ = writeln!(io.stdout, "3W {tp_pct} {tp_bar}  {tp_reset}");
                } else {
                    let _ = writeln!(io.stdout, "3W {tp_pct}  {tp_reset}");
                }
            }

            let age_str = crate::quota::format_short_quota_age(acc, now_ts as f64);
            let _ = writeln!(io.stdout, "age {age_str} · hits {hits}");
        }

        if i < acc_list.len() - 1 {
            let _ = writeln!(io.stdout);
        }
    }

    let strat = if pool.strategy.is_empty() {
        crate::config::DEFAULT_STRATEGY
    } else {
        &pool.strategy
    };
    let _ = writeln!(io.stdout);
    if daemon_running {
        let _ = writeln!(io.stdout, "{} · proxy up · :{port}\n", strat);
    } else {
        let _ = writeln!(io.stdout, "{strat} · proxy down\n");
    }
}

fn render_very_narrow_quota_dashboard(
    acc_list: &[&crate::storage::Account],
    pool: &crate::storage::Pool,
    now_ts: i64,
    width: usize,
    show_email: bool,
    io: &mut Io<'_>,
) {
    let _ = writeln!(io.stdout);
    let next_id = resolve_next_candidate_id(pool, now_ts);
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (state, state_clr) = get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();

        let (next_tag, next_tag_visible) = if !next_id.is_empty() && acc.id == next_id {
            (format!(" {}{}[Next ⚡]{}", CLR_BOLD, CLR_CYAN, CLR_RESET), " [Next ⚡]")
        } else {
            (String::new(), "")
        };

        let suffix = format!(" · {state}{next_tag_visible}");
        let suffix_len = suffix.chars().count();
        if disp_name.chars().count() + suffix_len > width {
            let avail = width.saturating_sub(suffix_len);
            if avail > 3 {
                disp_name = truncate_runes(&disp_name, avail);
            }
        }

        let _ = writeln!(
            io.stdout,
            "{}{}{} · {}{}{}{}",
            CLR_BOLD, disp_name, CLR_RESET, state_clr, state, CLR_RESET, next_tag
        );

        if show_email && !acc.email.is_empty() {
            let mut email_line = format!("{}Email: {}{}", CLR_DIM, acc.email, CLR_RESET);
            if strip_ansi(&email_line).chars().count() > width {
                email_line = truncate_runes(&email_line, width);
            }
            let _ = writeln!(io.stdout, "{email_line}");
        }

        if acc.status == crate::storage::STATUS_DISABLED {
            let _ = writeln!(io.stdout, "{}Disabled{}", CLR_DIM, CLR_RESET);
        } else if acc.status == crate::storage::STATUS_VALIDATION_REQUIRED {
            let _ = writeln!(io.stdout, "{}Verify Needed{}", CLR_YELLOW, CLR_RESET);
            let _ = writeln!(io.stdout, "agy-pool verify {}", i + 1);
        } else if acc.status == crate::storage::STATUS_VERIFICATION_PENDING {
            let _ = writeln!(io.stdout, "{}Waiting for confirmation{}", CLR_CYAN, CLR_RESET);
        } else if acc.status == crate::storage::STATUS_AUTH_ERROR {
            let _ = writeln!(io.stdout, "{}Auth Error{}", CLR_RED, CLR_RESET);
        } else {
            let q = acc.last_quota.as_ref();
            let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(q);
            let wk_blocked = weekly_blocked(gw_frac);
            let (reset5, reset7) = extract_reset_times(acc, q, now_ts);

            if let Some(frac) = q5_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let g5_pct = format!("{:>4}", format!("{pct_val}%"));
                let g5_reset = crate::quota::format_compact_remaining_time(reset5);
                let _ = writeln!(io.stdout, "5H {g5_pct}  {g5_reset}");
            } else {
                let _ = writeln!(io.stdout, "5H -- unknown");
            }

            if let Some(frac) = gw_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let gw_pct = format!("{:>4}", format!("{pct_val}%"));
                let mut gw_reset = crate::quota::format_compact_remaining_time(reset7);
                if wk_blocked {
                    gw_reset.push_str(" · all models blocked");
                }
                let _ = writeln!(io.stdout, "WK {gw_pct}  {gw_reset}");
            } else {
                let _ = writeln!(io.stdout, "{}", crate::quota::weekly_unknown_label(acc));
            }

            if let Some(q) = q
                && let Some(tp5) = &q.third_party_5h
                && let Some(frac) = tp5.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_reset = crate::quota::format_compact_remaining_time(tp5.reset_time.as_ref());
                let _ = writeln!(io.stdout, "3P {tp_pct}  {tp_reset}");
            }
            if let Some(q) = q
                && let Some(tpw) = &q.third_party_weekly
                && let Some(frac) = tpw.fraction
                && frac < 1.0
            {
                let pct_val = (frac * 100.0).round() as i64;
                let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                let tp_reset = crate::quota::format_compact_remaining_time(tpw.reset_time.as_ref());
                let _ = writeln!(io.stdout, "3W {tp_pct}  {tp_reset}");
            }

            let age_str = crate::quota::format_short_quota_age(acc, now_ts as f64);
            let _ = writeln!(io.stdout, "{age_str} · {hits} hits");
        }

        if i < acc_list.len() - 1 {
            let _ = writeln!(io.stdout);
        }
    }
    let _ = writeln!(io.stdout);
}

pub async fn rename_account(io: &mut Io<'_>, target: &str, name: &str) -> bool {
    if name.trim().is_empty() {
        let _ = writeln!(
            io.stdout,
            "{}[Error] Account name cannot be empty.{}",
            CLR_RED, CLR_RESET
        );
        return false;
    }
    match crate::accounts::rename_account(target, name) {
        Ok(acc) => {
            let _ = writeln!(
                io.stdout,
                "{}✓ Renamed account [{}] to: {}{}{}",
                CLR_GREEN, acc.id, CLR_BOLD, acc.name, CLR_RESET
            );
            true
        }
        Err(_) => {
            let _ = writeln!(
                io.stdout,
                "{}[Error] The selected account was not found.{}",
                CLR_RED, CLR_RESET
            );
            false
        }
    }
}

pub async fn remove_account(io: &mut Io<'_>, target: &str) -> bool {
    match crate::accounts::remove_account(target).await {
        Ok(acc) => {
            let disp = crate::accounts::display_account_name(Some(&acc), None);
            let _ = writeln!(
                io.stdout,
                "{}✓ Removed account {} from pool.{}",
                CLR_GREEN, disp, CLR_RESET
            );
            true
        }
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not remove the selected account.{}",
                CLR_RED, CLR_RESET
            );
            false
        }
    }
}

pub async fn disable_account(io: &mut Io<'_>, target: &str) -> bool {
    match crate::accounts::disable_account(target) {
        Ok(acc) => {
            let disp = crate::accounts::display_account_name(Some(&acc), None);
            let _ = writeln!(io.stdout, "{}✓ Disabled account {}.{}", CLR_GREEN, disp, CLR_RESET);
            true
        }
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not disable the selected account.{}",
                CLR_RED, CLR_RESET
            );
            false
        }
    }
}

pub async fn enable_account(io: &mut Io<'_>, target: &str) -> bool {
    match crate::accounts::enable_account(target) {
        Ok(acc) => {
            let disp = crate::accounts::display_account_name(Some(&acc), None);
            let _ = writeln!(io.stdout, "{}✓ Enabled account {}.{}", CLR_GREEN, disp, CLR_RESET);
            true
        }
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not enable the selected account.{}",
                CLR_RED, CLR_RESET
            );
            false
        }
    }
}

pub async fn switch_account(io: &mut Io<'_>, target: &str) -> bool {
    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not load the account pool.{}",
                CLR_RED, CLR_RESET
            );
            return false;
        }
    };
    if pool.accounts.is_empty() {
        let _ = writeln!(io.stderr, "{}[Error] Account pool is empty.{}", CLR_RED, CLR_RESET);
        return false;
    }
    match crate::accounts::switch_account(target).await {
        Ok(acc) => {
            let disp = crate::accounts::display_account_name(Some(&acc), None);
            let _ = writeln!(
                io.stdout,
                "{}✓ Switched CLI base account to: {}{}{}",
                CLR_GREEN, CLR_BOLD, disp, CLR_RESET
            );
            true
        }
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not switch the selected account.{}",
                CLR_RED, CLR_RESET
            );
            false
        }
    }
}

pub async fn handle_export(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut file_path = String::new();
    let mut encrypt = false;
    let mut password = String::new();
    let mut no_stats = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-e" || arg == "--encrypt" {
            encrypt = true;
        } else if arg == "--no-stats" {
            no_stats = true;
        } else if arg == "-p" || arg == "--password" {
            if i + 1 < args.len() {
                password = args[i + 1].clone();
                i += 1;
            }
        } else if let Some(p) = arg.strip_prefix("--password=") {
            password = p.to_string();
        } else if !arg.starts_with('-') && file_path.is_empty() {
            file_path = arg.clone();
        }
        i += 1;
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
    if pool.accounts.is_empty() {
        let _ = writeln!(io.stdout, "\n{}No accounts in pool to export.{}", CLR_YELLOW, CLR_RESET);
        return 0;
    }

    match crate::accounts::export_pool(&file_path, encrypt, &password, no_stats) {
        Ok(out_path) => {
            if file_path != "-" {
                let enc_label = if encrypt || !password.is_empty() {
                    format!(" ({}Encrypted with passphrase{})", CLR_GREEN, CLR_RESET)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    io.stdout,
                    "\n{}✓ Successfully exported {} account(s) to:{}",
                    CLR_GREEN,
                    pool.accounts.len(),
                    CLR_RESET
                );
                let _ = writeln!(io.stdout, "  {}{}{}{}", CLR_BOLD, out_path, CLR_RESET, enc_label);
                let _ = writeln!(
                    io.stdout,
                    "{}  File permissions: 0600 (Restricted to current user).{}\n",
                    CLR_DIM, CLR_RESET
                );
            }
            0
        }
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] Export failed: {}{}", CLR_RED, e, CLR_RESET);
            1
        }
    }
}

pub async fn handle_import(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut file_path = String::new();
    let mut password = String::new();
    let mut replace = false;
    let mut skip_existing = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--replace" {
            replace = true;
        } else if arg == "--skip-existing" {
            skip_existing = true;
        } else if arg == "-p" || arg == "--password" {
            if i + 1 < args.len() {
                password = args[i + 1].clone();
                i += 1;
            }
        } else if let Some(p) = arg.strip_prefix("--password=") {
            password = p.to_string();
        } else if !arg.starts_with('-') && file_path.is_empty() {
            file_path = arg.clone();
        }
        i += 1;
    }

    if file_path.is_empty() {
        let _ = write!(
            io.stderr,
            "usage: agy-pool import [-h] [-p PASSWORD] [--replace] [--skip-existing] file\nagy-pool import: error: the following arguments are required: file\n"
        );
        return 2;
    }

    let res = match crate::accounts::import_pool(&file_path, &password, replace, skip_existing) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] Import failed: {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };
    let final_pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Import completed but the account pool could not be verified.{}",
                CLR_RED, CLR_RESET
            );
            return 1;
        }
    };
    let total = final_pool.accounts.len();
    let _ = writeln!(
        io.stdout,
        "{}✓ Successfully imported {} account(s) (Pool total: {}){}",
        CLR_GREEN,
        res.added + res.updated,
        total,
        CLR_RESET
    );
    0
}

pub async fn handle_migrate_legacy(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut source_path = String::new();
    let mut data_dir = String::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-s" || arg == "--source" {
            if i + 1 < args.len() {
                source_path = args[i + 1].clone();
                i += 1;
            }
        } else if let Some(s) = arg.strip_prefix("--source=") {
            source_path = s.to_string();
        } else if arg == "-D" || arg == "--directory" {
            if i + 1 < args.len() {
                data_dir = args[i + 1].clone();
                i += 1;
            }
        } else if let Some(d) = arg.strip_prefix("--directory=") {
            data_dir = d.to_string();
        } else if !arg.starts_with('-') && source_path.is_empty() {
            source_path = arg.clone();
        }
        i += 1;
    }

    if source_path.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        source_path = std::path::Path::new(&home)
            .join(".gemini")
            .join("agy-pool-accounts.json")
            .to_string_lossy()
            .into_owned();
    }
    if data_dir.is_empty() {
        data_dir = crate::config::data_dir().to_string_lossy().into_owned();
    }

    match crate::storage::migrate_legacy_pool(std::path::Path::new(&source_path), std::path::Path::new(&data_dir)) {
        Ok(res) => {
            let _ = writeln!(
                io.stdout,
                "{}✓ Successfully migrated {} account(s) to {}{}",
                CLR_GREEN, res.accounts_count, res.target_file, CLR_RESET
            );
            let _ = writeln!(
                io.stdout,
                "{}Source file {} remains untouched.{}",
                CLR_DIM, res.source_path, CLR_RESET
            );
            0
        }
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Legacy migration failed: {}{}",
                CLR_RED, e, CLR_RESET
            );
            1
        }
    }
}

pub async fn login_cmd(io: &mut Io<'_>) -> i32 {
    login_cmd_with_options(
        io,
        crate::accounts::LoginOptions {
            auth_endpoint: None,
            token_endpoint: None,
            prompt_fn: None,
            browser_opener: None,
            port_range: None,
            timeout: None,
            stdout: None,
        },
    )
    .await
}

pub async fn login_cmd_with_options(io: &mut Io<'_>, extra_opts: crate::accounts::LoginOptions) -> i32 {
    let browser_opened = Arc::new(AtomicBool::new(false));
    let bo_flag = browser_opened.clone();
    let opener_override = extra_opts.browser_opener;
    let cli_opener = crate::cli::login_opener();
    let browser_opener: crate::accounts::BrowserOpenerFn = Box::new(move |url: &str| {
        let res = if let Some(ref op) = opener_override {
            op(url)
        } else if let Some(op) = cli_opener {
            op(url).map_err(crate::accounts::AccountsError::Message)
        } else {
            crate::accounts::default_browser_opener(url)
        };
        if res.is_ok() {
            bo_flag.store(true, Ordering::SeqCst);
        }
        res
    });

    let out_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let bo_for_prompt = browser_opened.clone();
    let stdout_for_prompt = out_buf.clone();

    let in_data = if io.stdin_fd.is_none() {
        let mut s = String::new();
        let _ = io.stdin.read_to_string(&mut s);
        Some(Arc::new(Mutex::new(s)))
    } else {
        None
    };

    let prompt_fn: crate::accounts::PromptFn = if let Some(p) = extra_opts.prompt_fn {
        p
    } else {
        Box::new(move |_auth_url: &str| {
            if bo_for_prompt.load(Ordering::SeqCst) {
                return Ok(String::new());
            }
            {
                let mut out = stdout_for_prompt.lock().unwrap_or_else(|e| e.into_inner());
                let _ = writeln!(out);
                let _ = writeln!(out, "Paste the authorization code or callback URL:");
                let _ = write!(out, "> ");
            }
            if let Some(ref in_arc) = in_data {
                let text = in_arc.lock().unwrap_or_else(|e| e.into_inner());
                let line = text.lines().next().unwrap_or("").trim().to_string();
                Ok(line)
            } else {
                let stdin = std::io::stdin();
                let mut line = String::new();
                let _ = stdin.lock().read_line(&mut line);
                Ok(line.trim().to_string())
            }
        })
    };

    let opts = crate::accounts::LoginOptions {
        auth_endpoint: extra_opts.auth_endpoint,
        token_endpoint: extra_opts.token_endpoint,
        prompt_fn: Some(prompt_fn),
        browser_opener: Some(browser_opener),
        port_range: extra_opts.port_range,
        timeout: extra_opts.timeout,
        stdout: Some(out_buf.clone()),
    };

    let res = crate::accounts::login(opts).await;

    let buffered = out_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !buffered.is_empty() {
        let _ = io.stdout.write_all(&buffered);
    }

    match res {
        Ok(acc) => {
            let disp = crate::accounts::display_account_name(Some(&acc), None);
            let _ = writeln!(
                io.stdout,
                "\n{}✓ Successfully authenticated as {}{}",
                CLR_GREEN, disp, CLR_RESET
            );
            0
        }
        Err(e) => {
            let err_str = e.to_string();
            let mut message = "Login failed.";
            if err_str.contains("login timed out or cancelled") {
                message = "login timed out or cancelled";
            } else if err_str.contains("OAuth callback error: access_denied") {
                message = "OAuth callback error: access_denied";
            }
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, message, CLR_RESET);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_mock_oauth_token_server(
        expected_code: &'static str,
        user_email: &'static str,
        access_token: &'static str,
        refresh_token: &'static str,
    ) -> crate::testutil::TestServer {
        crate::testutil::TestServer::start(move |req| {
            if req.form_value("code").as_deref() != Some(expected_code) {
                return (400, vec![], b"{\"error\":\"invalid_grant\"}".to_vec());
            }
            use base64::Engine;
            let payload_json = serde_json::json!({ "email": user_email }).to_string();
            let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json);
            let mock_jwt = format!("eyJhbGciOiJub25lIn0.{payload_b64}.");
            let resp = serde_json::json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "id_token": mock_jwt,
                "expires_in": 3600,
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                serde_json::to_vec(&resp).unwrap(),
            )
        })
    }

    static AUTH_URL_TX: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<String>>> = std::sync::Mutex::new(None);

    fn capturing_login_opener(url: &str) -> Result<(), String> {
        let lock = AUTH_URL_TX.lock().unwrap();
        if let Some(ref tx) = *lock {
            let _ = tx.try_send(url.to_string());
        }
        Ok(())
    }

    fn failing_login_opener_headless(_url: &str) -> Result<(), String> {
        Err("headless: no X11 DISPLAY".to_string())
    }

    fn failing_login_opener_no_browser(_url: &str) -> Result<(), String> {
        Err("headless: no browser installed".to_string())
    }

    fn failing_login_opener_generic(_url: &str) -> Result<(), String> {
        Err("headless".to_string())
    }

    async fn trigger_http_callback(url_str: &str, query: &str) {
        if let Ok(u) = url::Url::parse(url_str)
            && let Some((_, uri_str)) = u.query_pairs().find(|(k, _)| k == "redirect_uri")
            && let Ok(callback_url) = url::Url::parse(&uri_str)
            && let Some(host) = callback_url.host_str()
            && let Some(port) = callback_url.port()
        {
            let path = callback_url.path();
            let req = format!("GET {path}?{query} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                if let Ok(mut stream) = tokio::net::TcpStream::connect((host, port)).await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let _ = stream.write_all(req.as_bytes()).await;
                    let mut resp = Vec::new();
                    let _ = stream.read_to_end(&mut resp).await;
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_rename_command() {
        let _env = crate::cli::test_support::cli_test_env();

        // 1. Missing arguments -> exit 2
        let (code, _, _) = crate::cli::test_support::run_cli(&["rename"], "").await;
        assert_eq!(code, 2, "expected exit 2 on missing rename args");

        let (code, _, _) = crate::cli::test_support::run_cli(&["rename", "1"], "").await;
        assert_eq!(code, 2, "expected exit 2 on missing rename target name");

        // 2. Empty name -> exit 1
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["rename", "1", "   "], "").await;
        assert_eq!(code, 1, "expected exit 1 on empty rename name");
        assert!(stdout.contains("[Error] Account name cannot be empty."));

        // 3. Target not found in empty pool -> exit 1
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["rename", "nonexistent", "NewName"], "").await;
        assert_eq!(code, 1, "expected exit 1 on account not found");
        assert!(stdout.contains("[Error] The selected account was not found."));

        // 4. Successful rename
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "OldName".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["rename", "1", "BrandNewName"], "").await;
        assert_eq!(code, 0, "expected exit 0 on successful rename: {stderr}");
        assert!(strip_ansi(&stdout).contains("Renamed account [acc_1] to: BrandNewName"));
    }

    #[tokio::test]
    async fn test_disable_and_enable_command() {
        let _env = crate::cli::test_support::cli_test_env();

        // Missing arguments -> exit 2
        let (code, _, _) = crate::cli::test_support::run_cli(&["disable"], "").await;
        assert_eq!(code, 2);
        let (code, _, _) = crate::cli::test_support::run_cli(&["enable"], "").await;
        assert_eq!(code, 2);

        // Target not found -> exit 1
        let (code, _, _) = crate::cli::test_support::run_cli(&["disable", "nonexistent"], "").await;
        assert_eq!(code, 1);

        // Add an account
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "TestAcc".to_string(),
                email: "test@example.com".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        // Disable acc_1
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["disable", "1"], "").await;
        assert_eq!(code, 0, "failed to disable: {stderr}");
        assert!(stdout.contains("Disabled account TestAcc"));

        let loaded = crate::storage::load_pool().unwrap();
        let (state, _) = get_account_state(&loaded.accounts[0], &loaded, crate::clock::now_unix() as i64, None);
        assert_eq!(state, "Disabled");

        // Enable acc_1
        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["enable", "acc_1"], "").await;
        assert_eq!(code, 0, "failed to enable: {stderr}");
        assert!(stdout.contains("Enabled account TestAcc"));

        let loaded = crate::storage::load_pool().unwrap();
        let (state, _) = get_account_state(&loaded.accounts[0], &loaded, crate::clock::now_unix() as i64, None);
        assert_eq!(state, "Ready");
    }

    #[tokio::test]
    async fn test_disabled_account_quota_and_list_rendering() {
        let _env = crate::cli::test_support::cli_test_env();

        let full_frac = 1.0;
        let low_frac = 0.08;
        let h5_frac = 0.40;

        let acc_disabled = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "DisabledAcc".to_string(),
            status: crate::storage::STATUS_DISABLED.to_string(),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(full_frac),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(full_frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(full_frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_protected = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "ProtectedAcc".to_string(),
            status: String::new(),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(low_frac),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(h5_frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(low_frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_2".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc_disabled, acc_protected],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        // 1. Wide quota dashboard (width 100)
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stdin = std::io::Cursor::new(Vec::new());
        {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", 100, &mut io, false);
        }
        let clean_wide = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(clean_wide.contains("Disabled (Excluded from pool routing)"));
        assert!(clean_wide.contains("[Reserve Protection:"));
        assert!(clean_wide.contains("[Protected]") && clean_wide.contains("[Disabled]"));

        // 2. Tablet quota dashboard (width 70)
        out.clear();
        err.clear();
        {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", 70, &mut io, false);
        }
        let clean_tablet = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(clean_tablet.contains("Disabled (Excluded from pool routing)"));
        assert!(clean_tablet.contains("[Reserve Protection:"));
        assert!(clean_tablet.contains("[Protected]") && clean_tablet.contains("[Disabled]"));

        // 3. agy-pool list (ListCmd)
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list"], "").await;
        assert_eq!(code, 0);
        let clean_list = strip_ansi(&stdout);
        let lines: Vec<&str> = clean_list.trim().split('\n').collect();
        let mut found_disabled = false;
        let mut found_protected = false;
        for line in lines {
            if line.contains("DisabledAcc") {
                found_disabled = true;
                assert!(line.contains("Disabled"));
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 7 {
                    let q5 = fields[5];
                    let qw = fields[6];
                    assert_eq!(q5, "--");
                    assert_eq!(qw, "--");
                }
            }
            if line.contains("ProtectedAcc") {
                found_protected = true;
                assert!(line.contains("Protected"));
            }
        }
        assert!(found_disabled && found_protected);
    }

    #[tokio::test]
    async fn test_list_accounts_empty_and_populated() {
        let _env = crate::cli::test_support::cli_test_env();

        // 1. Empty pool
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("No accounts in pool yet."));

        // 2. Populate pool
        let now = crate::clock::now_unix() as i64;
        let frac = 0.75;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "user1@gmail.com".to_string(),
            name: "Personal".to_string(),
            status: "active".to_string(),
            request_count: 42,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("Personal"));
        assert!(stdout.contains("CLI Base"));
        assert!(stdout.contains("42"));

        let (code, q_out, _) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0);
        assert!(q_out.contains("Personal") && q_out.contains("hits 42") && q_out.contains("CLI Base"));

        // 3. Target not found -> exit 0
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list", "nonexistent"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("[Error] The selected account was not found."));
    }

    #[tokio::test]
    async fn test_migrate_legacy_command() {
        let env = crate::cli::test_support::cli_test_env();

        let src_file = env.dir.path().join("legacy_pool.json");
        let target_dir = env.dir.path().join("target_data");
        let content = r#"{
  "version": 1,
  "strategy": "max_quota",
  "accounts": [
    {"id": "acc_1", "email": "migrated@example.com", "access_token": "tok1"}
  ]
}"#;
        std::fs::write(&src_file, content).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(
            &[
                "migrate-legacy",
                "-s",
                src_file.to_str().unwrap(),
                "-D",
                target_dir.to_str().unwrap(),
            ],
            "",
        )
        .await;
        assert_eq!(code, 0, "migrate-legacy failed: {stderr}");
        assert!(stdout.contains("Successfully migrated 1 account"));

        let target_accounts = target_dir.join("accounts.json");
        assert!(target_accounts.exists());

        let data = std::fs::read_to_string(&src_file).unwrap();
        assert_eq!(data, content);
    }

    #[test]
    fn test_terminal_width_non_tty() {
        let w = terminal_width(None);
        assert_eq!(w, 100);

        let _env = crate::cli::test_support::cli_test_env();
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_test".to_string()),
            accounts: vec![crate::storage::Account {
                id: "acc_test".to_string(),
                name: "TestAccount".to_string(),
                status: "ready".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        list_accounts("", &mut io, false);
        let clean = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(clean.contains("Antigravity Multi-Account Pool"));
    }

    #[test]
    fn test_list_accounts_responsive_widths() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let frac1_5 = 1.0;
        let frac1_w = 0.499;
        let r1_5 = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(4).minutes(57))
            .unwrap()
            .to_string();
        let r1_w = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(135))
            .unwrap()
            .to_string();
        let up1 = now - 154;

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            status: "ready".to_string(),
            request_count: 1035,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(up1),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac1_5),
                    reset_time: Some(serde_json::Value::String(r1_5)),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac1_w),
                    reset_time: Some(serde_json::Value::String(r1_w)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let frac2_5 = 0.916;
        let frac2_w = 0.554;
        let r2_5 = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(4).minutes(53))
            .unwrap()
            .to_string();
        let r2_w = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(136))
            .unwrap()
            .to_string();
        let up2 = now - 46;

        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Backup".to_string(),
            status: "ready".to_string(),
            request_count: 2110,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(up2),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac2_5),
                    reset_time: Some(serde_json::Value::String(r2_5)),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac2_w),
                    reset_time: Some(serde_json::Value::String(r2_w)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1, acc2],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let widths = [120, 96, 95, 80, 60, 59, 50, 45, 40, 39, 35];
        for w in widths {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::Cursor::new(Vec::new());
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", w, &mut io, false);

            let output = String::from_utf8_lossy(&out);
            let lines: Vec<&str> = output.split('\n').collect();

            for (line_idx, line) in lines.iter().enumerate() {
                let v_len = strip_ansi(line).chars().count();
                if w >= 40 && v_len > w {
                    panic!("width {w}: line {line_idx} exceeds terminal width ({v_len} > {w}):\n{line}");
                }
                let trimmed = line.trim();
                if (trimmed == "Hits:"
                    || trimmed == "hits"
                    || trimmed.starts_with("hits ")
                    || trimmed.ends_with(" hits"))
                    && !trimmed.contains("·")
                {
                    panic!("width {w}: Hits wrapped onto its own line:\n{line}");
                }
            }

            let clean_output = strip_ansi(&output);
            if w >= 60 {
                assert!(clean_output.contains("Antigravity Multi-Account Pool"), "width {w}");
                assert!(
                    clean_output.contains("Main · CLI Base") && clean_output.contains("hits 1035"),
                    "width {w}"
                );
                assert!(
                    clean_output.contains("5H  ████████████████████ 100%   resets in 4h5"),
                    "width {w}: {clean_output}"
                );
                assert!(
                    clean_output.contains("WK  ██████████░░░░░░░░░░  50%   resets in 5d1"),
                    "width {w}: {clean_output}"
                );
                assert!(
                    clean_output.contains("quota age") || clean_output.contains("age"),
                    "width {w}"
                );
                assert!(
                    clean_output.contains("Backup · Ready") && clean_output.contains("hits 2110"),
                    "width {w}"
                );
            } else if w >= 40 {
                assert!(clean_output.contains("Main · CLI Base"), "width {w}");
                assert!(!clean_output.contains("[1] Main"), "width {w}");
                assert!(
                    clean_output.contains("5H 100% ██████████  4h5"),
                    "width {w}: {clean_output}"
                );
                assert!(
                    clean_output.contains("WK  50% █████░░░░░  5d1"),
                    "width {w}: {clean_output}"
                );
                assert!(clean_output.contains("age 2m · hits 1035"), "width {w}: {clean_output}");
                assert!(!clean_output.contains("reset 4h"), "width {w}");
                assert!(clean_output.contains("max_quota · proxy"), "width {w}");
            } else {
                assert!(clean_output.contains("Main · CLI Base"), "width {w}");
                assert!(clean_output.contains("5H 100%  4h5"), "width {w}: {clean_output}");
                assert!(clean_output.contains("WK  50%  5d1"), "width {w}: {clean_output}");
                assert!(clean_output.contains("2m · 1035 hits"), "width {w}: {clean_output}");
                assert!(!clean_output.contains("██"), "width {w}");
            }
        }
    }

    #[tokio::test]
    async fn test_login_browser_opener_succeeds_http_callback_succeeds() {
        let _env = crate::cli::test_support::cli_test_env();
        let server = setup_mock_oauth_token_server(
            "browser-code-123",
            "browser_user@gmail.com",
            "at-browser-123",
            "rt-browser-123",
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        *AUTH_URL_TX.lock().unwrap() = Some(tx);
        crate::cli::set_login_opener(Some(capturing_login_opener));

        tokio::spawn(async move {
            if let Some(auth_url) = rx.recv().await {
                trigger_http_callback(&auth_url, "code=browser-code-123").await;
            }
        });

        let mut stdin = std::io::Cursor::new(Vec::new());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: Some(server.url()),
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18250, 18300]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("Opening browser for Google authentication..."));
        assert!(out_str.contains("Waiting for authorization..."));
        assert!(!out_str.contains("Could not open a browser automatically."));
        assert!(!out_str.contains("Paste the authorization code or callback URL:"));
        assert!(out_str.contains("Successfully authenticated as Account 1"));
        assert!(!out_str.contains("browser_user@gmail.com"));

        let pool = crate::storage::load_pool().unwrap();
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(pool.accounts[0].email, "browser_user@gmail.com");
        assert_eq!(pool.accounts[0].access_token, "at-browser-123");
    }

    #[tokio::test]
    async fn test_login_browser_opener_fails_manual_raw_code_succeeds() {
        let _env = crate::cli::test_support::cli_test_env();
        let server = setup_mock_oauth_token_server(
            "manual-raw-code-456",
            "manual_user@gmail.com",
            "at-manual-456",
            "rt-manual-456",
        );
        crate::cli::set_login_opener(Some(failing_login_opener_headless));

        let mut stdin = std::io::Cursor::new(b"manual-raw-code-456\n".to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: Some(server.url()),
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18301, 18350]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("Could not open a browser automatically."));
        assert!(out_str.contains("Open this URL:\nhttps://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(out_str.contains("Paste the authorization code or callback URL:\n> "));
        assert!(!out_str.contains("Opening browser for Google authentication..."));
        assert!(out_str.contains("Successfully authenticated as Account 1"));
        assert!(!out_str.contains("manual_user@gmail.com"));

        let pool = crate::storage::load_pool().unwrap();
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(pool.accounts[0].email, "manual_user@gmail.com");
        assert_eq!(pool.accounts[0].access_token, "at-manual-456");
    }

    #[tokio::test]
    async fn test_login_browser_opener_fails_pasted_callback_url_succeeds() {
        let _env = crate::cli::test_support::cli_test_env();
        let server = setup_mock_oauth_token_server(
            "pasted-url-code-789",
            "pasted_user@gmail.com",
            "at-pasted-789",
            "rt-pasted-789",
        );
        crate::cli::set_login_opener(Some(failing_login_opener_no_browser));

        let pasted_url = "http://localhost:8085/auth/callback?code=pasted-url-code-789&scope=email\n";
        let mut stdin = std::io::Cursor::new(pasted_url.as_bytes().to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: Some(server.url()),
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18351, 18400]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("Could not open a browser automatically."));
        assert!(out_str.contains("Paste the authorization code or callback URL:\n> "));
        assert!(out_str.contains("Successfully authenticated as Account 1"));
        assert!(!out_str.contains("pasted_user@gmail.com"));

        let pool = crate::storage::load_pool().unwrap();
        assert_eq!(pool.accounts.len(), 1);
        assert_eq!(pool.accounts[0].email, "pasted_user@gmail.com");
        assert_eq!(pool.accounts[0].access_token, "at-pasted-789");
    }

    #[tokio::test]
    async fn test_login_timeout() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::cli::set_login_opener(Some(failing_login_opener_generic));

        let mut stdin = std::io::Cursor::new(Vec::new());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: None,
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18401, 18450]),
                    timeout: Some(std::time::Duration::from_millis(50)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&err).contains("login timed out or cancelled"));
    }

    #[tokio::test]
    async fn test_login_oauth_callback_error() {
        let _env = crate::cli::test_support::cli_test_env();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        *AUTH_URL_TX.lock().unwrap() = Some(tx);
        crate::cli::set_login_opener(Some(capturing_login_opener));

        tokio::spawn(async move {
            if let Some(auth_url) = rx.recv().await {
                trigger_http_callback(&auth_url, "error=access_denied").await;
            }
        });

        let mut stdin = std::io::Cursor::new(Vec::new());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: None,
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18451, 18500]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&err).contains("OAuth callback error: access_denied"));
    }

    #[tokio::test]
    async fn test_login_no_token_or_secret_leakage() {
        let _env = crate::cli::test_support::cli_test_env();
        let secret_token = crate::auth::client_secret();
        let access_token = "leaked-at-secret-12345";
        let refresh_token = "leaked-rt-secret-67890";

        let server =
            setup_mock_oauth_token_server("code-leak-check", "leak_check@gmail.com", access_token, refresh_token);
        crate::cli::set_login_opener(Some(failing_login_opener_generic));

        let mut stdin = std::io::Cursor::new(b"code-leak-check\n".to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: Some(server.url()),
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18501, 18550]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 0);
        let combined = format!("{}\n{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
        assert!(!combined.contains(&secret_token));
        assert!(!combined.contains(access_token));
        assert!(!combined.contains(refresh_token));
        assert!(!combined.contains("eyJhbGciOiJub25l"));
        assert!(!combined.contains("leak_check@gmail.com"));
        assert!(String::from_utf8_lossy(&out).contains("Successfully authenticated as Account 1"));
        assert!(String::from_utf8_lossy(&out).contains("https://accounts.google.com/o/oauth2/v2/auth?"));
    }

    #[tokio::test]
    async fn test_login_existing_account_update() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64 - 1000;
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_existing_1".to_string(),
                name: "Work".to_string(),
                email: "existing_user@gmail.com".to_string(),
                access_token: "old-access-token".to_string(),
                refresh_token: "old-refresh-token".to_string(),
                request_count: 77,
                created_at: Some(now),
                updated_at: Some(now),
                ..Default::default()
            }],
            active_account_id: Some("acc_existing_1".to_string()),
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let server = setup_mock_oauth_token_server(
            "update-code-999",
            "existing_user@gmail.com",
            "new-access-token-999",
            "new-refresh-token-999",
        );
        crate::cli::set_login_opener(Some(failing_login_opener_generic));

        let mut stdin = std::io::Cursor::new(b"update-code-999\n".to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            login_cmd_with_options(
                &mut io,
                crate::accounts::LoginOptions {
                    auth_endpoint: None,
                    token_endpoint: Some(server.url()),
                    prompt_fn: None,
                    browser_opener: None,
                    port_range: Some([18551, 18600]),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    stdout: None,
                },
            )
            .await
        };
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("Successfully authenticated as Work"));
        assert!(!out_str.contains("existing_user@gmail.com"));

        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        let acc = &loaded.accounts[0];
        assert_eq!(acc.id, "acc_existing_1");
        assert_eq!(acc.name, "Work");
        assert_eq!(acc.access_token, "new-access-token-999");
        assert_eq!(acc.refresh_token, "new-refresh-token-999");
        assert_eq!(acc.request_count, 77);
        assert_eq!(acc.created_at, Some(now));
    }

    #[test]
    fn test_quota_dashboard_edge_cases_and_layouts() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let up12 = now - 12;
        let frac100 = 1.0;
        let frac21 = 0.21;
        let frac75 = 0.75;

        let reset_min = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().minutes(45).seconds(30))
            .unwrap()
            .to_string();
        let reset_hour = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(4).minutes(59).seconds(30))
            .unwrap()
            .to_string();
        let reset_day = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(134).seconds(30))
            .unwrap()
            .to_string();

        let acc_full = crate::storage::Account {
            id: "acc_full".to_string(),
            name: "Main".to_string(),
            status: "ready".to_string(),
            request_count: 2395,
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(up12),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac100),
                    reset_time: Some(serde_json::Value::String(reset_hour.clone())),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac21),
                    reset_time: Some(serde_json::Value::String(reset_day.clone())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_5h_only = crate::storage::Account {
            id: "acc_5h_only".to_string(),
            name: "FiveHourOnly".to_string(),
            status: "ready".to_string(),
            request_count: 150,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac75),
                    reset_time: Some(serde_json::Value::String(reset_min)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_wk_only = crate::storage::Account {
            id: "acc_wk_only".to_string(),
            name: "WeeklyOnly".to_string(),
            status: "ready".to_string(),
            request_count: 200,
            last_quota: Some(crate::storage::QuotaState {
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac21),
                    reset_time: Some(serde_json::Value::String(reset_day.clone())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_unknown = crate::storage::Account {
            id: "acc_unknown".to_string(),
            name: "UnknownAccount".to_string(),
            status: "ready".to_string(),
            request_count: 0,
            ..Default::default()
        };

        let acc_restricted = crate::storage::Account {
            id: "acc_restricted".to_string(),
            name: "RestrictedAcc".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            request_count: 42,
            ..Default::default()
        };

        let acc_auth_err = crate::storage::Account {
            id: "acc_auth_err".to_string(),
            name: "AuthErrAcc".to_string(),
            status: crate::storage::STATUS_AUTH_ERROR.to_string(),
            request_count: 99,
            ..Default::default()
        };

        let acc_long_and_large = crate::storage::Account {
            id: "acc_long_large".to_string(),
            name: "SuperLongAccountNameExceedingNormalTerminalWidthBoundariesEasily".to_string(),
            status: "ready".to_string(),
            request_count: 12345678,
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac100),
                    reset_time: Some(serde_json::Value::String(reset_hour)),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac21),
                    reset_time: Some(serde_json::Value::String(reset_day)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_full".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                acc_full,
                acc_5h_only,
                acc_wk_only,
                acc_unknown,
                acc_restricted,
                acc_auth_err,
                acc_long_and_large,
            ],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let test_widths = [120, 96, 95, 80, 60, 59, 50, 40, 39, 35];
        for w in test_widths {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::Cursor::new(Vec::new());
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", w, &mut io, false);

            let output = String::from_utf8_lossy(&out);
            let lines: Vec<&str> = output.split('\n').collect();

            for (line_idx, line) in lines.iter().enumerate() {
                let v_len = strip_ansi(line).chars().count();
                if w >= 40 && v_len > w {
                    panic!("width {w}: line {line_idx} exceeds terminal width ({v_len} > {w}):\n{line}");
                }
                let trimmed = line.trim();
                if (trimmed == "hits"
                    || trimmed == "Hits:"
                    || trimmed.starts_with("hits ")
                    || trimmed.ends_with(" hits"))
                    && !trimmed.contains("·")
                {
                    panic!("width {w}: hits wrapped onto its own line:\n{line}");
                }
            }

            let clean = strip_ansi(&output);
            assert!(clean.contains("Main · CLI Base"), "width {w}");
            assert!(clean.contains("hits 2395") || clean.contains("2395 hits"), "width {w}");
            assert!(clean.contains("FiveHourOnly"), "width {w}");
            assert!(clean.contains("WeeklyOnly"), "width {w}");
            assert!(clean.contains("WK -- unknown"), "width {w}");
            assert!(clean.contains("5H -- unknown"), "width {w}");
            assert!(clean.contains("UnknownAccount"), "width {w}");

            if w >= 40 {
                assert!(clean.contains("Action Required (Verify needed)"), "width {w}");
                assert!(clean.contains("Auth Failure (Re-authenticate)"), "width {w}");
            } else {
                assert!(clean.contains("Verify Needed"), "width {w}");
                assert!(clean.contains("Auth Error"), "width {w}");
            }

            assert!(clean.contains("4h59m"), "width {w}");
            assert!(clean.contains("5d14h"), "width {w}");
            assert!(clean.contains("45m"), "width {w}");
            assert!(clean.contains("12345678"), "width {w}");

            if w >= 60 {
                assert!(
                    clean.contains("5H  ████████████████████ 100%   resets in 4h59m"),
                    "width {w}:\n{clean}"
                );
                assert!(
                    clean.contains("WK  ████░░░░░░░░░░░░░░░░  21%   resets in 5d14h"),
                    "width {w}:\n{clean}"
                );
                assert!(clean.contains("quota age") || clean.contains("age"), "width {w}");
            } else if w >= 40 {
                assert!(clean.contains("5H 100% ██████████  4h59m"), "width {w}:\n{clean}");
                assert!(clean.contains("WK  21% ██░░░░░░░░  5d14h"), "width {w}:\n{clean}");
            } else {
                assert!(clean.contains("5H 100%  4h59m"), "width {w}:\n{clean}");
                assert!(clean.contains("WK  21%  5d14h"), "width {w}:\n{clean}");
            }
        }
    }

    #[test]
    fn test_render_representative_widths() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let up12 = now - 12;
        let frac100 = 1.0;
        let frac21 = 0.21;
        let reset_hour = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(4).minutes(59))
            .unwrap()
            .to_string();
        let reset_day = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(134))
            .unwrap()
            .to_string();

        let acc = crate::storage::Account {
            id: "acc_main".to_string(),
            name: "Main".to_string(),
            status: "ready".to_string(),
            request_count: 2395,
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(up12),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac100),
                    reset_time: Some(serde_json::Value::String(reset_hour)),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac21),
                    reset_time: Some(serde_json::Value::String(reset_day)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_unknown = crate::storage::Account {
            id: "acc_unknown".to_string(),
            name: "Secondary".to_string(),
            status: "ready".to_string(),
            request_count: 10,
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_main".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc, acc_unknown],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        for w in [120, 80, 50, 35] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::Cursor::new(Vec::new());
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", w, &mut io, false);
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn test_list_accounts_next_candidate_tag() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.8;
        let r_soon = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().minutes(30))
            .unwrap()
            .to_string();
        let r_later = jiff::Timestamp::from_second(now)
            .unwrap()
            .checked_add(jiff::Span::new().hours(2))
            .unwrap()
            .to_string();

        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "Main".to_string(),
            status: "ready".to_string(),
            request_count: 100,
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(now),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    reset_time: Some(serde_json::Value::String(r_soon.clone())),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    reset_time: Some(serde_json::Value::String(r_soon)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "Work".to_string(),
            status: "ready".to_string(),
            request_count: 50,
            last_quota: Some(crate::storage::QuotaState {
                updated_at: Some(now),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    reset_time: Some(serde_json::Value::String(r_later.clone())),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    reset_time: Some(serde_json::Value::String(r_later)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_SMART.to_string(),
            accounts: vec![acc1, acc2],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        list_accounts_with_width("", 80, &mut io, false);
        let clean = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(clean.contains("Main · CLI Base [Next ⚡]"));
        assert!(!clean.contains("Work · Ready [Next ⚡]"));
    }

    #[tokio::test]
    async fn test_list_cmd_compact_and_privacy() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.90;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "confidential-user@example.com".to_string(),
            name: "WorkDev".to_string(),
            status: "ready".to_string(),
            request_count: 123,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            email: "private-test@gmail.com".to_string(),
            status: "ready".to_string(),
            request_count: 45,
            created_at: Some(now),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1, acc2],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("#") && stdout.contains("ACCOUNT ID") && stdout.contains("STATUS"));
        assert!(stdout.contains("acc_1") && stdout.contains("WorkDev"));
        assert!(stdout.contains("acc_2") && stdout.contains("Account 2"));
        for email in [
            "confidential-user@example.com",
            "private-test@gmail.com",
            "@example.com",
            "@gmail.com",
        ] {
            assert!(!stdout.contains(email), "email leaked: {email}");
        }
        assert!(!stdout.contains("Gemini 5-Hour:") && !stdout.contains("[━━━━"));
    }

    #[tokio::test]
    async fn test_list_cmd_show_email() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = crate::storage::Pool {
            version: 1,
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                email: "myuser@example.com".to_string(),
                name: "DevAccount".to_string(),
                status: "ready".to_string(),
                created_at: Some(now),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        for flag in ["-e", "--email", "--show-email"] {
            let (code, stdout, _) = crate::cli::test_support::run_cli(&["list", flag], "").await;
            assert_eq!(code, 0);
            assert!(stdout.contains("EMAIL"));
            assert!(stdout.contains("myuser@example.com"));
        }
    }

    #[test]
    fn test_quota_unknown_is_explicit_across_layouts() {
        let _env = crate::cli::test_support::cli_test_env();
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "UnknownQuota".to_string(),
                request_count: 1035,
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        for width in [120, 80, 50, 35] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::Cursor::new(Vec::new());
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width("", width, &mut io, false);
            let text = strip_ansi(&String::from_utf8_lossy(&out));
            assert!(!text.contains("100%"));
            assert!(!text.contains("████"));
            assert!(!text.contains("━━━━"));
            assert!(text.contains("unknown") || text.contains("--"));
        }
    }

    #[test]
    fn test_quota_partial_windows_display_independently() {
        let _env = crate::cli::test_support::cli_test_env();
        let five_hour = 0.72;
        let weekly = 0.52;
        let pool = crate::storage::Pool {
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    name: "FiveHourOnly".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        gemini_5h: Some(crate::storage::QuotaWindow {
                            fraction: Some(five_hour),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    name: "WeeklyOnly".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        gemini_weekly: Some(crate::storage::QuotaWindow {
                            fraction: Some(weekly),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        list_accounts_with_width("", 80, &mut io, false);
        let text = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(text.contains("72%"));
        assert!(text.contains("52%"));
        assert!(text.matches("unknown").count() >= 2);
    }

    #[tokio::test]
    async fn test_quota_cmd_contains_progress_bars() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let frac = 0.70;
        let acc = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "QuotaTest".to_string(),
            status: "ready".to_string(),
            request_count: 88,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0);
        let clean = strip_ansi(&stdout);
        for expected in [
            "Antigravity Multi-Account Pool",
            "QuotaTest",
            "hits 88",
            "5H",
            "WK",
            "70%",
            "██████████████░░░░░░",
        ] {
            assert!(clean.contains(expected), "missing {expected}:\n{clean}");
        }

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota", "1"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("QuotaTest"));
    }

    #[tokio::test]
    async fn test_quota_cmd_show_email() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let frac = 0.8;
        let pool = crate::storage::Pool {
            version: 1,
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                email: "dev@example.com".to_string(),
                name: "DevAccount".to_string(),
                status: "ready".to_string(),
                created_at: Some(now),
                last_quota: Some(crate::storage::QuotaState {
                    remaining_fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        for flag in ["-e", "--email", "--show-email"] {
            let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota", flag], "").await;
            assert_eq!(code, 0);
            assert!(stdout.contains("Email: dev@example.com"));
        }

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0);
        assert!(!stdout.contains("dev@example.com"));
    }

    #[tokio::test]
    async fn test_quota_cmd_auto_hides_unused_third_party_windows() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let half = 0.70;
        let full = 1.0;
        let used = 0.97;
        let mk = |id: &str, tp5: Option<f64>, tpw: Option<f64>| crate::storage::Account {
            id: id.to_string(),
            name: id.to_string(),
            status: "ready".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(half),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(half),
                    ..Default::default()
                }),
                third_party_5h: Some(crate::storage::QuotaWindow {
                    fraction: tp5,
                    ..Default::default()
                }),
                third_party_weekly: Some(crate::storage::QuotaWindow {
                    fraction: tpw,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("unused".to_string()),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![mk("unused", Some(full), Some(full)), mk("used", Some(used), Some(full))],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0, "stderr: {stderr}");
        let out = strip_ansi(&stdout);
        assert_eq!(out.matches("3P ").count(), 1, "expected one 3P line:\n{out}");
        assert!(!out.contains("3W "), "3W at 100% must be hidden:\n{out}");
    }

    #[tokio::test]
    async fn test_quota_cmd_skips_live_fetch_for_fresh_snapshots() {
        let _env = crate::cli::test_support::cli_test_env();

        let hits = Arc::new(Mutex::new(std::collections::HashMap::<String, usize>::new()));
        let hits_clone = hits.clone();
        let server = crate::testutil::TestServer::start(move |req| {
            if let Some(auth) = req.header("authorization") {
                let mut map = hits_clone.lock().unwrap();
                *map.entry(auth.to_string()).or_default() += 1;
            }
            (500, vec![], b"internal error".to_vec())
        });

        let s_url = server.url();
        let _bg = crate::quota::set_backend_url_base_provider_for_test(Some(Box::new(move || s_url.clone())));
        let _tg = crate::quota::set_token_refresher_for_test(Some(Box::new(|acc| Ok(format!("token-{}", acc.id)))));

        let now = crate::clock::now_unix() as i64;
        let old = now - 600;
        let frac = 0.5;
        let pool = crate::storage::Pool {
            version: 1,
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                crate::storage::Account {
                    id: "acc_fresh".to_string(),
                    name: "Fresh".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        updated_at: Some(now),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_stale".to_string(),
                    name: "Stale".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        updated_at: Some(old),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, _, _) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0);

        let map = hits.lock().unwrap();
        assert_eq!(map.get("Bearer token-acc_fresh").copied().unwrap_or(0), 0);
        assert!(map.get("Bearer token-acc_stale").copied().unwrap_or(0) > 0);
    }

    #[test]
    fn test_resolve_next_candidate_id_skips_daemon_paused_accounts() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix();
        let hi = 0.95;
        let lo = 0.90;
        let pool = crate::storage::Pool {
            version: 1,
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    name: "First".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(hi),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    name: "Second".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(lo),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(resolve_next_candidate_id(&pool, now as i64), "acc_1");

        let write_stats = |updated: jiff::Timestamp, last_model: Option<&str>| {
            let mut model_pauses = std::collections::HashMap::new();
            let mut acc1_pauses = std::collections::HashMap::new();
            let pause_until = updated.checked_add(jiff::Span::new().hours(1)).unwrap();
            acc1_pauses.insert("m".to_string(), pause_until);
            model_pauses.insert("acc_1".to_string(), acc1_pauses);

            let stats = crate::daemon::RuntimeStats {
                pid: 1,
                started_at: updated,
                updated_at: updated,
                goroutines: 1,
                alloc_bytes: 0,
                heap_bytes: 0,
                sys_bytes: 0,
                num_gc: 0,
                go_version: "go1.26.7".to_string(),
                in_flight: None,
                model_pauses: Some(model_pauses),
                last_model: last_model.map(|s| s.to_string()),
                smart_rr_cursor: None,
                snapshot: crate::observability::Snapshot::default(),
            };
            crate::daemon::write_runtime_stats(&crate::daemon::runtime_file_in(&crate::config::data_dir()), &stats)
                .unwrap();
        };

        let now_ts = jiff::Timestamp::from_second(now as i64).unwrap();
        write_stats(now_ts, Some("m"));
        assert_eq!(resolve_next_candidate_id(&pool, now as i64), "acc_2");

        write_stats(now_ts, Some("other"));
        assert_eq!(resolve_next_candidate_id(&pool, now as i64), "acc_1");

        let stale_ts = now_ts.checked_sub(jiff::Span::new().seconds(120)).unwrap();
        write_stats(stale_ts, Some("m"));
        assert_eq!(resolve_next_candidate_id(&pool, now as i64), "acc_1");
    }

    #[test]
    fn test_resolve_next_candidate_id_follows_daemon_smart_rotation() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix();
        let frac = 0.95;
        let pool = crate::storage::Pool {
            version: 1,
            strategy: crate::config::STRATEGY_SMART.to_string(),
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    name: "First".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    name: "Second".to_string(),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let first = resolve_next_candidate_id(&pool, now as i64);
        let second = if first == "acc_1" { "acc_2" } else { "acc_1" };

        let now_ts = jiff::Timestamp::from_second(now as i64).unwrap();
        for (cursor, want) in [(0u64, &first), (1u64, &second.to_string()), (2u64, &first)] {
            let stats = crate::daemon::RuntimeStats {
                pid: 1,
                started_at: now_ts,
                updated_at: now_ts,
                goroutines: 1,
                alloc_bytes: 0,
                heap_bytes: 0,
                sys_bytes: 0,
                num_gc: 0,
                go_version: "go1.26.7".to_string(),
                in_flight: None,
                model_pauses: None,
                last_model: None,
                smart_rr_cursor: Some(cursor),
                snapshot: crate::observability::Snapshot::default(),
            };
            crate::daemon::write_runtime_stats(&crate::daemon::runtime_file_in(&crate::config::data_dir()), &stats)
                .unwrap();
            let got = resolve_next_candidate_id(&pool, now as i64);
            assert_eq!(&got, want, "cursor {cursor}: next = {got}, want {want}");
        }
    }

    #[tokio::test]
    async fn list_and_quota_on_non_tty_use_go_default_width() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix() as i64;
        let frac1 = 0.8;
        let frac2 = 0.5;
        let frac3 = 0.2;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            name: "AccountOne".to_string(),
            email: "one@example.com".to_string(),
            status: "ready".to_string(),
            request_count: 10,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac1),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac1),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac1),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = crate::storage::Account {
            id: "acc_2".to_string(),
            name: "AccountTwo".to_string(),
            email: "two@example.com".to_string(),
            status: "ready".to_string(),
            request_count: 20,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac2),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac2),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac2),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc3 = crate::storage::Account {
            id: "acc_3".to_string(),
            name: "AccountThree".to_string(),
            email: "three@example.com".to_string(),
            status: "ready".to_string(),
            request_count: 30,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac3),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac3),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac3),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some("acc_1".to_string()),
            strategy: crate::config::STRATEGY_SMART.to_string(),
            accounts: vec![acc1, acc2, acc3],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        // 1. `list` on non-TTY
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["list"], "").await;
        assert_eq!(code, 0);
        let expected_list = "#  ACCOUNT ID  NAME          STATUS    HITS  5H QUOTA  WEEKLY QUOTA  LAST USED\n1  acc_1       AccountOne    CLI Base  10    80.0%     80.0%         never\n2  acc_2       AccountTwo    Ready     20    50.0%     50.0%         never\n3  acc_3       AccountThree  Ready     30    20.0%     20.0%         never\n\nStrategy: smart | CLI Base: acc_1 | Total: 3 account(s)\n";
        assert_eq!(stdout, expected_list);

        // 2. `quota` on non-TTY (width 100 default)
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota"], "").await;
        assert_eq!(code, 0);
        let clean_q = strip_ansi(&stdout);
        assert!(clean_q.contains("Antigravity Multi-Account Pool v"));
        assert!(clean_q.contains("AccountOne · CLI Base [Next ⚡]    hits 10 · quota age unknown"));
        assert!(clean_q.contains("5H  ████████████████░░░░  80%   N/A"));
        assert!(clean_q.contains("WK  ████████████████░░░░  80%   N/A"));
        assert!(clean_q.contains("AccountTwo · Ready                hits 20 · quota age unknown"));
        assert!(clean_q.contains("5H  ██████████░░░░░░░░░░  50%   N/A"));
        assert!(clean_q.contains("AccountThree · Ready              hits 30 · quota age unknown"));
        assert!(clean_q.contains("5H  ████░░░░░░░░░░░░░░░░  20%   N/A"));
        assert!(clean_q.contains("Strategy: smart | Gateway Proxy:"));
    }

    #[tokio::test]
    async fn test_quota_footer_dynamic_port() {
        let _env = crate::cli::test_support::cli_test_env();
        let pool = crate::storage::Pool {
            version: 1,
            strategy: "smart".to_string(),
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "TestAcc".to_string(),
                status: "ready".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        // 1. Bind an ephemeral port so is_port_listening returns true for custom_port
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let custom_port = listener.local_addr().unwrap().port();

        let render_output = |width: usize| -> String {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::Cursor::new(Vec::new());
            let mut io = Io {
                stdin: &mut stdin,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            list_accounts_with_width_and_port("", width, &mut io, false, Some(custom_port));
            strip_ansi(&String::from_utf8_lossy(&out))
        };

        // Wide layout (width 100) with explicit port
        let text_wide = render_output(100);
        assert!(
            text_wide.contains(&format!("RUNNING (127.0.0.1:{custom_port})")),
            "wide layout must display running custom port: {text_wide}"
        );

        // Medium narrow layout (width 62)
        let text_med = render_output(62);
        assert!(
            text_med.contains(&format!("RUNNING (:{custom_port})")),
            "medium layout must display short running custom port: {text_med}"
        );

        // Mobile layout (width 45)
        let text_mobile = render_output(45);
        assert!(
            text_mobile.contains(&format!("proxy up · :{custom_port}")),
            "mobile layout must display custom port: {text_mobile}"
        );

        // When custom_port is dropped / not listening
        drop(listener);
        let text_stopped = render_output(100);
        assert!(
            text_stopped.contains("Gateway Proxy: STOPPED"),
            "wide layout must display STOPPED when port is not listening: {text_stopped}"
        );

        // 2. Integration via run_cli with -p flag
        let l2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p2 = l2.local_addr().unwrap().port();
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["quota", "-p", &p2.to_string()], "").await;
        assert_eq!(code, 0);
        let text_cli = strip_ansi(&stdout);
        assert!(
            text_cli.contains(&format!("RUNNING (127.0.0.1:{p2})")),
            "run_cli quota -p must resolve custom port: {text_cli}"
        );
    }
}
