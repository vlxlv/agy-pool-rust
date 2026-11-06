use crate::cli::Io;

pub async fn handle_eval(cmd: &str, args: &[String], io: &mut Io<'_>) -> bool {
    if cmd == "proxy-test-server" {
        run_proxy_test_server(args, io).await;
        return true;
    }
    if cmd.starts_with("eval-") {
        let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
        std::process::exit(2);
    }
    false
}

async fn run_proxy_test_server(args: &[String], io: &mut Io<'_>) {
    if args.len() < 4 {
        let _ = writeln!(
            io.stderr,
            "usage: agy-pool proxy-test-server <port> <state-dir> <backend-url>"
        );
        std::process::exit(1);
    }
    let port = &args[1];
    let state_dir = &args[2];
    let backend_url = &args[3];

    if let Err(e) = crate::config::configure_state_dir(std::path::Path::new(state_dir)) {
        let _ = writeln!(io.stderr, "config error: {e}");
        std::process::exit(1);
    }
    let u = match url::Url::parse(backend_url) {
        Ok(u) => u,
        Err(e) => {
            let _ = writeln!(io.stderr, "invalid backend-url: {e}");
            std::process::exit(1);
        }
    };
    let host = u.host_str().unwrap_or("").to_string();
    let port_suffix = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    let host_port = format!("{host}{port_suffix}");
    crate::proxy::set_backend_host(&host_port);
    crate::proxy::set_backend_url(backend_url);
    crate::proxy::set_token_refresher(|acc| Ok(format!("token-{}", acc.id)));

    let addr: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(io.stderr, "invalid listen address: {e}");
            std::process::exit(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io.stderr, "bind error: {e}");
            std::process::exit(1);
        }
    };
    let _ = writeln!(io.stdout, "READY");
    let _ = io.stdout.flush();

    let handler = std::sync::Arc::new(crate::proxy::Handler::new());
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(res) => res,
            Err(_) => break,
        };
        let io_tokio = hyper_util::rt::TokioIo::new(stream);
        let h = handler.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let h = h.clone();
                async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io_tokio, service)
                .await;
        });
    }
}
