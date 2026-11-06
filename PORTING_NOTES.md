# Porting Notes

agy-pool-rust is a port of [agy-pool-go](https://github.com/vlxlv/agy-pool-go). This file records:

- where the Rust behaviour knowingly differs from Go (**Known deviations**, below);
- which Go test each Rust test replaces, so a Go test name can be traced to its Rust counterpart;
- Go tests that were not ported 1:1, why, and what covers them instead.

Go test names refer to agy-pool-go as of the split (commit `0d9e320` on its `rust-rewrite` branch). That branch also keeps the Go-parity suites that ran both binaries side by side (`differential`, `interop`, `cli_golden`, `eval_differential`, `lifecycle_differential`); they were dropped here at the split because they need the Go tree.

## Known deviations

- **installer**: the "not a regular file or symlink" refusal prints the file mode in octal; Go prints `os.FileMode` (e.g. `drwxr-xr-x`).
- **update**: in test builds `github_token()` never runs `gh auth token` (Go does), so tests never pick up a real token.
- **`-h` / `--help` after a command**: commands without their own help (`switch`, `remove`, `start`, `list`, …) print the global help and exit 0. Go ignored the flag and ran the command, so `agy-pool switch 2 --help` switched accounts.
- **quota footer**: hardcodes the default port 8899 like Go, so an instance on another port shows the wrong address there.

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
   - **Covering Rust mechanism**: `crate::daemon::test_env` RAII guard and `scripts/test.sh` namespace sandbox.

2. **TestHelperMultiProcess**
   - **Reason**: Go sub-process trampoline (`GO_WANT_HELPER_PROCESS=1`) called via `os.Args` to simulate separate daemon binaries in Go's single-binary test runner. In Rust, multi-process tests either invoke real background binaries or run in dedicated integration test targets (`tests/daemon_child.rs`).
   - **Covering Rust test**: `tests/daemon_child.rs::test_daemon_child_production_lifecycle` and `lifecycle::tests::test_multi_instance_subprocess_lifecycle`.

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

## CLI (`internal/cli`, `internal/installer`)

### CLI core: help, doctor, verify, text width, QR code, bars

| Go test | Rust test |
| --- | --- |
| `TestVersionAndHelp` | `cli::tests::test_version_and_help` |
| `TestUnknownCommandAndInvalidChoice` | `cli::tests::test_unknown_command_and_invalid_choice` |
| `TestDoctorCommand` | `cli::tests::test_doctor_command` |
| `TestDoctorCmd_BehaviorUnchanged` | `cli::tests::test_doctor_cmd_behavior_unchanged` |
| `TestVerifyCmd_SuccessClearsRestriction` | `cli::tests::test_verify_cmd_success_clears_restriction` |
| `TestVerifyCmd_PersistenceFailureExitsNonZero` | `cli::tests::test_verify_cmd_persistence_failure_exits_non_zero` |
| `TestWidth_RuneWidth` | `cli::width::tests::test_rune_width` |
| `TestWidth_VisibleWidth` | `cli::width::tests::test_visible_width` |
| `TestWidth_TruncateVisible` | `cli::width::tests::test_truncate_visible` |
| `TestWidth_StripANSI` | `cli::width::tests::test_strip_ansi` |
| `TestRenderTerminalQRCode_SkipsOnEmpty` | `cli::qrcode::tests::test_render_terminal_qr_code_skips_on_empty` |
| `TestRenderTerminalQRCode_ForcedViaEnv` | `cli::qrcode::tests::test_render_terminal_qr_code_forced_via_env` |
| `TestRenderTerminalQRCode_SkipsOnNonTTYByDefault` | `cli::qrcode::tests::test_render_terminal_qr_code_skips_on_non_tty_by_default` |
| `TestRedactUserURL` | `cli::tests::test_redact_user_url` |
| `TestCP1DVerifyRedactsSensitiveURL` | `cli::tests::test_cp1d_verify_redacts_sensitive_url` |
| `TestCP1DRequiredCommandFailures` | `cli::tests::test_cp1d_required_command_failures` |
| `TestChooseBarWidth` | `cli::render::tests::test_choose_bar_width` |
| `TestGetAccountState_Protected` | `cli::render::tests::test_get_account_state_protected` |
| `TestWeeklyBlockedDimsOtherBars` | `cli::render::tests::test_weekly_blocked_dims_other_bars` |

### Accounts: list, quota, rename, enable/disable, login

| Go test | Rust test |
| --- | --- |
| `TestRenameCommand` | `cli::accounts::tests::test_rename_command` |
| `TestDisableAndEnableCommand` | `cli::accounts::tests::test_disable_and_enable_command` |
| `TestDisabledAccount_QuotaAndListRendering` | `cli::accounts::tests::test_disabled_account_quota_and_list_rendering` |
| `TestListAccounts_EmptyAndPopulated` | `cli::accounts::tests::test_list_accounts_empty_and_populated` |
| `TestMigrateLegacyCommand` | `cli::accounts::tests::test_migrate_legacy_command` |
| `TestTerminalWidth_NonTTY` | `cli::accounts::tests::test_terminal_width_non_tty` |
| `TestListAccounts_ResponsiveWidths` | `cli::accounts::tests::test_list_accounts_responsive_widths` |
| `TestLogin_BrowserOpenerSucceeds_HTTPCallbackSucceeds` | `cli::accounts::tests::test_login_browser_opener_succeeds_http_callback_succeeds` |
| `TestLogin_BrowserOpenerFails_ManualRawCodeSucceeds` | `cli::accounts::tests::test_login_browser_opener_fails_manual_raw_code_succeeds` |
| `TestLogin_BrowserOpenerFails_PastedCallbackURLSucceeds` | `cli::accounts::tests::test_login_browser_opener_fails_pasted_callback_url_succeeds` |
| `TestLogin_Timeout` | `cli::accounts::tests::test_login_timeout` |
| `TestLogin_OAuthCallbackError` | `cli::accounts::tests::test_login_oauth_callback_error` |
| `TestLogin_NoTokenOrSecretLeakage` | `cli::accounts::tests::test_login_no_token_or_secret_leakage` |
| `TestLogin_ExistingAccountUpdate` | `cli::accounts::tests::test_login_existing_account_update` |
| `TestQuotaDashboard_EdgeCasesAndLayouts` | `cli::accounts::tests::test_quota_dashboard_edge_cases_and_layouts` |
| `TestRenderRepresentativeWidths` | `cli::accounts::tests::test_render_representative_widths` |
| `TestListAccounts_NextCandidateTag` | `cli::accounts::tests::test_list_accounts_next_candidate_tag` |
| `TestListCmd_CompactAndPrivacy` | `cli::accounts::tests::test_list_cmd_compact_and_privacy` |
| `TestListCmd_ShowEmail` | `cli::accounts::tests::test_list_cmd_show_email` |
| `TestQuotaUnknownIsExplicitAcrossLayouts` | `cli::accounts::tests::test_quota_unknown_is_explicit_across_layouts` |
| `TestQuotaPartialWindowsDisplayIndependently` | `cli::accounts::tests::test_quota_partial_windows_display_independently` |
| `TestQuotaCmd_ContainsProgressBars` | `cli::accounts::tests::test_quota_cmd_contains_progress_bars` |
| `TestQuotaCmd_ShowEmail` | `cli::accounts::tests::test_quota_cmd_show_email` |
| `TestQuotaCmd_AutoHidesUnusedThirdPartyWindows` | `cli::accounts::tests::test_quota_cmd_auto_hides_unused_third_party_windows` |
| `TestQuotaCmd_SkipsLiveFetchForFreshSnapshots` | `cli::accounts::tests::test_quota_cmd_skips_live_fetch_for_fresh_snapshots` |
| `TestResolveNextCandidateID_SkipsDaemonPausedAccounts` | `cli::accounts::tests::test_resolve_next_candidate_id_skips_daemon_paused_accounts` |
| `TestResolveNextCandidateID_FollowsDaemonSmartRotation` | `cli::accounts::tests::test_resolve_next_candidate_id_follows_daemon_smart_rotation` |
| `list_and_quota_on_non_tty_use_go_default_width` | `cli::accounts::tests::list_and_quota_on_non_tty_use_go_default_width` |

### Status, strategy, logs, run, update

| Go test | Rust test |
| --- | --- |
| `TestStrategyCommand` | `cli::status::tests::test_strategy_command` |
| `TestStatusCmd_NoQuotaBarsAndDashboard` | `cli::status::tests::test_status_cmd_no_quota_bars_and_dashboard` |
| `TestStatusCmd_RunningDaemonWithRuntimeStats` | `cli::status::tests::test_status_cmd_running_daemon_with_runtime_stats` |
| `TestStatusCmd_RunningDaemonStatsFromDaemonNotCLI` | `cli::status::tests::test_status_cmd_running_daemon_stats_from_daemon_not_cli` |
| `TestStatusCmd_RunningDaemonStatsUnavailable` | `cli::status::tests::test_status_cmd_running_daemon_stats_unavailable` |
| `TestAuditRouting_RemovedFromCLI` | `cli::status::tests::test_audit_routing_removed_from_cli` |
| `TestCLI_Status_MultiInstance` | `cli::status::tests::test_cli_status_multi_instance` |
| `TestCLI_Stop_CrossInstanceRejection` | `cli::daemon_cmd::tests::test_cli_stop_cross_instance_rejection` |
| `TestObservability_CaseO_StatusVerboseAndPrivacy` | `cli::status::tests::test_observability_case_o_status_verbose_and_privacy` |
| `TestLogsCommand` | `cli::daemon_cmd::tests::test_logs_command` |
| `TestDaemonRun_RemovedFromCLI` | `cli::daemon_cmd::tests::test_daemon_run_removed_from_cli` |
| `TestRunWrapperAndAgyDirect` | `cli::run::tests::test_run_wrapper_and_agy_direct` |
| `TestParseRunArgs` | `cli::run::tests::test_parse_run_args` |
| `TestRunArgumentScopingAndDelimiter` | `cli::run::tests::test_run_argument_scoping_and_delimiter` |
| `TestRunTopLevelOptionsRejected` | `cli::run::tests::test_run_top_level_options_rejected` |
| `TestRunBareContinueResolvesConversation` | `cli::run::tests::test_run_bare_continue_resolves_conversation` |
| `TestFindRealAgyBinaryExcludesShims` | `cli::run::tests::test_find_real_agy_binary_excludes_shims` |
| `TestRunAgyWithLB_ArgumentPreservation` | `cli::run::tests::test_run_agy_with_lb_argument_preservation` |
| `TestRunAgyWithLB_ResolvesManagedNativeBinary` | `cli::run::tests::test_run_agy_with_lb_resolves_managed_native_binary` |
| `TestRunAgyDirect_ResolvesManagedNativeBinary` | `cli::run::tests::test_run_agy_direct_resolves_managed_native_binary` |
| `TestRunAgyWithLB_ValidSyncExecutesAgy` | `cli::run::tests::test_run_agy_with_lb_valid_sync_executes_agy` |
| `TestRunAgyWithLB_SyncFailureAborts` | `cli::run::tests::test_run_agy_with_lb_sync_failure_aborts` |
| `TestUpdateCmd` | `cli::update::tests::test_update_cmd` |
| `TestCP1DRunSyncFailureDoesNotExec` | `cli::run::tests::test_cp1d_run_sync_failure_does_not_exec` |
| `update_checksum_mismatch_leaves_binary_untouched` | `cli::update::tests::update_checksum_mismatch_leaves_binary_untouched` |
| `update_download_failure_leaves_binary_untouched` | `cli::update::tests::update_download_failure_leaves_binary_untouched` |

### Ordering, config, forecast, stats

| Go test | Rust test |
| --- | --- |
| `TestMoveCmd` | `cli::order::tests::test_move_cmd` |
| `TestSwapCmd` | `cli::order::tests::test_swap_cmd` |
| `TestSortCmd` | `cli::order::tests::test_sort_cmd` |
| `TestReorderCmd` | `cli::order::tests::test_reorder_cmd` |
| `TestMain_OrderSubcommands` | `cli::order::tests::test_main_order_subcommands` |
| `TestConfigCmd` | `cli::order::tests::test_config_cmd` |
| `TestForecastCmd_EmptyPool` | `cli::forecast::tests::test_forecast_cmd_empty_pool` |
| `TestForecastCmd_AllRestricted` | `cli::forecast::tests::test_forecast_cmd_all_restricted` |
| `TestForecastCmd_NormalAccountsAndPace` | `cli::forecast::tests::test_forecast_cmd_normal_accounts_and_pace` |
| `TestRunCLI_ForecastAliases` | `cli::forecast::tests::test_run_cli_forecast_aliases` |
| `TestStatsCmd_TerminalOutput` | `cli::stats::tests::test_stats_cmd_terminal_output` |
| `TestStatsCmd_JSONOutput` | `cli::stats::tests::test_stats_cmd_json_output` |

### `top` dashboard

| Go test | Rust test |
| --- | --- |
| `TestTop_ResponsiveLayouts` | `cli::top::tests::test_top_responsive_layouts` |
| `TestTop_AccountStates` | `cli::top::tests::test_top_account_states` |
| `TestTop_UnknownQuota` | `cli::top::tests::test_top_unknown_quota` |
| `TestTop_LongFriendlyNames_And_CJK` | `cli::top::tests::test_top_long_friendly_names_and_cjk` |
| `TestTop_InFlight_ZeroOneN` | `cli::top::tests::test_top_in_flight_zero_one_n` |
| `TestTop_IntervalBounds` | `cli::top::tests::test_top_interval_bounds` |
| `TestTop_KeyHandling_Q_Plus_Minus_R` | `cli::top::tests::test_top_key_handling_q_plus_minus_r` |
| `TestTop_RawMode_AlternateScreenAndCRLF` | `cli::top::tests::test_top_raw_mode_alternate_screen_and_crlf` |
| `TestTop_TerminalRestorationOnPanic` | `cli::top::tests::test_top_terminal_restoration_on_panic` |
| `TestTop_Privacy` | `cli::top::tests::test_top_privacy` |
| `TestTop_ShowEmail` | `cli::top::tests::test_top_show_email` |
| `TestTop_AliasesAndFlags` | `cli::top::tests::test_top_aliases_and_flags` |
| `TestTop_BoundedWidthsAssertion` | `cli::top::tests::test_top_bounded_widths_assertion` |
| `TestTop_PrintRepresentativeWidths` | `cli::top::tests::test_top_print_representative_widths` |
| `TestTop_TelemetryBlock` | `cli::top::tests::test_top_telemetry_block` |
| `top_restores_terminal_mode_on_every_exit` | `cli::top::tests::top_restores_terminal_mode_on_every_exit` |

### Installer: install, uninstall, native `agy` upgrades

| Go test | Rust test |
| --- | --- |
| `TestDetermineTargetDir_Precedence` | `installer::tests::test_determine_target_dir_precedence` |
| `TestInstall_IdempotenceAndUninstallation` | `installer::tests::test_install_idempotence_and_uninstallation` |
| `TestInstall_PreconditionFailsWhenNativeAgyMissing` | `installer::tests::test_install_precondition_fails_when_native_agy_missing` |
| `TestInstall_SystemModeSyntheticRoot` | `installer::tests::test_install_system_mode_synthetic_root` |
| `TestAssertSafeInstallTarget_FailClosed` | `installer::tests::test_assert_safe_install_target_fail_closed` |
| `TestInstallAndUninstall_NoShellRCModification` | `installer::tests::test_install_and_uninstall_no_shell_rc_modification` |
| `TestUninstall_LegacyRCOptInCleanup` | `installer::tests::test_uninstall_legacy_rc_opt_in_cleanup` |
| `TestInstall_NativePreservationBeforeShimPublication` | `installer::tests::test_install_native_preservation_before_shim_publication` |
| `TestInstall_ReinstallAndOfficialUpgradeRefresh` | `installer::tests::test_install_reinstall_and_official_upgrade_refresh` |
| `TestUninstall_RemovesManagedNativeWithoutDeletingExternalBinary` | `installer::tests::test_uninstall_removes_managed_native_without_deleting_external_binary` |
| `TestIntegration_AgyManagedShimAndRawBypass` | `installer::tests::test_integration_agy_managed_shim_and_raw_bypass` |
| `TestInstallerIsolation_UserModeHermeticRoots` | `installer::tests::test_installer_isolation_user_mode_hermetic_roots` |
| `TestInstallerIsolation_SystemModeSyntheticRoot` | `installer::tests::test_installer_isolation_system_mode_synthetic_root` |
| `TestInstallerIsolation_CleanupNeverResolvesToRealUserHome` | `installer::tests::test_installer_isolation_cleanup_never_resolves_to_real_user_home` |
| `TestUninstall_OwnershipEnforcement` | `installer::tests::test_uninstall_ownership_enforcement` |
| `TestUninstall_TOCTOUHardening_SymlinkSwapped` | `installer::tests::test_uninstall_toctou_hardening_symlink_swapped` |
| `TestUninstall_TOCTOUHardening_HardLinkSwapped` | `installer::tests::test_uninstall_toctou_hardening_hard_link_swapped` |
| `TestNativeAgyUpgradeAndMovement` | `installer::tests::test_native_agy_upgrade_and_movement` |
| `TestTokenPath_NoFictitiousPathRegression` | `installer::tests::test_token_path_no_fictitious_path_regression` |
| `TestInstall_NoGoStateInGemini` | `installer::tests::test_install_no_go_state_in_gemini` |
| `TestUpgrade_ModelA_InPlaceManagedNativeSelfUpdate` | `installer::tests::test_upgrade_model_a_in_place_managed_native_self_update` |
| `TestUpgrade_ModelB_OfficialUpdaterOverwritesShimAndRepairFlow` | `installer::tests::test_upgrade_model_b_official_updater_overwrites_shim_and_repair_flow` |
| `TestInstallCommand_PreconditionFailure` | `cli::install::tests::test_install_command_precondition_failure` |
| `TestInstallAndUninstallCommand_UserMode` | `cli::install::tests::test_install_and_uninstall_command_user_mode` |
| `TestInstallAndUninstall_InvalidArgs` | `cli::install::tests::test_install_and_uninstall_invalid_args` |

### `eval` subcommands

The `eval-*` subcommands were checked against Go by `tests/eval_differential.rs`, which needs the Go binary and stays on agy-pool-go's `rust-rewrite` branch. The unit tests of each module they call (scheduler, quota, accounts, auth, crypto, daemon, conversation) cover them here.
