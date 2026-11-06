# Porting Notes

This document records Go tests that are not ported 1:1 into the Rust codebase or deferred to later phase tasks, explaining why each test does not directly apply to Rust or its current module and documenting the covering Rust test.

## internal/proxy/transport_test.go

### 1. TestTransportFailure_Timeout_NoReplay
- **Reason**: This tests the full HTTP proxy routing handler (`Handler.ServeHTTP`), mock token refresher, account state, and account failover loop when an upstream timeout occurs. In Rust, proxy request routing and the multi-account dispatch loop belong to Task 13 (`rust/src/proxy/`).
- **Covering Rust test**: `upstream::tests::is_timeout_classification` at the transport layer, and `proxy::tests::transport_failure_timeout_no_replay` in Task 13 for handler failover.

### 2. TestTransportFailure_ConnectionRefused_NoReplay
- **Reason**: This tests that connection refused errors fail over across all accounts until exhaustion (returning 503 Service Unavailable). The failover loop belongs to Task 13 (`rust/src/proxy/`).
- **Covering Rust test**: `upstream::tests::connect_refused_is_connect_error` (verifies connection refusal is classified as a pre-connect `ConnectError::Tcp` safe for failover) and Task 13 `proxy::tests::transport_failure_connection_refused_no_replay`.

### 3. TestClientDisconnect_NoReplay
- **Reason**: Tests downstream HTTP client context cancellation (`context.WithCancel`) terminating the Go HTTP handler execution promptly. Belongs to Task 13 (`rust/src/proxy/`).
- **Covering Rust test**: Task 13 `proxy::tests::client_disconnect_releases_everything` (verifies downstream client disconnect terminates the request future and releases leases, in-flight counts, and claims).

### 4. TestNetHttpTransport_NoAutomaticRetry
- **Reason**: Go's standard library `net/http.Transport` has internal retry mechanisms for broken persistent connections that agy-pool tested by hijacking the TCP connection. In Rust, generation requests use `upstream::connect` to establish a dedicated, fresh connection per attempt with hyper HTTP/1, which has no hidden retry layer.
- **Covering Rust test**: `upstream::tests::connect_ok_then_reset_is_send_error` (verifies a broken socket after connect returns terminal `SendError::Io` without retrying) and `upstream::tests::generation_connection_not_reused`.

### 5. TestRequestClassification
- **Reason**: Tests `IsGenerationRequest` and `IsStreamRequest` on URL paths and `Accept` headers. In Go, these classification functions are part of `internal/proxy/proxy.go`, ported in Task 13 (`rust/src/proxy/mod.rs`).
- **Covering Rust test**: Task 13 `proxy::tests::test_request_classification`.

### 6. TestGenerationTransport_NoReplayConfiguration
- **Reason**: Asserts fields on Go's `http.Transport` struct (`DisableKeepAlives == true`, `ForceAttemptHTTP2 == false`, `TLSHandshakeTimeout != 0`). In Rust, generation requests do not use Go's struct; instead, `upstream::connect` explicitly dials a fresh single-use TCP connection and does not reuse connections.
- **Covering Rust test**: `upstream::tests::generation_connection_not_reused` (verifies every `connect` call creates a new connection on the wire) and `upstream::tests::tls_handshake_failure_is_connect_error`.

### 7. TestGenerationRoundTripper_StripsIdempotencyAndGetBody
- **Reason**: Tests Go's `GenerationRoundTripper` wrapper which clears `req.GetBody` and deletes `Idempotency-Key` / `X-Idempotency-Key`. In Rust, generation requests are dispatched via `upstream::connect` with buffered `Bytes`, and request header filtering is performed by `proxy::filter_request_headers` (Task 13).
- **Covering Rust test**: Task 13 `proxy::tests::test_generation_round_tripper_strips_idempotency` and `proxy::tests::filter_request_headers_strips_idempotency_for_generation`.

### 8. TestFilterRequestHeaders_StripsIdempotencyForGeneration
- **Reason**: Tests header filtering for generation vs non-generation requests (`FilterRequestHeaders`). Belongs to Task 13 (`rust/src/proxy/mod.rs`).
- **Covering Rust test**: Task 13 `proxy::tests::filter_request_headers_strips_idempotency_for_generation`.

## Deferred to Task 13

The following tests are handler-level (they call `Handler.ServeHTTP`/`HandleProxy` or configure `BackendURLProvider` with an `httptest` upstream server) and are deferred to Task 13 when the full `proxy::Handler` HTTP dispatch loop and streaming pipeline are implemented:

### internal/proxy/model_cooldown_test.go
- `TestProxy_ModelCapacityExhausted_CoolsDownOnlyThatModel`
- `TestProxy_SlowStart_CoolsDownOnlyThatModel`

### internal/proxy/failover_test.go
- `TestGeneration_AllAccountsRestricted_NoUpstreamRequest`
- `TestValidationPersistenceFailureStillFailsOverWithoutReplay`
- `TestExplicitFailover_429`
- `TestExplicitFailover_ValidationRequired`
- `TestExplicitFailover_AuthError`
- `TestExplicitFailover_TokenRefreshFailure`
- `TestOrdinaryUpstreamError_NoFailover`
- `TestAllAccountsExhausted_503`
- `TestUnknownFieldPreservationDuringFailover`
- `TestProxy_SoftFailover_FailsOverAndSoftCooldown`
- `TestFailover_TransportPreConnectAndAfterConnect`

### internal/proxy/verification_pending_test.go
- `TestVerificationPending_OnlyOneConfirmationInFlight`
- `TestVerificationPending_MultiplePendingOnlyOneTriedPerRequest`
- `TestVerificationPending_TransportFailureRemainsPending`
- `TestVerificationPending_ClaimReleasedOnEveryExitPath`
- `TestVerificationPending_NoReplayAfterCommit`
- `TestVerificationPending_ForcesTokenRefreshOnConfirmation`
