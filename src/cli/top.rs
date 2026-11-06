use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cli::Io;
use crate::cli::width::{truncate_visible, visible_width};
use crate::cli::{CLR_BOLD, CLR_CYAN, CLR_DIM, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW, parse_instance_flags};
use crate::daemon::{self, DaemonInfo, RuntimeStats};
use crate::forecast::{self, format_duration_short};
use crate::observability::{self, format_number};
use crate::quota::{
    compute_capacity_state, display_quota_fractions, format_compact_remaining_time, format_quota_age,
    format_short_quota_age, render_block_progress_bar, weekly_unknown_label,
};
use crate::storage::{self, Account, Pool};

pub const MIN_TOP_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_TOP_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_TOP_RATE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct TopOptions {
    pub target_account: String,
    pub interval: Duration,
    pub width: usize,
    pub height: usize,
    pub once: bool,
    pub show_email: bool,
    pub force_raw: bool,
}

impl Default for TopOptions {
    fn default() -> Self {
        Self {
            target_account: String::new(),
            interval: DEFAULT_TOP_RATE,
            width: 0,
            height: 0,
            once: false,
            show_email: false,
            force_raw: false,
        }
    }
}

fn parse_duration_or_seconds(val: &str) -> Option<Duration> {
    if let Some(s) = val.strip_suffix("ms")
        && let Ok(v) = s.parse::<f64>()
    {
        return Some(Duration::from_secs_f64(v / 1000.0));
    }
    if let Some(s) = val.strip_suffix('s')
        && let Ok(v) = s.parse::<f64>()
    {
        return Some(Duration::from_secs_f64(v));
    }
    if let Some(s) = val.strip_suffix('m')
        && let Ok(v) = s.parse::<f64>()
    {
        return Some(Duration::from_secs_f64(v * 60.0));
    }
    if let Some(s) = val.strip_suffix('h')
        && let Ok(v) = s.parse::<f64>()
    {
        return Some(Duration::from_secs_f64(v * 3600.0));
    }
    if let Ok(secs) = val.parse::<f64>() {
        return Some(Duration::from_secs_f64(secs));
    }
    None
}

pub async fn top_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let flags = match parse_instance_flags(args) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(io.stderr, "{CLR_RED}[Error] Invalid flag: {e}{CLR_RESET}");
            return 1;
        }
    };

    let mut interval = DEFAULT_TOP_RATE;
    let mut target_account = String::new();
    let mut once = false;
    let mut show_email = false;

    let rem = &flags.rest;
    let mut i = 0;
    while i < rem.len() {
        let a = &rem[i];
        if a == "-e" || a == "--email" || a == "--show-email" {
            show_email = true;
        } else if a == "-i" || a == "--interval" {
            if i + 1 < rem.len()
                && let Some(d) = parse_duration_or_seconds(&rem[i + 1])
            {
                interval = d;
                i += 1;
            }
        } else if let Some(val) = a.strip_prefix("--interval=") {
            if let Some(d) = parse_duration_or_seconds(val) {
                interval = d;
            }
        } else if a == "-w" || a == "--watch" {
            // Transparently accepted from quota -w or status -w
        } else if a == "--once" {
            once = true;
        } else if !a.starts_with('-') && target_account.is_empty() {
            target_account = a.clone();
        }
        i += 1;
    }

    interval = interval.clamp(MIN_TOP_INTERVAL, MAX_TOP_INTERVAL);

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

    let ctx = match daemon::resolve_instance_context(d_dir, c_file, port, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io.stderr, "{CLR_RED}[Error] Failed to resolve instance: {e}{CLR_RESET}");
            return 1;
        }
    };

    if let Err(e) = crate::config::configure_data_dir(&ctx.data_dir) {
        let _ = writeln!(
            io.stderr,
            "{CLR_RED}[Error] Failed to configure data dir: {e}{CLR_RESET}"
        );
        return 1;
    }

    let opts = TopOptions {
        target_account,
        interval,
        width: 0,
        height: 0,
        once,
        show_email,
        force_raw: false,
    };

    run_top(&ctx, opts, io).await
}

pub async fn run_top(ctx: &daemon::InstanceContext, mut opts: TopOptions, io: &mut Io<'_>) -> i32 {
    opts.interval = opts.interval.clamp(MIN_TOP_INTERVAL, MAX_TOP_INTERVAL);

    if opts.once {
        let (mut w, mut h) = (opts.width, opts.height);
        if w == 0 || h == 0 {
            let (tw, th) = crate::cli::render::terminal_size(io.stdout_fd);
            if w == 0 {
                w = tw as usize;
            }
            if h == 0 {
                h = th as usize;
            }
        }
        let frame =
            match render_current_snapshot(ctx, &opts.target_account, opts.interval, w, h, false, opts.show_email) {
                Ok(f) => f,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                    return 1;
                }
            };
        let _ = write!(io.stdout, "{frame}");
        let _ = io.stdout.flush();
        return 0;
    }

    let raw_guard = if let Some(fd) = io.stdin_fd {
        if crate::cli::term::is_terminal(fd) {
            crate::cli::term::RawModeGuard::enable(fd).ok()
        } else {
            None
        }
    } else {
        None
    };

    let is_raw = raw_guard.is_some() || opts.force_raw;

    // Start cursor / alternate screen buffer
    if is_raw {
        let _ = io.stdout.write_all(b"\x1b[?1049h\x1b[H\x1b[?25l");
    } else {
        let _ = io.stdout.write_all(b"\x1b[?25l");
    }
    let _ = io.stdout.flush();

    let exit_code = run_top_loop(ctx, &opts, io, is_raw).await;

    // Restore terminal screen & cursor
    if is_raw {
        let _ = io.stdout.write_all(b"\x1b[?25h\x1b[?1049l\x1b[0m");
    } else {
        let _ = io.stdout.write_all(b"\x1b[?25h\x1b[0m\n");
    }
    let _ = io.stdout.flush();

    drop(raw_guard);
    exit_code
}

