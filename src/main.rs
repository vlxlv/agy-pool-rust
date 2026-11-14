fn agy_ua() -> String {
    agy_pool::quota::agy_client_ua(&agy_pool::diagnostics::get_installed_agy_version())
}

fn main() {
    // As Go's main: upstream only reports Gemini quota to a versioned agy UA. Quota refreshes re-read the
    // installed agy version each time; the proxy caches it on first use.
    agy_pool::quota::set_user_agent_provider(Box::new(agy_ua));
    agy_pool::proxy::set_user_agent_provider(agy_ua);

    if std::env::var("AGY_POOL_DAEMON_CHILD").as_deref() == Ok("1") && std::env::args().len() <= 1 {
        let code = agy_pool::daemon::run_daemon_child(&mut std::io::stderr());
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize tokio runtime");
    rt.block_on(async_main());
}

async fn async_main() {
    let args: Vec<String> = std::env::args().collect();
    // Not stdin().lock(): holding the lock for the whole process would deadlock any later stdin reader.
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut io = agy_pool::cli::Io {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        stdout_fd: Some(1),
        stdin_fd: Some(0),
    };

    if args.len() > 1 {
        let cmd = &args[1];
        if (cmd.starts_with("eval-") || cmd == "proxy-test-server")
            && agy_pool::cli::eval::handle_eval(cmd, &args[1..], &mut io).await
        {
            return;
        }
    }

    let cli_args = if args.len() > 1 { &args[1..] } else { &[] };
    let code = agy_pool::cli::main(cli_args, &mut io).await;
    if code != 0 {
        std::process::exit(code);
    }
}
