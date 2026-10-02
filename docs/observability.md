# rcoder 可观测性指南

统一整理本地开发排查工具的启用方式与使用方法。release/CI 构建不带任何观测 feature——零代码零开销。

## 快速索引

| 工具 | 用途 | 启用方式 | 排查场景 |
|------|------|----------|----------|
| **OTLP → Tempo** | 分布式追踪（跨服务全链路 trace） | compose 常开 | 全链路瀑布/火焰图、生产事故排查 |
| **trace_id 日志注入** | 日志 JSON 顶层 trace_id 字段 | 自动（有 traceparent 继承；无则合成） | 跨服务全链路日志过滤 |
| **Loki + fluent-bit** | 结构化日志采集检索（生产同链路） | compose 常开（`make logs-*`） | 关键字/trace_id 查日志、Log context |
| **dial9** | 事件级 Tokio tracing（poll/wake/task 时间线） | feature 恒编入 dev；`DIAL9_ENABLED` 运行期开关（默认关） | 长 poll、调度延迟、task 生命周期、off-CPU 根因 |
| **hotpath** | 本地性能剖析（函数耗时/路由/runtime/通道/锁） | dev 默认编入基础档；`hotpath-mcp` 显式叠加 | 函数热点、路由耗时、通道排队、锁等待 |
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
5. 可叠加：trace to logs（Loki）

## trace_id 日志注入（自动）

e2e 或上游注入 W3C `traceparent` header 时，rcoder 的日志 JSON **顶层自动出现** `trace_id` 字段：

```bash
# 发请求带 traceparent
curl -H "traceparent: 00-abcdef1234567890abcdef1234567890-0123456789abcdef-01" ...

# 日志过滤（每行 JSON 顶层）
jq 'select(.trace_id == "abcdef1234567890abcdef1234567890")' logs/rcoder.$(date +%Y-%m-%d)
```

**无需配置**——span field 方案直接工作（不依赖 OTLP exporter）。无 traceparent 时请求 span
**合成新 trace_id**（`remote_context_or_synthesized` 装成 remote parent），日志 field、OTel span、
注入出去的 traceparent、Tempo 四处同一 id——每个请求的日志都可按 trace_id 检索与串联。

## Loki + fluent-bit 日志链路（生产同构）

```
/app/logs/*.log（stdout 重定向产物） ──> fluent-bit:3.2.10 ──> Loki:3.7.7 ──> Grafana
```

本地复刻生产采集链路（同版本、OUTPUT 逐字复制、`Line_Format` 随 stage 演进）；配置在
`docker/fluent-bit/`、`docker/loki/`。生产无 kube-apiserver 的两处替身见文件头注释
（Lua `synthesize_k8s_meta` 构造同构 kubernetes map + Merge_Log/Keep_Log 仿真）。

```bash
make logs-up          # 启动 Loki + fluent-bit（Grafana 重启装载合并数据源）
make logs-query Q='{job="fluent-bit"} |= "关键字"'
make logs-fidelity    # fluent-bit health/storage + Loki ready 自检
```

**控制台 JSON 开关**：`TELEMETRY_CONSOLE_JSON=1` 时 stdout 输出与文件层同款的单行 JSON
（root trace_id），stdout 重定向进 `rcoder.log`（生产 `start-services.sh` 即此形态）后
采集器直接拿到结构化日志；默认 0=ANSI 文本（线上零影响）。JSON 模式额外拦掉两种
OTel 导出噪声拼写（`opentelemetry-otlp` / `opentelemetry_sdk`，B0 基线占 20.3%）。

Grafana（http://localhost:3000）→ Explore → Loki：日志行内 trace_id 生成可点击
**TraceID** 字段跳 Tempo；Tempo trace 视图反向 tracesToLogsV2 跳回日志行。

## dial9 事件级 Tokio tracing

dial9（crates.io 0.5）经 Tokio runtime hooks 记录每个 poll/wake/task 事件到
磁盘分段文件，离线分析零观测容器——定位"这个 task 在等什么 / 这个 poll 为什么长"，
这是 OTLP/Tempo（span 粒度）与 /metrics（聚合粒度）覆盖不到的层次。
取代了已移除的 tokio-console（无背压 OOM）与 Pyroscope/eBPF 持续剖析链。

```bash
# 启用（重建 rcoder 容器注入 DIAL9_ENABLED=1；binary 恒编入 dial9 feature，
# 复用 target-unstable volume 产物，不触发重编）
make dial9-on
# ...复现场景...
make dial9-off

# 离线查看（单二进制 viewer；首次先 cargo binstall dial9）
make dial9-view          # = dial9 serve --local-dir ./docker/logs/dial9
```