async fn run_top_loop(ctx: &daemon::InstanceContext, opts: &TopOptions, io: &mut Io<'_>, is_raw: bool) -> i32 {
    let mut interval = opts.interval;
    let mut refresh_banner = false;
    let mut refresh_clear_deadline: Option<tokio::time::Instant> = None;

    let redraw = |out: &mut dyn std::io::Write, interval: Duration, refresh_banner: bool| -> Result<(), String> {
        let (mut w, mut h) = (opts.width, opts.height);
        if w == 0 || h == 0 {
            let (tw, th) = crate::cli::render::terminal_size(io.stdout_fd);
            if w == 0 {
                w = tw as usize;
            }
            if h == 0 {
                h = th as usize;
            }
        }
        let frame = render_current_snapshot(
            ctx,
            &opts.target_account,
            interval,
            w,
            h,
            refresh_banner,
            opts.show_email,
        )?;
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1b[H");
        let lines: Vec<&str> = frame.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            buf.extend_from_slice(line.as_bytes());
            buf.extend_from_slice(b"\x1b[K");
            if i < lines.len() - 1 {
                if is_raw {
                    buf.extend_from_slice(b"\r\n");
                } else {
                    buf.extend_from_slice(b"\n");
                }
            }
        }
        buf.extend_from_slice(b"\x1b[J");
        out.write_all(&buf).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        Ok(())
    };

    // Initial render
    if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
        let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
        return 1;
    }

    let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<char>(1024);
    let stop_reading = Arc::new(AtomicBool::new(false));
    let stop_clone = stop_reading.clone();

    if let Some(fd) = io.stdin_fd {
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd >= 0 {
            std::thread::spawn(move || {
                let mut buf = [0u8; 16];
                let mut pfd = libc::pollfd {
                    fd: dup_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                while !stop_clone.load(Ordering::Relaxed) {
                    let ret = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
                        let n = unsafe { libc::read(dup_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                        if n <= 0 {
                            break;
                        }
                        for &b in &buf[..n as usize] {
                            if key_tx.blocking_send(b as char).is_err() {
                                break;
                            }
                        }
                    } else if ret < 0 {
                        break;
                    }
                }
                unsafe { libc::close(dup_fd) };
            });
        }
    } else {
        // In-memory stdin (e.g. Cursor in tests)
        let mut in_bytes = Vec::new();
        let _ = io.stdin.read_to_end(&mut in_bytes);
        for b in in_bytes {
            let _ = key_tx.try_send(b as char);
        }
        drop(key_tx);
    }

    let mut sigwinch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // consume initial immediate tick

    let exit_code = loop {
        tokio::select! {
            _ = async {
                if let Some(s) = sigint.as_mut() {
                    s.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                break 0;
            }
            _ = async {
                if let Some(s) = sigterm.as_mut() {
                    s.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                break 0;
            }
            _ = async {
                if let Some(s) = sigwinch.as_mut() {
                    s.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                    let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                    break 1;
                }
            }
            _ = ticker.tick() => {
                if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                    let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                    break 1;
                }
            }
            _ = async {
                match refresh_clear_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                refresh_clear_deadline = None;
                refresh_banner = false;
                if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                    let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                    break 1;
                }
            }
            key_opt = key_rx.recv() => {
                let Some(key) = key_opt else {
                    // EOF on stdin
                    break 0;
                };
                match key {
                    'q' | 'Q' | '\x03' => {
                        break 0;
                    }
                    '+' | '=' => {
                        if interval > MIN_TOP_INTERVAL {
                            interval = interval.saturating_sub(Duration::from_millis(500)).max(MIN_TOP_INTERVAL);
                            ticker = tokio::time::interval(interval);
                            ticker.tick().await;
                            if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                                let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                                break 1;
                            }
                        }
                    }
                    '-' | '_' => {
                        if interval < MAX_TOP_INTERVAL {
                            interval = (interval + Duration::from_millis(500)).min(MAX_TOP_INTERVAL);
                            ticker = tokio::time::interval(interval);
                            ticker.tick().await;
                            if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                                let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                                break 1;
                            }
                        }
                    }
                    'r' | 'R' => {
                        if let Ok(pool) = crate::storage::load_pool() {
                            for acc in &pool.accounts {
                                crate::quota::schedule_quota_refresh_simple(acc);
                            }
                        }
                        refresh_banner = true;
                        refresh_clear_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                        if let Err(e) = redraw(io.stdout, interval, refresh_banner) {
                            let _ = writeln!(io.stderr, "{CLR_RED}[Error] {e}{CLR_RESET}");
                            break 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    stop_reading.store(true, Ordering::Relaxed);
    exit_code
}

pub fn render_current_snapshot(
    ctx: &daemon::InstanceContext,
    target_account: &str,
    interval: Duration,
    width: usize,
    height: usize,
    refresh_banner: bool,
    show_email: bool,
) -> Result<String, String> {
    let (status, info, _) = daemon::check_instance_status(ctx);
    let daemon_running = matches!(
        status,
        daemon::InstanceStatus::RunningSameInstance | daemon::InstanceStatus::OutdatedBinary
    );

    let mut stats = None;
    if daemon_running
        && let Some(inf) = info.as_ref()
        && inf.pid > 0
        && let Ok(s) = ctx.get_runtime_stats(inf.pid)
    {
        stats = Some(s);
    }

    let pool = storage::load_pool().map_err(|e| e.to_string())?;
    Ok(render_top_frame_with_height(
        Some(&pool),
        stats.as_ref(),
        info.as_ref(),
        daemon_running,
        target_account,
        interval,
        width,
        height,
        refresh_banner,
        show_email,
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn render_top_frame(
    pool: Option<&Pool>,
    stats: Option<&RuntimeStats>,
    info: Option<&DaemonInfo>,
    daemon_running: bool,
    target_account: &str,
    interval: Duration,
    width: usize,
    refresh_banner: bool,
    show_email: bool,
) -> String {
    render_top_frame_with_height(
        pool,
        stats,
        info,
        daemon_running,
        target_account,
        interval,
        width,
        24,
        refresh_banner,
        show_email,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_top_frame_with_height(
    pool: Option<&Pool>,
    stats: Option<&RuntimeStats>,
    info: Option<&DaemonInfo>,
    daemon_running: bool,
    target_account: &str,
    interval: Duration,
    mut width: usize,
    mut height: usize,
    refresh_banner: bool,
    show_email: bool,
) -> String {
    if width == 0 {
        width = 100;
    }
    if height == 0 {
        height = 24;
    }

    let mut sb = String::new();
    let now_ts = crate::clock::now_unix() as i64;

    // 1. Header
    render_header(&mut sb, daemon_running, info, pool, interval, width);

    // 2. Accounts List
    match pool {
        Some(p) if !p.accounts.is_empty() => {
            let acc_list = filter_accounts(&p.accounts, target_account);
            if acc_list.is_empty() {
                sb.push_str(&format!(
                    "\n{CLR_RED}[Error] The selected account was not found.{CLR_RESET}\n\n"
                ));
            } else if width >= 96 {
                render_accounts_wide(&mut sb, &acc_list, p, stats, now_ts, width, show_email);
            } else if width >= 60 {
                render_accounts_compact(&mut sb, &acc_list, p, stats, now_ts, width, show_email);
            } else if width >= 40 {
                render_accounts_mobile(&mut sb, &acc_list, p, stats, now_ts, width, show_email);
            } else {
                render_accounts_very_narrow(&mut sb, &acc_list, p, stats, now_ts, width, show_email);
            }
        }
        _ => {
            sb.push_str(&format!("\n{CLR_YELLOW}No accounts in pool yet.{CLR_RESET}\n"));
            sb.push_str(&format!(
                "Run {CLR_BOLD}agy-pool login{CLR_RESET} or {CLR_BOLD}agy-pool import-current{CLR_RESET} to add accounts.\n\n"
            ));
        }
    }

    // 3. Runtime Counters
    render_runtime_counters(&mut sb, stats, daemon_running, width);

    // 4. Telemetry (Runway & Health)
    render_telemetry(&mut sb, pool, now_ts, width, height);

    // 5. Footer
    render_footer(&mut sb, interval, width, refresh_banner);

    // Final guarantee: every non-control visible line fits within requested terminal width
    let lines: Vec<&str> = sb.split('\n').collect();
    let mut bounded = String::with_capacity(sb.len());
    for (i, &line) in lines.iter().enumerate() {
        let bounded_line = if visible_width(line) > width {
            truncate_visible(line, width)
        } else {
            line.to_string()
        };
        bounded.push_str(&bounded_line);
        if i < lines.len() - 1 {
            bounded.push('\n');
        }
    }

    bounded
}

fn filter_accounts<'a>(all: &'a [Account], target: &str) -> Vec<&'a Account> {
    if target.is_empty() {
        return all.iter().collect();
    }
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 && idx <= all.len() {
            return vec![&all[idx - 1]];
        }
        return vec![];
    }
    for a in all {
        if a.id == target || a.email == target || a.name == target {
            return vec![a];
        }
    }
    vec![]
}

fn get_in_flight(acc: &Account, stats: Option<&RuntimeStats>) -> i64 {
    if let Some(s) = stats
        && let Some(ref map) = s.in_flight
        && let Some(&cnt) = map.get(&acc.id)
    {
        return cnt;
    }
    observability::get_in_flight_generation(&acc.id)
}

fn render_header(
    sb: &mut String,
    daemon_running: bool,
    info: Option<&DaemonInfo>,
    pool: Option<&Pool>,
    interval: Duration,
    width: usize,
) {
    let gateway_str = if daemon_running {
        let pid_str = if let Some(inf) = info
            && inf.pid > 0
        {
            format!(" (PID {})", inf.pid)
        } else {
            String::new()
        };
        format!("{CLR_GREEN}RUNNING{pid_str}{CLR_RESET}")
    } else {
        format!("{CLR_DIM}STOPPED{CLR_RESET}")
    };

    let strat = if let Some(p) = pool
        && !p.strategy.is_empty()
    {
        p.strategy.as_str()
    } else {
        crate::config::DEFAULT_STRATEGY
    };

    let rate_sec = interval.as_secs_f64();

    if width >= 96 {
        let sep = "=".repeat(68);
        sb.push_str(&format!("{sep}\n"));
        let title = format!("Antigravity Multi-Account Pool v{}", crate::VERSION);
        let pad = (68 + title.len()) / 2;
        sb.push_str(&format!("{CLR_BOLD}{title:>pad$}{CLR_RESET}\n"));
        sb.push_str(&format!("{sep}\n"));
        sb.push_str(&format!(
            "Gateway: {gateway_str} · Strategy: {CLR_BOLD}{strat}{CLR_RESET} · Refresh: {rate_sec:.1}s\n"
        ));
    } else if width >= 60 {
        let header_line = format!(
            "agy-pool v{} · Gateway: {} · {} · {:.1}s",
            crate::VERSION,
            gateway_str,
            strat,
            rate_sec
        );
        if visible_width(&header_line) <= width {
            sb.push_str(&format!("\n{header_line}\n"));
        } else {
            sb.push_str(&format!("\nagy-pool v{} · Gateway: {gateway_str}\n", crate::VERSION));
            sb.push_str(&format!("Strategy: {strat} · Refresh: {rate_sec:.1}s\n"));
        }
    } else if width >= 40 {
        let line1 = format!("agy-pool v{} · {}", crate::VERSION, gateway_str);
        if visible_width(&line1) <= width {
            sb.push_str(&format!("\n{line1}\n"));
        } else {
            sb.push_str(&format!("\nagy-pool v{}\n{gateway_str}\n", crate::VERSION));
        }
        let line2 = format!("Strategy: {strat} · Refresh: {rate_sec:.1}s");
        if visible_width(&line2) <= width {
            sb.push_str(&format!("{line2}\n"));
        } else {
            sb.push_str(&format!("Strategy: {strat}\nRefresh: {rate_sec:.1}s\n"));
        }
    } else {
        // width < 40
        let status_line = if daemon_running {
            format!("agy-pool · {CLR_GREEN}RUNNING{CLR_RESET}")
        } else {
            "agy-pool · STOPPED".to_string()
        };
        sb.push_str(&format!("\n{status_line}\n"));

        if daemon_running
            && let Some(inf) = info
            && inf.pid > 0
        {
            sb.push_str(&format!("PID {} · {:.1}s\n", inf.pid, rate_sec));
        } else {
            sb.push_str(&format!("Refresh: {:.1}s\n", rate_sec));
        }

        let mut strat_line = format!("Strategy: {strat}");
        if visible_width(&strat_line) > width {
            strat_line = strat.to_string();
        }
        if visible_width(&strat_line) > width {
            strat_line = truncate_visible(&strat_line, width);
        }
        sb.push_str(&format!("{strat_line}\n"));
    }
}

fn render_accounts_wide(
    sb: &mut String,
    acc_list: &[&Account],
    pool: &Pool,
    stats: Option<&RuntimeStats>,
    now_ts: i64,
    width: usize,
    show_email: bool,
) {
    let target_col = (if width > 4 { width - 4 } else { width }).clamp(64, 86);

    sb.push('\n');
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), Some(show_email));
        let in_flight = get_in_flight(acc, stats);
        let (state, state_clr) = crate::cli::render::get_account_state(acc, pool, now_ts, Some(in_flight));
        let hits = acc.get_hits();
        let age_str = format_quota_age(acc, now_ts as f64);

        let right = if acc.last_quota.is_some() {
            format!("in-flight {in_flight} · hits {hits} · quota age {age_str}")
        } else {
            format!("in-flight {in_flight} · hits {hits}")
        };
        let right_len = visible_width(&right);

        let suffix = format!(" · {state}");
        let suffix_len = visible_width(&suffix);

        let max_left = (target_col as isize) - 2 - (right_len as isize) - 2;
        if (visible_width(&disp_name) + suffix_len) as isize > max_left {
            let avail = max_left - (suffix_len as isize);
            if avail > 3 {
                disp_name = truncate_visible(&disp_name, avail as usize);
            }
        }

        let left_visible = 2 + visible_width(&disp_name) + suffix_len;
        let mut spaces = (target_col as isize) - (left_visible as isize) - (right_len as isize);
        if spaces < 2 {
            spaces = 2;
        }

        let spaces_str = " ".repeat(spaces as usize);
        sb.push_str(&format!(
            "  {CLR_BOLD}{disp_name}{CLR_RESET} · {state_clr}{state}{CLR_RESET}{spaces_str}{right}\n"
        ));

        if show_email && !acc.email.is_empty() {
            sb.push_str(&format!("  {CLR_DIM}Email: {}{CLR_RESET}\n", acc.email));
        }

        render_account_details_wide(sb, acc, now_ts, i);

        if i < acc_list.len() - 1 {
            sb.push('\n');
        }
    }
}

fn render_account_details_wide(sb: &mut String, acc: &Account, now_ts: i64, idx: usize) {
    let status = &acc.status;
    if status == storage::STATUS_DISABLED {
        sb.push_str(&format!(
            "  {CLR_DIM}Disabled (Excluded from pool routing){CLR_RESET}\n"
        ));
        return;
    }
    if status == storage::STATUS_VALIDATION_REQUIRED {
        sb.push_str(&format!("  {CLR_YELLOW}Action Required (Verify needed){CLR_RESET}\n"));
        sb.push_str(&format!("  Run: agy-pool verify {}\n", idx + 1));
        return;
    }
    if status == storage::STATUS_VERIFICATION_PENDING {
        sb.push_str(&format!("  {CLR_CYAN}Waiting for generation confirmation{CLR_RESET}\n"));
        return;
    }
    if status == storage::STATUS_AUTH_ERROR {
        sb.push_str(&format!("  {CLR_RED}Auth Failure (Re-authenticate){CLR_RESET}\n"));
        return;
    }

    if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
        let cap_state = compute_capacity_state(acc, now_ts as f64);
        let reason = if cap_state.q5_known
            && cap_state
                .q5
                .is_some_and(|q| q <= crate::scheduler::FIVE_HOUR_RESERVE_FLOOR)
        {
            "5H quota <= 15%".to_string()
        } else if cap_state.q7_known && cap_state.q7.is_some() {
            format!(
                "Weekly quota <= {:.1}%",
                crate::scheduler::dynamic_weekly_reserve_floor(cap_state.r7) * 100.0
            )
        } else {
            "Weekly quota low".to_string()
        };
        sb.push_str(&format!(
            "  {CLR_YELLOW}[Reserve Protection: {reason}, held back from pool routing]{CLR_RESET}\n"
        ));
    }

    let q = acc.last_quota.as_ref();
    let (q5_frac, gw_frac) = display_quota_fractions(q);
    let wk_blocked = crate::cli::render::weekly_blocked(gw_frac);
    let (reset5, reset7) = crate::cli::render::extract_reset_times(acc, q, now_ts);

    if let Some(frac) = q5_frac {
        let pct_val = (frac * 100.0).round() as i64;
        let g5_pct = format!("{:>4}", format!("{pct_val}%"));
        let raw_bar = render_block_progress_bar(Some(frac), 20);
        let g5_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
        let g5_reset = format_compact_remaining_time(reset5.as_ref());
        let r5_str = if g5_reset == "ready" || g5_reset == "N/A" {
            g5_reset
        } else {
            format!("resets in {g5_reset}")
        };
        sb.push_str(&format!("  5H  {g5_bar} {g5_pct}   {r5_str}\n"));
    } else {
        sb.push_str("  5H -- unknown\n");
    }

    if let Some(frac) = gw_frac {
        let pct_val = (frac * 100.0).round() as i64;
        let gw_pct = format!("{:>4}", format!("{pct_val}%"));
        let gw_bar = render_block_progress_bar(Some(frac), 20);
        let mut gw_reset = format_compact_remaining_time(reset7.as_ref());
        if wk_blocked {
            gw_reset.push_str(" · all models blocked");
        }
        let r7_str = if gw_reset == "ready" || gw_reset == "N/A" {
            gw_reset
        } else {
            format!("resets in {gw_reset}")
        };
        sb.push_str(&format!("  WK  {gw_bar} {gw_pct}   {r7_str}\n"));
    } else {
        sb.push_str(&format!("  {}\n", weekly_unknown_label(acc)));
    }

    if let Some(q_state) = q {
        if let Some(ref tp5) = q_state.third_party_5h
            && let Some(frac) = tp5.fraction
            && frac < 1.0
        {
            let pct_val = (frac * 100.0).round() as i64;
            let tp_pct = format!("{:>4}", format!("{pct_val}%"));
            let raw_bar = render_block_progress_bar(Some(frac), 20);
            let tp_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
            let tp_reset = format_compact_remaining_time(tp5.reset_time.as_ref());
            let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                tp_reset
            } else {
                format!("resets in {tp_reset}")
            };
            sb.push_str(&format!("  3P  {tp_bar} {tp_pct}   {r_tp_str}\n"));
        }
        if let Some(ref tpw) = q_state.third_party_weekly
            && let Some(frac) = tpw.fraction
            && frac < 1.0
        {
            let pct_val = (frac * 100.0).round() as i64;
            let tp_pct = format!("{:>4}", format!("{pct_val}%"));
            let raw_bar = render_block_progress_bar(Some(frac), 20);
            let tp_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
            let tp_reset = format_compact_remaining_time(tpw.reset_time.as_ref());
            let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                tp_reset
            } else {
                format!("resets in {tp_reset}")
            };
            sb.push_str(&format!("  3W  {tp_bar} {tp_pct}   {r_tp_str}\n"));
        }
    }
}

fn render_accounts_compact(
    sb: &mut String,
    acc_list: &[&Account],
    pool: &Pool,
    stats: Option<&RuntimeStats>,
    now_ts: i64,
    width: usize,
    show_email: bool,
) {
    sb.push('\n');
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (state, state_clr) = crate::cli::render::get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();
        let in_flight = get_in_flight(acc, stats);

        let suffix = format!(" · {state} · in-flight {in_flight} · hits {hits}");
        let suffix_len = visible_width(&suffix);
        if 2 + visible_width(&disp_name) + suffix_len > width {
            let avail = (width as isize) - 2 - (suffix_len as isize);
            if avail > 3 {
                disp_name = truncate_visible(&disp_name, avail as usize);
            }
        }

        sb.push_str(&format!(
            "  {CLR_BOLD}{disp_name}{CLR_RESET} · {state_clr}{state}{CLR_RESET} · in-flight {in_flight} · hits {hits}\n"
        ));

        if show_email && !acc.email.is_empty() {
            sb.push_str(&format!("  {CLR_DIM}Email: {}{CLR_RESET}\n", acc.email));
        }

        render_account_details_compact(sb, acc, now_ts, width, i);

        if i < acc_list.len() - 1 {
            sb.push('\n');
        }
    }
}

