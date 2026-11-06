fn main() {
    if std::env::var("AGY_POOL_DAEMON_CHILD").as_deref() == Ok("1") {
        let code = agy_pool::daemon::run_daemon_child(&mut std::io::stderr());
        std::process::exit(code);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to initialize tokio runtime");
    rt.block_on(async_main());
}

async fn async_main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "-v" | "--version" | "version" => {
                println!("agy-pool {}", agy_pool::VERSION);
                return;
            }
            "proxy-test-server" => {
                if args.len() < 5 {
                    eprintln!("usage: agy-pool proxy-test-server <port> <state-dir> <backend-url>");
                    std::process::exit(1);
                }
                let port = &args[2];
                let state_dir = &args[3];
                let backend_url = &args[4];

                if let Err(e) = agy_pool::config::configure_state_dir(std::path::Path::new(state_dir)) {
                    eprintln!("config error: {e}");
                    std::process::exit(1);
                }
                let u = match url::Url::parse(backend_url) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("invalid backend-url: {e}");
                        std::process::exit(1);
                    }
                };
                let host = u.host_str().unwrap_or("").to_string();
                let port_suffix = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                let host_port = format!("{host}{port_suffix}");
                agy_pool::proxy::set_backend_host(&host_port);
                agy_pool::proxy::set_backend_url(backend_url);
                agy_pool::proxy::set_token_refresher(|acc| Ok(format!("token-{}", acc.id)));

                let addr: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("invalid listen address: {e}");
                        std::process::exit(1);
                    }
                };
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("bind error: {e}");
                        std::process::exit(1);
                    }
                };
                println!("READY");

                let handler = std::sync::Arc::new(agy_pool::proxy::Handler::new());
                loop {
                    let (stream, _) = match listener.accept().await {
                        Ok(res) => res,
                        Err(_) => break,
                    };
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let h = handler.clone();
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            let h = h.clone();
                            async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service)
                            .await;
                    });
                }
                return;
            }
            _ => {}
        }
    }
    eprintln!("agy-pool (rust): only --version and proxy-test-server are implemented yet");
    std::process::exit(2);
}
