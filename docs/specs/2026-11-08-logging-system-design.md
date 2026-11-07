# agy-pool-rust 双轨日志与审计系统设计规范

**状态:** 评审中 (Pending Approval)  
**作者:** Pair Programming with User  
**日期:** 2026-11-08  
**目标模块:** `src/observability/`, `src/daemon/log.rs`, `src/proxy/`

---

## 1. 架构目标与背景

当前 `agy-pool-rust` 采用紧耦合的同步文本日志 (`log_message`)，存在三大痛点：
1. **热路径 I/O 阻塞**：每次请求转发、错误记录都在请求处理线程内获取全局互斥锁并调用 `w.flush()`，高并发下导致线程等待；
2. **职责混淆**：运维警告（如 Token 过期、进程启动）与数据级请求流水（如会话亲和、TTFB、Token 用量）混合输出为非结构化文本，难以通过程序进行精准统计与离线分析；
3. **无动态过滤**：缺少日志级别（Level）控制，无法在生产环境下静音常规转发日志，也无法在调试时开启详情。

### 设计原则 (Ponytail & Zero-Replay Aligned)
- **双轨解耦**：系统运维日志（纯文本）与访问审计流水（JSONL）物理分离，互不影响；
- **异步非阻塞**：热路径只负责内存入队（MPSC Bounded Channel），由专用后台任务批量刷盘；
- **丢弃保命 (Drop-on-Pressure)**：审计流水设立有界缓冲，极端过载时宁可丢弃审计日志，绝不拖慢 LLM 上游流式传输；
- **100% 向后兼容**：`agy-pool logs`、`agy-pool-go` 历史行为及现有单元测试无缝兼容。

---

## 2. 双轨架构体系

```
                       [ Incoming Request / Proxy Dispatch ]
                                        │
           ┌────────────────────────────┴────────────────────────────┐
           ▼                                                         ▼
   【轨一：系统运维日志】                                     【轨二：访问与审计流水】
   (System Operational Log)                               (Access & Audit Stream)
           │                                                         │
     Level 过滤器 (WARN/INFO)                                 丰富指标收集 (Tokens/TTFB/Quota)
           │                                                         │
   异步通道 (mpsc, capacity=1024)                             有界通道 (mpsc, capacity=4096)
           │                                                         │
   后台批量写入 Worker (200ms 刷新)                           后台批量写入 Worker (500ms 刷新)
           │                                                         │
           ▼                                                         ▼
  ~/.local/share/agy-pool/                                  ~/.local/share/agy-pool/
       agy-pool.log                                              audit-events.jsonl
  (滚动策略: 5MB copytruncate)                                (滚动策略: 20MB copytruncate)
```

---

## 3. 详细设计规范

### 3.1 轨一：系统运维日志 (Operational Log)

#### 输出文件
- 路径：`~/.local/share/agy-pool/agy-pool.log`
- 格式：`[{YYYY-MM-DD HH:MM:SS}] [{LEVEL}] [{SUBSYSTEM}] {message}`
  - 例：`[2026-11-08 00:15:30] [INFO] [PROXY] POST gemini:streamGenerateContent -> Life (Status: 200, ttfb 112ms, total 1.45s)`
  - 例：`[2026-11-08 00:15:32] [WARN] [FAILOVER] Account Work hit 429! Cooldown 300s. Switching to Backup...`

#### 日志级别 (LogLevel)
- `DEBUG`：握手细节、内部状态变迁、候选者打分明细；
- `INFO`（默认）：请求成功（一行）、正常切号、账号启动刷新；
- `WARN`：HTTP 429、Token 刷新重试、亲和力配额熔断溢出；
- `ERROR`：HTTP 401/403 封锁、上游连接完全耗尽、数据库写入失败。
- **配置支持**：可通过环境变量 `AGY_LOG_LEVEL=debug|info|warn|error` 或 `config.json` 的 `"logging.level"` 控制。

#### CLI 体验
- `agy-pool logs -n 100` 与 `agy-pool logs -f` 输出运维日志，展示颜色高亮与级别过滤。

---

### 3.2 轨二：请求路由与审计流水 (Access & Audit JSONL)

#### 输出文件
- 路径：`~/.local/share/agy-pool/audit-events.jsonl`
- 格式：每行一个独立 JSON 对象（与现有 `RoutingEvent` 结构平滑扩展）。

#### 数据 Schema 定义 (`AuditEvent`)
```json
{
  "timestamp": "2026-11-08T00:15:30.123456Z",
  "request_id": "c1a9e872-...",
  "session_key": "sess_89ab...",
  "method": "POST",
  "path": "/v1internal:streamGenerateContent",
  "model": "gemini-2.5-pro",
  "strategy": "smart",
  "attempt": 1,
  "account_id": "acc_life_01",
  "account_name": "Life",
  "status_code": 200,
  "ttfb_ms": 112,
  "total_ms": 1450,
  "tokens": {
    "prompt": 8192,
    "cached": 6144
  },
  "quota_before": {
    "q5": 0.72,
    "q7": 0.88,
    "r5_secs": 5400,
    "r7_secs": 388800
  },
  "failover_reason": ""
}
```

#### 关键特性
1. **零内存拷贝**：Token 计数直接由 `UsageTap` 产生，TTFB 由 `Instant::elapsed` 采集，汇聚为单一结构体入队；
2. **压测韧性**：通道满时调用 `record_audit_dropped()`，绝不阻塞代理请求；
3. **独立滚动**：文件达 20MB 时自动执行 `copytruncate` 滚动并保留 `.1`，防止撑爆磁盘。

---

## 4. 迁移与兼容性计划

1. **对已有测试的影响**：
   - 现有的 `proxy::tests::transport_failure` 等测试直接断言日志输出文本。设计中提供 `flush_logs_sync_for_test()` 测试辅助函数，确保测试环境下日志立即可见，现有 548 个测试无感通过。
2. **对配置系统的扩展**：
   - `config.json` 保持向后兼容，可选新增字段：
     ```json
     {
       "logging": {
         "level": "info",
         "audit_enabled": true,
         "audit_max_size_bytes": 20971520
       }
     }
     ```
3. **CLI 扩展**：
   - `agy-pool audit`（或 `agy-pool logs --audit`）：支持快捷查看最近的审计事件，无需手动 `cat` 或 `jq`。

---

## 5. 替代方案对比 (Approaches Considered)

| 维度 | 方案 A：单轨文本追加 (现状) | 方案 B：重构为标准 `tracing` 依赖 | 方案 C：双轨异步分流 (本案) |
|---|---|---|---|
| **热路径性能** | 差（全局同步锁 + flush） | 优（non-blocking appender） | **优（轻量有界 mpsc + 异步刷盘）** |
| **外部依赖** | 零依赖 | 需引入 `tracing`, `tracing-subscriber`, `tracing-appender` | **零新外部依赖（基于 tokio/std）** |
| **可观测性** | 弱（非结构化排障困难） | 强（Span/Trace） | **极佳（运维与审计流水解耦）** |
| **二进制与构建影响** | 0KB / 0s | +350KB / 编译耗时增加 | **< 30KB / 几乎无构建影响** |
| **现有契约匹配** | 100% | 需大规模重写宏与断言 | **100%（兼容现有 logs CLI 与测试）** |
