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

> Rust 版 env 全部对齐 nuwax 名:`FILE_SERVER_PORT` / `PROJECT_SOURCE_DIR` / `COMPUTER_WORKSPACE_DIR` /
> `UPLOAD_PROJECT_DIR` / `DIST_TARGET_DIR` / `LOG_BASE_DIR` / `COMPUTER_LOG_DIR` / `TEMPLATE_CACHE_DIR` /
> `NODE_MODULES_LOCAL_DIR` / `INIT_PROJECT_DIR` / `DEPLOYMENT_MODE` / `FAST_RESTART_ENABLED` / `GIT_*`。
> build-agent-docker configmap 无需改 env。

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

> Rust 版 `/health` 契约与 nuwax 一致:`GET /health` → 200 `{status:"ok",service:"file-server"}`,
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

## 5. Rust 版相对 node 版的差异(已知)

- **不跑** `@xagi/dev-inject` + `vite-plugin-design-mode`(监控面板 + 设计模式插件;已确认不需要)
- **不跑** pnpm store 定时清理(nuwax 的 `pnpmPruneScheduler`,独立功能,可在 rcoder 侧另行加 cron)
- 错误分类、进程组 kill、早退检测、日志原子写等比 node 版更强
