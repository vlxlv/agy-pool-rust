use crate::cli::Io;
use base64::Engine as _;
use std::path::Path;
use std::time::Duration;

pub async fn handle_eval(cmd: &str, args: &[String], io: &mut Io<'_>) -> bool {
    if cmd == "proxy-test-server" {
        run_proxy_test_server(args, io).await;
        return true;
    }

    match cmd {
        "eval-scheduler" => {
            #[derive(serde::Deserialize)]
            struct EvalSchedulerInput {
                #[serde(default)]
                candidates: Vec<serde_json::Value>,
                #[serde(default)]
                strategy: String,
                #[serde(default)]
                pool: Option<crate::storage::Pool>,
                #[serde(default)]
                now: f64,
            }

            #[derive(serde::Serialize)]
            struct EvalSchedulerOutput {
                ordered_ids: Vec<String>,
            }

            let input: EvalSchedulerInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let mut candidates = Vec::with_capacity(input.candidates.len());
            for c in input.candidates {
                match deserialize_account_lenient(c) {
                    Ok(a) => candidates.push(a),
                    Err(e) => {
                        let _ = writeln!(io.stderr, "{e}");
                        std::process::exit(1);
                    }
                }
            }

            let pool = input.pool.unwrap_or_default();
            let ordered_indices = crate::scheduler::order_candidates(&candidates, &input.strategy, &pool, input.now);
            let mut ordered_ids = Vec::with_capacity(ordered_indices.len());
            for idx in ordered_indices {
                if let Some(acc) = candidates.get(idx) {
                    ordered_ids.push(acc.id.clone());
                }
            }

            let out = serde_json::to_string(&EvalSchedulerOutput { ordered_ids }).unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-capacity" => {
            #[derive(serde::Deserialize)]
            struct EvalCapacityInput {
                #[serde(default)]
                account: Option<serde_json::Value>,
                #[serde(default)]
                now: f64,
            }

            #[derive(serde::Serialize)]
            struct EvalCapacityStateOutput {
                pub q5: Option<f64>,
                pub q7: Option<f64>,
                pub r5: f64,
                pub r7: f64,
                pub pace5: Option<f64>,
                pub pace7: Option<f64>,
                pub worst_pace: f64,
                pub total_pace: f64,
                pub raw_floor: f64,
                pub q5_known: bool,
                pub q7_known: bool,
                pub known_window_count: usize,
                pub legacy_fraction_known: bool,
                pub is_depleted: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                pub reset5_time: Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                pub reset7_time: Option<String>,
            }

            #[derive(serde::Serialize)]
            struct EvalCapacityOutput {
                capacity_state: EvalCapacityStateOutput,
                freshness_info: crate::quota::FreshnessInfo,
                freshness_rank: i32,
                refresh_needed: bool,
            }

            let input: EvalCapacityInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let account = match input.account {
                Some(v) => match deserialize_account_lenient(v) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        let _ = writeln!(io.stderr, "{e}");
                        std::process::exit(1);
                    }
                },
                None => None,
            };

            let (cap_state, fresh, rank, needed) = match &account {
                Some(acc) => {
                    let cap = crate::quota::compute_capacity_state(acc, input.now);
                    let f = crate::quota::freshness(acc, input.now);
                    let r = crate::quota::freshness_rank(acc, input.now);
                    let n = crate::quota::refresh_needed(acc, input.now);
                    (cap, f, r, n)
                }
                None => {
                    let dummy = crate::storage::Account::default();
                    let cap = crate::quota::compute_capacity_state(&dummy, input.now);
                    let f = crate::quota::freshness(&dummy, input.now);
                    let r = crate::quota::freshness_rank(&dummy, input.now);
                    let n = crate::quota::refresh_needed(&dummy, input.now);
                    (cap, f, r, n)
                }
            };

            let eval_cap_state = EvalCapacityStateOutput {
                q5: cap_state.q5,
                q7: cap_state.q7,
                r5: cap_state.r5,
                r7: cap_state.r7,
                pace5: cap_state.pace5,
                pace7: cap_state.pace7,
                worst_pace: cap_state.worst_pace,
                total_pace: cap_state.total_pace,
                raw_floor: cap_state.raw_floor,
                q5_known: cap_state.q5_known,
                q7_known: cap_state.q7_known,
                known_window_count: cap_state.known_window_count,
                legacy_fraction_known: cap_state.legacy_fraction_known,
                is_depleted: cap_state.is_depleted,
                reset5_time: cap_state.reset5_time.map(format_system_time_rfc3339),
                reset7_time: cap_state.reset7_time.map(format_system_time_rfc3339),
            };

            let out = serde_json::to_string(&EvalCapacityOutput {
                capacity_state: eval_cap_state,
                freshness_info: fresh,
                freshness_rank: rank,
                refresh_needed: needed,
            })
            .unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-account-target" => {
            #[derive(serde::Deserialize)]
            struct EvalAccountTargetInput {
                #[serde(default)]
                accounts: Vec<serde_json::Value>,
                #[serde(default)]
                target: String,
            }

            let input: EvalAccountTargetInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let mut accounts = Vec::with_capacity(input.accounts.len());
            for a in input.accounts {
                match deserialize_account_lenient(a) {
                    Ok(acc) => accounts.push(acc),
                    Err(e) => {
                        let _ = writeln!(io.stderr, "{e}");
                        std::process::exit(1);
                    }
                }
            }

            let found = crate::accounts::find_account_by_target(&accounts, &input.target);
            let found_id = found.map(|a| a.id.clone());
            let out = serde_json::to_string(&serde_json::json!({ "found_id": found_id })).unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-display-name" => {
            #[derive(serde::Deserialize)]
            struct EvalDisplayNameInput {
                #[serde(default)]
                account: Option<serde_json::Value>,
            }

            let input: EvalDisplayNameInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let account = match input.account {
                Some(v) => match deserialize_account_lenient(v) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        let _ = writeln!(io.stderr, "{e}");
                        std::process::exit(1);
                    }
                },
                None => None,
            };

            let name = crate::accounts::display_account_name(account.as_ref(), None);
            let out = serde_json::to_string(&serde_json::json!({ "display_name": name })).unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-jwt" => {
            #[derive(serde::Deserialize)]
            struct EvalJwtInput {
                #[serde(default)]
                jwt: String,
            }

            let input: EvalJwtInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let claims = crate::auth::decode_jwt_payload(&input.jwt);
            let out = serde_json::to_string(&serde_json::json!({ "claims": claims })).unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-error-classifier" => {
            #[derive(serde::Deserialize)]
            struct EvalErrorClassifierInput {
                #[serde(default)]
                status: u16,
                #[serde(default)]
                body: String,
            }

            let input: EvalErrorClassifierInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            let body_bytes = input.body.as_bytes();
            let is_val = crate::auth::is_validation_error(input.status, body_bytes);
            let val_url = crate::auth::extract_validation_url(body_bytes);
            let is_auth = crate::auth::is_auth_error(input.status, body_bytes);

            let out = serde_json::to_string(&serde_json::json!({
                "is_validation": is_val,
                "validation_url": val_url,
                "is_auth_error": is_auth,
            }))
            .unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-crypto" => {
            #[derive(serde::Deserialize)]
            struct EvalCryptoBundleInput {
                #[serde(default)]
                pub format: String,
                #[serde(default)]
                pub kdf: String,
                #[serde(default)]
                pub iterations: usize,
                #[serde(default)]
                pub salt: String,
                #[serde(default)]
                pub nonce: String,
                #[serde(default)]
                pub ciphertext: String,
                #[serde(default)]
                pub tag: String,
            }

            #[derive(serde::Deserialize)]
            struct EvalCryptoInput {
                #[serde(default)]
                action: String,
                #[serde(default)]
                data: String,
                #[serde(default)]
                password: String,
                #[serde(default)]
                salt: String,
                #[serde(default)]
                nonce: String,
                #[serde(default)]
                bundle: Option<EvalCryptoBundleInput>,
            }

            let input: EvalCryptoInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            if input.action == "encrypt" {
                let mut salt = Vec::new();
                if !input.salt.is_empty()
                    && let Ok(s) = base64::engine::general_purpose::STANDARD.decode(&input.salt)
                {
                    salt = s;
                }
                let mut nonce = Vec::new();
                if !input.nonce.is_empty()
                    && let Ok(n) = base64::engine::general_purpose::STANDARD.decode(&input.nonce)
                {
                    nonce = n;
                }

                match crate::accounts::crypto::encrypt_bundle(input.data.as_bytes(), &input.password, &salt, &nonce) {
                    Ok(b) => {
                        let out = serde_json::to_string(&serde_json::json!({ "bundle": b })).unwrap();
                        let _ = writeln!(io.stdout, "{out}");
                        let _ = io.stdout.flush();
                        true
                    }
                    Err(e) => {
                        let _ = writeln!(io.stderr, "encrypt error: {e}");
                        std::process::exit(1);
                    }
                }
            } else if input.action == "decrypt" {
                let res = match input.bundle {
                    Some(b) => {
                        let bundle = crate::accounts::crypto::EncryptedBundle {
                            format: b.format,
                            kdf: if b.kdf.is_empty() {
                                "pbkdf2-hmac-sha256".to_string()
                            } else {
                                b.kdf
                            },
                            iterations: b.iterations,
                            salt: b.salt,
                            nonce: b.nonce,
                            ciphertext: b.ciphertext,
                            tag: b.tag,
                        };
                        crate::accounts::crypto::decrypt_bundle(&bundle, &input.password)
                    }
                    None => Err(crate::accounts::AccountsError::Message(
                        "Unsupported or invalid encrypted bundle format".to_string(),
                    )),
                };
                match res {
                    Ok(pt) => {
                        let pt_str = String::from_utf8_lossy(&pt);
                        let out = serde_json::to_string(&serde_json::json!({ "plaintext": pt_str })).unwrap();
                        let _ = writeln!(io.stdout, "{out}");
                        let _ = io.stdout.flush();
                        true
                    }
                    Err(e) => {
                        let out = serde_json::to_string(&serde_json::json!({ "error": e.to_string() })).unwrap();
                        let _ = writeln!(io.stdout, "{out}");
                        let _ = io.stdout.flush();
                        true
                    }
                }
            } else {
                false
            }
        }

        "eval-daemon" => {
            #[derive(serde::Deserialize)]
            struct EvalDaemonInput {
                #[serde(default)]
                action: String,
                #[serde(default)]
                bytes: i64,
                #[serde(default)]
                path: String,
                #[serde(default)]
                max_bytes: i64,
                #[serde(default)]
                backup_count: i32,
                #[serde(default)]
                force: bool,
                #[serde(default)]
                port: u16,
            }

            let input: EvalDaemonInput = match read_stdin_json(io) {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(io.stderr, "{e}");
                    std::process::exit(1);
                }
            };

            match input.action.as_str() {
                "format_size" => {
                    let formatted = crate::daemon::format_size(input.bytes);
                    let out = serde_json::to_string(&serde_json::json!({ "formatted": formatted })).unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                "read_pid" => {
                    let (info, err_msg) = match crate::daemon::pid::read_pid_file(Path::new(&input.path)) {
                        Ok(info) => (Some(info), String::new()),
                        Err(e) => (None, e.to_string()),
                    };
                    let out = serde_json::to_string(&serde_json::json!({
                        "info": info,
                        "error": err_msg,
                    }))
                    .unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                "get_daemon_info" => {
                    let path_opt = if input.path.is_empty() {
                        None
                    } else {
                        Some(Path::new(&input.path))
                    };
                    let info = crate::daemon::get_daemon_info(path_opt);
                    let out = serde_json::to_string(&serde_json::json!({ "info": info })).unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                "rotate_log" => {
                    let path_opt = if input.path.is_empty() {
                        None
                    } else {
                        Some(Path::new(&input.path))
                    };
                    let (rotated, err_msg) = match crate::daemon::rotate_log_if_needed(
                        path_opt,
                        input.max_bytes,
                        input.backup_count,
                        input.force,
                    ) {
                        Ok(r) => (r, String::new()),
                        Err(e) => (false, e.to_string()),
                    };
                    let out = serde_json::to_string(&serde_json::json!({
                        "rotated": rotated,
                        "error": err_msg,
                    }))
                    .unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                "clear_log" => {
                    let path_opt = if input.path.is_empty() {
                        None
                    } else {
                        Some(Path::new(&input.path))
                    };
                    let err_msg = match crate::daemon::clear_log(path_opt, input.backup_count) {
                        Ok(()) => String::new(),
                        Err(e) => e.to_string(),
                    };
                    let out = serde_json::to_string(&serde_json::json!({ "error": err_msg })).unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                "stop_daemon" => {
                    let path_opt = if input.path.is_empty() {
                        None
                    } else {
                        Some(Path::new(&input.path))
                    };
                    let err_msg =
                        match crate::daemon::control::stop_daemon(path_opt, input.port, Duration::from_secs(3)) {
                            Ok(()) => String::new(),
                            Err(e) => e.to_string(),
                        };
                    let out = serde_json::to_string(&serde_json::json!({ "error": err_msg })).unwrap();
                    let _ = writeln!(io.stdout, "{out}");
                    let _ = io.stdout.flush();
                    true
                }
                _ => false,
            }
        }

        "eval-conversation" => {
            let dir = if args.len() > 1 { &args[1] } else { "" };
            let (cid, title, dir_name) = crate::conversation::find_latest_conversation_for_dir(dir).unwrap_or_default();
            let out = serde_json::to_string(&serde_json::json!({
                "cid": cid,
                "title": title,
                "dir_name": dir_name,
            }))
            .unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        "eval-resolve-continue" => {
            let extra_args = if args.len() > 1 { &args[1..] } else { &[] };
            let mut sink = std::io::sink();
            let res = crate::conversation::resolve_continue_arg(extra_args, &mut sink);
            let out = serde_json::to_string(&serde_json::json!({ "args": res })).unwrap();
            let _ = writeln!(io.stdout, "{out}");
            let _ = io.stdout.flush();
            true
        }

        _ => false,
    }
}

fn deserialize_account_lenient(v: serde_json::Value) -> Result<crate::storage::Account, serde_json::Error> {
    let mut v = v;
    if let serde_json::Value::Object(ref mut map) = v {
        map.entry("id")
            .or_insert_with(|| serde_json::Value::String(String::new()));
    }
    serde_json::from_value(v)
}

fn format_system_time_rfc3339(st: std::time::SystemTime) -> String {
    let dur = st
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let ts = jiff::Timestamp::from_nanosecond(dur.as_nanos() as i128).unwrap();
    ts.to_string()
}

fn read_stdin_json<T: serde::de::DeserializeOwned>(io: &mut Io<'_>) -> Result<T, String> {
    let mut data = Vec::new();
    io.stdin.read_to_end(&mut data).map_err(|e| e.to_string())?;
    serde_json::from_slice(&data).map_err(|e| e.to_string())
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

    if let Err(e) = crate::config::configure_state_dir(Path::new(state_dir)) {
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
