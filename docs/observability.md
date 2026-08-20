# rcoder 可观测性指南

统一整理本地开发排查工具的启用方式与使用方法。release/CI 构建不带任何观测 feature——零代码零开销。

## 快速索引

| 工具 | 用途 | 启用方式 | 排查场景 |
|------|------|----------|----------|
| **OTLP → Tempo** | 分布式追踪（跨服务全链路 trace） | compose 常开 | 全链路瀑布/火焰图、生产事故排查 |
| **trace_id 日志注入** | 日志 JSON 顶层 trace_id 字段 | 自动（有 traceparent 时） | 跨服务全链路日志过滤 |
| **tokio-console** | 异步任务/锁/waker 运行时观测 | `console` feature | 死锁、锁等待、任务泄漏 |
| **tracing-flame** | span 耗时火焰图 | `flame` feature | 哪个 async 函数慢 |
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

## tracing-flame 火焰图

```bash
# compose 容器（推荐，配合 e2e 负载采样）
# ① make dev-hot 默认已带 flame feature（DEV_FLAME=1；DEV_FLAME=0 关闭）
# ② 容器带输出路径重建（一次性）：
RCODER_FLAME=/app/logs/flame.folded docker compose -f docker/docker-compose.yml up -d rcoder
# ③ 跑负载（e2e / curl）
# ④ 停机落盘（SIGTERM 优雅退出自动 flush），⚠️ 必须先拷贝再重启——
#    重启会 truncate 同路径文件（FlameLayer 以 create 模式打开）：
docker stop rcoder-rcoder-1
cp docker/logs/flame.folded /tmp/flame-keep.folded
docker start rcoder-rcoder-1

# 本地运行（产生 .folded 文件）
make run-flame                          # 默认 logs/tracing.folded
make run-flame RCODER_FLAME=/tmp/f.folded

# 渲染 SVG（一次性安装：cargo install inferno）
inferno-flamegraph < logs/tracing.folded > flame.svg
open flame.svg                          # 浏览器查看
```

**与 Pyroscope 的区别**：Pyroscope 是 CPU 采样（哪些函数在烧 CPU），tracing-flame 是 span 耗时（哪些 async 路径在等待/耗时）。

**⚠️ 实测结论（2026-08-20 对照实验）——folded 耗时数据会失真**：tracing-flame
**不测 span 自身耗时**——源码（0.2.0 lib.rs:478）里每行数字 = `线程上距上一个 span 事件的
墙钟间隔`（thread-local `LAST_EVENT`），在 span 切换时刻把 gap 归因给当前栈。单线程顺序
嵌套（benchmark 场景）下这与耗时等价；多线程并发 async 下失效：①挂起期间线程被其他任务
复用，gap 被无关事件不断重置；②tokio work-stealing 使任务跨线程迁移，thread-local 时钟断链。
对照实验：`grpc_dial` span 同源双记录，metrics 直方图 p50=3.1s / p99=10s，folded 里 max 仅
67ms（差 150 倍）；`forward_chat`（9-20s）在 folded 里 max 16ms。**耗时分析一律以
/metrics 的 span 直方图为准**（SpanMetricsLayer，on_new_span/on_close 墙钟、无栈假设），
火焰图只看**调用结构与冷启动子阶段**（父子链取自 registry，结构是准的）。

**读图要点**（实测经验）：
- folded 里裸 `all-threads` 大数值行 = `on_enter` 根 span 时把该线程的整段空闲 gap 记在
  根上（线程刚被唤醒），不代表任何热点，分析时跳过；
- 只有 2 帧以上的栈才是真实 span 链；无子 span 的热点叶子帧（如 handler）说明该函数内部
  缺 `#[instrument]`，是可观测性缺口而非"函数本身慢"。

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
