use std::sync::LazyLock;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::http;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioIo;
use tokio_rustls::rustls;

#[derive(Debug)]
pub enum ConnectError {
    Dns(String),
    Tcp(String),
    Proxy(String),
    Tls(String),
    Timeout(&'static str),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Dns(s) => write!(f, "DNS resolution failed: {s}"),
            ConnectError::Tcp(s) => write!(f, "TCP connect failed: {s}"),
            ConnectError::Proxy(s) => write!(f, "proxy connection failed: {s}"),
            ConnectError::Tls(s) => write!(f, "TLS handshake failed: {s}"),
            ConnectError::Timeout(stage) => write!(f, "connect timeout during {stage}"),
        }
    }
}

impl std::error::Error for ConnectError {}

#[derive(Debug)]
pub enum SendError {
    Io(String),
    Timeout,
    Malformed(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Io(s) => write!(f, "IO error sending request: {s}"),
            SendError::Timeout => write!(f, "request send timed out"),
            SendError::Malformed(s) => write!(f, "malformed response: {s}"),
        }
    }
}

impl std::error::Error for SendError {}

pub fn is_timeout(e: &SendError) -> bool {
    match e {
        SendError::Timeout => true,
        SendError::Io(s) => {
            let lower = s.to_ascii_lowercase();
            lower.contains("timed out")
                || lower.contains("timeout")
                || lower.contains("deadline has elapsed")
                || lower.contains("deadline")
        }
        SendError::Malformed(_) => false,
    }
}

pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin + 'static> AsyncStream for T {}

pub struct UpstreamStream {
    inner: TokioIo<Box<dyn AsyncStream>>,
    pub is_proxied: bool,
}

impl UpstreamStream {
    pub fn new<S: AsyncStream>(stream: S, is_proxied: bool) -> Self {
        Self {
            inner: TokioIo::new(Box::new(stream)),
            is_proxied,
        }
    }
}

impl hyper::rt::Read for UpstreamStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for UpstreamStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl hyper_util::client::legacy::connect::Connection for UpstreamStream {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        let mut c = hyper_util::client::legacy::connect::Connected::new();
        if self.is_proxied {
            c = c.proxy(true);
        }
        c
    }
}

#[derive(Clone, Default)]
pub struct ProxyConnector;

impl tower_service::Service<http::Uri> for ProxyConnector {
    type Response = UpstreamStream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        Box::pin(async move {
            connect_stream(&uri)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

static POOLED_CLIENT: LazyLock<HyperClient<ProxyConnector, Full<Bytes>>> = LazyLock::new(|| {
    HyperClient::builder(hyper_util::rt::TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .build(ProxyConnector)
});

pub fn pooled_client() -> &'static HyperClient<ProxyConnector, Full<Bytes>> {
    &POOLED_CLIENT
}

pub struct Connected {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    _conn_task: tokio::task::JoinHandle<()>,
    is_proxied: bool,
    target_uri: http::Uri,
    proxy_auth: Option<String>,
}

impl Connected {
    pub async fn send(
        mut self,
        mut req: http::Request<Full<Bytes>>,
    ) -> Result<http::Response<hyper::body::Incoming>, SendError> {
        if !req.headers().contains_key(hyper::header::HOST)
            && let Some(auth) = self.target_uri.authority()
            && let Ok(val) = hyper::header::HeaderValue::from_str(auth.as_str())
        {
            req.headers_mut().insert(hyper::header::HOST, val);
        }

        if self.is_proxied && self.target_uri.scheme_str() == Some("http") {
            let scheme = "http";
            let authority = self.target_uri.authority().map(|a| a.as_str()).unwrap_or("");
            let path_and_query = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
            let abs_uri_str = format!("{}://{}{}", scheme, authority, path_and_query);
            if let Ok(abs_uri) = abs_uri_str.parse::<http::Uri>() {
                *req.uri_mut() = abs_uri;
            }
            if let Some(auth) = &self.proxy_auth
                && !req.headers().contains_key(hyper::header::PROXY_AUTHORIZATION)
                && let Ok(val) = hyper::header::HeaderValue::from_str(auth)
            {
                req.headers_mut().insert(hyper::header::PROXY_AUTHORIZATION, val);
            }
        }

        let fut = self.sender.send_request(req);
        match tokio::time::timeout(Duration::from_secs(300), fut).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(err)) => {
                if err.is_timeout() {
                    Err(SendError::Timeout)
                } else if err.is_parse() {
                    Err(SendError::Malformed(err.to_string()))
                } else {
                    Err(SendError::Io(err.to_string()))
                }
            }
            Err(_) => Err(SendError::Timeout),
        }
    }
}

pub async fn connect(uri: &http::Uri) -> Result<Connected, ConnectError> {
    let stream = connect_stream(uri).await?;
    let is_proxied = stream.is_proxied;
    let proxy_auth = if is_proxied {
        proxy_for(uri).and_then(|p| url::Url::parse(&p.to_string()).ok().and_then(|u| proxy_basic_auth(&u)))
    } else {
        None
    };

    let (sender, conn) = hyper::client::conn::http1::handshake(stream)
        .await
        .map_err(|e| ConnectError::Tcp(e.to_string()))?;

    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    Ok(Connected {
        sender,
        _conn_task: conn_task,
        is_proxied,
        target_uri: uri.clone(),
        proxy_auth,
    })
}

pub fn proxy_for(uri: &http::Uri) -> Option<http::Uri> {
    let scheme = uri.scheme_str()?;
    if scheme != "http" && scheme != "https" {
        return None;
    }

    let host = uri.host()?;
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');

    // Loopback rule: localhost or loopback IPs are never proxied
    if clean_host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    if let Ok(ip) = clean_host.parse::<std::net::IpAddr>()
        && ip.is_loopback()
    {
        return None;
    }

    let proxy_env = if scheme == "https" {
        std::env::var("HTTPS_PROXY")
            .or_else(|_| std::env::var("https_proxy"))
            .ok()
    } else {
        std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .ok()
    }?;

    let proxy_trimmed = proxy_env.trim();
    if proxy_trimmed.is_empty() {
        return None;
    }

    let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });

    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if !no_proxy.is_empty() && no_proxy_matches(clean_host, port, &no_proxy) {
        return None;
    }

    let parsed_url = parse_proxy_url(proxy_trimmed)?;
    parsed_url.as_str().parse::<http::Uri>().ok()
}