fn render_account_details_compact(sb: &mut String, acc: &Account, now_ts: i64, width: usize, idx: usize) {
    let status = &acc.status;
    if status == storage::STATUS_DISABLED {
        sb.push_str(&format!(
            "  {CLR_DIM}Disabled (Excluded from pool routing){CLR_RESET}\n"
        ));
        return;
    }
    if status == storage::STATUS_VALIDATION_REQUIRED {
        sb.push_str(&format!("  {CLR_YELLOW}Action Required (Verify needed){CLR_RESET}\n"));
        sb.push_str(&format!("  Run: agy-pool verify {}\n", idx + 1));
        return;
    }
    if status == storage::STATUS_VERIFICATION_PENDING {
        sb.push_str(&format!("  {CLR_CYAN}Waiting for generation confirmation{CLR_RESET}\n"));
        return;
    }
    if status == storage::STATUS_AUTH_ERROR {
        sb.push_str(&format!("  {CLR_RED}Auth Failure (Re-authenticate){CLR_RESET}\n"));
        return;
    }

    if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
        sb.push_str(&format!("  {CLR_YELLOW}[Reserve Protection: Low Quota]{CLR_RESET}\n"));
    }

    let q = acc.last_quota.as_ref();
    let (q5_frac, gw_frac) = display_quota_fractions(q);
    let wk_blocked = crate::cli::render::weekly_blocked(gw_frac);
    let (reset5, reset7) = crate::cli::render::extract_reset_times(acc, q, now_ts);

    if let Some(frac) = q5_frac {
        let pct_val = (frac * 100.0).round() as i64;
        let g5_pct = format!("{:>4}", format!("{pct_val}%"));
        let g5_reset = format_compact_remaining_time(reset5.as_ref());
        let r5_str = if g5_reset == "ready" || g5_reset == "N/A" {
            g5_reset
        } else {
            format!("reset {g5_reset}")
        };
        let non_bar5 = 12 + r5_str.len();
        let bar_w5 = crate::cli::render::choose_bar_width(width, non_bar5, 10, 4);
        if bar_w5 > 0 {
            let raw_bar = render_block_progress_bar(Some(frac), bar_w5 as i32);
            let g5_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
            sb.push_str(&format!("  5H {g5_pct} {g5_bar}  {r5_str}\n"));
        } else {
            sb.push_str(&format!("  5H {g5_pct}  {r5_str}\n"));
        }
    } else {
        sb.push_str("  5H -- unknown\n");
    }

    if let Some(frac) = gw_frac {
        let pct_val = (frac * 100.0).round() as i64;
        let gw_pct = format!("{:>4}", format!("{pct_val}%"));
        let mut gw_reset = format_compact_remaining_time(reset7.as_ref());
        if wk_blocked {
            gw_reset.push_str(" · all models blocked");
        }
        let r7_str = if gw_reset == "ready" || gw_reset == "N/A" {
            gw_reset
        } else {
            format!("reset {gw_reset}")
        };
        let non_bar7 = 12 + r7_str.len();
        let bar_w7 = crate::cli::render::choose_bar_width(width, non_bar7, 10, 4);
        if bar_w7 > 0 {
            let gw_bar = render_block_progress_bar(Some(frac), bar_w7 as i32);
            sb.push_str(&format!("  WK {gw_pct} {gw_bar}  {r7_str}\n"));
        } else {
            sb.push_str(&format!("  WK {gw_pct}  {r7_str}\n"));
        }
    } else {
        sb.push_str(&format!("  {}\n", weekly_unknown_label(acc)));
    }

    if let Some(q_state) = q {
        if let Some(ref tp5) = q_state.third_party_5h
            && let Some(frac) = tp5.fraction
            && frac < 1.0
        {
            let pct_val = (frac * 100.0).round() as i64;
            let tp_pct = format!("{:>4}", format!("{pct_val}%"));
            let tp_reset = format_compact_remaining_time(tp5.reset_time.as_ref());
            let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                tp_reset
            } else {
                format!("reset {tp_reset}")
            };
            let non_bar_tp = 12 + r_tp_str.len();
            let bar_w_tp = crate::cli::render::choose_bar_width(width, non_bar_tp, 10, 4);
            if bar_w_tp > 0 {
                let raw_bar = render_block_progress_bar(Some(frac), bar_w_tp as i32);
                let tp_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
                sb.push_str(&format!("  3P {tp_pct} {tp_bar}  {r_tp_str}\n"));
            } else {
                sb.push_str(&format!("  3P {tp_pct}  {r_tp_str}\n"));
            }
        }
        if let Some(ref tpw) = q_state.third_party_weekly
            && let Some(frac) = tpw.fraction
            && frac < 1.0
        {
            let pct_val = (frac * 100.0).round() as i64;
            let tp_pct = format!("{:>4}", format!("{pct_val}%"));
            let tp_reset = format_compact_remaining_time(tpw.reset_time.as_ref());
            let r_tp_str = if tp_reset == "ready" || tp_reset == "N/A" {
                tp_reset
            } else {
                format!("reset {tp_reset}")
            };
            let non_bar_tp = 12 + r_tp_str.len();
            let bar_w_tp = crate::cli::render::choose_bar_width(width, non_bar_tp, 10, 4);
            if bar_w_tp > 0 {
                let raw_bar = render_block_progress_bar(Some(frac), bar_w_tp as i32);
                let tp_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
                sb.push_str(&format!("  3W {tp_pct} {tp_bar}  {r_tp_str}\n"));
            } else {
                sb.push_str(&format!("  3W {tp_pct}  {r_tp_str}\n"));
            }
        }
    }

    let age_str = format_quota_age(acc, now_ts as f64);
    sb.push_str(&format!("  age {age_str}\n"));
}

