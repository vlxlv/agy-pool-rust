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

## internal/daemon (Daemon Lifecycle, PID, Runtime Stats, Log Management)

### Tests Not Ported 1:1 (With Reasons)

1. **TestMain**
   - **Reason**: Go test runner hook (`testing.M`) for global test process initialization and cleanup. Not applicable in Rust, which uses cargo's test harness with scoped `TestEnvGuard` / `crate::config::TEST_LOCK` isolation per test.
   - **Covering Rust mechanism**: `crate::daemon::test_env` RAII guard and `scripts/rust-test.sh` unshare sandbox.

2. **TestHelperMultiProcess**
   - **Reason**: Go sub-process trampoline (`GO_WANT_HELPER_PROCESS=1`) called via `os.Args` to simulate separate daemon binaries in Go's single-binary test runner. In Rust, multi-process tests either invoke real background binaries or run in dedicated integration test targets (`tests/daemon_child.rs`).
   - **Covering Rust test**: `rust/tests/daemon_child.rs:test_daemon_child_production_lifecycle` and `lifecycle::tests::test_multi_instance_subprocess_lifecycle`.

### 50 Ported Tests (Go -> Rust Mapping)

| Go Test Name | Rust Module & Test Name |
| --- | --- |
| `TestCanonicalizeDir` | `daemon::instance::tests::test_canonicalize_dir` |
| `TestCgroupScope` | `daemon::systemd::tests::test_cgroup_scope` |
| `TestCheckInstanceStatus` | `daemon::instance::tests::test_check_instance_status` |
| `TestClearLog_Semantics` | `daemon::log::tests::test_clear_log_semantics` |
| `TestCollectRuntimeStats` | `daemon::runtime::tests::test_collect_runtime_stats` |
| `TestConcurrentStart_Protection` | `daemon::lifecycle::tests::test_concurrent_start_protection` |
| `TestConfigMismatch_And_Tampering` | `daemon::lifecycle::tests::test_config_mismatch_and_tampering` |
| `TestDaemonChild_ProductionLifecycle` | `tests/daemon_child.rs::test_daemon_child_production_lifecycle` |
| `TestDaemon_ProactiveTokenRefresh` | `daemon::server::tests::test_daemon_proactive_token_refresh` |
| `TestDaemon_StartsQuotaRefresher` | `daemon::server::tests::test_daemon_starts_quota_refresher` |
| `TestDataDir_SymlinkCanonicalization` | `daemon::instance::tests::test_data_dir_symlink_canonicalization` |
| `TestFailClosedProductionLogPath` | `daemon::pid::tests::test_fail_closed_production_log_path` |
| `TestFailClosedProductionPIDPath` | `daemon::pid::tests::test_fail_closed_production_pid_path` |
| `TestFormatGoVersion` | `daemon::runtime::tests::test_format_go_version` |
| `TestFormatMemoryBytes` | `daemon::runtime::tests::test_format_memory_bytes` |
| `TestFormatSize` | `daemon::log::tests::test_format_size` |
| `TestFormatUptime` | `daemon::runtime::tests::test_format_uptime` |
| `TestGetRuntimeStats_FreshnessAndPID` | `daemon::runtime::tests::test_get_runtime_stats_freshness_and_pid` |
| `TestHelperProcess_BackgroundDaemonSpawnAndStop` | `daemon::lifecycle::tests::test_helper_process_background_daemon_spawn_and_stop` |
| `TestIsAgyPoolProcess_PID1` | `daemon::lifecycle::tests::test_is_agy_pool_process_pid1` |
| `TestIsDaemonOutdated` | `daemon::server::tests::test_is_daemon_outdated` |
| `TestIsProcessAlive_ESRCH` | `daemon::lifecycle::tests::test_is_process_alive_esrch` |
| `TestMatchesInstance` | `daemon::instance::tests::test_matches_instance` |
| `TestMaybeRotateLog_RateLimiting` | `daemon::log::tests::test_maybe_rotate_log_rate_limiting` |
| `TestMultiInstance_ForegroundIsolation` | `daemon::instance::tests::test_multi_instance_foreground_isolation` |
| `TestMultiInstance_SubprocessLifecycle` | `daemon::instance::tests::test_multi_instance_subprocess_lifecycle` |
| `TestObservability_CaseM_OlderRuntimeJSONDecodesSafely` | `daemon::runtime::tests::test_observability_case_m_older_runtime_json_decodes_safely` |
| `TestObservability_CaseN_StalePIDRejectsStats` | `daemon::runtime::tests::test_observability_case_n_stale_pid_rejects_stats` |
| `TestPerInstance_PIDLocks` | `daemon::instance::tests::test_per_instance_pid_locks` |
| `TestPerInstance_StateDB_And_Log_Isolation` | `daemon::instance::tests::test_per_instance_state_db_and_log_isolation` |
| `TestPIDFile_LegacyFormat` | `daemon::pid::tests::test_pid_file_legacy_format` |
| `TestPIDFile_MalformedAndMissing` | `daemon::pid::tests::test_pid_file_malformed_and_missing` |
| `TestPIDFile_OwnershipSafeRemoval` | `daemon::pid::tests::test_pid_file_ownership_safe_removal` |
| `TestPIDFileTampering_FailClosed` | `daemon::lifecycle::tests::test_pid_file_tampering_fail_closed` |
| `TestPIDFile_WriteAndReadRoundtrip` | `daemon::pid::tests::test_pid_file_write_and_read_roundtrip` |
| `TestResolveInstanceContext` | `daemon::instance::tests::test_resolve_instance_context` |
| `TestRestartDaemon_Sequencing` | `daemon::lifecycle::tests::test_restart_daemon_sequencing` |
| `TestRotateLogIfNeeded_CopytruncateAndShift` | `daemon::log::tests::test_rotate_log_if_needed_copytruncate_and_shift` |
| `TestRunForeground_LifecycleAndGracefulShutdown` | `daemon::server::tests::test_run_foreground_lifecycle_and_graceful_shutdown` |
| `TestRuntimeStats_CorruptFile` | `daemon::runtime::tests::test_runtime_stats_corrupt_file` |
| `TestRuntimeStats_EmptyFile` | `daemon::runtime::tests::test_runtime_stats_empty_file` |
| `TestRuntimeStats_MissingFile` | `daemon::runtime::tests::test_runtime_stats_missing_file` |
| `TestRuntimeStats_WriteAndReadRoundtrip` | `daemon::runtime::tests::test_runtime_stats_write_and_read_roundtrip` |
| `TestRuntimeStatsWriter_100EventCoalescenceAndFinalZero` | `daemon::runtime::tests::test_runtime_stats_writer_100_event_coalescence_and_final_zero` |
| `TestRuntimeStatsWriter_MultipleAccountsStressCoalescence` | `daemon::runtime::tests::test_runtime_stats_writer_multiple_accounts_stress_coalescence` |
| `TestSameDataDir_DifferentConfigFile_Rejected` | `daemon::instance::tests::test_same_data_dir_different_config_file_rejected` |
| `TestSamePort_Collision` | `daemon::instance::tests::test_same_port_collision` |
| `TestStalePIDDetection` | `daemon::pid::tests::test_stale_pid_detection` |
| `TestStopDaemon_Scenarios` | `daemon::lifecycle::tests::test_stop_daemon_scenarios` |
| `TestTryAcquirePIDLock` | `daemon::pid::tests::test_try_acquire_pid_lock` |

