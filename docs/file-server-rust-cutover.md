# file-server Rust 版 cutover 交接文档

RCoder 的 file-server(node `nuwax-file-server`)已用 Rust 重写(`crates/file-server/`)。
本文记录**镜像侧改动(本仓)**与**启动切换(在 build-agent-docker 仓的 start-services.sh)**,
以及回滚开关和上线后冒烟清单。

## 1. 本仓已完成(镜像侧)

`docker/rcoder-master/Dockerfile`:
- builder 阶段 `cargo build --release --bin rcoder --bin agent_runner --bin file-server`
- 运行阶段 `COPY --from=rust-builder /build/target/release/file-server /app/bin/file-server` + chmod

镜像内现在同时存在:
- `/app/bin/file-server` —— **Rust 版**(新)
- `nuwax-file-server`(npm 全局,node 版)—— **保留作回滚**

> Rust 版兼容 `FILE_SERVER_PORT`，也兼容 nuwax 原生 `PORT`；请求体上限同时支持字节数
> `REQUEST_BODY_MAX_BYTES` 和 nuwax 字符串格式 `REQUEST_BODY_LIMIT=1gb`，默认且硬上限为 1 GiB。其余业务 env 使用:
> `PROJECT_SOURCE_DIR` / `COMPUTER_WORKSPACE_DIR` /
> `UPLOAD_PROJECT_DIR` / `DIST_TARGET_DIR` / `LOG_BASE_DIR` / `COMPUTER_LOG_DIR` /
> `INIT_PROJECT_DIR` / `DEPLOYMENT_MODE` / `FAST_RESTART_ENABLED` / `GIT_*`。
> build-agent-docker configmap 无需改 env。

> 所有 multipart 文件上传统一由 `UPLOAD_MAX_FILE_SIZE_BYTES` 控制，默认且硬上限为
> `1073741824` 字节（1 GiB）；旧的 `UPLOAD_SINGLE_FILE_SIZE_BYTES` 已废弃。

> 可设置 `FILE_SERVER_CONFIG=/path/file-server.yaml` 使用 YAML/TOML/JSON 配置文件；环境变量继续作为覆盖层。服务日志按天写入 `FILE_SERVER_LOG_DIR`（默认 `/app/logs/file-server`），保留最近 7 个每日日志文件，日志级别可由 `RUST_LOG` 覆盖。

## 2. 启动切换(在 build-agent-docker 仓)

`start-services.sh` 当前启动 node `nuwax-file-server`(读 `FILE_SERVER_PORT`,周期 curl `/health`,连续 3 次失败 kill+重启)。
切换为 Rust 版只需把启动命令改成 `/app/bin/file-server`,建议加 env 回滚开关:

```bash
# 回滚开关: FILE_SERVER_BACKEND=node 走旧 node 版; 默认/=rust 走 Rust 版
FILE_SERVER_BIN="${FILE_SERVER_BACKEND:-rust}"
if [ "$FILE_SERVER_BIN" = "node" ]; then
  FILE_SERVER_CMD="nuwax-file-server"   # 旧 node 版
else
  FILE_SERVER_CMD="/app/bin/file-server" # 新 Rust 版
fi
# 原有 nohup ... & 写 /tmp/subapp.pid + /health 探活逻辑保持不变
nohup "$FILE_SERVER_CMD" > /app/logs/file-server.out 2>&1 &
```

> Rust 版 `/health` 契约与 nuwax 一致:`GET /health` → 200 且 `status:"ok"`,
> start-services.sh 的探活逻辑无需改。

## 3. 回滚

`FILE_SERVER_BACKEND=node` 重启 pod → 回到 node 版(node 版仍 `npm i -g` 在镜像里)。
数据无影响(同一套 PVC/路径,文件契约一致)。

## 4. 上线后端到端冒烟清单

Rust 版启动后(经 pingora `/proxy/{port}` 反代,与 node 版同链路):

- [ ] `/health` 返回 200
- [ ] **project**:get-project-content(树)/ create-project / files-update / upload / zip 回归
- [ ] **git**:`/api/git/*` 19 路由(status/diff/log/branch/tag/reset/revert/checkout/switch)对照前端
- [ ] **dev server**:start-dev 起 vite,经 pingora `/proxy/{port}/` 浏览器能访问 + **HMR 热更新生效**
  (HMR 靠 vite 默认 + pingora ws 透传,详见 memory `vite-hmr-via-default-plus-proxy-ws`)
- [ ] **dev server 错误分类**:故意起一个缺依赖项目,start-dev 返回结构化错误(如"缺少依赖 'xxx'")
- [ ] **computer**:`/api/computer/*`(file-list/files-update/upload/execute-command/install/create/delete/get-logs/push-skills/build-agent-package)
- [ ] 对比 node 版无功能退化

### 4.1 本地双服务差异探针

使用彼此隔离的工作区启动两个服务。TS 启动时会清理 `INIT_PROJECT_DIR` 下已解压目录，
因此先复制 ZIP 到临时模板目录，禁止直接把 TS 指向本仓源模板目录：

```bash
# 一次性准备隔离模板
mkdir -p /tmp/file-server-diff/templates
cp /Users/soddy/Documents/git-workspace/rcoder/tmp/template/*.zip \
  /tmp/file-server-diff/templates/

# terminal 1: TS
cd /Users/soddy/Documents/git-workspace/nuwax-file-server
PORT=60000 \
PROJECT_SOURCE_DIR=/tmp/file-server-diff/ts/projects \
COMPUTER_WORKSPACE_DIR=/tmp/file-server-diff/ts/computers \
INIT_PROJECT_DIR=/tmp/file-server-diff/templates \
node src/server.js

# terminal 2: Rust
cd /Users/soddy/Documents/git-workspace/rcoder
FILE_SERVER_PORT=60001 \
PROJECT_SOURCE_DIR=/tmp/file-server-diff/rust/projects \
COMPUTER_WORKSPACE_DIR=/tmp/file-server-diff/rust/computers \
INIT_PROJECT_DIR=/tmp/file-server-diff/templates \
cargo run -p file-server

# terminal 3: 只读/失败路径契约差异测试
node scripts/file-server-differential.mjs
```

脚本会比较健康检查、404、JSON rejection、业务校验、缺失 Git 工作区和构建错误解析；
任何业务字段差异都会以非零退出码结束，可直接接入 CI。涉及创建项目、上传 ZIP、Vite/HMR、
Git branch/tag/reset 的有状态场景仍按上面的冒烟清单执行，并确保两个服务使用不同工作区。

## 5. Rust 版相对 node 版的差异(已知)

- `@xagi/dev-inject` + `vite-plugin-design-mode` 已在 Rust dev 启动流程中执行。
- **不跑** pnpm store 定时清理(nuwax 的 `pnpmPruneScheduler`,独立功能,可在 rcoder 侧另行加 cron)
- TS 的进程级 CLI `start/stop/restart/status` 不迁移；K8s 由容器生命周期管理 file-server 本身。
- Rust 增强了错误分类、进程组 kill、上传原子切换、路径/Zip Slip 校验和 Git `spawn_blocking` 隔离。