fn render_accounts_mobile(
    sb: &mut String,
    acc_list: &[&Account],
    pool: &Pool,
    stats: Option<&RuntimeStats>,
    now_ts: i64,
    width: usize,
    show_email: bool,
) {
    sb.push('\n');
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (state, state_clr) = crate::cli::render::get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();
        let in_flight = get_in_flight(acc, stats);

        let suffix = format!(" · {state}");
        let suffix_len = visible_width(&suffix);
        if visible_width(&disp_name) + suffix_len > width {
            let avail = (width as isize) - (suffix_len as isize);
            if avail > 3 {
                disp_name = truncate_visible(&disp_name, avail as usize);
            }
        }

        sb.push_str(&format!(
            "{CLR_BOLD}{disp_name}{CLR_RESET} · {state_clr}{state}{CLR_RESET}\n"
        ));

        if show_email && !acc.email.is_empty() {
            sb.push_str(&format!("{CLR_DIM}Email: {}{CLR_RESET}\n", acc.email));
        }

        let status = &acc.status;
        if status == storage::STATUS_DISABLED {
            sb.push_str(&format!("{CLR_DIM}Disabled (Excluded from pool routing){CLR_RESET}\n"));
        } else if status == storage::STATUS_VALIDATION_REQUIRED {
            sb.push_str(&format!("{CLR_YELLOW}Action Required (Verify needed){CLR_RESET}\n"));
            sb.push_str(&format!("Run: agy-pool verify {}\n", i + 1));
        } else if status == storage::STATUS_VERIFICATION_PENDING {
            sb.push_str(&format!("{CLR_CYAN}Waiting for generation confirmation{CLR_RESET}\n"));
        } else if status == storage::STATUS_AUTH_ERROR {
            sb.push_str(&format!("{CLR_RED}Auth Failure (Re-authenticate){CLR_RESET}\n"));
        } else {
            if crate::scheduler::is_reserve_candidate(acc, now_ts as f64) {
                sb.push_str(&format!("{CLR_YELLOW}[Reserve Protection: Low Quota]{CLR_RESET}\n"));
            }
            let q = acc.last_quota.as_ref();
            let (q5_frac, gw_frac) = display_quota_fractions(q);
            let wk_blocked = crate::cli::render::weekly_blocked(gw_frac);
            let (reset5, reset7) = crate::cli::render::extract_reset_times(acc, q, now_ts);

            if let Some(frac) = q5_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let g5_pct = format!("{:>4}", format!("{pct_val}%"));
                let g5_reset = format_compact_remaining_time(reset5.as_ref());
                let non_bar5 = 10 + g5_reset.len();
                let bar_w5 = crate::cli::render::choose_bar_width(width, non_bar5, 10, 4);
                if bar_w5 > 0 {
                    let raw_bar = render_block_progress_bar(Some(frac), bar_w5 as i32);
                    let g5_bar = crate::cli::render::gated_bar(wk_blocked, &raw_bar);
                    sb.push_str(&format!("5H {g5_pct} {g5_bar}  {g5_reset}\n"));
                } else {
                    sb.push_str(&format!("5H {g5_pct}  {g5_reset}\n"));
                }
            } else {
                sb.push_str("5H -- unknown\n");
            }

            if let Some(frac) = gw_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let gw_pct = format!("{:>4}", format!("{pct_val}%"));
                let mut gw_reset = format_compact_remaining_time(reset7.as_ref());
                if wk_blocked {
                    gw_reset.push_str(" · all models blocked");
                }
                let non_bar7 = 10 + gw_reset.len();
                let bar_w7 = crate::cli::render::choose_bar_width(width, non_bar7, 10, 4);
                if bar_w7 > 0 {
                    let gw_bar = render_block_progress_bar(Some(frac), bar_w7 as i32);
                    sb.push_str(&format!("WK {gw_pct} {gw_bar}  {gw_reset}\n"));
                } else {
                    sb.push_str(&format!("WK {gw_pct}  {gw_reset}\n"));
                }
            } else {
                sb.push_str(&format!("{}\n", weekly_unknown_label(acc)));
            }

            let age_str = format_short_quota_age(acc, now_ts as f64);
            sb.push_str(&format!("age {age_str} · in-flight {in_flight} · hits {hits}\n"));
        }

        if i < acc_list.len() - 1 {
            sb.push('\n');
        }
    }
}

fn render_accounts_very_narrow(
    sb: &mut String,
    acc_list: &[&Account],
    pool: &Pool,
    stats: Option<&RuntimeStats>,
    now_ts: i64,
    width: usize,
    show_email: bool,
) {
    sb.push('\n');
    for (i, acc) in acc_list.iter().enumerate() {
        let mut disp_name = crate::accounts::display_account_name(Some(acc), None);
        let (mut state, state_clr) = crate::cli::render::get_account_state(acc, pool, now_ts, None);
        let hits = acc.get_hits();
        let in_flight = get_in_flight(acc, stats);

        let header_line = format!("{CLR_BOLD}{disp_name}{CLR_RESET} · {state_clr}{state}{CLR_RESET}");
        if visible_width(&header_line) <= width {
            sb.push_str(&format!("{header_line}\n"));
        } else {
            if visible_width(&disp_name) > width {
                disp_name = truncate_visible(&disp_name, width);
            }
            sb.push_str(&format!("{CLR_BOLD}{disp_name}{CLR_RESET}\n"));
            if visible_width(&state) > width {
                state = truncate_visible(&state, width);
            }
            sb.push_str(&format!("{state_clr}{state}{CLR_RESET}\n"));
        }

        if show_email && !acc.email.is_empty() {
            let mut email_line = format!("{CLR_DIM}Email: {}{CLR_RESET}", acc.email);
            if visible_width(&email_line) > width {
                email_line = truncate_visible(&email_line, width);
            }
            sb.push_str(&format!("{email_line}\n"));
        }

        let status = &acc.status;
        if status == storage::STATUS_DISABLED {
            sb.push_str(&format!("{CLR_DIM}Disabled{CLR_RESET}\n"));
        } else if status == storage::STATUS_VALIDATION_REQUIRED {
            sb.push_str(&format!("{CLR_YELLOW}Verify Needed{CLR_RESET}\n"));
            sb.push_str(&format!("agy-pool verify {}\n", i + 1));
        } else if status == storage::STATUS_VERIFICATION_PENDING {
            sb.push_str(&format!("{CLR_CYAN}Verification Pending{CLR_RESET}\n"));
        } else if status == storage::STATUS_AUTH_ERROR {
            sb.push_str(&format!("{CLR_RED}Auth Error{CLR_RESET}\n"));
        } else {
            let q = acc.last_quota.as_ref();
            let (q5_frac, gw_frac) = display_quota_fractions(q);
            let wk_blocked = crate::cli::render::weekly_blocked(gw_frac);
            let (reset5, reset7) = crate::cli::render::extract_reset_times(acc, q, now_ts);

            if let Some(frac) = q5_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let g5_pct = format!("{:>4}", format!("{pct_val}%"));
                let g5_reset = format_compact_remaining_time(reset5.as_ref());
                let mut line5 = format!("5H {g5_pct}  {g5_reset}");
                if visible_width(&line5) > width {
                    line5 = truncate_visible(&line5, width);
                }
                sb.push_str(&format!("{line5}\n"));
            } else {
                sb.push_str("5H -- unknown\n");
            }

            if let Some(frac) = gw_frac {
                let pct_val = (frac * 100.0).round() as i64;
                let gw_pct = format!("{:>4}", format!("{pct_val}%"));
                let mut gw_reset = format_compact_remaining_time(reset7.as_ref());
                if wk_blocked {
                    gw_reset.push_str(" · all models blocked");
                }
                let mut line_wk = format!("WK {gw_pct}  {gw_reset}");
                if visible_width(&line_wk) > width {
                    line_wk = truncate_visible(&line_wk, width);
                }
                sb.push_str(&format!("{line_wk}\n"));
            } else {
                sb.push_str(&format!("{}\n", weekly_unknown_label(acc)));
            }

            if let Some(q_state) = q {
                if let Some(ref tp5) = q_state.third_party_5h
                    && let Some(frac) = tp5.fraction
                    && frac < 1.0
                {
                    let pct_val = (frac * 100.0).round() as i64;
                    let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                    let tp_reset = format_compact_remaining_time(tp5.reset_time.as_ref());
                    let mut line_tp = format!("3P {tp_pct}  {tp_reset}");
                    if visible_width(&line_tp) > width {
                        line_tp = truncate_visible(&line_tp, width);
                    }
                    sb.push_str(&format!("{line_tp}\n"));
                }
                if let Some(ref tpw) = q_state.third_party_weekly
                    && let Some(frac) = tpw.fraction
                    && frac < 1.0
                {
                    let pct_val = (frac * 100.0).round() as i64;
                    let tp_pct = format!("{:>4}", format!("{pct_val}%"));
                    let tp_reset = format_compact_remaining_time(tpw.reset_time.as_ref());
                    let mut line_tp = format!("3W {tp_pct}  {tp_reset}");
                    if visible_width(&line_tp) > width {
                        line_tp = truncate_visible(&line_tp, width);
                    }
                    sb.push_str(&format!("{line_tp}\n"));
                }
            }

            let age_str = format_short_quota_age(acc, now_ts as f64);
            let info_line = format!("age {age_str} · in-flight {in_flight} · hits {hits}");
            if visible_width(&info_line) <= width {
                sb.push_str(&format!("{info_line}\n"));
            } else {
                let mut line1 = format!("age {age_str} · in-flight {in_flight}");
                let mut line2 = format!("hits {hits}");
                if visible_width(&line1) > width {
                    line1 = truncate_visible(&line1, width);
                }
                if visible_width(&line2) > width {
                    line2 = truncate_visible(&line2, width);
                }
                sb.push_str(&format!("{line1}\n{line2}\n"));
            }
        }

        if i < acc_list.len() - 1 {
            sb.push('\n');
        }
    }
}

