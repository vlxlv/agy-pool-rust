# Porting Notes

This document records Go tests and internals that are not ported 1:1 into the Rust codebase, explaining why each does not directly apply to Rust and documenting the covering Rust test.

## internal/proxy/transport_test.go

### 1. TestTransportFailure_Timeout_NoReplay
- **Reason**: Tests full HTTP proxy routing handler, token refresher, and account failover loop when an upstream timeout occurs.
- **Covering Rust test**: `upstream::tests::is_timeout_classification` at the transport layer, and `proxy::tests::transport_failure_timeout_no_replay` for handler failover.

### 2. TestTransportFailure_ConnectionRefused_NoReplay
- **Reason**: Tests that connection refused errors fail over across all accounts until exhaustion (returning 503 Service Unavailable).
- **Covering Rust test**: `upstream::tests::connect_refused_is_connect_error` (verifies connection refusal is classified as a pre-connect `ConnectError::Tcp` safe for failover) and `proxy::tests::transport_failure_connection_refused_no_replay`.

### 3. TestClientDisconnect_NoReplay
- **Reason**: Tests downstream HTTP client cancellation terminating handler execution promptly.
- **Covering Rust test**: `proxy::tests::client_disconnect_releases_everything` (verifies downstream client disconnect terminates the request and releases leases, in-flight counts, and claims).

### 4. TestNetHttpTransport_NoAutomaticRetry
- **Reason**: Go's standard library `net/http.Transport` has internal retry mechanisms for broken persistent connections that agy-pool tested by hijacking the TCP connection. In Rust, generation requests use `upstream::connect` to establish a dedicated, fresh connection per attempt with hyper HTTP/1, which has no hidden retry layer.
- **Covering Rust test**: `upstream::tests::connect_ok_then_reset_is_send_error` (verifies a broken socket after connect returns terminal `SendError::Io` without retrying) and `upstream::tests::generation_connection_not_reused`.

### 5. TestRequestClassification
- **Reason**: Tests `IsGenerationRequest` and `IsStreamRequest` on URL paths and `Accept` headers in `proxy.go`.
- **Covering Rust test**: `proxy::tests::test_request_classification`.

### 6. TestGenerationTransport_NoReplayConfiguration
- **Reason**: Asserts fields on Go's `http.Transport` struct (`DisableKeepAlives == true`, `ForceAttemptHTTP2 == false`, `TLSHandshakeTimeout != 0`). In Rust, generation requests do not use Go's struct; instead, `upstream::connect` explicitly dials a fresh single-use TCP connection and does not reuse connections.
- **Covering Rust test**: `upstream::tests::generation_connection_not_reused` and `upstream::tests::tls_handshake_failure_is_connect_error`.

### 7. TestGenerationRoundTripper_StripsIdempotencyAndGetBody
- **Reason**: Tests Go's `GenerationRoundTripper` wrapper which clears `req.GetBody` and deletes `Idempotency-Key` / `X-Idempotency-Key`. In Rust, generation requests are dispatched via `upstream::connect` with buffered `Bytes`, and request header filtering is performed by `proxy::filter_request_headers`.
- **Covering Rust test**: `proxy::tests::test_generation_round_tripper_strips_idempotency` and `proxy::tests::filter_request_headers_strips_idempotency_for_generation`.

### 8. TestFilterRequestHeaders_StripsIdempotencyForGeneration
- **Reason**: Tests header filtering for generation vs non-generation requests (`FilterRequestHeaders`).
- **Covering Rust test**: `proxy::tests::filter_request_headers_strips_idempotency_for_generation`.

## internal/proxy/commitment.go (Go-Only Wrapper Internals)

### 1. TestResponseCommitter_StateTransitions and TestResponseCommitter_WriteTransitionsState
- **Reason**: In Go, `ResponseCommitter` is an imperative mutable wrapper around `http.ResponseWriter` implementing `http.Hijacker` that transitions states (`stateUncommitted` -> `stateHeadersCommitted` -> `stateDataCommitted`) across explicit `WriteHeader` and `Write` calls. In Rust's hyper-based architecture, HTTP responses are values (`http::Response<ProxyBody>`). Commitment is structural: once headers are sent to the client via `send_buffered` or `send_stream`, the response is returned as `Attempt::Done(Response<ProxyBody>)` and streaming body chunks stream through `StreamBody`. There is no separate mutable wrapper or `Hijack` trait needed.
- **Covering Rust test**: `proxy::tests::core::test_send_stream_commit_and_truncation_error` (verifies streaming commitment semantics and truncation handling) and `proxy::tests::core::test_send_buffered_commit_state` (verifies buffered response commitment and header state).