| 变量 | 默认 | 说明 |
|------|------|------|
| `DIAL9_ENABLED` | `0` | 主开关；关=纯 passthrough runtime 零开销 |
| `DIAL9_TRACE_DIR` | `/app/logs/dial9`（rcoder）/ `/app/container-logs/dial9`（agent 容器） | 分段 trace 目录，均 bind 宿主可直取 |
| `DIAL9_ROTATION_SECS` | `60` | 分段轮转周期 |
| `DIAL9_MAX_DISK_USAGE_MB` | `1024` | 磁盘预算封顶 |

行为要点：rcoder 主进程开启时新建 agent 容器自动透传 `DIAL9_*`（已有 agent
容器需重建）；构建链按 "dial9 在 features 列表" 自动附
`RUSTFLAGS="--cfg tokio_unstable"`（全量 task 覆盖必需，缺它 poll 只覆盖
`dial9::spawn` 的 task）；生产构建不含该 feature。dial9 的 cpu/memory-profiling
未启用（需 force-frame-pointers，改 RUSTFLAGS 指纹）。
AI agent 分析可用 dial9 自带 skills（`dial9 agents skills <目录>` 解包）。

### dial9 spawner 接线（业务 spawn 点）

业务关键漏斗点的任务经 `crates/rcoder-obs` 门面 spawn：feature 关=直通原生
Tokio API（零插桩），开=带 instrumented 标记记录 wake 因果（ready→实际 poll
调度延迟）与 task 生命周期。当前接入的六个核心点：

| 进程 | spawn 点 |
|---|---|
| rcoder | HTTP accept 外层任务 + 每连接任务（`http-server/src/server.rs`，JoinSet 收集/关停不变） |
| rcoder | SSE 转发任务（`rcoder-engine/src/grpc/sse_stream.rs`，catch_unwind/span 原样） |
| agent_runner | SessionWorker 命令循环（`service/session_cache/worker.rs`） |
| agent_runner | SubscribeProgress 订阅转发（`grpc/subscribe_progress.rs`，locale/清理原样） |
| agent_runner | ACP 连接任务（`agent_abstraction/.../launcher_impl.rs`） |

**未覆盖（有意保留，勿混淆）**：Hyper H2 executor 内部任务（可选路径为本地
`hyper::rt::Executor` impl 替换 `TokioExecutor`，按需再接）；Axum
WebSocket/on_upgrade 自行 spawn 的应用任务两者都不经上述门面。后台 reaper
循环刻意不接（无 wake 因果排查需求）。

### 挂线核对（有没有数据，三轴分开看）

"进程有 trace"必须同时满足三条，缺一条都会静默无数据（任务照常运行）：

1. **二进制 feature**：构建是否带 `dial9`（`--features dial9`；生产镜像不含）；
2. **runtime attach**：进程经 dial9 装配的 runtime 启动（bin 的 dial9 变体 main）；
3. **运行 env**：`DIAL9_ENABLED=1`（默认 0=纯 passthrough）。

已知矩阵：dev compose 的 rcoder 与 agent_runner 三轴可同时满足（make dial9-on）；
`rcoder-obs` 门面在未 attach 线程上自动退化直通 Tokio（不 panic、无标记）；
taskdump 诊断构建（见下）另需 Linux + `tokio_unstable`。嵌套/子进程（UserApp、
app-cli、proxy）各自按上表核对，不因父进程开了 dial9 而继承。

### taskdump（空闲挂起点回溯，Linux 诊断构建）

taskdump 是 dial9 独有能力：对空闲超阈值（`DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS`，
默认 10ms）的 instrumented 任务在挂起点采 async 栈。它要求 `dial9/taskdump`
（→ `tokio/taskdump`，仅 Linux x86_64/aarch64）+ `--cfg tokio_unstable`——
与常规构建指纹互斥，**绝不进主仓 feature 面**（`--all-features` 门禁），只提供
脚本化诊断构建（主仓零改动）：

```bash
# Linux 环境（dev 镜像容器内亦可）：
bash tools/taskdump/build-taskdump-linux.sh [源码目录] [诊断目录]
# 产物 <诊断目录>/target-taskdump/release/rcoder；运行期：
DIAL9_ENABLED=1 DIAL9_TASK_DUMP_ENABLED=1 DIAL9_TRACE_DIR=<诊断目录>/traces <binary>
# 解码断言 dump 事件（宿主 macOS 可跑）：
cd tools/dial9-probe && cargo run -- --expect-dump <诊断目录>/traces
```

