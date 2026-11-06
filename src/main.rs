fn main() {
    if std::env::var("AGY_POOL_DAEMON_CHILD").as_deref() == Ok("1") {
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
    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut io = agy_pool::cli::Io {
        stdin: &mut stdin_lock,
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
