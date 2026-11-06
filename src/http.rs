use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client as HyperClient;

pub type Client = HyperClient<crate::upstream::ProxyConnector, Full<Bytes>>;

pub fn client() -> &'static Client {
    crate::upstream::pooled_client()
}

#[derive(Debug)]
pub enum HttpError {
    InvalidUrl(String),
    Build(String),
    Network(String),
    Timeout,
    Body(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::InvalidUrl(s) => write!(f, "invalid URL: {}", s),
            HttpError::Build(s) => write!(f, "request build error: {}", s),
            HttpError::Network(s) => write!(f, "network error: {}", s),
            HttpError::Timeout => write!(f, "request timed out"),
            HttpError::Body(s) => write!(f, "body read error: {}", s),
        }
    }
}

impl std::error::Error for HttpError {}

pub async fn post_form(url: &str, form: &[(&str, &str)], timeout: Duration) -> Result<(u16, Vec<u8>), HttpError> {
    let body_str = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let body_bytes = Bytes::from(body_str);
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Full::new(body_bytes))
        .map_err(|e| HttpError::Build(e.to_string()))?;

    execute_request(req, timeout).await
}

pub async fn get_json(url: &str, bearer: Option<&str>, timeout: Duration) -> Result<(u16, Vec<u8>), HttpError> {
    let mut builder = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(url)
        .header(hyper::header::ACCEPT, "application/json");

    if let Some(token) = bearer {
        builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {}", token));
    }

    let req = builder
        .body(Full::new(Bytes::new()))
        .map_err(|e| HttpError::Build(e.to_string()))?;

    execute_request(req, timeout).await
}

pub async fn post_json(
    url: &str,
    bearer: Option<&str>,
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, Vec<u8>), HttpError> {
    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json");

    if let Some(token) = bearer {
        builder = builder.header(hyper::header::AUTHORIZATION, format!("Bearer {}", token));
    }

    let req = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| HttpError::Build(e.to_string()))?;

    execute_request(req, timeout).await
}

async fn execute_request(req: hyper::Request<Full<Bytes>>, timeout: Duration) -> Result<(u16, Vec<u8>), HttpError> {
    let fut = async {
        let resp = client()
            .request(req)
            .await
            .map_err(|e| HttpError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body_bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| HttpError::Body(e.to_string()))?
            .to_bytes()
            .to_vec();
        Ok::<_, HttpError>((status, body_bytes))
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(HttpError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestServer;

    #[tokio::test]
    async fn post_form_round_trip() {
        let srv = TestServer::start(|_req| {
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                br#"{"ok":true}"#.to_vec(),
            )
        });

        let url = format!("{}/token", srv.url());
        let (status, body) = post_form(&url, &[("a", "1")], Duration::from_secs(5))
            .await
            .expect("post_form failed");

        assert_eq!(status, 200);
        assert_eq!(body, br#"{"ok":true}"#);

        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/token");
        assert_eq!(req.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(req.body, b"a=1");
    }
}
