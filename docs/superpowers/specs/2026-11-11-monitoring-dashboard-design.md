# agy-pool-rust 运行与配额监控看板设计方案

- **日期**：2026-11-11
- **状态**：已就绪待审批 (Ready for Approval)
- **目标组件**：`agy-pool-rust` 守护进程、反向代理与前端监控看板

---

## 1. 背景与动机 (Intent & The Why)

`agy-pool-rust` 是基于 Rust 的高性能 Google Antigravity CLI (`agy`) 多账号配额网关与负载均衡器。目前服务运行于本地（或通过 Tailscale 绑定在私有网卡上，默认端口 `8899`），终端已具备完善的 CLI 命令（如 `agy-pool top`、`quota`、`status`）和原始 Prometheus 指标（`/metrics`）。

然而，用户在远程工作环境（如通过手机、笔记本或跨主机浏览器）下，无法便捷地通过可视化 Web 界面监控网关运行状态、多账号配额余量与重置倒计时、实时请求吞吐与路由事件。

本设计旨在为 `agy-pool-rust` 提供一个内置的、轻量高密度的 Web 监控看板与配套 JSON / SSE 接口，让用户在任何接入 Tailnet 或局域网的浏览器中，输入网关地址即可实时掌握系统健康度与配额使用情况。

---

## 2. 目标、非目标与反向目标 (Goals, Non-Goals, Anti-Goals)

### 2.1 目标 (Goals)
1. **单二进制开箱即用**：前端静态资源通过 Rust `include_str!` 编译内嵌，无需安装任何额外依赖（如 Node.js、Nginx），保持单一可执行文件分发。
2. **只读轻量监控**：提供清晰直观的运行状态（Uptime、内存占用、QPS、并发数）、账号配额池矩阵（5小时与周配额双进度条、精确重置倒计时）及实时请求审计流水。
3. **高密度运维看板 (方案 B)**：采用顶部聚合指标卡 + 账号配额数据表格 + 底部实时事件流的高密度布局，兼顾多账号横向对比与延迟监控。
4. **低资源消耗与零代理干扰**：
   - 看板开启与访问时，网关内存增量控制在极低水平（< 2MB）。
   - Web 页面与 API 路由匹配绝不干扰正常的 Gemini/Antigravity 代理转发请求（`/v1beta/*` 等）。
5. **实时长连接推送 (SSE)**：通过 Server-Sent Events 流式推送实时指标与审计事件，避免前端频繁轮询带来的无效 CPU 与网络开销。

### 2.2 非目标 (Non-Goals)
- **在线写操作与配置管理**：本阶段不涉及在 Web 端添加/删除 Google 账号、修改配置文件或切换路由策略（这些操作继续由 CLI 处理）。
- **用户登录认证系统**：由于当前网关部署在受信任的 Tailscale 虚拟局域网内，已有网络层 WireGuard 隧道与 ACL 保护，初期不引入重型的 Cookie 会话认证或数据库用户表。
- **重型数据可视化分析**：不引入大型前端图表库（如 ECharts/Chart.js 完整包），仅使用原生 HTML/CSS/Canvas 绘制高对比度进度条与简易指标卡。

### 2.3 反向目标 (Anti-Goals)
- **禁止破坏高性能特性**：绝不因增加 Web 看板而引入垃圾回收抖动、重锁争用或降低代理转发的吞吐率（当前 10,779 QPS、P50 1.46ms）。
- **禁止外置静态文件依赖**：禁止要求部署环境存在额外的静态资源目录或 CDN 必须在线（在离线内网下必须可正常渲染）。

---

## 3. 架构方案比选 (Approaches & Tradeoffs)

在方案探索阶段，我们探讨了三种构建方式：

| 方案 | 实现机制 | 优势 | 劣势 | 结论 |
| :--- | :--- | :--- | :--- | :--- |
| **途径一：主端口内置路由 + 单文件极简 SPA** | 在现有的 Hyper 监听器（8899）上增加 `/dashboard` 及 `/api/v1/*` 路由，内嵌单文件 HTML | 零配置、零额外端口、单二进制开箱即用，Tailscale 访问极度省心 | 前端无法使用大型框架库（但对监控看板完全足够） | **用户采纳（选中方案）** |
| **途径二：业务与管理双端口隔离** | 新增 `admin_port`（如 8900），单独启动监听器专门承载 Web 与 API | 业务流量与监控流量物理隔离 | 多占一个系统端口，Tailscale 防火墙需额外配置放行，偏重 | 否决 |
| **途径三：独立前端项目 + 纯 REST API** | Rust 端仅暴露 API，前端单独作为 Vue/React 项目构建和托管 | 前端可使用现代化重型组件库 | 破坏单二进制体验，用户部署维护成本翻倍，有跨域问题 | 否决 |

---

## 4. 详细设计 (System Architecture & How It Gets Built)