### Additional Safety and Integration Tests
- `daemon::lifecycle::tests::send_signal_refuses_live_gateway_pid` (refuses signal delivery to live host gateway PID)
- `daemon::systemd::tests::systemctl_refused_in_tests` (panics unconditionally in test builds)
- `tests/daemon_child.rs::second_rust_gateway_refused_while_go_runs` (cross-runtime binary port-collision detection)
- `daemon::server::tests::test_run_foreground_refuses_production_port_in_test_mode` (prevents binding production port 8899 in tests)

## internal/diagnostics (System Doctor, Effectiveness, Routing Audit)

| Go Test | Rust Test Equivalent |
| --- | --- |
| `TestBinaryDiscoveryAndRecursionProtection` | `diagnostics::tests::test_binary_discovery_and_recursion_protection` |
| `TestDiscovery_ExplicitOverrideFailure` | `diagnostics::tests::test_discovery_explicit_override_failure` |
| `TestDiscovery_HardLinkProtection` | `diagnostics::tests::test_discovery_hard_link_protection` |
| `TestDiscovery_LegitimateScriptContent` | `diagnostics::tests::test_discovery_legitimate_script_content` |
| `TestDiscovery_ManagedNativePrecedence` | `diagnostics::tests::test_discovery_managed_native_precedence` |
| `TestDiscovery_PrecedenceAndFallbacks` | `diagnostics::tests::test_discovery_precedence_and_fallbacks` |
| `TestDiscovery_SymlinkChainProtection` | `diagnostics::tests::test_discovery_symlink_chain_protection` |
| `TestDoctor_NativeIntegrationDiagnostics` | `diagnostics::tests::test_doctor_native_integration_diagnostics` |
| `TestLeaseFallbackRateHigh` | `diagnostics::tests::test_lease_fallback_rate_high` |
| `TestRunDoctor_EmptyEnvironment` | `diagnostics::tests::test_run_doctor_empty_environment` |
| `TestRunDoctor_FatalErrorOnCorruptedPool` | `diagnostics::tests::test_run_doctor_fatal_error_on_corrupted_pool` |
| `TestRunDoctor_NativeIdentityMissingAndMalformed` | `diagnostics::tests::test_run_doctor_native_identity_missing_and_malformed` |
| `TestRunDoctor_NativeIdentityStates` | `diagnostics::tests::test_run_doctor_native_identity_states` |
| `TestRunDoctor_PopulatedPoolAndPrivacy` | `diagnostics::tests::test_run_doctor_populated_pool_and_privacy` |
| `TestRunDoctor_StateDBInspectionAndCorruption` | `diagnostics::tests::test_run_doctor_state_db_inspection_and_corruption` |
| `TestRunDoctor_TLSFailureWarning` | `diagnostics::tests::test_run_doctor_tls_failure_warning` |
| `TestComputeEffectivenessReport_BalancedDistribution` | `diagnostics::effectiveness::tests::test_compute_effectiveness_report_balanced_distribution` |
| `TestComputeEffectivenessReport_EmptyPool` | `diagnostics::effectiveness::tests::test_compute_effectiveness_report_empty_pool` |
| `TestComputeEffectivenessReport_SkewedDistribution` | `diagnostics::effectiveness::tests::test_compute_effectiveness_report_skewed_distribution` |
| `TestAudit_A_HealthySelection_Pass` | `diagnostics::routing_audit_eval::tests::test_audit_a_healthy_selection_pass` |
| `TestAudit_B_ReserveWhileHealthyExists_Violation` | `diagnostics::routing_audit_eval::tests::test_audit_b_reserve_while_healthy_exists_violation` |
| `TestAudit_C_ReserveFallback_Pass` | `diagnostics::routing_audit_eval::tests::test_audit_c_reserve_fallback_pass` |
| `TestAudit_D_DepletedWhileReserveExists_Violation` | `diagnostics::routing_audit_eval::tests::test_audit_d_depleted_while_reserve_exists_violation` |
| `TestAudit_E_UnknownClassification_Warning` | `diagnostics::routing_audit_eval::tests::test_audit_e_unknown_classification_warning` |
| `TestAudit_F_LegacyUnidentifiedWindows` | `diagnostics::routing_audit_eval::tests::test_audit_f_legacy_unidentified_windows` |
| `TestAudit_G_Smart_ReserveViolations` | `diagnostics::routing_audit_eval::tests::test_audit_g_smart_reserve_violations` |
| `TestAudit_H_OtherStrategy_NoReserveViolations` | `diagnostics::routing_audit_eval::tests::test_audit_h_other_strategy_no_reserve_violations` |
| `TestAudit_I_FailoverTwoAttemptsWithoutFalseViolation` | `diagnostics::routing_audit_eval::tests::test_audit_i_failover_two_attempts_without_false_violation` |
| `TestAudit_J_EventConsumerCannotBlockGenerationPath` | `diagnostics::routing_audit_eval::tests::test_audit_j_event_consumer_cannot_block_generation_path` |
| `TestAudit_K_SensitiveDataNeverExposed` | `diagnostics::routing_audit_eval::tests::test_audit_k_sensitive_data_never_exposed` |
| `TestAudit_L_JSONOutputValid` | `diagnostics::routing_audit_eval::tests::test_audit_l_json_output_valid` |
| `TestFrozenResetTimestampsAndRankingInputs` | `diagnostics::routing_audit_eval::tests::test_frozen_reset_timestamps_and_ranking_inputs` |
| `TestTemporalAudit_A_HealthySelectionRemainsPassAfterStateDegrades` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_a_healthy_selection_remains_pass_after_state_degrades` |
| `TestTemporalAudit_B_ReserveViolationRemainsViolationAfterStateChanges` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_b_reserve_violation_remains_violation_after_state_changes` |
| `TestTemporalAudit_C_PreSessionEventsNotReported` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_c_pre_session_events_not_reported` |
| `TestTemporalAudit_D_NoActiveAuditorDoesNotFillQueue` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_d_no_active_auditor_does_not_fill_queue` |
| `TestTemporalAudit_E_BlockedConsumerCannotDelayGeneration` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_e_blocked_consumer_cannot_delay_generation` |
| `TestTemporalAudit_F_QueueOverflowIncrementsDroppedCountWithoutBlocking` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_f_queue_overflow_increments_dropped_count_without_blocking` |
| `TestTemporalAudit_I_SessionStopUnregistersAndNoStaleLeak` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_i_session_stop_unregisters_and_no_stale_leak` |
| `TestTemporalAudit_J_PrivacyNoSensitiveData` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_j_privacy_no_sensitive_data` |
| `TestTemporalAudit_K_LegacySemanticsIntact` | `diagnostics::routing_audit_eval::tests::test_temporal_audit_k_legacy_semantics_intact` |