## hotpath 本地性能剖析

hotpath-rs（锁定 0.28.0）按需插桩四类对象：函数（`#[hotpath::measure]`）、
HTTP 路由（`hotpath::axum!`）、Tokio runtime（`hotpath::tokio_runtime!()`）、
通道/锁（`hotpath::channel!` / `hotpath::mutex!`）。dev 构建默认编入基础档
（`--features hotpath`）；feature 关闭时宏全 no-op、零依赖编译；生产构建不含。

**指标 server 绑容器内 loopback 127.0.0.1:6770**（无端口映射，须容器内 curl）：

```bash
DEV_CID=$(docker ps -q -f name=rcoder-rcoder-1)
docker exec $DEV_CID curl -s 127.0.0.1:6770/functions_timing  # 函数耗时（measure 选点）
docker exec $DEV_CID curl -s 127.0.0.1:6770/server            # server 维度路由耗时
docker exec $DEV_CID curl -s 127.0.0.1:6770/tokio_runtime     # runtime 指标采样
docker exec $DEV_CID curl -s 127.0.0.1:6770/threads           # 线程 CPU
docker exec $DEV_CID curl -s 127.0.0.1:6770/profiler_status   # 剖析器状态
docker exec $DEV_CID curl -s 127.0.0.1:6770/channels          # 通道排队延迟（channel! 选点）
docker exec $DEV_CID curl -s 127.0.0.1:6770/mutexes           # 登记表锁 wait/acquire（mutex! 选点）
```

注意 `GET /` 不在路由表（404）——验收/排查必须用真实端点并断言 200 与非空数据。
`docker stop`（SIGTERM）触发优雅退出时自动落最终报告。

**通道与锁选点**（stable 别名 `hotpath::wrap::*`，feature 统一后类型自动切换，
禁止消费 crate 本地 cfg 选型）：

| 选点 | label | 观测含义 |
|---|---|---|
| SSE 客户端事件通道（engine `sse_stream.rs`，容量 100） | `sse_client_events` | send→recv 排队延迟（含队满背压等待） |
| SessionWorker 命令通道（runner `session_cache.rs`，容量 1000） | `session_worker_commands` | 命令排队延迟 |
| builder 创建通知登记表（engine `creation/mod.rs`，std Mutex） | `userapp_builder_signals` | wait=锁等待，acquire=持锁时长 |

通道容量/关闭/游标/终态与锁作用域、poison 语义均不因观测改变；DashMap 与
OwnedMutexGuard（租约锁）不接。`hotpath-alloc` 维持禁用（Turso 全局分配器冲突）。

**MCP 档（AI 实时查询，显式 opt-in）**：构建叠加 `--features hotpath-mcp`
（默认集不含；`make dev-hot` 的 feature 透传与 dev-restart 一致，不会静默丢弃）。
MCP server 绑容器内 127.0.0.1:6771，走 streamable HTTP：

```bash
docker exec $DEV_CID curl -s -X POST 127.0.0.1:6771/mcp \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
# 依次 tools/list、tools/call（未开 alloc/cpu 的工具会列出但返回 unsupported）
```


## tracing-flame（已移除，勿再引入）

依赖已删除（2026-08-21）。**耗时数据在多任务并发 async 下系统性失真**——它不测
span 自身耗时，folded 每行数字 = thread-local `LAST_EVENT` 的"距上一 span 事件
间隔"（源码 0.2.0 lib.rs:478），挂起期线程被其他任务复用会不断重置 gap、
tokio work-stealing 跨线程迁移直接断链。对照实验：`grpc_dial` span 同源双记录，
metrics 直方图 p50=3.1s / p99=10s，folded 里 max 仅 67ms（差 150 倍）。

其原有职责的承接：**耗时** → SpanMetricsLayer 直方图（/metrics）；**调用结构 +
正确耗时的火焰图/瀑布** → OTLP → Tempo（Grafana Explore 自带 Flame graph 视图）；
**事件级 async 行为** → dial9（Pyroscope/eBPF 持续剖析链已下线；CPU 采样可按需
开 dial9 cpu-profiling feature，需 frame pointers）。容器内手动诊断保留
bpftrace/strace/sysstat/jq（需在 config.yml services.security 显式提权）。

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
请求（有 traceparent 继承；无则合成——本指南）
  ↓
rcoder 日志 JSON 顶层 trace_id（jq / Loki `| json | log_processed_trace_id=`）
  ↓
Loki 关键字/字段检索
  ↓ TraceID 字段
Tempo trace 瀑布/Flame graph（tracesToLogsV2 回跳 Loki）
```