### 4.1 路由规划 (Routing Table)
在 `src/proxy/mod.rs`（或专用子模块 `src/dashboard/mod.rs`）的 Hyper 请求分发层进行精确路径匹配：

1. **静态 Web 页面**：
   - `GET /` 或 `GET /dashboard` 或 `GET /dashboard/` -> 返回 `200 OK`，`Content-Type: text/html; charset=utf-8`，内容为内嵌的 SPA HTML。
2. **REST API 数据端点**：
   - `GET /api/v1/status` -> 返回网关与运行状态（JSON）。
   - `GET /api/v1/accounts` -> 返回账号池各账号状态、5h 配额剩余百分比及重置时间戳、周配额剩余及重置时间戳、命中统计（JSON）。
   - `GET /api/v1/forecast` -> 返回配额跑道预测（可用小时数）、消耗速率、缓存命中率（JSON）。
3. **实时推送端点 (SSE)**：
   - `GET /api/v1/events` -> 返回 `Content-Type: text/event-stream; charset=utf-8`，长连接推送最近请求的审计摘要（时间戳、模型、命中账号、耗时 TTFB、状态码、是否 failover）与周期性（如每 2 秒）运行状态快照。
4. **其他请求**：
   - 保留现有的 `GET /metrics` 与 `GET /healthz`。
   - 其余请求一律穿透至原有的 `apply_model_mapping` 和 `dispatch_request` 代理转发链路，互不冲突。

### 4.2 数据结构设计 (Data Contracts)

#### 4.2.1 运行状态响应 (`/api/v1/status`)
延迟指标同时提供首字响应耗时（`ttfb_ms`，反映网关与上游网络连通延迟）与全流程耗时（`total_ms`，反映包含流式生成在内的完整耗时），由内存环形缓冲区（最近 100 次请求）计算：
```json
{
  "version": "0.3.2",
  "status": "running",
  "listen": "100.x.y.z:8899",
  "strategy": "smart",
  "uptime_secs": 309600,
  "memory_rss_bytes": 6029312,
  "in_flight_requests": 2,
  "total_requests": 14250,
  "qps": 1.46,
  "latency": {
    "sample_count": 100,
    "ttfb_p50_ms": 210,
    "ttfb_p95_ms": 480,
    "total_p50_ms": 1450,
    "total_p95_ms": 4100
  }
}
```

#### 4.2.2 账号配额列表 (`/api/v1/accounts`)
- **邮箱脱敏与切换**：默认返回打码脱敏邮箱 `email_masked`（如 `u***@gmail.com`）。支持查询参数 `?show_email=true`，当用户开启页面上的“显示邮箱”开关时携带该参数请求完整 `email` 字段，满足安全默认原则。
- 区分静态存储状态 (`storage_status`) 与运行时调度状态 (`scheduler_state`：`active`/`reserve`/`cooling`/`disabled`)。若账号尚未初始化配额或无周限制，字段支持为 `null`，前端优雅显示为占位符：
```json
{
  "accounts": [
    {
      "id": "1",
      "name": "Work-Pro",
      "email_masked": "u***@gmail.com",
      "email": "user@gmail.com",
      "storage_status": "active",
      "scheduler_state": "active",
      "five_hour": {
        "fraction": 0.85,
        "remaining_percent": 85.0,
        "reset_at_unix": 1794382000,
        "reset_in_secs": 4200
      },
      "weekly": {
        "fraction": 0.62,
        "remaining_percent": 62.0,
        "reset_at_unix": 1794640000,
        "reset_in_secs": 262200
      },
      "hits": 1420,
      "in_flight": 1
    }
  ],
  "total_accounts": 2,
  "active_accounts": 2
}
```
*(注：`email` 字段仅在传入 `?show_email=true` 时下发，默认省略。)*

#### 4.2.3 预测与分析 (`/api/v1/forecast`)
直接复用现有成熟的 `crate::forecast::get_burn_rate` 与 `crate::forecast::simulate_quota_runway` 计算逻辑，无需变更 SQLite 结构：
```json
{
  "runway_hours": 18.5,
  "runway_text": "18h 30m",
  "is_sustainable": false,
  "burn_rate_requests_per_hour": 58.0,
  "window_used": "15m"
}
```

#### 4.2.4 实时审计事件流 (`/api/v1/events`)
- 独立于磁盘日志审计机制：通过内存 `tokio::sync::broadcast` 通道广播，无论是否启用日志落盘均可工作。
- 新客户端连接时，首先回放内存环形缓冲区最近的 20 条事件，使新打开的页面立即呈现历史请求记录，随后无缝转入实时推送。
SSE 格式输出：
```text
event: audit
data: {"timestamp":"2026-11-11T14:30:00Z","model":"gemini-1.5-pro","account":"Work-Pro","ttfb_ms":210,"total_ms":850,"tokens":2450,"status":200,"failover":false}

event: stats
data: {"in_flight":2,"qps":1.5,"rss_mb":5.8}
```

