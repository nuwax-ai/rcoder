# app-cli 命令用法

`app-cli` 是 UserApp 容器运行时编排器（crate 位于 `crates/app-cli`），装在 app-runtime
镜像中，替代旧版 workspace `start.sh`。职责一句话：**读 `release.lock.toml` → 编排子服务
+ pingap 反向代理 → 提供健康探针、日志查询与代理热更的管理 API**。

> 注意：app-cli 被排除在 rcoder workspace 之外（独立 Cargo.lock），因为 pingap-config
> 的 bollard/regex 传递依赖会与 workspace 锁冲突。构建须在 `crates/app-cli` 目录单独
> `cargo build`（镜像内由 app-runtime-base Dockerfile 的 app-cli-builder stage 完成）。

## 运行模型

容器内进程层级（supervisord 为 PID 1，见 `docker/app-runtime-base/`）：

```text
supervisord (start-app.sh)
├── [program:postgresql]   priority=10
├── [program:pgweb]        priority=20
├── [program:ttyd]         priority=30
└── [program:app]          priority=40  ← app-cli（UserApp command 动态注册）
    ├── 管理 API            0.0.0.0:3010（--admin-addr）
    ├── 子服务 1..N         各自独立进程组，127.0.0.1:<locked_port>
    └── pingap             0.0.0.0:9080（平台唯一入口），admin 仅 loopback:3018
```

app-cli 前台阻塞 supervise：任一子进程退出或收到 SIGINT/SIGTERM → 优雅停掉全部子进程
→ 自身退出 → supervisord 感知 `[program:app]` 退出 → 整组重启。任何启动阶段失败也会先
清理已启动的子进程再退出（防止孤儿进程 reparent 到 PID 1 持端口，导致重启后 bind 冲突
crash loop）。

## 命令行参数

```text
Usage: app-cli [OPTIONS]

      --workspace <WORKSPACE>    workspace 根（含 workspace.manifest.toml + 各子项目）
                                 [env: APP_CLI_WORKSPACE=] [default: /app/code]
      --log-dir <LOG_DIR>        日志目录 [env: APP_CLI_LOG_DIR=] [default: /app/logs]
      --admin-addr <ADMIN_ADDR>  管理 API 监听地址
                                 [env: APP_CLI_ADMIN_ADDR=] [default: 0.0.0.0:3010]
      --pingap-bin <PINGAP_BIN>  pingap 二进制路径
                                 [env: APP_CLI_PINGAP_BIN=] [default: /usr/local/bin/pingap]
      --gen-lock <WORKSPACE>     本地开发：只生成 release.lock.toml + 预览 Pingap 配置后退出
                                 [env: APP_CLI_GEN_LOCK=]
  -h, --help
  -V, --version
```

每个参数都可用同名 env 覆盖；命令行值优先于 env。

## 两种运行模式

### 1. 编排模式（默认，生产用）

```bash
app-cli --workspace /app/code --log-dir /app/logs
# 或纯 env：
APP_CLI_WORKSPACE=/app/code app-cli
```

启动流程（`supervisor::run`）：

1. **读锁** — 加载 `<workspace>/release.lock.toml`，缺失即 Fail Fast。
2. **版本门禁** — 当前 app-cli 版本必须 ≥ 锁内 `minimum_app_cli_version`，否则拒绝启动。
   pingap 版本 / commit / 镜像 digest 与 `RCODER_PINGAP_VERSION` 等 env 不一致仅 warn，
   不阻断（保证平滑升级）。
3. **等 PG** — `pg_isready` 轮询最多 30 次 × 2s。连接参数读 `PGHOST` / `PGPORT` /
   `POSTGRES_USER` / `POSTGRES_PASSWORD`（默认 localhost:5432 app/app）。
4. **逐服务 migrate → start**（按锁内 services 顺序，即依赖拓扑序）：
   - `[run].migrate` 非空则先跑迁移命令，失败 Fail Fast（stdout/stderr 进 app-cli 日志）；
   - `[run].command` 启动服务（独立进程组），运行时注入 env 见下表；
   - stdout/stderr 重定向到 `<log_dir>/<service_id>/runtime.{out,err}.log`（10MB 轮转，
     保留 3 份，append 不 truncate）。
5. **启动 pingap** — 按 `[pingap].mode`（managed/extend/custom）编译生效配置 →
   `pingap -t` 语法校验 → 原子写入 `/run/app-cli/pingap/<release_id>/pingap.toml`
   （0600，旧配置备份为 `.prev`）→ 以 `--autoreload` 启动 → 经 loopback admin 读
   `config_hash` 确认配置实际生效（确认失败 = 启动失败，整组重启）。