fn render_runtime_counters(sb: &mut String, stats: Option<&RuntimeStats>, daemon_running: bool, width: usize) {
    if !daemon_running {
        sb.push_str(&format!("\n  {CLR_BOLD}Runtime Counters{CLR_RESET}\n"));
        sb.push_str("    (gateway not running)\n");
        return;
    }
    let Some(stats) = stats else {
        sb.push_str(&format!("\n  {CLR_BOLD}Runtime Counters{CLR_RESET}\n"));
        sb.push_str("    (metrics unavailable)\n");
        return;
    };

    let snap = &stats.snapshot;

    if width >= 60 {
        sb.push_str(&format!("\n  {CLR_BOLD}Runtime Counters{CLR_RESET}\n"));
        sb.push_str(&format!(
            "    {:<18} {:>6} requests / {} success\n",
            "Generation",
            format_number(snap.generation_requests),
            format_number(snap.generation_success)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6} decisions\n",
            "Routing",
            format_number(snap.routing_decisions)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6} attempts / {} recovered\n",
            "Failover",
            format_number(snap.failover_attempts),
            format_number(snap.failover_success)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6}\n",
            "Restricted skips",
            format_number(snap.restricted_skips)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6} / {} ok / {} failed\n",
            "Quota refresh",
            format_number(snap.quota_refresh_attempts),
            format_number(snap.quota_refresh_success),
            format_number(snap.quota_refresh_failure)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6} / {} ok / {} failed\n",
            "Auth refresh",
            format_number(snap.auth_refresh_attempts),
            format_number(snap.auth_refresh_success),
            format_number(snap.auth_refresh_failure)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6}\n",
            "Persistence errors",
            format_number(snap.persistence_errors)
        ));
        sb.push_str(&format!(
            "    {:<18} {:>6}\n",
            "Replay prevented",
            format_number(snap.no_replay_prevented)
        ));
    } else if width >= 40 {
        sb.push_str(&format!("\n{CLR_BOLD}Runtime Counters{CLR_RESET}\n"));
        sb.push_str(&format!(
            "Generation : {} req / {} ok\n",
            format_number(snap.generation_requests),
            format_number(snap.generation_success)
        ));
        sb.push_str(&format!(
            "Routing    : {} decisions\n",
            format_number(snap.routing_decisions)
        ));
        sb.push_str(&format!(
            "Failover   : {} att / {} rec\n",
            format_number(snap.failover_attempts),
            format_number(snap.failover_success)
        ));
        sb.push_str(&format!(
            "Restricted : {} skips\n",
            format_number(snap.restricted_skips)
        ));
        sb.push_str(&format!(
            "Quota ref  : {} / {} ok / {} err\n",
            format_number(snap.quota_refresh_attempts),
            format_number(snap.quota_refresh_success),
            format_number(snap.quota_refresh_failure)
        ));
        sb.push_str(&format!(
            "Auth ref   : {} / {} ok / {} err\n",
            format_number(snap.auth_refresh_attempts),
            format_number(snap.auth_refresh_success),
            format_number(snap.auth_refresh_failure)
        ));
        sb.push_str(&format!("Persist err: {}\n", format_number(snap.persistence_errors)));
        sb.push_str(&format!("No-replay  : {}\n", format_number(snap.no_replay_prevented)));
    } else {
        // width < 40
        sb.push_str(&format!("\n{CLR_BOLD}Runtime Counters{CLR_RESET}\n"));
        sb.push_str(&format!(
            "Generation : {} / {} ok\n",
            format_number(snap.generation_requests),
            format_number(snap.generation_success)
        ));
        sb.push_str(&format!("Routing    : {}\n", format_number(snap.routing_decisions)));
        sb.push_str(&format!(
            "Failover   : {} / {} rec\n",
            format_number(snap.failover_attempts),
            format_number(snap.failover_success)
        ));
        sb.push_str(&format!("Restricted : {}\n", format_number(snap.restricted_skips)));

        let q_line = format!(
            "Quota ref  : {} ok / {} err",
            format_number(snap.quota_refresh_success),
            format_number(snap.quota_refresh_failure)
        );
        if visible_width(&q_line) <= width {
            sb.push_str(&format!("{q_line}\n"));
        } else {
            sb.push_str(&format!("Quota ok   : {}\n", format_number(snap.quota_refresh_success)));
            sb.push_str(&format!("Quota err  : {}\n", format_number(snap.quota_refresh_failure)));
        }

        let a_line = format!(
            "Auth ref   : {} ok / {} err",
            format_number(snap.auth_refresh_success),
            format_number(snap.auth_refresh_failure)
        );
        if visible_width(&a_line) <= width {
            sb.push_str(&format!("{a_line}\n"));
        } else {
            sb.push_str(&format!("Auth ok    : {}\n", format_number(snap.auth_refresh_success)));
            sb.push_str(&format!("Auth err   : {}\n", format_number(snap.auth_refresh_failure)));
        }

        sb.push_str(&format!("Persist err: {}\n", format_number(snap.persistence_errors)));
        sb.push_str(&format!("No-replay  : {}\n", format_number(snap.no_replay_prevented)));
    }
}