### 4.3 前端页面实现 (Single-file SPA - Scheme B)
- **技术栈**：纯单文件原生 HTML5 + 嵌入式 CSS (Dark Theme, 支持现代化圆角阴影) + 原生 ES6 JavaScript（零第三方库，体积约 35KB）。
- **页面架构**：
  1. **Header Bar & Top Metrics Row**：
     - 顶部提供“显示/隐藏完整邮箱”切换开关（对应 API `?show_email=true`，默认隐藏）。
     - 4 张指标卡：“5H 池总余量”、“当前并发与 QPS”、“P50 响应延迟”、“预测可用跑道”。
  2. **Account Table View**：
     - 表头：账号名/标签（及邮箱）、状态徽标（活跃/保留/冷却/禁用）、5H 配额进度条、5H 倒计时、周配额进度条、累计命中次数。
     - 客户端倒计时器：前端根据 `reset_at_unix` 本地递减秒数，避免频繁重刷整个表格。
  3. **Live Request Stream (底部审计流水)**：
     - 保持最新的 20 条请求记录，显示时间戳、目标模型、命中的账号标签、TTFB 与总耗时、Token 数量、状态码药丸标签（绿色 200，红色 429/500）。
     - 支持快捷清空或暂停滚动。
- **网络容错**：
  - 自动通过 `EventSource` 连接 `/api/v1/events`。
  - 若 SSE 断开，前端以 3 秒间隔自动重试重连，并带黄色断网重连提示栏。

---

## 5. 模块划分与代码组织 (Module Structure)

建议在 `src/` 下新增 `dashboard` 模块：
```
src/dashboard/
├── mod.rs          // 路由分发器、HTTP Handler、SSE 广播器
├── assets/
│   └── index.html  // 单文件原生 SPA 源码（HTML+CSS+JS 内联）
└── dto.rs          // JSON 传输对象序列化定义 (StatusResponse, AccountDto, etc.)
```
在 `src/proxy/mod.rs` 中：
在 `handle_proxy` 开头拦截 `/`、`/dashboard`、`/api/v1/*` 并转交 `crate::dashboard::handle_request(req).await` 处理。

---

## 6. 构建与测试计划 (Testing & Verification)

1. **单元测试与 DTO 序列化测试**：
   - 验证 `StatusResponse`、`AccountDto`、`ForecastDto` 能正确从 `storage::Pool` 与 `RuntimeStats` 生成并序列化为标准 JSON。
2. **路由隔离性测试**：
   - 编写集成测试，确保访问 `/dashboard`、`/api/v1/status` 返回预期 200 与 JSON/HTML。
   - 确保以 `/v1beta/*` 开头的正常代理请求绝不受 dashboard 路由影响。
3. **SSE 广播与并发测试**：
   - 验证多个浏览器标签同时订阅 `/api/v1/events` 时连接保持与事件推送正常，客户端关闭后连接平稳释放，无内存泄漏。
4. **编译与尺寸验证**：
   - 确认二进制文件尺寸增量小于 100KB，`cargo build --release` 保持通过。

---

## 7. 技术决策裁定与交由构建者实现的细节 (Design Calls & Left to Builder)

### 7.1 我方在设计评审中做出的技术裁定 (Our Calls)
1. **复用现有跑道与燃烧率算法**：不新增 token 历史表或更改 SQLite/JSON 持久层 Schema，直接复用 `crate::forecast::get_burn_rate`（请求速率）与 `simulate_quota_runway`；当处于可持续闲置态时 `runway_hours` 返回 `null` 并由 `is_sustainable: true` 标识。
2. **延迟与 QPS 统计窗口**：在代理核心路径上维护一个固定长度（最近 100 次请求）的无锁/低争用环形采样窗口，同时暴露 `ttfb_ms`（网关/首字响应延迟）和 `total_ms`（含流式输出总耗时），以及基于最近窗口的瞬时 QPS，不影响当前 10,779 QPS 吞吐性能。
3. **独立于磁盘日志的 SSE 广播机制**：通过代理请求完成钩子向 `tokio::sync::broadcast` 内存总线发送审计事件，确保即使未开启文件审计日志落盘，面板实时流依然可用；且新客户端连入时自动回放最近 20 条历史事件，避免页面打开空白。
4. **账号状态与空配额容错**：账号 DTO 同时提供存储状态和运行时调度状态（`active`/`reserve`/`cooling`/`disabled`）；未初始化或无周限额的窗口统一返回 `null`，前端安全降级展示。

### 7.2 交由构建者裁定的细节 (Left to the Builder)
- SPA 页面的具体配色细节（深蓝暗黑 slate 配色方案）。
- 环形缓冲区的具体数据结构实现（如 `Arc<Mutex<VecDeque>>` 或原子环形数组）。
- 邮箱脱敏展示的精确掩码算法（例如保留首尾各 1~2 位加星号）。
