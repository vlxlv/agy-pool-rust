use bytes::Bytes;
use hyper::body::{Body, Frame};
use hyper::http::{Response, StatusCode};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::Interval;

use crate::proxy::body::ProxyBody;

/// Heartbeat chunk sent periodically to keep active SSE streams alive.
pub const SSE_HEARTBEAT_CHUNK: &[u8] = b": keep-alive\n\n";

/// Formats an error into a standard SSE error frame.
pub fn format_sse_error(status: StatusCode, body_bytes: &[u8]) -> Bytes {
    let trimmed = std::str::from_utf8(body_bytes).unwrap_or("").trim();
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Ok(compact) = serde_json::to_string(&val)
    {
        return Bytes::from(format!("data: {compact}\n\n"));
    }
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Error");
    let msg = if trimmed.is_empty() { reason } else { trimmed };
    let err_obj = serde_json::json!({
        "error": {
            "code": code,
            "message": msg,
            "status": reason
        }
    });
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(&err_obj).unwrap_or_else(|_| "{}".to_string())
    ))
}

pub enum KeepAlivePhase {
    InitialHeartbeat {
        work_rx: oneshot::Receiver<Response<ProxyBody>>,
        interval: Interval,
    },
    Waiting {
        work_rx: oneshot::Receiver<Response<ProxyBody>>,
        interval: Interval,
    },
    ReadingErrorBody {
        status: StatusCode,
        inner: ProxyBody,
        collected: Vec<u8>,
    },
    StreamingPayload {
        inner: ProxyBody,
    },
    EmittedTerminal,
}

/// A streaming body that periodically yields `: keep-alive\n\n` SSE comments while waiting
/// for upstream request processing to complete, and then transparently delegates to the
/// real upstream payload stream once ready.
pub struct KeepAliveBody {
    phase: KeepAlivePhase,
    abort_handle: Option<AbortHandle>,
}

impl KeepAliveBody {
    pub fn new(
        work_rx: oneshot::Receiver<Response<ProxyBody>>,
        interval_duration: Duration,
        abort_handle: Option<AbortHandle>,
    ) -> Self {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + interval_duration, interval_duration);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            phase: KeepAlivePhase::InitialHeartbeat { work_rx, interval },
            abort_handle,
        }
    }
}

impl Drop for KeepAliveBody {
    fn drop(&mut self) {
        if let Some(ref handle) = self.abort_handle {
            handle.abort();
        }
    }
}

