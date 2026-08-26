# @nuwax-ai/file-server-proxy

Rust 原生 `file-server-proxy`——**60000 单一入口**的文件服务代理，单二进制自带内嵌 Rust file-server，并按路由策略在 Rust 与 TypeScript 版 [nuwax-file-server](https://www.npmjs.com/package/nuwax-file-server)（作为依赖自动安装）之间分流。

面向 nuwax Electron 客户端与本地开发环境的 sidecar 分发形态（平台二进制从阿里云 OSS 下载；rcoder 主 pod / agent-runner 容器内嵌形态不走本包）。

## 安装

```bash
npm install @nuwax-ai/file-server-proxy
```

安装时自动：

1. 从 OSS 预下载当前平台的 proxy 二进制（失败仅警告，首次运行会重试；`FILE_SERVER_PROXY_SKIP_DOWNLOAD=1` 跳过）；
2. 安装 `nuwax-file-server`（**精确版本 pin**，与 proxy 版本同步测试，`npm update` 不会拉动它）。

要求 Node ≥ 22（nuwax-file-server 的硬门槛）。

## 架构

```
外部调用方 ──→ :60000 file-server-proxy（单 Rust 进程）
                 ├─ 分流代理（本二进制）
                 ├─ 内嵌 Rust file-server（以 lib 集成，rust 域请求进程内 oneshot
                 │  直调——无内部监听端口、零 loopback 跳）
                 └─ TS nuwax-file-server（唯一额外进程，随机端口，由 CLI 拉起托管）
```

四档路由策略（`--policy`；同一词汇表贯穿 helm/config.yml/CLI/env 三层）：

| 策略 | 行为 | TS 进程 |
|---|---|---|
| `userapp_split`（默认） | `/api/userapp*` 或 `x-service-type: userapp` → 内嵌 Rust；其余 → TS | 需要 |
| `all_rust` | 全部 → 内嵌 Rust file-server（路径白名单：`/api/*`、`/health`、`/`、`/api-docs*`） | 不需要 |
| `all_ts` | 全部 → TS nuwax-file-server | 需要 |
| `ts_first` | **仅** `/api/userapp*` → 内嵌 Rust；存量同名接口全走 TS——**含带 `x-service-type` 标记的请求**（header 判据失效，由 TS 以 `service_type` 入参内部处理 userApp 业务） | 需要 |

> `ts_first` 的前提：TS 源工程（nuwax-file-server）已支持 `service_type` 入参处理
> userApp 业务。TS 未就绪时切此模式 = 存量 userApp 业务按 TS 普通业务处理（正是
> 验证 TS 兼容性的切换目的）。

### 三层控制链路（过渡期切流同一词汇表）

| 层 | 控制入口 | 生效方式 |
|---|---|---|
| K8s/helm | `rcoder.fileServerProxyPolicy`（values，四值直配） | configmap → config.yml，pod 重启生效 |
| 容器/bin | `file-server-proxy --policy <四值>` 或 `ROUTE_POLICY` env（参数优先） | 改 supervisord conf → `supervisorctl reread && update` |
| npm CLI | `file-server-proxy start --policy <四值>` | start/restart 生效 |

## 命令

```bash
file-server-proxy start [--policy <userapp_split|all_rust|all_ts>]
                        [--port <60000>] [--rust-port <8086>]
                        [--ts-port <N>] [--detached]
file-server-proxy stop [--all]
file-server-proxy status
file-server-proxy restart [start flags]
file-server-proxy --version
```

- `start` 默认**前台**运行（Ctrl-C 退出时清理：杀代理；由本 CLI 拉起的 TS 一并停止）。`--detached` 后台运行，日志写 `os.tmpdir()/file-server-proxy/proxy.log`；
- TS 端口默认**随机分配未占用端口**（`--ts-port` 可固定；见下方单实例说明）；
- `stop` 停代理与**由本 CLI 拉起的** TS；`--all` 强制停 TS（即使它先于本 CLI 存在）；
- `status` 报告代理/TS 各组件状态；`restart` 携带的 flags 原样透传给 `start`。

## nuwax-file-server 托管语义

- TS 自身是**全局单实例**设计（PID 文件 + 启动锁在 `os.tmpdir()/nuwax-file-server/`）；
- `start` 时若探测到**已有健康 TS 实例**（任意端口），直接复用该端口转发（`status` 显示 `reused/external`，`stop` 不会动它）；
- 没有实例时以随机端口拉起（`status` 显示 `managed`，`stop`/前台退出时连带停止）；
- 若上次异常退出留下陈旧启动锁，TS 自带 120 秒自动清理窗口——期间 `start` 报错属预期，稍候重试即可。

## 日志

**双文件日志**（按日滚动，目录 = `FILE_SERVER_LOG_DIR`，本机未设时兜底
`os.tmpdir()/file-server-proxy/logs`；目录不可用时 Rust 侧自动回退系统临时目录）：

| 文件 | 内容 | 排查什么 |
|---|---|---|
| `file-server-proxy.log` | 代理自身：分流决策/上游错误/生命周期 | 请求走错边、502、启停问题 |
| `file-server.log` | 内嵌 file-server：请求日志/业务错误 | 文件操作本身（上传/列表/删除） |

后台模式的 console 输出另存 `os.tmpdir()/file-server-proxy/proxy.log`（npm CLI 的
stdout 重定向）；`file-server-proxy status` 不读日志，排障直接看上表文件。

## 端口总览

| 端口 | 服务 | 说明 |
|---|---|---|
| 60000 | file-server-proxy | 对外唯一入口（`--port` 可改） |
| —（无内部端口） | 内嵌 Rust file-server | 以 lib 集成进程内直连；`--rust-port` 仅纯转发形态（`--no-embed`/`EMBED_FILE_SERVER=0`，上游为外部进程）时生效 |
| 随机 | nuwax-file-server | 仅 loopback（`--ts-port` 固定） |

## 内嵌 Rust file-server 的环境变量

代理进程内嵌的 Rust file-server 用环境变量配置工作目录等（默认值面向容器 `/app/...`，本机使用需覆盖）：

| 变量 | 默认 | 说明 |
|---|---|---|
| `PROJECT_SOURCE_DIR` | `/app/project_workspace` | 项目源码 workspace 根 |
| `COMPUTER_WORKSPACE_DIR` | `/app/computer-project-workspace` | computer 域 workspace 根 |
| `USERAPP_WORKSPACE_DIR` | `/app/userapp-workspace` | userApp 开发卷根 |
| `INIT_PROJECT_DIR` / `UPLOAD_PROJECT_DIR` / `DIST_TARGET_DIR` | `/app/...` | 初始化/上传/构建产物目录 |
| `FILE_SERVER_LOG_DIR` / `LOG_BASE_DIR` | `/app/logs/...` | 日志目录 |

完整清单见仓库 `crates/file-server/src/config/`。注意：`FILE_SERVER_PORT` 在本包语义固定为**代理监听口**，内嵌 file-server 端口一律以 `--rust-port`（`RUST_UPSTREAM_PORT`）为准。

## 环境变量（本包 CLI）

| 变量 | 说明 |
|---|---|
| `FILE_SERVER_PROXY_BINARY` | 使用自定义 proxy 二进制，跳过 OSS 下载（调试/离线分发/本地冒烟） |
| `FILE_SERVER_PROXY_TARGET` | 覆盖 Rust target triple（交叉打包用） |
| `FILE_SERVER_PROXY_SKIP_DOWNLOAD=1` | postinstall 跳过预下载 |

## 平台支持

| 平台 | Rust target |
|---|---|
| macOS arm64 / x64 | `aarch64-apple-darwin` / `x86_64-apple-darwin` |
| Linux x64 (glibc / musl) | `x86_64-unknown-linux-gnu` / `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-gnu` |
| Windows x64 | `x86_64-pc-windows-msvc` |

## 排错

- **postinstall 下载失败**：不阻断安装，首次 `start` 时重试；也可手动 `FILE_SERVER_PROXY_BINARY=<path>` 指定二进制；
- **`start` 报 already running**：先 `stop`；`status` 看 pid；
- **60000 端口被占**：本机 rcoder 本地开发也可能占 60000——用 `--port` 换口，或先停占用方；
- **TS start 报 stale lock**：见上文 120 秒自愈窗口；
- **代理 502**：上游（TS 随机口 / 直连内嵌 rust 异常）未就绪或已死——`status` 检查，必要时 `restart`；后台模式日志在 `os.tmpdir()/file-server-proxy/proxy.log`。

## 版本与发版

- npm 包版本由 rcoder 仓库 git tag `file-server-proxy-v*` 驱动 CI 注入；`nuwax-file-server` 精确 pin，升级 TS 时随本包发版手动 bump 并回归；
- 二进制内嵌 Rust file-server（cargo feature `embed-file-server`），与 `@nuwax-ai/file-server` npm 包产物同源。