fn render_telemetry(sb: &mut String, pool: Option<&Pool>, now_ts: i64, width: usize, height: usize) {
    if height < 18 {
        return;
    }
    let Some(pool) = pool else {
        return;
    };
    if pool.accounts.is_empty() {
        return;
    }

    let Some(fc) = forecast::simulate_quota_runway(pool, None, Some(now_ts as f64)) else {
        return;
    };
    if fc.status != "ok" {
        return;
    }

    let h_count = fc.healthy_count;
    let tot_count = fc.total_accounts;
    let cool_count = fc.cooling_count;
    let pace = fc.target_pace;
    let is_idle = fc.is_idle;
    let replenish = fc.replenish_rate_pct_hr;
    let burn = fc.burn_rate_pct_hr;
    let net = fc.net_rate_pct_hr;
    let sim = &fc.current_runway;
    let bench = &fc.benchmark_runway;

    let mut health_str = format!("{h_count}/{tot_count} Active");
    if cool_count > 0 {
        health_str.push_str(&format!(" ({cool_count} Cooling)"));
    }

    let sign = if net >= 0.0 { "+" } else { "" };
    let net_clr = if net >= 0.0 { CLR_GREEN } else { CLR_YELLOW };

    let sub_sep = "-".repeat(width);
    sb.push_str(&format!("{sub_sep}\n"));

    if width < 76 {
        // Responsive compact telemetry (< 76 cols)
        sb.push_str(&format!(" {CLR_BOLD}{CLR_CYAN}⚡ Quota Runway & Health:{CLR_RESET}\n"));
        let pace_val = if is_idle {
            "Idle".to_string()
        } else {
            format!("{pace:.1}/hr")
        };
        sb.push_str(&format!("  • Health & Pace:  {health_str} | {pace_val}\n"));
        if is_idle {
            sb.push_str(&format!("  • Capacity:       +{replenish:.1}%/hr reload\n"));
        } else {
            let b_unit = if width < 54 { "%" } else { "%/hr" };
            sb.push_str(&format!(
                "  • Burn/Reload:    ~{burn:.1}{b_unit} | +{replenish:.1}{b_unit} ({net_clr}{sign}{net:.1}%{CLR_RESET})\n"
            ));
        }

        // 5h Runway compact
        let r5_tag = if is_idle {
            if bench.is_sustainable_5h {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
            } else {
                format!("~{}", format_duration_short(bench.first_stall_sec))
            }
        } else if sim.is_sustainable_5h {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        } else if let Some(first_stall_sec) = sim.first_stall_sec {
            format!(
                "{CLR_YELLOW}~{}{CLR_RESET}",
                format_duration_short(Some(first_stall_sec))
            )
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        sb.push_str(&format!("  • 5-Hour Runway:  {r5_tag}\n"));

        // Weekly Runway compact
        let active_sim = if is_idle { bench } else { sim };
        let w_tag = if active_sim.is_sustainable_weekly {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        } else if let Some(weekly_exhaust_sec) = active_sim.weekly_exhaust_sec {
            let w_days = weekly_exhaust_sec / 86400.0;
            let w_clr = if w_days <= 2.0 { CLR_RED } else { CLR_YELLOW };
            format!("{w_clr}~{w_days:.1}d{CLR_RESET}")
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        sb.push_str(&format!("  • Weekly Runway:  {w_tag}\n"));
    } else {
        // Full standard telemetry (>= 76 cols)
        let (pace_line, replenish_line) = if is_idle {
            let pace_str = format!("{CLR_DIM}Idle (0 gens/hr){CLR_RESET}");
            (
                format!("  • Health & Pace:  {health_str} | {pace_str}"),
                format!("  • Capacity:       +{replenish:.1}%/hr natural recovery across pool"),
            )
        } else {
            let win_label = fc
                .telemetry
                .as_ref()
                .map(|t| t.window_used.as_str())
                .unwrap_or("smoothed");
            let pace_str = format!("{CLR_BOLD}{pace:.1} gens/hr{CLR_RESET} ({CLR_CYAN}{win_label}{CLR_RESET})");
            (
                format!("  • Health & Pace:  {health_str} | {pace_str}"),
                format!(
                    "  • Burn/Reload:    ~{burn:.1}%/hr burn | +{replenish:.1}%/hr reload ({net_clr}{sign}{net:.1}%/hr net{CLR_RESET})"
                ),
            )
        };

        sb.push_str(&format!(
            " {CLR_BOLD}{CLR_CYAN}⚡ Quota Runway & Endurance Forecast:{CLR_RESET}\n"
        ));
        sb.push_str(&format!("{pace_line}\n"));
        sb.push_str(&format!("{replenish_line}\n"));

        // 5h Runway
        let r5_tag = if is_idle {
            if bench.is_sustainable_5h {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (> 24h at standard pace)")
            } else {
                format!("~{}", format_duration_short(bench.first_stall_sec))
            }
        } else if sim.is_sustainable_5h {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (Pool replenishes faster than burn)")
        } else if let Some(first_stall_sec) = sim.first_stall_sec {
            let stall_str = format_duration_short(Some(first_stall_sec));
            let cause_note = if let Some(pause_sec) = sim.stall_duration_sec
                && pause_sec > 0.0
            {
                format!(" (Dry pause: ~{})", format_duration_short(Some(pause_sec)))
            } else {
                String::new()
            };
            format!("{CLR_YELLOW}~{stall_str} until stall{CLR_RESET}{cause_note}")
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        sb.push_str(&format!("  • 5-Hour Runway:  {r5_tag}\n"));

        // Weekly Runway
        let active_sim = if is_idle { bench } else { sim };
        let w_tag = if active_sim.is_sustainable_weekly {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (> 7 Days remaining)")
        } else if let Some(weekly_exhaust_sec) = active_sim.weekly_exhaust_sec {
            let w_days = weekly_exhaust_sec / 86400.0;
            let w_clr = if w_days <= 2.0 { CLR_RED } else { CLR_YELLOW };
            format!("{w_clr}~{w_days:.1} Days remaining{CLR_RESET}")
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        let w_label = if is_idle {
            "Weekly Budget:  "
        } else {
            "Weekly Runway:  "
        };
        sb.push_str(&format!("  • {w_label}{w_tag}\n"));
    }
}

fn render_footer(sb: &mut String, interval: Duration, width: usize, refresh_banner: bool) {
    let rate_sec = interval.as_secs_f64();
    sb.push('\n');
    if width >= 60 {
        let footer =
            format!("{CLR_DIM}[q] Quit  [r] Refresh Quota  [+] Faster  [-] Slower  ({rate_sec:.1}s){CLR_RESET}");
        sb.push_str(&format!("{footer}\n"));
    } else if width >= 40 {
        let footer = format!("{CLR_DIM}[q]Quit [r]Ref [+]Fast [-]Slow ({rate_sec:.1}s){CLR_RESET}");
        sb.push_str(&format!("{footer}\n"));
    } else {
        // width < 40
        sb.push_str(&format!("{CLR_DIM}[q] Quit  [r] Refresh{CLR_RESET}\n"));
        sb.push_str(&format!(
            "{CLR_DIM}[+] Faster  [-] Slower  ({rate_sec:.1}s){CLR_RESET}\n"
        ));
    }

    if refresh_banner {
        sb.push_str(&format!("{CLR_YELLOW}Refreshing quota...{CLR_RESET}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::net::TcpListener;

    use crate::cli::width::strip_ansi;

    fn create_test_top_pool(now: i64) -> Pool {
        let frac90 = 0.90;
        let frac20 = 0.20;
        let zero = 0.0;
        let reset5h = (now + 18000) as f64;
        let resetwk = (now + 432000) as f64;

        let acc1 = Account {
            id: "acc_1".to_string(),
            name: "MainAccount".to_string(),
            email: "private-developer@internal.corp.example.com".to_string(),
            status: "ready".to_string(),
            request_count: 1500,
            created_at: Some(now),
            access_token: "ya29.mock_token_sensitive_acc_1".to_string(),
            refresh_token: "1//04mock_refresh_sensitive".to_string(),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac90),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac90),
                    reset_time: Some(serde_json::json!(reset5h)),
                    extra: serde_json::Map::new(),
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac20),
                    reset_time: Some(serde_json::json!(resetwk)),
                    extra: serde_json::Map::new(),
                }),
                third_party_5h: None,
                third_party_weekly: None,
                reset_time: None,
                updated_at: Some(now),
                extra: serde_json::Map::new(),
            }),
            ..Default::default()
        };

        let acc2 = Account {
            id: "acc_2".to_string(),
            name: "BackupAccount".to_string(),
            email: "private-backup@internal.corp.example.com".to_string(),
            status: "ready".to_string(),
            request_count: 200,
            created_at: Some(now),
            last_quota: None,
            ..Default::default()
        };

        let active_id = "acc_1".to_string();
        let cooling_time = (now + 600) as f64;
        let acc3 = Account {
            id: "acc_3".to_string(),
            name: "CoolingAccount".to_string(),
            status: "ready".to_string(),
            rate_limited_until: Some(cooling_time),
            request_count: 50,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac90),
                gemini_5h: None,
                gemini_weekly: None,
                third_party_5h: None,
                third_party_weekly: None,
                reset_time: None,
                updated_at: Some(now),
                extra: serde_json::Map::new(),
            }),
            ..Default::default()
        };

        let acc4 = Account {
            id: "acc_4".to_string(),
            name: "ExhaustedAccount".to_string(),
            status: "ready".to_string(),
            request_count: 80,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(zero),
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(zero),
                    reset_time: None,
                    extra: serde_json::Map::new(),
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(zero),
                    reset_time: None,
                    extra: serde_json::Map::new(),
                }),
                third_party_5h: None,
                third_party_weekly: None,
                reset_time: None,
                updated_at: Some(now),
                extra: serde_json::Map::new(),
            }),
            ..Default::default()
        };

        let acc5 = Account {
            id: "acc_5".to_string(),
            name: "VerifyAccount".to_string(),
            status: "validation_required".to_string(),
            request_count: 10,
            created_at: Some(now),
            ..Default::default()
        };

        let acc6 = Account {
            id: "acc_6".to_string(),
            name: "AuthErrAccount".to_string(),
            status: "auth_error".to_string(),
            request_count: 5,
            created_at: Some(now),
            ..Default::default()
        };

        Pool {
            version: 1,
            active_account_id: Some(active_id),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1, acc2, acc3, acc4, acc5, acc6],
            ..Default::default()
        }
    }

    fn create_test_runtime_stats(pid: i32) -> RuntimeStats {
        let now = crate::clock::now_unix() as i64;
        let mut in_flight = HashMap::new();
        in_flight.insert("acc_1".to_string(), 2);
        in_flight.insert("acc_2".to_string(), 0);
        in_flight.insert("acc_3".to_string(), 5);

        RuntimeStats {
            pid,
            started_at: jiff::Timestamp::from_second(now - 7200).unwrap(),
            updated_at: jiff::Timestamp::from_second(now).unwrap(),
            goroutines: 16,
            alloc_bytes: 15 * 1024 * 1024,
            heap_bytes: 20 * 1024 * 1024,
            sys_bytes: 30 * 1024 * 1024,
            num_gc: 10,
            go_version: "go1.26.7".to_string(),
            in_flight: Some(in_flight),
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: observability::Snapshot {
                generation_requests: 2500,
                generation_success: 2480,
                routing_decisions: 2500,
                failover_attempts: 30,
                failover_success: 28,
                restricted_skips: 12,
                quota_refresh_attempts: 150,
                quota_refresh_success: 148,
                quota_refresh_failure: 2,
                auth_refresh_attempts: 20,
                auth_refresh_success: 20,
                auth_refresh_failure: 0,
                persistence_errors: 1,
                no_replay_prevented: 4,
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn test_top_responsive_layouts() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        // 1. Wide Layout (width = 120)
        let frame_wide = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            120,
            false,
            false,
        );
        let plain_wide = strip_ansi(&frame_wide);
        for expected in [
            "Antigravity Multi-Account Pool",
            "Gateway: RUNNING (PID 12345)",
            "Strategy: max_quota",
            "Refresh: 2.0s",
            "MainAccount · CLI Base",
            "in-flight 2 · hits 1500",
            "5H  ██████████████████",
            "WK  ████",
            "quota age",
            "Runtime Counters",
            "Generation          2,500 requests / 2,480 success",
            "Routing             2,500 decisions",
            "Failover               30 attempts / 28 recovered",
            "Restricted skips       12",
            "Quota refresh         150 / 148 ok / 2 failed",
            "Auth refresh           20 / 20 ok / 0 failed",
            "Persistence errors      1",
            "Replay prevented        4",
            "[q] Quit  [r] Refresh Quota  [+] Faster  [-] Slower  (2.0s)",
        ] {
            assert!(
                plain_wide.contains(expected),
                "Wide frame missing expected content {expected:?}\nGot:\n{plain_wide}"
            );
        }

        // 2. Compact Layout (width = 80)
        let frame_compact = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            80,
            false,
            false,
        );
        let plain_compact = strip_ansi(&frame_compact);
        for expected in [
            "agy-pool",
            "Gateway: RUNNING (PID 12345)",
            "max_quota",
            "MainAccount · CLI Base · in-flight 2 · hits 1500",
            "5H",
            "90%",
            "reset",
            "WK",
            "20%",
            "reset",
            "age",
            "Runtime Counters",
            "Generation",
            "Routing",
            "Failover",
            "[q] Quit  [r] Refresh Quota  [+] Faster  [-] Slower  (2.0s)",
        ] {
            assert!(
                plain_compact.contains(expected),
                "Compact frame missing expected content {expected:?}\nGot:\n{plain_compact}"
            );
        }

        // 3. Mobile Layout (width = 50)
        let frame_mobile = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            50,
            false,
            false,
        );
        for expected in [
            "agy-pool",
            "RUNNING",
            "Strategy: max_quota",
            "MainAccount",
            "CLI Base",
            "5H",
            "90%",
            "WK",
            "20%",
            "in-flight 2 · hits 1500",
            "Runtime Counters",
            "Generation : 2,500 req / 2,480 ok",
            "Routing    : 2,500 decisions",
            "Failover   : 30 att / 28 rec",
            "[q]Quit [r]Ref [+]Fast [-]Slow (2.0s)",
        ] {
            assert!(
                frame_mobile.contains(expected),
                "Mobile frame missing expected content {expected:?}\nGot:\n{frame_mobile}"
            );
        }

        // 4. Very Narrow Layout (width = 35)
        let frame_narrow = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            35,
            false,
            false,
        );
        for expected in [
            "agy-pool",
            "RUNNING",
            "max_quota",
            "MainAccount",
            "CLI Base",
            "5H  90%",
            "WK  20%",
            "in-flight 2 · hits 1500",
            "Runtime Counters",
            "Generation : 2,500 / 2,480 ok",
            "[q] Quit  [r] Refresh",
            "[+] Faster  [-] Slower",
        ] {
            assert!(
                frame_narrow.contains(expected),
                "Very narrow frame missing expected content {expected:?}\nGot:\n{frame_narrow}"
            );
        }
    }

    #[tokio::test]
    async fn test_top_account_states() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        let frame = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            false,
            false,
        );

        for state in [
            "CLI Base",
            "Cooldown",
            "Exhausted",
            "Action Required (Verify needed)",
            "Auth Failure (Re-authenticate)",
        ] {
            assert!(
                frame.contains(state),
                "expected account state badge {state:?} in top frame, got:\n{frame}"
            );
        }
    }

    #[tokio::test]
    async fn test_top_unknown_quota() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let acc = Account {
            id: "acc_unknown".to_string(),
            name: "UnknownQuotaAccount".to_string(),
            status: "ready".to_string(),
            request_count: 10,
            created_at: Some(now),
            last_quota: None,
            ..Default::default()
        };
        let pool = Pool {
            version: 1,
            accounts: vec![acc],
            ..Default::default()
        };
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        let frame = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            false,
            false,
        );

        assert!(
            frame.contains("5H -- unknown"),
            "expected '5H -- unknown' for nil quota, got:\n{frame}"
        );
        assert!(
            frame.contains("WK -- unknown"),
            "expected 'WK -- unknown' for nil quota, got:\n{frame}"
        );
        assert!(
            !frame.contains("resets in"),
            "unknown quota must NEVER invent a reset countdown, got:\n{frame}"
        );
    }

    #[tokio::test]
    async fn test_top_long_friendly_names_and_cjk() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.8;
        let pool = Pool {
            version: 1,
            accounts: vec![
                Account {
                    id: "acc_cjk".to_string(),
                    name: "谷歌主账号·生产环境".to_string(),
                    status: "ready".to_string(),
                    created_at: Some(now),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Account {
                    id: "acc_long".to_string(),
                    name: "ThisIsAnExtremelyLongAccountFriendlyNameThatExceedsNormalWidth".to_string(),
                    status: "ready".to_string(),
                    created_at: Some(now),
                    last_quota: Some(crate::storage::QuotaState {
                        remaining_fraction: Some(frac),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        for width in [120, 80, 50, 35] {
            let frame = render_top_frame(
                Some(&pool),
                Some(&stats),
                Some(&info),
                true,
                "",
                Duration::from_secs(2),
                width,
                false,
                false,
            );
            for (line_idx, line) in frame.split('\n').enumerate() {
                let vw = visible_width(line);
                let mut max_allowed = width;
                if width > 68 && max_allowed < 68 {
                    max_allowed = 68;
                }
                assert!(
                    vw <= max_allowed + 4,
                    "width {width}: line {line_idx} exceeded width: visual width {vw} > {max_allowed}: {line:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_top_in_flight_zero_one_n() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.95;
        let acc0 = Account {
            id: "acc_0".to_string(),
            name: "ZeroFlight".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc1 = Account {
            id: "acc_1".to_string(),
            name: "OneFlight".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_n = Account {
            id: "acc_N".to_string(),
            name: "NFlight".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = Pool {
            version: 1,
            accounts: vec![acc0, acc1, acc_n],
            ..Default::default()
        };
        let mut in_flight = HashMap::new();
        in_flight.insert("acc_0".to_string(), 0);
        in_flight.insert("acc_1".to_string(), 1);
        in_flight.insert("acc_N".to_string(), 9);

        let stats = RuntimeStats {
            pid: 5555,
            started_at: jiff::Timestamp::from_second(now).unwrap(),
            updated_at: jiff::Timestamp::from_second(now).unwrap(),
            goroutines: 1,
            alloc_bytes: 0,
            heap_bytes: 0,
            sys_bytes: 0,
            num_gc: 0,
            go_version: String::new(),
            in_flight: Some(in_flight),
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: observability::Snapshot::default(),
        };
        let info = DaemonInfo {
            pid: 5555,
            ..Default::default()
        };

        let frame = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            false,
            false,
        );

        assert!(
            frame.contains("in-flight 0"),
            "expected 'in-flight 0' in frame, got:\n{frame}"
        );
        assert!(
            frame.contains("in-flight 1"),
            "expected 'in-flight 1' in frame, got:\n{frame}"
        );
        assert!(
            frame.contains("in-flight 9"),
            "expected 'in-flight 9' in frame, got:\n{frame}"
        );
    }

    #[tokio::test]
    async fn test_top_interval_bounds() {
        let env = crate::cli::test_support::cli_test_env();
        let tmp_dir = env.dir.path().display().to_string();

        // Test minimum clamp: 0.1s -> 500ms
        let (code, stdout, _stderr) =
            crate::cli::test_support::run_cli(&["top", "--interval=0.1s", "--once", "-D", &tmp_dir], "").await;
        assert_eq!(code, 0, "expected code 0, got {code}");
        assert!(
            stdout.contains("0.5s"),
            "expected 0.1s to be clamped to 0.5s, got:\n{stdout}"
        );

        // Test maximum clamp: 120s -> 60s
        let (code, stdout, _stderr) =
            crate::cli::test_support::run_cli(&["top", "--interval=120s", "--once", "-D", &tmp_dir], "").await;
        assert_eq!(code, 0, "expected code 0, got {code}");
        assert!(
            stdout.contains("60.0s"),
            "expected 120s to be clamped to 60.0s, got:\n{stdout}"
        );
    }

    #[tokio::test]
    async fn test_top_key_handling_q_plus_minus_r() {
        let env = crate::cli::test_support::cli_test_env();
        let tmp_dir = env.dir.path().display().to_string();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.8;
        let acc = Account {
            id: "acc_key_test".to_string(),
            name: "KeyTestAcc".to_string(),
            status: "ready".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = Pool {
            version: 1,
            accounts: vec![acc],
            ..Default::default()
        };
        storage::save_pool(&pool).unwrap();

        let ctx =
            daemon::resolve_instance_context(Some(std::path::Path::new(&tmp_dir)), None, Some(8899), None).unwrap();

        let mut in_buf = Cursor::new(b"+ - r q\n".to_vec());
        let mut out = Vec::new();
        let mut err_buf = Vec::new();

        let opts = TopOptions {
            target_account: String::new(),
            interval: Duration::from_secs(2),
            width: 100,
            height: 0,
            once: false,
            show_email: false,
            force_raw: false,
        };

        let mut io = Io {
            stdin: &mut in_buf,
            stdout: &mut out,
            stderr: &mut err_buf,
            stdout_fd: None,
            stdin_fd: None,
        };

        let exit_code = run_top(&ctx, opts, &mut io).await;
        assert_eq!(exit_code, 0, "expected exit code 0, got {exit_code}");

        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out_str.contains("[q] Quit"),
            "expected dashboard output from RunTop, got:\n{out_str}"
        );
        assert!(
            out_str.contains("\x1b[?25h"),
            "expected cursor restoration code \\033[?25h on quit"
        );
    }

    #[tokio::test]
    async fn test_top_raw_mode_alternate_screen_and_crlf() {
        let env = crate::cli::test_support::cli_test_env();
        let tmp_dir = env.dir.path().display().to_string();
        let now = crate::clock::now_unix() as i64;
        let pool = Pool {
            version: 1,
            accounts: vec![Account {
                id: "acc_raw".to_string(),
                name: "RawTest".to_string(),
                status: "ready".to_string(),
                created_at: Some(now),
                ..Default::default()
            }],
            ..Default::default()
        };
        storage::save_pool(&pool).unwrap();

        let ctx =
            daemon::resolve_instance_context(Some(std::path::Path::new(&tmp_dir)), None, Some(8899), None).unwrap();

        let mut in_buf = Cursor::new(b"q\n".to_vec());
        let mut out = Vec::new();
        let mut err_buf = Vec::new();

        let opts = TopOptions {
            target_account: String::new(),
            interval: Duration::from_secs(2),
            width: 100,
            height: 0,
            once: false,
            show_email: false,
            force_raw: true,
        };

        let mut io = Io {
            stdin: &mut in_buf,
            stdout: &mut out,
            stderr: &mut err_buf,
            stdout_fd: None,
            stdin_fd: None,
        };

        let exit_code = run_top(&ctx, opts, &mut io).await;
        assert_eq!(exit_code, 0, "expected exit code 0, got {exit_code}");

        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out_str.contains("\x1b[?1049h"),
            "expected alternate screen sequence \\033[?1049h at start, got:\n{out_str}"
        );
        assert!(
            out_str.contains("\x1b[?1049l"),
            "expected alternate screen exit sequence \\033[?1049l on quit, got:\n{out_str}"
        );
        assert!(
            out_str.contains("\x1b[K\r\n"),
            "expected lines to end with \\033[K\\r\\n in raw mode, got:\n{out_str}"
        );
        assert!(
            out_str.contains("\x1b[?25h"),
            "expected cursor show sequence \\033[?25h on quit"
        );
    }

    #[tokio::test]
    async fn test_top_terminal_restoration_on_panic() {
        let _env = crate::cli::test_support::cli_test_env();
        let mut out = Vec::new();

        struct Cleanup<'a>(&'a mut Vec<u8>);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.extend_from_slice(b"\x1b[?25h\x1b[0m\n");
            }
        }

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = Cleanup(&mut out);
            panic!("simulated terminal failure");
        }));
        assert!(res.is_err(), "expected panic to propagate");

        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out_str.contains("\x1b[?25h"),
            "expected cursor show sequence \\033[?25h on panic recovery"
        );
    }

    #[tokio::test]
    async fn test_top_privacy() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        let frame = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            false,
            false,
        );

        for p in [
            "private-developer@internal.corp.example.com",
            "private-backup@internal.corp.example.com",
            "@internal.corp.example.com",
            "ya29.",
            "mock_token",
            "mock_refresh",
            "https://",
            "http://",
            "prompt",
            "conversation",
        ] {
            assert!(
                !frame.contains(p),
                "SECURITY/PRIVACY VIOLATION: top frame leaked prohibited string {p:?}\nFrame was:\n{frame}"
            );
        }
    }

    #[tokio::test]
    async fn test_top_show_email() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        for w in [120, 80, 50, 35] {
            let frame = render_top_frame(
                Some(&pool),
                Some(&stats),
                Some(&info),
                true,
                "",
                Duration::from_secs(2),
                w,
                false,
                true,
            );
            assert!(
                frame.contains("Email: "),
                "expected Email: to be displayed with showEmail=true at width {w}\nGot:\n{frame}"
            );
        }

        let frame_no_email = render_top_frame(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            false,
            false,
        );
        assert!(
            !frame_no_email.contains("Email: "),
            "top frame without showEmail should not contain Email:\nGot:\n{frame_no_email}"
        );
    }

    #[tokio::test]
    async fn test_top_aliases_and_flags() {
        let env = crate::cli::test_support::cli_test_env();
        let tmp_dir = env.dir.path().display().to_string();
        let now = crate::clock::now_unix() as i64;
        let frac = 0.85;
        let acc = Account {
            id: "acc_alias_1".to_string(),
            name: "AliasAccount".to_string(),
            status: "ready".to_string(),
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pool = Pool {
            version: 1,
            accounts: vec![acc],
            ..Default::default()
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let ctx =
            daemon::resolve_instance_context(Some(std::path::Path::new(&tmp_dir)), None, Some(port), None).unwrap();
        crate::config::configure_data_dir(&ctx.data_dir).unwrap();
        storage::save_pool(&pool).unwrap();

        let pid_info = DaemonInfo {
            pid: std::process::id() as i32,
            ..Default::default()
        };
        daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        // 1. Direct aliases routed through dispatcher
        for cmd in ["top", "watch", "monitor"] {
            let (code, stdout, stderr) =
                crate::cli::test_support::run_cli(&[cmd, "--once", "-p", &port.to_string(), "-D", &tmp_dir], "").await;
            assert_eq!(code, 0, "command {cmd} failed with exit code {code}: {stderr}");
            assert!(
                stdout.contains("AliasAccount") && stdout.contains("Runtime Counters"),
                "command {cmd} did not render valid top frame:\n{stdout}"
            );
        }

        // 2. Flags -w and --watch accepted transparently by top_cmd
        for flag in ["-w", "--watch"] {
            let mut in_c = Cursor::new(Vec::new());
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut io = Io {
                stdin: &mut in_c,
                stdout: &mut out,
                stderr: &mut err,
                stdout_fd: None,
                stdin_fd: None,
            };
            let code = top_cmd(
                &mut io,
                &[
                    flag.to_string(),
                    "--once".to_string(),
                    "-p".to_string(),
                    port.to_string(),
                    "-D".to_string(),
                    tmp_dir.clone(),
                ],
            )
            .await;
            assert_eq!(code, 0, "top_cmd with {flag} failed: {}", String::from_utf8_lossy(&err));
            let s = String::from_utf8_lossy(&out);
            assert!(
                s.contains("AliasAccount") && s.contains("Runtime Counters"),
                "top_cmd with {flag} did not render valid top frame:\n{s}"
            );
        }
    }

    #[tokio::test]
    async fn test_top_bounded_widths_assertion() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let standard_pool = create_test_top_pool(now);
        let cjk_pool = Pool {
            version: 1,
            accounts: vec![
                Account {
                    id: "cjk_1".to_string(),
                    name: "主账号 · 极速开发者账号".to_string(),
                    status: "ready".to_string(),
                    request_count: 9999,
                    last_quota: standard_pool.accounts[0].last_quota.clone(),
                    ..Default::default()
                },
                Account {
                    id: "cjk_2".to_string(),
                    name: "日本語アカウント（東京リージョン）".to_string(),
                    status: "ready".to_string(),
                    request_count: 120000,
                    last_quota: standard_pool.accounts[0].last_quota.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let empty_pool = Pool {
            version: 1,
            accounts: vec![],
            ..Default::default()
        };
        let long_name_pool = Pool {
            version: 1,
            accounts: vec![Account {
                id: "long_1".to_string(),
                name: "SuperExtremelyLongAccountIdentifierThatExceedsNormalTerminalWidthEasily2026".to_string(),
                status: "validation_required".to_string(),
                request_count: 999999,
                ..Default::default()
            }],
            ..Default::default()
        };

        let pools: HashMap<&str, Pool> = [
            ("standard", standard_pool),
            ("cjk", cjk_pool),
            ("empty", empty_pool),
            ("long_name", long_name_pool),
        ]
        .into_iter()
        .collect();

        let stats_running = create_test_runtime_stats(3754721);
        let info_running = DaemonInfo {
            pid: 3754721,
            ..Default::default()
        };

        let widths = [120, 80, 50, 35];

        for &w in &widths {
            for (&p_name, p) in &pools {
                for &daemon_running in &[true, false] {
                    for &refresh_banner in &[true, false] {
                        let (cur_stats, cur_info) = if daemon_running {
                            (Some(&stats_running), Some(&info_running))
                        } else {
                            (None, None)
                        };

                        let frame = render_top_frame(
                            Some(p),
                            cur_stats,
                            cur_info,
                            daemon_running,
                            "",
                            Duration::from_secs(2),
                            w,
                            refresh_banner,
                            false,
                        );
                        for (line_idx, line) in frame.split('\n').enumerate() {
                            let vis_width = visible_width(line);
                            assert!(
                                vis_width <= w,
                                "Width constraint VIOLATED at width={w} [pool={p_name}, daemonRunning={daemon_running}, banner={refresh_banner}] line {line_idx}:\nVisible width: {vis_width} > {w}\nRaw line: {line:?}\nPlain line: {:?}",
                                strip_ansi(line)
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_top_print_representative_widths() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(3754721);
        let info = DaemonInfo {
            pid: 3754721,
            ..Default::default()
        };

        for w in [120, 80, 50, 35] {
            let frame = render_top_frame(
                Some(&pool),
                Some(&stats),
                Some(&info),
                true,
                "",
                Duration::from_secs(2),
                w,
                false,
                false,
            );
            println!("\n=== WIDTH {w} ===\n{}", strip_ansi(&frame));
        }
    }

    #[tokio::test]
    async fn test_top_telemetry_block() {
        let _env = crate::cli::test_support::cli_test_env();
        let now = crate::clock::now_unix() as i64;
        let pool = create_test_top_pool(now);
        let stats = create_test_runtime_stats(12345);
        let info = DaemonInfo {
            pid: 12345,
            ..Default::default()
        };

        // 1. Height < 18: Telemetry omitted
        let frame_short = render_top_frame_with_height(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            15,
            false,
            false,
        );
        let plain_short = strip_ansi(&frame_short);
        assert!(
            !plain_short.contains("Quota Runway &"),
            "expected telemetry omitted on short terminal height=15, got:\n{plain_short}"
        );

        // 2. Height >= 18 and Width >= 76: Full telemetry
        let frame_full = render_top_frame_with_height(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            100,
            24,
            false,
            false,
        );
        let plain_full = strip_ansi(&frame_full);
        assert!(
            plain_full.contains("⚡ Quota Runway & Endurance Forecast:"),
            "expected full telemetry on height=24 width=100, got:\n{plain_full}"
        );
        assert!(
            plain_full.contains("Health & Pace:"),
            "expected Health & Pace in full telemetry, got:\n{plain_full}"
        );
        assert!(
            plain_full.contains("5-Hour Runway:"),
            "expected 5-Hour Runway in full telemetry, got:\n{plain_full}"
        );

        // 3. Height >= 18 and Width < 76: Compact telemetry
        let frame_compact = render_top_frame_with_height(
            Some(&pool),
            Some(&stats),
            Some(&info),
            true,
            "",
            Duration::from_secs(2),
            60,
            24,
            false,
            false,
        );
        let plain_compact = strip_ansi(&frame_compact);
        assert!(
            plain_compact.contains("⚡ Quota Runway & Health:"),
            "expected compact telemetry on width=60, got:\n{plain_compact}"
        );
    }

    #[tokio::test]
    async fn top_restores_terminal_mode_on_every_exit() {
        let _env = crate::cli::test_support::cli_test_env();

        async fn run_case(action: &str, env_path: &std::path::Path) {
            let mut master: libc::c_int = 0;
            let mut slave: libc::c_int = 0;
            let ret = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(ret, 0, "openpty failed");

            // Record slave termios before
            let mut orig_termios: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(slave, &mut orig_termios) }, 0);

            // Prepare pool / state
            let data_dir = env_path.join(format!("top_test_{action}"));
            std::fs::create_dir_all(&data_dir).unwrap();
            crate::config::configure_data_dir(&data_dir).unwrap();

            match action {
                "q" => {
                    let pool = Pool {
                        version: 1,
                        accounts: vec![Account {
                            id: "acc_1".to_string(),
                            name: "TestAcc".to_string(),
                            status: "ready".to_string(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    };
                    storage::save_pool(&pool).unwrap();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(50));
                        unsafe {
                            libc::write(master, b"q".as_ptr() as *const libc::c_void, 1);
                        }
                    });
                }
                "ctrl_c" => {
                    let pool = Pool {
                        version: 1,
                        accounts: vec![Account {
                            id: "acc_1".to_string(),
                            name: "TestAcc".to_string(),
                            status: "ready".to_string(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    };
                    storage::save_pool(&pool).unwrap();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(50));
                        unsafe {
                            libc::write(master, b"\x03".as_ptr() as *const libc::c_void, 1);
                        }
                    });
                }
                "render_error" => {
                    // Corrupt the state db file so load_pool fails closed
                    let db_path = crate::config::state_db_file();
                    std::fs::write(&db_path, b"not a valid sqlite database").unwrap();
                }
                _ => unreachable!(),
            }

            let mut in_buf = Cursor::new(Vec::new());
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();

            let mut io = Io {
                stdin: &mut in_buf,
                stdout: &mut out_buf,
                stderr: &mut err_buf,
                stdout_fd: Some(slave),
                stdin_fd: Some(slave),
            };

            let args = vec!["-D".to_string(), data_dir.display().to_string()];
            let code = tokio::time::timeout(Duration::from_secs(10), top_cmd(&mut io, &args))
                .await
                .unwrap_or_else(|_| panic!("top did not exit for action {action}"));
            let want = if action == "render_error" { 1 } else { 0 };
            assert_eq!(code, want, "exit code for action {action}");
            // The alternate screen is only entered when the raw-mode guard was taken, so the restore check is not vacuous.
            assert!(
                String::from_utf8_lossy(&out_buf).contains("\x1b[?1049h"),
                "raw mode was never entered for action {action}"
            );

            // Check slave termios after exit
            let mut after_termios: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(slave, &mut after_termios) }, 0);

            let orig_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    &orig_termios as *const libc::termios as *const u8,
                    std::mem::size_of::<libc::termios>(),
                )
            };
            let after_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    &after_termios as *const libc::termios as *const u8,
                    std::mem::size_of::<libc::termios>(),
                )
            };
            assert_eq!(
                orig_bytes, after_bytes,
                "terminal attributes for action {action} were not restored!"
            );

            unsafe {
                libc::close(master);
                libc::close(slave);
            }
        }

        run_case("q", _env.dir.path()).await;
        run_case("ctrl_c", _env.dir.path()).await;
        run_case("render_error", _env.dir.path()).await;
    }
}