impl Body for KeepAliveBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        loop {
            match &mut self.phase {
                KeepAlivePhase::InitialHeartbeat { .. } => {
                    if let KeepAlivePhase::InitialHeartbeat { work_rx, interval } =
                        std::mem::replace(&mut self.phase, KeepAlivePhase::EmittedTerminal)
                    {
                        self.phase = KeepAlivePhase::Waiting { work_rx, interval };
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(SSE_HEARTBEAT_CHUNK)))));
                    }
                }
                KeepAlivePhase::Waiting { work_rx, interval } => match Pin::new(&mut *work_rx).poll(cx) {
                    Poll::Ready(Ok(resp)) => {
                        self.abort_handle = None;
                        let status = resp.status();
                        if status.is_success() {
                            self.phase = KeepAlivePhase::StreamingPayload {
                                inner: resp.into_body(),
                            };
                            continue;
                        } else {
                            self.phase = KeepAlivePhase::ReadingErrorBody {
                                status,
                                inner: resp.into_body(),
                                collected: Vec::new(),
                            };
                            continue;
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        self.abort_handle = None;
                        self.phase = KeepAlivePhase::EmittedTerminal;
                        let error_chunk = "data: {\"error\":{\"code\":500,\"message\":\"Gateway request aborted\",\"status\":\"INTERNAL\"}}\n\n";
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(error_chunk.as_bytes())))));
                    }
                    Poll::Pending => {
                        if interval.poll_tick(cx).is_ready() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(SSE_HEARTBEAT_CHUNK)))));
                        }
                        return Poll::Pending;
                    }
                },
                KeepAlivePhase::ReadingErrorBody {
                    status,
                    inner,
                    collected,
                } => match Pin::new(inner).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        if frame.is_data() {
                            collected.extend_from_slice(&frame.into_data().unwrap());
                        }
                        continue;
                    }
                    Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                        let stat = *status;
                        let bytes = std::mem::take(collected);
                        self.phase = KeepAlivePhase::EmittedTerminal;
                        return Poll::Ready(Some(Ok(Frame::data(format_sse_error(stat, &bytes)))));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                KeepAlivePhase::StreamingPayload { inner } => {
                    return Pin::new(inner).poll_frame(cx);
                }
                KeepAlivePhase::EmittedTerminal => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use hyper::http::StatusCode;

    #[tokio::test]
    async fn test_keep_alive_body_yields_heartbeat_then_payload() {
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let resp = Response::builder()
                .status(StatusCode::OK)
                .body(crate::proxy::body::full_body(Bytes::from_static(
                    b"data: {\"chunk\":1}\n\n",
                )))
                .unwrap();
            let _ = tx.send(resp);
        });
        let body = KeepAliveBody::new(rx, Duration::from_millis(100), Some(handle.abort_handle()));

        let mut collected_chunks = Vec::new();
        let mut pinned = std::pin::pin!(body);
        while let Some(frame_res) = pinned.as_mut().frame().await {
            let frame = frame_res.unwrap();
            if frame.is_data() {
                collected_chunks.push(frame.into_data().unwrap());
            }
        }

        assert_eq!(collected_chunks[0], Bytes::from_static(SSE_HEARTBEAT_CHUNK));
        assert_eq!(collected_chunks[1], Bytes::from_static(b"data: {\"chunk\":1}\n\n"));
    }

    #[tokio::test]
    async fn test_keep_alive_body_error_response_emits_sse_error() {
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let resp = Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(crate::proxy::body::full_body(Bytes::from_static(
                    b"all accounts exhausted",
                )))
                .unwrap();
            let _ = tx.send(resp);
        });
        let body = KeepAliveBody::new(rx, Duration::from_millis(100), Some(handle.abort_handle()));

        let mut collected_chunks = Vec::new();
        let mut pinned = std::pin::pin!(body);
        while let Some(frame_res) = pinned.as_mut().frame().await {
            let frame = frame_res.unwrap();
            if frame.is_data() {
                collected_chunks.push(frame.into_data().unwrap());
            }
        }

        assert_eq!(collected_chunks[0], Bytes::from_static(SSE_HEARTBEAT_CHUNK));
        let error_str = String::from_utf8_lossy(&collected_chunks[1]);
        assert!(error_str.contains("503"));
        assert!(error_str.contains("all accounts exhausted"));
    }

    #[test]
    fn test_format_sse_error_compacts_multiline_json() {
        let multiline = b"{\n  \"error\": {\n    \"code\": 404,\n    \"message\": \"model not found\"\n  }\n}";
        let formatted = format_sse_error(StatusCode::NOT_FOUND, multiline);
        let s = std::str::from_utf8(&formatted).unwrap();
        assert!(s.starts_with("data: "));
        assert!(s.ends_with("\n\n"));
        // Ensure no un-prefixed newlines in data payload (exactly 1 data line)
        assert_eq!(s.trim().lines().count(), 1);
        assert!(s.contains("\"code\":404"));
        assert!(s.contains("\"message\":\"model not found\""));
    }

    #[test]
    fn test_format_sse_error_plain_text() {
        let plain = b"internal error occurred";
        let formatted = format_sse_error(StatusCode::INTERNAL_SERVER_ERROR, plain);
        let s = std::str::from_utf8(&formatted).unwrap();
        assert!(s.starts_with("data: "));
        assert!(s.ends_with("\n\n"));
        let v: serde_json::Value = serde_json::from_str(s.strip_prefix("data: ").unwrap().trim()).unwrap();
        assert_eq!(v["error"]["code"], 500);
        assert_eq!(v["error"]["message"], "internal error occurred");
    }
}
