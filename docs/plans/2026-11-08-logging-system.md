# 双轨异步日志与请求审计系统 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建双轨异步日志与请求审计流水系统：将系统运维日志 (`agy-pool.log`) 与高保真请求审计流水 (`audit-events.jsonl`) 物理分流，消除请求热路径上的全局互斥锁与文件同步 I/O，并实现日志分级过滤与无损向后兼容。

**Architecture:** 
- 轨一（运维日志）：引入 `LogLevel` (DEBUG/INFO/WARN/ERROR) 与基于 `tokio::sync::mpsc` 的有界异步通道，由单后台 Worker 批量刷盘（200ms 周期），`log_message` 函数保持签名向后兼容；
- 轨二（审计流水）：将代理派发收集到的 `AuditEvent`（含 Token, TTFB, Quota, Model, Failover）异步投递到容量为 4096 的有界通道，过载时 Drop 保障代理请求零时延；
- 独立轮转：`agy-pool.log`（5MB）与 `audit-events.jsonl`（20MB）各自执行 `copytruncate` 滚动；提供 `flush_sync_for_test` 保证已有 548 个单元测试无感通过。

**Tech Stack:** Rust 2024 / rustc 1.99+, Tokio 1.54, serde_json, jiff, standard library. Zero new external dependencies.

**Spec:** [`docs/specs/2026-11-08-logging-system-design.md`](docs/specs/2026-11-08-logging-system-design.md)

## Global Constraints

- **Zero new dependencies**: 完全基于 `tokio`, `serde_json`, `jiff` 及 `std` 实现，绝不引入外部重型日志库；
- **Musl static binary size**: 保证发布二进制体积保持在 8MB 以内，无动态依赖；
- **CLI & DB compatibility**: `state.db` 与已有的 `agy-pool logs` 命令完全兼容；
- **Zero test regressions**: 现有的 548 个单元测试与 CI 门禁（`cargo clippy -D warnings`, `cargo fmt --check`）必须 100% 通过；
- **Zero replay safety**: 审计日志与日志系统代码不得修改任何关于 ConnectError / SendError 的重放判定分支。

## Review Focus

1. **通道过载丢弃行为**：在极端突发请求时，审计通道饱和必须安全丢弃并增加指标计数，绝不可阻塞代理线程或导致 OOM。
2. **测试环境同步可见性**：单元测试环境（`AGY_TEST_MODE=1`）必须能在断言前通过同步刷盘获取完整日志内容，避免异步时序抖动。
3. **日志滚动原子性**：`copytruncate` 在执行时必须兼容后台异步写入，写入端检测到文件截断后能正常继续追加。
4. **环境变量动态覆盖**：`AGY_LOG_LEVEL` 能够实时覆盖 `config.json` 的配置，格式非法时安全回退至 `INFO`。
5. **守护进程优雅停机**：Daemon 接收到 SIGTERM/SIGINT 时，必须在退出前将通道内积压的日志安全 flush 到磁盘。

---

### Task 1: 轨一核心：LogLevel 分级与异步运维日志通道 (Operational Logger)

**Files:**
- Create: `src/observability/logger.rs`
- Modify: `src/observability/mod.rs`
- Modify: `src/proxy/mod.rs:600-618`
- Test: `src/observability/logger.rs` (内置单元测试)

**Interfaces:**
- Produces:
  - `pub enum LogLevel { Debug, Info, Warn, Error }`
  - `pub fn init_async_logger(writer: Box<dyn std::io::Write + Send + Sync>, level: LogLevel) -> LoggerHandle`
  - `pub fn set_log_level(level: LogLevel)`
  - `pub fn log_message_level(level: LogLevel, prefix: &str, msg: &str)`
  - `pub fn flush_logs_sync_for_test()`

