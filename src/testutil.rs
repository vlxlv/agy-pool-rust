use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }

    pub fn form_value(&self, key: &str) -> Option<String> {
        for (k, v) in url::form_urlencoded::parse(&self.body) {
            if k == key {
                return Some(v.into_owned());
            }
        }
        None
    }
}

pub type Request = RecordedRequest;

pub struct TestServer {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub fn start(
        handler: impl Fn(Request) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
    ) -> TestServer {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind test server");
        std_listener.set_nonblocking(true).expect("failed to set nonblocking");
        let port = std_listener.local_addr().expect("failed to get local addr").port();
        let url = format!("http://127.0.0.1:{}", port);

        let listener = tokio::net::TcpListener::from_std(std_listener).expect("failed to create tokio listener");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs = Arc::clone(&requests);
        let handler = Arc::new(handler);

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    conn = listener.accept() => {
                        let (stream, _) = match conn {
                            Ok(c) => c,
                            Err(_) => break,
                        };
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let reqs = Arc::clone(&reqs);
                        let handler = Arc::clone(&handler);

                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(
                                move |req: hyper::Request<hyper::body::Incoming>| {
                                    let reqs = Arc::clone(&reqs);
                                    let handler = Arc::clone(&handler);
                                    async move {
                                        let method = req.method().as_str().to_string();
                                        let path = req.uri().path().to_string();
                                        let mut headers = HashMap::new();
                                        for (name, val) in req.headers() {
                                            if let Ok(v) = val.to_str() {
                                                headers.insert(
                                                    name.as_str().to_ascii_lowercase(),
                                                    v.to_string(),
                                                );
                                            }
                                        }
                                        let body_bytes = match http_body_util::BodyExt::collect(
                                            req.into_body(),
                                        )
                                        .await
                                        {
                                            Ok(collected) => collected.to_bytes().to_vec(),
                                            Err(_) => Vec::new(),
                                        };

                                        let rec = RecordedRequest {
                                            method,
                                            path,
                                            headers,
                                            body: body_bytes,
                                        };
                                        reqs.lock().unwrap_or_else(|e| e.into_inner()).push(rec.clone());

                                        let (status, resp_headers, resp_body) = tokio::task::spawn_blocking(move || {
                                            handler(rec)
                                        })
                                        .await
                                        .unwrap_or((
                                            hyper::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                                            Vec::new(),
                                            Vec::new(),
                                        ));

                                        let mut builder = hyper::Response::builder().status(
                                            hyper::StatusCode::from_u16(status)
                                                .unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR),
                                        );
                                        for (k, v) in resp_headers {
                                            builder = builder.header(k, v);
                                        }
                                        let response = builder
                                            .body(http_body_util::Full::new(bytes::Bytes::from(resp_body)))
                                            .unwrap_or_else(|_| {
                                                hyper::Response::new(http_body_util::Full::new(
                                                    bytes::Bytes::new(),
                                                ))
                                            });
                                        Ok::<_, std::convert::Infallible>(response)
                                    }
                                },
                            );

                            let conn_builder = hyper::server::conn::http1::Builder::new();
                            let _ = conn_builder.serve_connection(io, service).await;
                        });
                    }
                }
            }
        });

        TestServer {
            url,
            requests,
            shutdown: Some(shutdown_tx),
            handle,
        }
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.handle.abort();
    }
}