fn parse_proxy_url(s: &str) -> Option<url::Url> {
    if s.is_empty() {
        return None;
    }
    if let Ok(u) = url::Url::parse(s)
        && !u.scheme().is_empty()
        && u.host().is_some()
    {
        return Some(u);
    }
    let with_http = format!("http://{}", s);
    url::Url::parse(&with_http).ok()
}

fn no_proxy_matches(host: &str, port: u16, no_proxy: &str) -> bool {
    let target_host = host.to_ascii_lowercase();
    let target_ip = target_host.parse::<std::net::IpAddr>().ok();

    for rule in no_proxy.split(',') {
        let rule = rule.trim().to_ascii_lowercase();
        if rule.is_empty() {
            continue;
        }
        if rule == "*" {
            return true;
        }

        // Check CIDR notation
        if let Some((cidr_ip_str, prefix_str)) = rule.split_once('/')
            && let (Ok(cidr_ip), Ok(prefix)) = (cidr_ip_str.parse::<std::net::IpAddr>(), prefix_str.parse::<u8>())
            && let Some(tip) = target_ip
            && ip_in_cidr(tip, cidr_ip, prefix)
        {
            return true;
        }

        // Split host and optional port
        let (rule_host, rule_port) = split_host_port(&rule);

        if let Some(rport) = rule_port
            && rport != port
        {
            continue;
        }

        // IP match
        if let Ok(rip) = rule_host.parse::<std::net::IpAddr>()
            && let Some(tip) = target_ip
            && tip == rip
        {
            return true;
        }

        // Domain match (only applies if target is NOT an IP)
        if target_ip.is_none() {
            let mut domain = rule_host;
            if domain.starts_with("*.") {
                domain = &domain[1..]; // becomes .domain
            }
            if domain.starts_with('.') {
                if target_host.ends_with(domain) {
                    return true;
                }
            } else if target_host == domain || target_host.ends_with(&format!(".{}", domain)) {
                return true;
            }
        }
    }

    false
}

fn split_host_port(s: &str) -> (&str, Option<u16>) {
    if let Some(idx) = s.rfind(':') {
        if let Some(bracket) = s.rfind(']') {
            if idx > bracket {
                let host = &s[..idx];
                let clean_host = host.trim_start_matches('[').trim_end_matches(']');
                let port = s[idx + 1..].parse::<u16>().ok();
                return (clean_host, port);
            }
        } else {
            let host = &s[..idx];
            let port = s[idx + 1..].parse::<u16>().ok();
            return (host, port);
        }
    }
    let clean = s.trim_start_matches('[').trim_end_matches(']');
    (clean, None)
}

fn ip_in_cidr(ip: std::net::IpAddr, cidr_ip: std::net::IpAddr, prefix: u8) -> bool {
    match (ip, cidr_ip) {
        (std::net::IpAddr::V4(ip4), std::net::IpAddr::V4(net4)) => {
            if prefix > 32 {
                return false;
            }
            let mask = if prefix == 0 { 0 } else { !((1u32 << (32 - prefix)) - 1) };
            (u32::from(ip4) & mask) == (u32::from(net4) & mask)
        }
        (std::net::IpAddr::V6(ip6), std::net::IpAddr::V6(net6)) => {
            if prefix > 128 {
                return false;
            }
            let mask = if prefix == 0 {
                0
            } else {
                !((1u128 << (128 - prefix)) - 1)
            };
            (u128::from(ip6) & mask) == (u128::from(net6) & mask)
        }
        _ => false,
    }
}

fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%'
            && let (Some(h1), Some(h2)) = (chars.next(), chars.next())
            && let Ok(val) = u8::from_str_radix(std::str::from_utf8(&[h1, h2]).unwrap_or(""), 16)
        {
            bytes.push(val);
            continue;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn proxy_basic_auth(url: &url::Url) -> Option<String> {
    if !url.username().is_empty() || url.password().is_some() {
        let username = percent_decode(url.username());
        let password = percent_decode(url.password().unwrap_or(""));
        use base64::prelude::*;
        let encoded = BASE64_STANDARD.encode(format!("{}:{}", username, password).as_bytes());
        Some(format!("Basic {}", encoded))
    } else {
        None
    }
}

async fn resolve_host(host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, ConnectError> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![std::net::SocketAddr::new(ip, port)]);
    }
    match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
        Ok(addrs) => {
            let list: Vec<_> = addrs.collect();
            if list.is_empty() {
                Err(ConnectError::Dns(format!("no addresses found for {}:{}", host, port)))
            } else {
                Ok(list)
            }
        }
        Err(e) => Err(ConnectError::Dns(e.to_string())),
    }
}

