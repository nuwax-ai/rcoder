# rcoder 可观测性指南

统一整理本地开发排查工具的启用方式与使用方法。release/CI 构建不带任何观测 feature——零代码零开销。

## 快速索引

| 工具 | 用途 | 启用方式 | 排查场景 |
|------|------|----------|----------|
| **trace_id 日志注入** | 日志 JSON 顶层 trace_id 字段 | 自动（有 traceparent 时） | 跨服务全链路日志过滤 |
| **tokio-console** | 异步任务/锁/waker 运行时观测 | `console` feature | 死锁、锁等待、任务泄漏 |
| **tracing-flame** | span 耗时火焰图 | `flame` feature | 哪个 async 函数慢 |
| **Pyroscope** | CPU 火焰图持续剖析 | 已部署（compose） | CPU 热点 |
| **/metrics** | HTTP 请求量/延迟 | 默认开启 | 性能回归 |

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
# 本地运行（产生 .folded 文件）
make run-flame                          # 默认 logs/tracing.folded
make run-flame RCODER_FLAME=/tmp/f.folded

# 渲染 SVG（一次性安装：cargo install inferno）
inferno-flamegraph < logs/tracing.folded > flame.svg
open flame.svg                          # 浏览器查看
```

**与 Pyroscope 的区别**：Pyroscope 是 CPU 采样（哪些函数在烧 CPU），tracing-flame 是 span 耗时（哪些 async 路径在等待/耗时）。

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
