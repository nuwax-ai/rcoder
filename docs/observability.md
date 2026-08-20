# rcoder 可观测性指南

统一整理本地开发排查工具的启用方式与使用方法。release/CI 构建不带任何观测 feature——零代码零开销。

## 快速索引

| 工具 | 用途 | 启用方式 | 排查场景 |
|------|------|----------|----------|
| **OTLP → Tempo** | 分布式追踪（跨服务全链路 trace） | compose 常开 | 全链路瀑布/火焰图、生产事故排查 |
| **trace_id 日志注入** | 日志 JSON 顶层 trace_id 字段 | 自动（有 traceparent 时） | 跨服务全链路日志过滤 |
| **tokio-console** | 异步任务/锁/waker 运行时观测 | `console` feature | 死锁、锁等待、任务泄漏 |
| **Pyroscope** | CPU 火焰图持续剖析 | 已部署（compose） | CPU 热点 |
| **/metrics** | HTTP 请求量/延迟 | 默认开启 | 性能回归 |

## OTLP → otel-collector → Tempo 分布式追踪（本地常开，与生产同拓扑）

```
rcoder (OTLP gRPC) ──┐
                     ├──> otel-collector:4317 ──> tempo:4317 ──> Grafana (Tempo 数据源)
agent_runner (OTLP) ─┘         └─ self-metrics :8888（Prometheus 抓取）
```

**为什么走 collector 而非直连 Tempo**：本地验证的就是生产拓扑（App → Collector → 后端）；
后端重启/切换时 collector 缓冲重试兜底；探活噪声在 collector 过滤；:8888 指标是
"span 丢没丢"的排障抓手。

**链路组成**（compose 全部常开，无需手动操作）：
- **注入**：e2e/上游带 `traceparent` → rcoder `http_request` span 继承 trace；
  rcoder → agent_runner 的 gRPC 请求统一注入 W3C traceparent（`new_request_with_locale`）
- **提取**：agent_runner 每个 gRPC handler 入口 `attach_trace_parent` 挂到同一 trace，
  并把 trace_id 写进 span field——**agent 侧日志 JSON 顶层同样有 trace_id**（跨服务检索统一）
- **过滤**：collector 丢掉 `/health`、`/metrics` 的 http_request span（探活噪声不进 Tempo）
- **保留**：Tempo 72h（`docker/tempo/tempo.yml`）

**查看**：Grafana http://localhost:3000（admin/admin）→ Explore → 数据源 Tempo
- 按 trace_id 精确查：`Query type: TraceQL` 直接粘贴 trace_id（与日志 JSON 顶层的 trace_id 同值）
- 按 service 查：`{resource.service.name = "rcoder"}` / `= "agent_runner"`
- Trace 视图自带 **Flame graph** 与瀑布渲染；span 点按可见耗时；
  `tracesToMetrics` 联动跳 Prometheus 查同款耗时直方图（SpanMetricsLayer）
- 注意：agent 后台状态探测（get_status 等）无 traceparent，是独立根 trace（有意设计：
  trace 跟随请求，不跟随后台任务）

**生产升级路径**：
1. K8s 部署 Tempo（S3/对象存储后端 + 多副本拆分）与 otel-collector（DaemonSet/Gateway）
2. agent 容器 endpoint 由 `kubernetes_config.services.<svc>.environment` 的
   `OTEL_EXPORTER_OTLP_ENDPOINT` 同名覆盖（机制已存在，零代码）
3. 采样：`OTEL_TRACES_SAMPLER_ARG`（rcoder 侧比例采样）或 collector `tail_sampling`
   processor（错误全留+正常抽样）
4. Tempo 3.x 迁移用官方 `tempo config converter`（3.0 移除 ingester/compactor 配置块）
5. 可叠加：trace to logs（Loki）、span profiles（与 Pyroscope CPU 火焰按 span 关联）

## trace_id 日志注入（自动）

e2e 或上游注入 W3C `traceparent` header 时，rcoder 的日志 JSON **顶层自动出现** `trace_id` 字段：

```bash
# 发请求带 traceparent
curl -H "traceparent: 00-abcdef1234567890abcdef1234567890-0123456789abcdef-01" ...

# 日志过滤（每行 JSON 顶层）
jq 'select(.trace_id == "abcdef1234567890abcdef1234567890")' logs/rcoder.$(date +%Y-%m-%d)
```

**无需配置**——span field 方案直接工作（不依赖 OTLP exporter）。无 traceparent 时日志行为不变。

## tokio-console

```bash
# 启用（compose 环境）
DEV_CONSOLE=1 make dev-hot
tokio-console http://localhost:6669     # 注意 http:// 前缀

# 本地 cargo run
make run-console
```

面板操作：`t` 任务视图 / `r` 资源（锁）视图 / `l` 按锁持有排序 / `w` 唤醒时间 / `f` 过滤。

## tracing-flame（已移除，勿再引入）

依赖已删除（2026-08-21）。**耗时数据在多任务并发 async 下系统性失真**——它不测
span 自身耗时，folded 每行数字 = thread-local `LAST_EVENT` 的"距上一 span 事件
间隔"（源码 0.2.0 lib.rs:478），挂起期线程被其他任务复用会不断重置 gap、
tokio work-stealing 跨线程迁移直接断链。对照实验：`grpc_dial` span 同源双记录，
metrics 直方图 p50=3.1s / p99=10s，folded 里 max 仅 67ms（差 150 倍）。

其原有职责的承接：**耗时** → SpanMetricsLayer 直方图（/metrics）；**调用结构 +
正确耗时的火焰图/瀑布** → OTLP → Tempo（Grafana Explore 自带 Flame graph 视图）；
**CPU 火焰图** → Pyroscope。eBPF 诊断火焰图（`ebpf-tools/`）与此无关，保留。

## span 耗时指标（SpanMetricsLayer，精确计时）

`#[instrument]` 的 span 即计时事实源，`SpanMetricsLayer` 在 span 关闭时自动记录直方图——
**调用点零 `Instant` 侵入**。规则表在 `bootstrap.rs` 注册（span 名 → 指标族 + 标签）：

| 指标族 | span | 含义 |
|--------|------|------|
| `grpc_request_duration_seconds{method="chat"}` | `forward_chat` | 整个模型回合（含重试/智能等待） |
| `grpc_request_duration_seconds{method="dial"}` | `grpc_dial` | agent gRPC 连接建立（冷启动等待核心观测） |
| `container_ensure_duration_seconds{op="ensure"}` | `ensure_container_ready` | 容器就绪端到端（冷启动） |
| `sse_subscription_duration_seconds{kind="client"}` | `sse_subscribe` | SSE 订阅生命周期 |
| `grpc_requests_total{method,status}` | 调用点显式 | dial/chat ok+error 计数 |
| `sse_active_subscriptions` | RAII guard | SSE 在线订阅数 gauge |

新函数要计时：加 `#[instrument]` + 在 bootstrap 规则表加一行（或复用现有 span 名），
无需任何手动计时代码。

## 完整排查链路

```
e2e 注入 traceparent
  ↓
rcoder 日志 JSON 顶层 trace_id（本指南）
  ↓                          ↓
jq 过滤全链路          tokio-console 看锁/任务
                             ↓
                    tracing-flame 看 span 耗时火焰图
```