async fn dial_tcp(addrs: &[std::net::SocketAddr], timeout: Duration) -> Result<tokio::net::TcpStream, ConnectError> {
    let fut = async {
        let mut last_err = None;
        for addr in addrs {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addresses to connect")))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(ConnectError::Tcp(e.to_string())),
        Err(_) => Err(ConnectError::Timeout("tcp")),
    }
}

#[cfg(test)]
static EXTRA_ROOT_CERTS: std::sync::RwLock<Vec<rustls::pki_types::CertificateDer<'static>>> =
    std::sync::RwLock::new(Vec::new());

#[cfg(test)]
pub fn add_test_root_certificate(cert: rustls::pki_types::CertificateDer<'static>) {
    EXTRA_ROOT_CERTS.write().unwrap().push(cert);
}

pub(crate) fn client_config() -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let res = rustls_native_certs::load_native_certs();
    for cert in res.certs {
        let _ = roots.add(cert);
    }
    #[cfg(test)]
    {
        if let Ok(extra) = EXTRA_ROOT_CERTS.read() {
            for cert in extra.iter() {
                let _ = roots.add(cert.clone());
            }
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_safe_default_protocol_versions()
        .expect("safe default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    std::sync::Arc::new(config)
}

async fn tls_handshake<S: AsyncStream>(
    stream: S,
    server_name: &str,
    timeout: Duration,
) -> Result<tokio_rustls::client::TlsStream<S>, ConnectError> {
    let config = client_config();
    let connector = tokio_rustls::TlsConnector::from(config);
    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|e| ConnectError::Tls(format!("invalid server name {}: {}", server_name, e)))?;

    let handshake_fut = connector.connect(server_name, stream);
    match tokio::time::timeout(timeout, handshake_fut).await {
        Ok(Ok(tls_stream)) => Ok(tls_stream),
        Ok(Err(e)) => Err(ConnectError::Tls(e.to_string())),
        Err(_) => Err(ConnectError::Timeout("tls")),
    }
}

async fn http_connect_tunnel<S: AsyncStream>(
    mut stream: S,
    target_host: &str,
    target_port: u16,
    auth_header: Option<&str>,
    timeout: Duration,
) -> Result<S, ConnectError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fut = async {
        let authority = format!("{}:{}", target_host, target_port);
        let mut req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", authority, authority);
        if let Some(auth) = auth_header {
            req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }
        req.push_str("\r\n");

        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| ConnectError::Proxy(e.to_string()))?;
        stream.flush().await.map_err(|e| ConnectError::Proxy(e.to_string()))?;

        let mut resp_bytes = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            if n == 0 {
                return Err(ConnectError::Proxy(
                    "proxy closed connection during CONNECT".to_string(),
                ));
            }
            resp_bytes.push(buf[0]);
            if resp_bytes.ends_with(b"\r\n\r\n") {
                break;
            }
            if resp_bytes.len() > 8192 {
                return Err(ConnectError::Proxy(
                    "proxy CONNECT response headers too large".to_string(),
                ));
            }
        }

        let resp_str = String::from_utf8_lossy(&resp_bytes);
        let mut lines = resp_str.lines();
        let status_line = lines
            .next()
            .ok_or_else(|| ConnectError::Proxy("empty proxy response".to_string()))?;
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(ConnectError::Proxy(format!(
                "malformed proxy status line: {}",
                status_line
            )));
        }
        let status_code: u16 = parts[1]
            .parse()
            .map_err(|_| ConnectError::Proxy(format!("invalid status in proxy response: {}", status_line)))?;
        if status_code != 200 {
            return Err(ConnectError::Proxy(format!(
                "proxy CONNECT failed with status {}",
                status_code
            )));
        }

        Ok(stream)
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(ConnectError::Timeout("proxy")),
    }
}

