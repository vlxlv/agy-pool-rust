use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_RED, CLR_RESET};
use crate::storage::tokens::{self, TokenCounts, TokenRange};

const USAGE: &str = "usage: agy-pool tokens [--range 1h|24h|7d|total]\n\n\
Show token usage per account and for the whole pool (default range: 24h).\n\
  1h is the current clock hour; 24h and 7d are the current hour plus the 23 / 167 before it.\n\
  Cached tokens are part of prompt tokens; Total = prompt + output + thinking.\n";

/// Formats a token count compactly: 950, 12.3K, 1.85M, 2.10B.
pub fn format_tokens(n: i64) -> String {
    let f = n as f64;
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}K", f / 1e3)
    } else if n < 1_000_000_000 {
        format!("{:.2}M", f / 1e6)
    } else {
        format!("{:.2}B", f / 1e9)
    }
}

fn range_label(range: TokenRange, now_unix: i64) -> String {
    let name = match range {
        TokenRange::Hour => "this hour",
        TokenRange::Day => "last 24h",
        TokenRange::Week => "last 7d",
        TokenRange::Total => return "all time".to_string(),
    };
    let since = range
        .start_bucket(now_unix)
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    format!("{name} (since {since})")
}

fn share(part: &TokenCounts, total: &TokenCounts) -> String {
    if total.total() > 0 {
        format!("{:.1}%", part.total() as f64 * 100.0 / total.total() as f64)
    } else {
        "-".to_string()
    }
}

/// One-line pool summary for `status -v` and `stats`; None when state.db cannot be read.
pub fn summary_line(range: TokenRange, now_unix: i64) -> Option<String> {
    let s = tokens::sum(&tokens::usage_by_account(range, now_unix).ok()?);
    Some(format!(
        "Tokens  : {} {} total · prompt {} (cached {}) · output {} · thinking {} · {} requests",
        range.as_str(),
        format_tokens(s.total()),
        format_tokens(s.prompt),
        format_tokens(s.cached),
        format_tokens(s.output),
        format_tokens(s.thinking),
        crate::observability::format_number(s.requests.max(0) as u64)
    ))
}

pub async fn tokens_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (_ctx, rem) = match crate::cli::resolve_context_for_cmd(args) {
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

    let mut range = TokenRange::Day;
    let mut i = 0;
    while i < rem.len() {
        let arg = rem[i].as_str();
        let value = if arg == "-h" || arg == "--help" {
            let _ = write!(io.stdout, "{USAGE}");
            return 0;
        } else if arg == "--range" || arg == "-r" {
            i += 1;
            rem.get(i).map(String::as_str)
        } else if let Some(v) = arg.strip_prefix("--range=") {
            Some(v)
        } else {
            let _ = writeln!(io.stderr, "agy-pool tokens: error: unrecognized argument: {arg}");
            return 2;
        };
        match value.and_then(TokenRange::parse) {
            Some(r) => range = r,
            None => {
                let _ = writeln!(
                    io.stderr,
                    "agy-pool tokens: error: --range must be one of 1h, 24h, 7d, total"
                );
                return 2;
            }
        }
        i += 1;
    }

    let now = crate::clock::now_unix() as i64;
    let usage = match tokens::usage_by_account(range, now) {
        Ok(u) => u,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot read token usage: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };
    let pool = crate::storage::load_pool().unwrap_or_default();
    let total = tokens::sum(&usage);

    let _ = writeln!(
        io.stdout,
        "{}Token usage — {}{}\n",
        CLR_BOLD,
        range_label(range, now),
        CLR_RESET
    );
    if usage.is_empty() {
        let _ = writeln!(io.stdout, "No token usage recorded in this range.");
        return 0;
    }

    let header = format!(
        "{:<20} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6}",
        "Account", "Requests", "Prompt", "Cached", "Output", "Thinking", "Total", "Share"
    );
    let _ = writeln!(io.stdout, "{header}");
    let mut rows: Vec<(&String, &TokenCounts)> = usage.iter().collect();
    rows.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
    let mut print_row = |name: &str, c: &TokenCounts| {
        let name: String = name.chars().take(20).collect();
        let _ = writeln!(
            io.stdout,
            "{:<20} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6}",
            name,
            c.requests,
            format_tokens(c.prompt),
            format_tokens(c.cached),
            format_tokens(c.output),
            format_tokens(c.thinking),
            format_tokens(c.total()),
            share(c, &total)
        );
    };
    for (id, c) in rows {
        print_row(&tokens::account_label(&pool, id), c);
    }
    print_row("Pool", &total);
    if total.aborted > 0 {
        let _ = writeln!(
            io.stdout,
            "\n{} aborted stream(s) counted with the usage seen before they ended.",
            total.aborted
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_is_compact() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(1_850_240), "1.85M");
        assert_eq!(format_tokens(2_100_000_000), "2.10B");
    }

    #[tokio::test]
    async fn test_tokens_cmd_table_and_ranges() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::storage::tokens::clear_pending();
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Work".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();
        let now = crate::clock::now_unix() as i64;
        crate::storage::tokens::record_token_usage_at("acc_1", 12_000, 2_000, 3_000, 1_000, false, now);
        crate::storage::tokens::record_token_usage_at("acc_1", 50_000, 0, 0, 0, false, now - 3 * 86400);
        crate::storage::tokens::flush_token_usage().unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["tokens"], "").await;
        assert_eq!(code, 0, "stderr: {stderr}");
        assert!(stdout.contains("last 24h"), "{stdout}");
        assert!(stdout.contains("Work"), "{stdout}");
        assert!(stdout.contains("16.0K"), "24h total = 12K + 3K + 1K: {stdout}");
        assert!(stdout.contains("Pool"), "{stdout}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--range", "7d"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("66.0K"), "7d includes the 3-day-old 50K: {stdout}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--range=total"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("all time"), "{stdout}");

        let (code, _, stderr) = crate::cli::test_support::run_cli(&["tokens", "--range", "2d"], "").await;
        assert_eq!(code, 2);
        assert!(stderr.contains("1h, 24h, 7d, total"), "{stderr}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--help"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("usage: agy-pool tokens"));

        let line = summary_line(TokenRange::Day, now).unwrap();
        assert!(line.contains("16.0K total"), "{line}");
        assert!(line.contains("1 requests"), "{line}");
        crate::storage::tokens::clear_pending();
    }

    #[tokio::test]
    async fn test_tokens_cmd_empty_range() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::storage::tokens::clear_pending();
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "-r", "1h"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("No token usage recorded"), "{stdout}");
    }
}