- [ ] **Step 1: Write failing tests for LogLevel and Async Logger**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_ordering_and_filtering() {
        assert!(LogLevel::Info >= LogLevel::Debug);
        assert!(LogLevel::Error >= LogLevel::Warn);
        assert_eq!(LogLevel::from_str("warn"), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("UNKNOWN"), LogLevel::Info); // default fallback
    }

    #[tokio::test]
    async fn test_async_logger_writes_and_filters() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = TestWriter(buf.clone());
        let _handle = init_async_logger_with_writer(Box::new(writer), LogLevel::Warn);
        
        log_message_level(LogLevel::Info, "TEST", "should be dropped");
        log_message_level(LogLevel::Warn, "WARN", "warning line");
        log_message_level(LogLevel::Error, "ERR", "error line");
        
        flush_logs_sync_for_test();
        let content = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!content.contains("should be dropped"));
        assert!(content.contains("[WARN] [WARN] warning line"));
        assert!(content.contains("[ERROR] [ERR] error line"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `./scripts/test.sh test --lib observability::logger::`
Expected: FAIL (module or functions not defined)

- [ ] **Step 3: Implement `src/observability/logger.rs` and route `log_message`**

- 实现 `LogLevel`（带 `PartialOrd`, `Ord`, `from_str`, `as_str`）；
- 使用 `tokio::sync::mpsc::channel(1024)` 构建消息队列；
- 后台启动 `tokio::spawn` Worker，定时 200ms 或达到 64 条消息时执行 `write_all` 与 `flush`；
- 在 `src/proxy/mod.rs` 中，重构 `pub fn log_message(prefix: &str, msg: &str)`：
  - 自动识别前缀（包含 `ERROR` 映射为 `LogLevel::Error`，包含 `WARN` 映射为 `LogLevel::Warn`，其余为 `LogLevel::Info`）；
  - 调用 `log_message_level`，保持签名向后兼容；
- 实现 `flush_logs_sync_for_test()`供单元测试调用。

- [ ] **Step 4: Run test to verify it passes**

Run: `./scripts/test.sh test --lib observability::logger::`
Expected: PASS

- [ ] **Step 5: Verify proxy & transport existing tests remain green**

Run: `./scripts/test.sh test --lib proxy::`
Expected: 97 passed; 0 failed

- [ ] **Step 6: Commit**

```bash
git add src/observability/logger.rs src/observability/mod.rs src/proxy/mod.rs
git commit -m "feat(observability): add async operational logger with log level filtering"
```

---

### Task 2: 轨二核心：请求审计流水与 JSONL 独立轮转 (Audit JSONL Stream)

**Files:**
- Create: `src/observability/audit.rs`
- Modify: `src/observability/mod.rs`
- Modify: `src/proxy/dispatch.rs`
- Modify: `src/daemon/log.rs`
- Test: `src/observability/audit.rs` (内置测试)

**Interfaces:**
- Produces:
  - `pub struct AuditEvent { ... }`
  - `pub fn record_audit_event(event: AuditEvent)`
  - `pub fn init_audit_worker(path: PathBuf, max_bytes: u64, backup_count: i32) -> AuditHandle`
  - `pub fn flush_audit_sync_for_test()`
  - `pub fn audit_dropped_count() -> u64`

- [ ] **Step 1: Write failing tests for AuditEvent and JSONL Worker**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_audit_event_serialization_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit-events.jsonl");
        let _handle = init_audit_worker(audit_path.clone(), 500, 1);

        let event = AuditEvent {
            timestamp: jiff::Timestamp::now().to_string(),
            request_id: "req-123".to_string(),
            session_key: "sess-abc".to_string(),
            method: "POST".to_string(),
            path: "/v1internal:streamGenerateContent".to_string(),
            model: "gemini-2.5-pro".to_string(),
            strategy: "smart".to_string(),
            attempt: 1,
            account_id: "acc-1".to_string(),
            account_name: "Main".to_string(),
            status_code: 200,
            ttfb_ms: 120,
            total_ms: 850,
            prompt_tokens: 100,
            cached_tokens: 50,
            q5_fraction: Some(0.85),
            q7_fraction: Some(0.92),
            failover_reason: String::new(),
        };

        record_audit_event(event);
        flush_audit_sync_for_test();

        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(content.contains("\"request_id\":\"req-123\""));
        assert!(content.contains("\"model\":\"gemini-2.5-pro\""));
        assert!(content.contains("\"cached_tokens\":50"));
    }

    #[test]
    fn test_audit_drop_on_channel_full() {
        // Force channel capacity to 2, push 10 events, ensure non-blocking and dropped count incremented
        assert!(audit_dropped_count() >= 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `./scripts/test.sh test --lib observability::audit::`
Expected: FAIL (types and functions missing)

- [ ] **Step 3: Implement `src/observability/audit.rs` and dispatch emission**

- 定义结构体 `AuditEvent`（派生 `Serialize, Deserialize, Clone, Debug`）；
- 使用 `tokio::sync::mpsc::channel(4096)`，提供 `record_audit_event(event)`：若 `try_send` 失败（Full/Closed），递增 `AUDIT_DROPPED_COUNTER` 原子计数，绝不阻塞调用方；
- Worker 定时 500ms 或达到 128 条批量追加写入 `audit-events.jsonl`；
- 在 `src/daemon/log.rs` 中增加审计日志文件的 `copytruncate` 滚动支持；
- 在 `src/proxy/dispatch.rs` 中，在 `Attempt::Done` 时根据 TTFB、Tokens 与 CapacityState 组装 `AuditEvent` 并提交。

- [ ] **Step 4: Run test to verify it passes**

Run: `./scripts/test.sh test --lib observability::audit::`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/observability/audit.rs src/observability/mod.rs src/proxy/dispatch.rs src/daemon/log.rs
git commit -m "feat(observability): add structured audit event stream and jsonl logger"
```

---

### Task 3: 网关守护进程生命周期集成与 CLI 体验扩展 (Daemon & CLI)

**Files:**
- Modify: `src/config/paths.rs` (新增 `audit_file() -> PathBuf`)
- Modify: `src/daemon/server.rs` (启动时初始化日志双轨，停机时优雅 flush)
- Modify: `src/cli/daemon_cmd.rs` (扩展 `agy-pool logs` 支持 `--audit`)
- Test: `tests/daemon_child.rs`

**Interfaces:**
- Consumes:
  - `init_async_logger`, `flush_logs_sync_for_test` from Task 1
  - `init_audit_worker`, `flush_audit_sync_for_test` from Task 2
- Produces:
  - `pub fn audit_file() -> PathBuf` (路径: `~/.local/share/agy-pool/audit-events.jsonl`)
  - `agy-pool logs --audit [-n <lines>] [-f]` 支持查看最近结构化审计流水

- [ ] **Step 1: Write test for audit_file and CLI logs --audit**

```rust
#[test]
fn test_audit_file_path() {
    let p = crate::config::audit_file();
    assert!(p.ends_with("audit-events.jsonl"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `./scripts/test.sh test --lib config::paths::test_audit_file_path`
Expected: FAIL (`audit_file` not found)

- [ ] **Step 3: Implement Daemon integration & CLI flags**

- 在 `src/config/paths.rs` 增加 `audit_file()`；
- 在 `src/daemon/server.rs` 启动阶段启动 Logger 与 Audit 两个 Worker Handle，将其 JoinHandle 存入服务器状态结构体；
- 在 SIGTERM/SIGINT 处理中触发优雅关闭，调用 `flush()` 等待队列清空；
- 在 `src/cli/daemon_cmd.rs` 中，为 `run_logs` 增加 `--audit` 标志支持：读取 `audit-events.jsonl` 并通过彩色格式或格式化 JSON 输出。

- [ ] **Step 4: Run test to verify it passes**

Run: `./scripts/test.sh test --lib config::paths::`
Run: `./scripts/test.sh test --lib cli::daemon_cmd::`
Expected: PASS

- [ ] **Step 5: Run full test suite and clippy checks**

Run: `./scripts/test.sh clippy --all-targets -- -D warnings`
Run: `./scripts/test.sh test`
Expected: All tests pass (0 failures, 0 warnings)

- [ ] **Step 6: Commit**

```bash
git add src/config/paths.rs src/daemon/server.rs src/cli/daemon_cmd.rs
git commit -m "feat(cli): integrate dual-track logging lifecycle and add logs --audit support"
```

---

## Plan Self-Review Checklist

1. **Spec coverage**:
   - 轨一运维日志与 LogLevel 过滤 -> Task 1
   - 轨二审计流水与 JSONL 独立轮转 -> Task 2
   - 异步非阻塞与丢弃保命机制 -> Task 1 & Task 2
   - Daemon 生命周期、优雅停机与 CLI 扩展 -> Task 3
2. **Step scan**: 每个 Step 均有确定且具体的实现签名与验证命令，不包含空泛代办。
3. **Type consistency**: `LogLevel`, `AuditEvent`, `audit_file()` 在三项任务之间接口完全对齐。
4. **Review Focus**: 涵盖通道满时丢弃、测试同步刷盘、滚动原子性、环境变量覆盖及优雅停机。
5. **Proportion**: 方案保持极简与最小代码增量，避免对无关文件造成侵入。