async fn socks5_handshake<S: AsyncStream>(
    mut stream: S,
    target_host: &str,
    target_port: u16,
    user_info: Option<&(String, String)>,
    timeout: Duration,
) -> Result<S, ConnectError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fut = async {
        // 1. Greeting
        if let Some((user, pass)) = user_info {
            stream
                .write_all(&[0x05, 0x02, 0x00, 0x02])
                .await
                .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            stream.flush().await.map_err(|e| ConnectError::Proxy(e.to_string()))?;
            let mut resp = [0u8; 2];
            stream
                .read_exact(&mut resp)
                .await
                .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            if resp[0] != 0x05 {
                return Err(ConnectError::Proxy(format!("unexpected socks5 version: {}", resp[0])));
            }
            if resp[1] == 0xFF {
                return Err(ConnectError::Proxy("no acceptable authentication methods".to_string()));
            }
            if resp[1] == 0x02 {
                let mut auth_pkt = Vec::with_capacity(3 + user.len() + pass.len());
                auth_pkt.push(0x01);
                auth_pkt.push(user.len() as u8);
                auth_pkt.extend_from_slice(user.as_bytes());
                auth_pkt.push(pass.len() as u8);
                auth_pkt.extend_from_slice(pass.as_bytes());
                stream
                    .write_all(&auth_pkt)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
                stream.flush().await.map_err(|e| ConnectError::Proxy(e.to_string()))?;

                let mut auth_resp = [0u8; 2];
                stream
                    .read_exact(&mut auth_resp)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
                if auth_resp[0] != 0x01 || auth_resp[1] != 0x00 {
                    return Err(ConnectError::Proxy(format!(
                        "socks5 username/password auth failed (status {})",
                        auth_resp[1]
                    )));
                }
            } else if resp[1] != 0x00 {
                return Err(ConnectError::Proxy(format!(
                    "unsupported auth method selected: {}",
                    resp[1]
                )));
            }
        } else {
            stream
                .write_all(&[0x05, 0x01, 0x00])
                .await
                .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            stream.flush().await.map_err(|e| ConnectError::Proxy(e.to_string()))?;
            let mut resp = [0u8; 2];
            stream
                .read_exact(&mut resp)
                .await
                .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            if resp[0] != 0x05 {
                return Err(ConnectError::Proxy(format!("unexpected socks5 version: {}", resp[0])));
            }
            if resp[1] != 0x00 {
                return Err(ConnectError::Proxy("no acceptable authentication methods".to_string()));
            }
        }

        // 2. CONNECT command
        let mut cmd = Vec::with_capacity(7 + target_host.len());
        cmd.extend_from_slice(&[0x05, 0x01, 0x00]);
        if let Ok(ip) = target_host.parse::<std::net::IpAddr>() {
            match ip {
                std::net::IpAddr::V4(v4) => {
                    cmd.push(0x01);
                    cmd.extend_from_slice(&v4.octets());
                }
                std::net::IpAddr::V6(v6) => {
                    cmd.push(0x04);
                    cmd.extend_from_slice(&v6.octets());
                }
            }
        } else {
            cmd.push(0x03);
            cmd.push(target_host.len() as u8);
            cmd.extend_from_slice(target_host.as_bytes());
        }
        cmd.push((target_port >> 8) as u8);
        cmd.push((target_port & 0xFF) as u8);

        stream
            .write_all(&cmd)
            .await
            .map_err(|e| ConnectError::Proxy(e.to_string()))?;
        stream.flush().await.map_err(|e| ConnectError::Proxy(e.to_string()))?;

        // 3. Response
        let mut head = [0u8; 4];
        stream
            .read_exact(&mut head)
            .await
            .map_err(|e| ConnectError::Proxy(e.to_string()))?;
        if head[0] != 0x05 {
            return Err(ConnectError::Proxy(format!(
                "unexpected socks5 reply version: {}",
                head[0]
            )));
        }
        let rep = head[1];
        if rep != 0x00 {
            return Err(ConnectError::Proxy(format!("socks5 proxy error: 0x{:02x}", rep)));
        }
        let atyp = head[3];
        match atyp {
            0x01 => {
                let mut b = [0u8; 6];
                stream
                    .read_exact(&mut b)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            }
            0x04 => {
                let mut b = [0u8; 18];
                stream
                    .read_exact(&mut b)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            }
            0x03 => {
                let mut len = [0u8; 1];
                stream
                    .read_exact(&mut len)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
                let mut b = vec![0u8; len[0] as usize + 2];
                stream
                    .read_exact(&mut b)
                    .await
                    .map_err(|e| ConnectError::Proxy(e.to_string()))?;
            }
            _ => return Err(ConnectError::Proxy(format!("unknown socks5 address type: {}", atyp))),
        }

        Ok(stream)
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(ConnectError::Timeout("proxy")),
    }
}