6. **readiness 判定**（见下节）。
7. **supervise** — 阻塞直到信号或任一子进程退出；停机宽限期取各服务
   `shutdown_timeout_seconds` 最大值（默认 30s）：先 SIGTERM 全组，超时 SIGKILL 残留。

全部服务都因空 `[run].command` 被跳过时，视为启动失败（不允许"零服务"容器）。

启动成功后打印运行拓扑汇总（终端与日志文件同步输出）：

```text
🔌 运行拓扑 entrypoint=http://0.0.0.0:9080:
🔌   backend-python port=4583 running route=/api/python/ (strip_prefix=true)
🔌   backend-worker port=4584 running internal (无 [proxy])
```

每条 `🚀 start <service_id> (<name>) on :<port> (pid=...)` 与汇总行都以
service_id 为主标识（日志目录、pingap 路由同按 service_id 命名），从启动日志
即可反查端口与路由，无需另读 effective config。

**运行时注入给子服务的 env**（最后应用，manifest `[env]` 无法覆盖）：

| env | 值 |
|---|---|
| `HOSTNAME` | `0.0.0.0`（服务必须监听它） |
| `PORT` | 锁内分配的端口 |
| `APP_LOG_DIR` | `<log_dir>/<service_id>` |
| `APP_SERVICE_ID` | 服务 ID |
| `APP_RELEASE_ID` | 当前 release |

### 2. 本地开发模式 `--gen-lock`（不启服务）

```bash
app-cli --gen-lock ./my-workspace
```

复用与构建侧同一套纯函数（发现 → 校验 → 锁定 → Pingap 编译），**不需要 pingap 二进制、
PG 或镜像**，秒级完成：

1. 读 `workspace.manifest.toml`，自动发现 enabled 子项目并打印清单；
2. 生成 `release.lock.toml` 写入 workspace 根（pingap 版本 / digest 用本地占位值，
   `minimum_app_cli_version` 取当前二进制版本，保证紧接着能直接 `run`）；
3. 打印端口分配 + 启动拓扑序，以及编译出的 Pingap 生效配置 TOML 与 expected hash。

用途：模板与 manifest 设计的快速迭代 —— 改 toml → 重跑 → 立刻看端口 / 拓扑 / 路由 /
Pingap 配置是否如预期。验证通过后可直接：

```bash
APP_CLI_WORKSPACE=./my-workspace APP_CLI_PINGAP_BIN=<pingap路径> app-cli
```

## 环境变量总表

| 变量 | 默认 | 说明 |
|---|---|---|
| `APP_CLI_WORKSPACE` | `/app/code` | workspace 根 |
| `APP_CLI_LOG_DIR` | `/app/logs` | 日志根目录 |
| `APP_CLI_ADMIN_ADDR` | `0.0.0.0:3010` | 管理 API 监听地址 |
| `APP_CLI_PINGAP_BIN` | `/usr/local/bin/pingap` | pingap 二进制 |
| `APP_CLI_GEN_LOCK` | — | 等价 `--gen-lock` |
| `APP_CLI_PINGAP_RUNTIME_DIR` | `/run/app-cli/pingap` | 生效配置落盘根 |
| `APP_CLI_PINGAP_ADMIN_PORT` | `3018` | pingap admin 探测端口（仅 loopback 只读） |
| `APP_CLI_SKIP_PG_WAIT` | 未设 | **dev 逃生**：跳过 60s PG 等待，生产不设 |
| `APP_CLI_SKIP_PINGAP_CONFIRM` | 未设 | **dev 逃生**：跳过启动时 config_hash 确认，生产不设 |
| `PGHOST` / `PGPORT` / `POSTGRES_USER` / `POSTGRES_PASSWORD` | localhost/5432/app/app | PG 等待探测用 |
| `RCODER_PINGAP_VERSION` / `RCODER_PINGAP_COMMIT` / `RCODER_RUNTIME_IMAGE_DIGEST` | — | 运行时身份复核（发布激活时由 app_manager 注入），不匹配仅 warn |
| `RUST_LOG` | `app_cli=info` | tracing EnvFilter |

## 健康探针语义

app-cli 自带两个探针，K8s 的 liveness/readiness 直接指向管理 API：

| 探针 | 语义 |
|---|---|
| `GET /health` | **liveness**：app-cli 进程能响应即 200，永不为后端背锅 —— 后端 app 有 bug 起不来时容器不被杀，可 `kubectl exec` 进去排查 |
| `GET /ready` | **readiness**：默认 app-cli 初始化完成即 ready；`[health].bridge_service` 显式配置时，桥接等待该服务 `readiness_path`（120s 超时 → 保持 503 摘流，但 liveness 仍 200） |

即：**默认 app-cli 自给自足，不强依赖任何后端**；只有显式声明了 bridge_service 才做
深检查，且失败只摘流、不杀容器。