async fn connect_stream(uri: &http::Uri) -> Result<UpstreamStream, ConnectError> {
    let scheme = uri.scheme_str().unwrap_or("http");
    let host = uri
        .host()
        .ok_or_else(|| ConnectError::Tcp("missing host in URI".to_string()))?;
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });

    let maybe_proxy = proxy_for(uri);

    match maybe_proxy {
        None => {
            let addrs = resolve_host(clean_host, port).await?;
            let tcp = dial_tcp(&addrs, Duration::from_secs(30)).await?;
            if scheme == "https" {
                let tls = tls_handshake(tcp, clean_host, Duration::from_secs(10)).await?;
                Ok(UpstreamStream::new(tls, false))
            } else {
                Ok(UpstreamStream::new(tcp, false))
            }
        }
        Some(proxy_uri) => {
            let proxy_str = proxy_uri.to_string();
            let proxy_url = url::Url::parse(&proxy_str).map_err(|e| ConnectError::Proxy(e.to_string()))?;
            let p_scheme = proxy_url.scheme();
            if p_scheme != "http" && p_scheme != "https" && p_scheme != "socks5" && p_scheme != "socks5h" {
                return Err(ConnectError::Proxy(format!("unsupported proxy scheme: {}", p_scheme)));
            }

            let p_host = proxy_url
                .host_str()
                .ok_or_else(|| ConnectError::Proxy("missing proxy host".to_string()))?;
            let clean_p_host = p_host.trim_start_matches('[').trim_end_matches(']');
            let p_port = proxy_url.port().unwrap_or(if p_scheme == "https" {
                443
            } else if p_scheme == "socks5" || p_scheme == "socks5h" {
                1080
            } else {
                80
            });

            let p_addrs = resolve_host(clean_p_host, p_port).await?;
            let p_tcp = dial_tcp(&p_addrs, Duration::from_secs(30)).await?;

            if p_scheme == "socks5" || p_scheme == "socks5h" {
                let user_info = if !proxy_url.username().is_empty() || proxy_url.password().is_some() {
                    Some((
                        percent_decode(proxy_url.username()),
                        percent_decode(proxy_url.password().unwrap_or("")),
                    ))
                } else {
                    None
                };
                let s5_stream =
                    socks5_handshake(p_tcp, clean_host, port, user_info.as_ref(), Duration::from_secs(30)).await?;

                if scheme == "https" {
                    let tls = tls_handshake(s5_stream, clean_host, Duration::from_secs(10)).await?;
                    Ok(UpstreamStream::new(tls, false))
                } else {
                    Ok(UpstreamStream::new(s5_stream, false))
                }
            } else {
                let p_stream: Box<dyn AsyncStream> = if p_scheme == "https" {
                    let tls = tls_handshake(p_tcp, clean_p_host, Duration::from_secs(10)).await?;
                    Box::new(tls)
                } else {
                    Box::new(p_tcp)
                };

                if scheme == "https" {
                    let auth_header = proxy_basic_auth(&proxy_url);
                    let tunnel = http_connect_tunnel(
                        p_stream,
                        clean_host,
                        port,
                        auth_header.as_deref(),
                        Duration::from_secs(30),
                    )
                    .await?;
                    let tls = tls_handshake(tunnel, clean_host, Duration::from_secs(10)).await?;
                    Ok(UpstreamStream::new(tls, false))
                } else {
                    Ok(UpstreamStream::new(p_stream, true))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        vars: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for (k, v) in vars {
                saved.push((*k, std::env::var(k).ok()));
                // SAFETY: TEST_LOCK held for duration of test
                unsafe { std::env::set_var(k, v) };
            }
            Self {
                _lock: lock,
                vars: saved,
            }
        }

        fn clear(keys: &[&'static str]) -> Self {
            let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for k in keys {
                saved.push((*k, std::env::var(k).ok()));
                // SAFETY: TEST_LOCK held for duration of test
                unsafe { std::env::remove_var(k) };
            }
            Self {
                _lock: lock,
                vars: saved,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.vars {
                match v {
                    // SAFETY: TEST_LOCK held
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    #[tokio::test]
    async fn connect_refused_is_connect_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let uri = format!("http://127.0.0.1:{}", port).parse::<http::Uri>().unwrap();
        let res = connect(&uri).await;
        match res {
            Err(ConnectError::Tcp(_)) => {}
            other => panic!("expected Err(ConnectError::Tcp), got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn connect_ok_then_reset_is_send_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drop(socket); // Immediately drop socket
            }
        });

        let uri = format!("http://127.0.0.1:{}", port).parse::<http::Uri>().unwrap();
        let connected = connect(&uri).await.expect("connect must succeed");

        let req = http::Request::builder()
            .method("POST")
            .uri("/")
            .body(Full::new(Bytes::from("test")))
            .unwrap();

        let res = connected.send(req).await;
        match res {
            Err(SendError::Io(_)) => {}
            other => panic!("expected Err(SendError::Io), got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn tls_handshake_failure_is_connect_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"not a valid tls handshake\r\n\r\n").await;
            }
        });

        let uri = format!("https://127.0.0.1:{}", port).parse::<http::Uri>().unwrap();
        let res = connect(&uri).await;
        match res {
            Err(ConnectError::Tls(_)) => {}
            other => panic!("expected Err(ConnectError::Tls), got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn connect_tunnel_through_http_proxy() {
        // Generate self-signed certificate for upstream.test
        let certified = rcgen::generate_simple_self_signed(vec!["upstream.test".to_string()]).unwrap();
        add_test_root_certificate(certified.cert.der().clone());

        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let server_config =
            rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key_der)
                .unwrap();
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

        // Fake upstream server speaking TLS
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((socket, _)) = upstream_listener.accept().await {
                let acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(tls_socket) = acceptor.accept(socket).await {
                        let service = hyper::service::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(200)
                                    .body(Full::new(Bytes::from("hello from upstream")))
                                    .unwrap(),
                            )
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(tls_socket), service)
                            .await;
                    }
                });
            }
        });

        // Fake HTTP proxy
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let connects_seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connects_clone = connects_seen.clone();

        tokio::spawn(async move {
            while let Ok((mut client_sock, _)) = proxy_listener.accept().await {
                let connects = connects_clone.clone();
                tokio::spawn(async move {
                    let mut line_buf = Vec::new();
                    let mut b = [0u8; 1];
                    loop {
                        if client_sock.read(&mut b).await.unwrap_or(0) == 0 {
                            return;
                        }
                        line_buf.push(b[0]);
                        if line_buf.ends_with(b"\r\n") {
                            break;
                        }
                    }
                    let line = String::from_utf8_lossy(&line_buf);

                    if line.starts_with("CONNECT ") {
                        let expected_authority = format!("upstream.test:{}", upstream_port);
                        if line.contains(&expected_authority) {
                            connects.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        // Consume headers
                        let mut hdr_buf = Vec::new();
                        loop {
                            if client_sock.read(&mut b).await.unwrap_or(0) == 0 {
                                break;
                            }
                            hdr_buf.push(b[0]);
                            if hdr_buf.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let _ = client_sock
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .await;

                        // Splice to upstream server
                        if let Ok(mut up_sock) =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{}", upstream_port)).await
                        {
                            let _ = tokio::io::copy_bidirectional(&mut client_sock, &mut up_sock).await;
                        }
                    } else if line.starts_with("GET http://upstream.test:") {
                        // Absolute form request for plain HTTP target
                        let mut hdr_buf = Vec::new();
                        loop {
                            if client_sock.read(&mut b).await.unwrap_or(0) == 0 {
                                break;
                            }
                            hdr_buf.push(b[0]);
                            if hdr_buf.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let body = "absolute-ok\r\n";
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = client_sock.write_all(resp.as_bytes()).await;
                        let _ = client_sock.flush().await;
                    }
                });
            }
        });

        let proxy_url = format!("http://127.0.0.1:{}", proxy_port);
        let _guard = EnvGuard::set(&[
            ("HTTPS_PROXY", &proxy_url),
            ("HTTP_PROXY", &proxy_url),
            ("NO_PROXY", ""),
        ]);

        // Case 1: https target over HTTPS_PROXY
        let target_uri = format!("https://upstream.test:{}/v1/generate", upstream_port)
            .parse::<http::Uri>()
            .unwrap();
        let connected = connect(&target_uri).await.expect("connect via proxy tunnel failed");
        let req = http::Request::builder()
            .method("POST")
            .uri("/v1/generate")
            .header("host", "upstream.test")
            .body(Full::new(Bytes::from("hello")))
            .unwrap();
        let resp = connected.send(req).await.expect("send failed");
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello from upstream");
        assert_eq!(connects_seen.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Case 2: http target over HTTP_PROXY sends absolute form (GET http://upstream.test:.../x)
        let http_target_uri = format!("http://upstream.test:{}/x", upstream_port)
            .parse::<http::Uri>()
            .unwrap();
        let connected_http = connect(&http_target_uri).await.expect("connect http via proxy failed");
        let http_req = http::Request::builder()
            .method("GET")
            .uri("/x")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let http_resp = connected_http.send(http_req).await.expect("send http failed");
        assert_eq!(http_resp.status().as_u16(), 200);
        let http_body = http_resp.into_body().collect().await.unwrap().to_bytes();
        assert!(http_body.starts_with(b"absolute-ok"));
    }

    #[tokio::test]
    async fn loopback_never_proxied() {
        let fake_proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = fake_proxy.local_addr().unwrap().port();
        let proxy_connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p_conn = proxy_connections.clone();

        tokio::spawn(async move {
            while let Ok((_sock, _)) = fake_proxy.accept().await {
                p_conn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let target_server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target_server.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = target_server.accept().await {
                let service = hyper::service::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from("direct-ok")))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(sock), service)
                    .await;
            }
        });

        let proxy_url = format!("http://127.0.0.1:{}", proxy_port);
        let _guard = EnvGuard::set(&[
            ("HTTP_PROXY", &proxy_url),
            ("HTTPS_PROXY", &proxy_url),
            ("NO_PROXY", ""),
        ]);

        let target_uri = format!("http://127.0.0.1:{}/direct", target_port)
            .parse::<http::Uri>()
            .unwrap();
        assert!(proxy_for(&target_uri).is_none());

        let connected = connect(&target_uri).await.expect("connect direct failed");
        let req = http::Request::builder()
            .method("GET")
            .uri("/direct")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = connected.send(req).await.expect("send failed");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(proxy_connections.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn no_proxy_bypasses_tunnel() {
        let fake_proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = fake_proxy.local_addr().unwrap().port();
        let proxy_connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p_conn = proxy_connections.clone();

        tokio::spawn(async move {
            while let Ok((_sock, _)) = fake_proxy.accept().await {
                p_conn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let proxy_url = format!("http://127.0.0.1:{}", proxy_port);
        let _guard = EnvGuard::set(&[("HTTPS_PROXY", &proxy_url), ("NO_PROXY", "upstream.test")]);

        let target_uri = "https://upstream.test:1234/test".parse::<http::Uri>().unwrap();
        assert!(proxy_for(&target_uri).is_none());

        let res = connect(&target_uri).await;
        match res {
            Err(ConnectError::Dns(_)) => {}
            other => panic!("expected Err(ConnectError::Dns), got {:?}", other.map(|_| ())),
        }
        assert_eq!(proxy_connections.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn http_proxy_basic_auth() {
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let seen_auth = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_auth_clone = seen_auth.clone();

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = proxy_listener.accept().await {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                loop {
                    if sock.read(&mut b).await.unwrap_or(0) == 0 {
                        break;
                    }
                    buf.push(b[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let req_text = String::from_utf8_lossy(&buf);
                for line in req_text.lines() {
                    if let Some(val) = line.strip_prefix("Proxy-Authorization: ") {
                        *seen_auth_clone.lock().unwrap() = Some(val.to_string());
                    }
                }
                let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
            }
        });

        // user:p%40ss -> user:p@ss -> Basic dXNlcjpwQHNz
        let proxy_url = format!("http://user:p%40ss@127.0.0.1:{}", proxy_port);
        let _guard = EnvGuard::set(&[("HTTPS_PROXY", &proxy_url), ("NO_PROXY", "")]);

        let target_uri = "https://upstream.test:9999/x".parse::<http::Uri>().unwrap();
        let _ = connect(&target_uri).await; // TLS handshake over raw socket will fail after tunnel, but CONNECT already occurred
        let auth = seen_auth.lock().unwrap().clone();
        assert_eq!(auth.as_deref(), Some("Basic dXNlcjpwQHNz"));
    }

    #[tokio::test]
    async fn https_proxy_tls_to_proxy() {
        // Self-signed certificate for localhost
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        add_test_root_certificate(certified.cert.der().clone());

        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let server_config =
            rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key_der)
                .unwrap();
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let seen_connect = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_connect_clone = seen_connect.clone();

        tokio::spawn(async move {
            if let Ok((sock, _)) = proxy_listener.accept().await
                && let Ok(mut tls_sock) = tls_acceptor.accept(sock).await
            {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                loop {
                    if tls_sock.read(&mut b).await.unwrap_or(0) == 0 {
                        break;
                    }
                    buf.push(b[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let req_text = String::from_utf8_lossy(&buf);
                if req_text.starts_with("CONNECT upstream.test:8888") {
                    seen_connect_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = tls_sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
            }
        });

        let proxy_url = format!("https://localhost:{}", proxy_port);
        let _guard = EnvGuard::set(&[("HTTPS_PROXY", &proxy_url), ("NO_PROXY", "")]);

        let target_uri = "https://upstream.test:8888/test".parse::<http::Uri>().unwrap();
        let _ = connect(&target_uri).await;
        assert!(seen_connect.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn socks5_remote_dns_and_auth() {
        // Fake upstream server
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = upstream.accept().await {
                let service = hyper::service::service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from("socks5-ok")))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(sock), service)
                    .await;
            }
        });

        // Fake SOCKS5 server
        let socks_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_port = socks_listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut client, _)) = socks_listener.accept().await {
                tokio::spawn(async move {
                    let mut head = [0u8; 2];
                    client.read_exact(&mut head).await.unwrap();
                    assert_eq!(head[0], 0x05); // SOCKS5
                    let nmethods = head[1] as usize;
                    let mut methods = vec![0u8; nmethods];
                    client.read_exact(&mut methods).await.unwrap();

                    // Go offers exactly [NotRequired, UsernamePassword] when the URL has userinfo, else [NotRequired].
                    // A panic here drops the socket, so the client side of the test fails too.
                    if methods.contains(&0x02) {
                        assert_eq!(&methods[..], &[0x00, 0x02]);
                        client.write_all(&[0x05, 0x02]).await.unwrap();
                        let mut auth_head = [0u8; 2];
                        client.read_exact(&mut auth_head).await.unwrap();
                        assert_eq!(auth_head[0], 0x01); // RFC 1929 subnegotiation ver 1
                        let ulen = auth_head[1] as usize;
                        let mut user = vec![0u8; ulen];
                        client.read_exact(&mut user).await.unwrap();
                        let mut plen = [0u8; 1];
                        client.read_exact(&mut plen).await.unwrap();
                        let mut pass = vec![0u8; plen[0] as usize];
                        client.read_exact(&mut pass).await.unwrap();
                        assert_eq!(&user[..], b"u");
                        assert_eq!(&pass[..], b"pw");
                        // Auth success
                        client.write_all(&[0x01, 0x00]).await.unwrap();
                    } else {
                        assert_eq!(&methods[..], &[0x00]);
                        client.write_all(&[0x05, 0x00]).await.unwrap();
                    }

                    // CONNECT request
                    let mut cmd = [0u8; 4];
                    client.read_exact(&mut cmd).await.unwrap();
                    assert_eq!(cmd[0], 0x05);
                    assert_eq!(cmd[1], 0x01); // CONNECT
                    assert_eq!(cmd[3], 0x03); // ATYP Domain
                    let mut dlen = [0u8; 1];
                    client.read_exact(&mut dlen).await.unwrap();
                    let mut domain = vec![0u8; dlen[0] as usize];
                    client.read_exact(&mut domain).await.unwrap();
                    assert_eq!(&domain[..], b"upstream.test");
                    let mut port_bytes = [0u8; 2];
                    client.read_exact(&mut port_bytes).await.unwrap();
                    let port = u16::from_be_bytes(port_bytes);
                    assert_eq!(port, upstream_port);

                    // CONNECT reply success
                    client
                        .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                        .await
                        .unwrap();

                    // Splice to upstream
                    let mut up = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", upstream_port))
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                });
            }
        });

        // Case (a): socks5://
        {
            let proxy_url = format!("socks5://127.0.0.1:{}", socks_port);
            let _guard = EnvGuard::set(&[("HTTP_PROXY", &proxy_url), ("NO_PROXY", "")]);
            let uri = format!("http://upstream.test:{}/test", upstream_port)
                .parse::<http::Uri>()
                .unwrap();
            let connected = connect(&uri).await.expect("socks5 connect failed");
            let req = http::Request::builder()
                .method("GET")
                .uri("/test")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let resp = connected.send(req).await.expect("socks5 send failed");
            assert_eq!(resp.status().as_u16(), 200);
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], b"socks5-ok");
        }

        // Case (b): socks5h://
        {
            let proxy_url = format!("socks5h://127.0.0.1:{}", socks_port);
            let _guard = EnvGuard::set(&[("HTTP_PROXY", &proxy_url), ("NO_PROXY", "")]);
            let uri = format!("http://upstream.test:{}/test", upstream_port)
                .parse::<http::Uri>()
                .unwrap();
            let connected = connect(&uri).await.expect("socks5h connect failed");
            let req = http::Request::builder()
                .method("GET")
                .uri("/test")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let resp = connected.send(req).await.expect("socks5h send failed");
            assert_eq!(resp.status().as_u16(), 200);
        }

        // Case (c): socks5://u:pw@
        {
            let proxy_url = format!("socks5://u:pw@127.0.0.1:{}", socks_port);
            let _guard = EnvGuard::set(&[("HTTP_PROXY", &proxy_url), ("NO_PROXY", "")]);
            let uri = format!("http://upstream.test:{}/test", upstream_port)
                .parse::<http::Uri>()
                .unwrap();
            let connected = connect(&uri).await.expect("socks5 auth connect failed");
            let req = http::Request::builder()
                .method("GET")
                .uri("/test")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let resp = connected.send(req).await.expect("socks5 auth send failed");
            assert_eq!(resp.status().as_u16(), 200);
        }

        // Case (d): no port in proxy URL -> client dials port 1080
        {
            let test_uri = "http://upstream.test/test".parse::<http::Uri>().unwrap();
            let _guard = EnvGuard::set(&[("HTTP_PROXY", "socks5://127.0.0.1"), ("NO_PROXY", "")]);
            let p = proxy_for(&test_uri).expect("proxy_for should return proxy");
            assert_eq!(p.host(), Some("127.0.0.1"));

            if let Ok(listener_1080) = tokio::net::TcpListener::bind("127.0.0.1:1080").await {
                tokio::spawn(async move {
                    let _ = listener_1080.accept().await;
                });
                let _ = connect(&test_uri).await;
            } else {
                // If 1080 is not bindable, assert parsed default port logic
                let u = url::Url::parse(&p.to_string()).unwrap();
                let port = u.port().unwrap_or(1080);
                assert_eq!(port, 1080);
            }
        }
    }

    #[tokio::test]
    async fn socks5_proxy_refusal_is_connect_error() {
        // Server 1: answers REP 0x05 (connection refused)
        let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut client, _)) = listener1.accept().await {
                let mut b = [0u8; 3];
                let _ = client.read_exact(&mut b).await;
                let _ = client.write_all(&[0x05, 0x00]).await;
                let _ = client.flush().await;
                let mut cmd = [0u8; 4];
                let _ = client.read_exact(&mut cmd).await;
                // Reply with 0x05 error
                let _ = client.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
                let _ = client.flush().await;
            }
        });

        let proxy1_url = format!("socks5://127.0.0.1:{}", port1);
        let _guard1 = EnvGuard::set(&[("HTTP_PROXY", &proxy1_url), ("NO_PROXY", "")]);
        let uri1 = "http://upstream.test:80/x".parse::<http::Uri>().unwrap();
        let res1 = connect(&uri1).await;
        match res1 {
            Err(ConnectError::Proxy(_)) => {}
            other => panic!("expected Err(ConnectError::Proxy), got {:?}", other.map(|_| ())),
        }
        drop(_guard1);

        // Server 2: rejects auth (status 0x01)
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut client, _)) = listener2.accept().await {
                let mut b = [0u8; 4];
                let _ = client.read_exact(&mut b).await;
                let _ = client.write_all(&[0x05, 0x02]).await; // Pick user/pass
                let _ = client.flush().await;
                let mut auth_req = [0u8; 10];
                let _ = client.read(&mut auth_req).await;
                let _ = client.write_all(&[0x01, 0x01]).await; // Auth fail
                let _ = client.flush().await;
            }
        });

        let proxy2_url = format!("socks5://u:p@127.0.0.1:{}", port2);
        let _guard2 = EnvGuard::set(&[("HTTP_PROXY", &proxy2_url), ("NO_PROXY", "")]);
        let uri2 = "http://upstream.test:80/x".parse::<http::Uri>().unwrap();
        let res2 = connect(&uri2).await;
        match res2 {
            Err(ConnectError::Proxy(_)) => {}
            other => panic!("expected Err(ConnectError::Proxy), got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn unknown_proxy_scheme_rejected() {
        let _guard = EnvGuard::set(&[("HTTPS_PROXY", "ftp://h:1"), ("NO_PROXY", "")]);
        let uri = "https://upstream.test:443/x".parse::<http::Uri>().unwrap();
        let res = connect(&uri).await;
        match res {
            Err(ConnectError::Proxy(msg)) => {
                assert!(msg.contains("ftp"), "expected error to name ftp, got: {}", msg);
            }
            other => panic!("expected Err(ConnectError::Proxy), got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn generation_connection_not_reused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = count.clone();

        tokio::spawn(async move {
            while let Ok((_sock, _)) = listener.accept().await {
                count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let _guard = EnvGuard::clear(&["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"]);
        let uri = format!("http://127.0.0.1:{}", port).parse::<http::Uri>().unwrap();

        let _conn1 = connect(&uri).await.expect("connect 1 failed");
        let _conn2 = connect(&uri).await.expect("connect 2 failed");

        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn is_timeout_classification() {
        assert!(is_timeout(&SendError::Timeout));
        assert!(is_timeout(&SendError::Io("connection timed out".to_string())));
        assert!(is_timeout(&SendError::Io("request deadline has elapsed".to_string())));
        assert!(!is_timeout(&SendError::Io("connection reset by peer".to_string())));
        assert!(!is_timeout(&SendError::Malformed("invalid chunk size".to_string())));
    }
}