## 管理 API（默认 :3010）

**响应形态**：JSON 端点统一 `HttpResult` 信封 `{code, message, data, tid, success}`
（`"0000"`=成功；失败 `data` 恒 `null` + 端点特定错误码如 `INVALID_LOG_QUERY`），并保留
语义 HTTP 状态码（202/400/403/409）。**豁免信封**：`/health`、`/ready`（kubelet 探针只看
状态码）、`/v1/logs/stream`（SSE 事件流）、`/v1/proxy/effective-config`（TOML 文本直读）。

### 探针与元信息

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | liveness，恒 200（豁免信封） |
| GET | `/ready` | readiness，200 / 503（豁免信封） |
| GET | `/openapi.json` | 完整 OpenAPI schema（utoipa 生成） |

### 日志（Runtime Logs）

三个接口共用同一请求体（全 snake_case wire）；`sources/query` 与 `query` 响应为
HttpResult 信封（data = 日志源列表 / 日志快照对象），`stream` 为 SSE（豁免信封）：

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/v1/logs/sources/query` | 列出各服务声明的日志源及匹配到的文件 |
| POST | `/v1/logs/query` | 历史查询，返回分页 cursor |
| POST | `/v1/logs/stream` | SSE 实时尾随（500ms 轮询 + 15s 心跳） |

请求体（字段全部可选）：

```json
{
  "selectors": [{"service_id": "backend-go", "source_ids": ["application"]}],
  "levels": ["WARN", "ERROR"],
  "keyword": "timeout",
  "since": "2026-07-29T10:00:00+08:00",
  "until": "2026-07-29T12:00:00+08:00",
  "tail": 100,
  "cursor": null
}
```

- `selectors` 空 = 全部服务的全部日志源；`source_ids` 空 = 该服务全部源。
- `since`/`until` 须为 RFC3339；`keyword` ≤ 256 字节。
- `tail`：**首次查询**（无 cursor）每源回看的条数，默认 100，上限 10000；带 cursor 的
  续查从上次 offset 继续，tail 不再生效。
- 限额：≤ 64 个服务、≤ 128 个源、cursor ≤ 64KB。
- cursor 绑定 app-cli 启动实例（boot_id）；跨重启携带旧 cursor 返回 `cursor_reset: true`
  并从头开始。

SSE 事件类型：`log`（单条记录）、`source_error` / `source_recovered`（源读写故障与恢复，
故障期间只报一次）、`cursor_reset`、`checkpoint`（cursor 变化时推送，客户端可持久化用于
断线续传）、`heartbeat`（15s）。

外部访问走 rcoder 转发：`POST /api/v1/apps/{app_id}/logs/{sources/query|query|stream}`
（rcoder 透明代理——请求/响应体与状态码原样透传，信封直达调用方，详见
[04-logs.md](04-logs.md)）。

### 代理（Runtime Proxy）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/v1/proxy/validate` | 重新编译 + `pingap -t` 校验生效配置（不应用） |
| POST | `/v1/proxy/reload` | 原子替换生效配置 → autoreload 感知 → admin 读 config_hash 确认真正生效；**确认失败自动回切 `pingap.toml.prev`** 并二次确认 |
| GET | `/v1/proxy/status` | release / pingap 版本身份与生效配置路径 |
| GET | `/v1/proxy/effective-config` | 当前生效 Pingap TOML 全文 |
| GET | `/v1/proxy/upstreams` | 各服务 → `127.0.0.1:<port>` 映射及是否被代理 |

reload 的 fail-safe 语义：basic/storages/server addr 类变更在 `--autoreload` 下本就热更
不生效 → hash 永不匹配 → 超时 + 回切是正确行为（宁可回退也不留未确认的新配置）。admin
通道仅 loopback 只读，凭证每次启动随机生成，永不通过 admin 写配置 —— TOML 是唯一权威。

## 日志文件布局

```text
/app/logs/
├── app-cli.log.2026-08-21        # app-cli 自身日志（daily 轮转 + stderr 同步输出）
├── <service_id>/
│   ├── runtime.out.log           # 子服务 stdout（10MB 轮转 ×3）
│   ├── runtime.err.log           # 子服务 stderr
│   └── application*.log          # 应用框架自写的文件日志（[logs] 声明，app-cli 只读）
```

## 相关文档

- [01-quick-start.md](01-quick-start.md) — 目录约定与运行时固定目录
- [02-manifest-reference.md](02-manifest-reference.md) — workspace / project manifest 字段
- [03-pingap.md](03-pingap.md) — 三种代理模式与护栏
- [04-logs.md](04-logs.md) — 多服务文件日志与外部 API
- [05-releases.md](05-releases.md) — release.lock.toml 与发布链
