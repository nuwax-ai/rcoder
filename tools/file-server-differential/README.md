# file-server 双实现差异测试

本目录模拟生产 `rcoder` 容器中与文件服务有关的挂载，并使用同一组测试向量分别调用 TypeScript `nuwax-file-server`（默认端口 `61000`）和 Rust `file-server`（默认端口 `61001`）。

两个实现拥有完全隔离的项目、Computer、上传、构建产物、日志和缓存目录。测试会比较 HTTP 状态/响应、普通文件、符号链接、文件权限、Agent Hook 配置和 Git 语义；不会直接比较 `.git` 内部文件，因为提交时间和对象 ID 天然可能不同。

## 快速运行

核心套件要求 Node.js 22+、Rust/Cargo 和 git；运行 Vite 慢速套件时还需要 pnpm。TS 源码默认位于 `/Users/soddy/Documents/git-workspace/nuwax-file-server`，可覆盖。

```bash
cd /Users/soddy/Documents/git-workspace/rcoder
tools/file-server-differential/run.sh
```

只管理服务或重复执行测试：

```bash
tools/file-server-differential/prepare.sh
tools/file-server-differential/start.sh
node tools/file-server-differential/test.mjs
tools/file-server-differential/stop.sh
```

覆盖源码位置或端口：

```bash
TS_SERVER_ROOT=/path/to/nuwax-file-server TS_PORT=62000 RUST_PORT=62001 \
  tools/file-server-differential/run.sh
```

`prepare.sh` 会重建本目录下被忽略的 `runtime/`，并把 `tmp/template/*.zip` 分别复制到两套 `project_init`。报告写入 `runtime/report/`；失败时服务日志保留在 `runtime/*/logs/server.log`。

默认套件覆盖确定性的核心路径：React/Vue 模板创建、项目内容读写、静态文件、单个/批量/附件上传、Computer 工作区文件操作、Claude/Codex/OpenCode Hook 产物，以及 Git 初始化、状态、提交、日志、分支和标签。Vite/pnpm 的启动、重启、停止、构建应作为慢速套件加入，避免核心测试受网络和依赖缓存影响。

报告中的 `ACCEPT` 不是被忽略的差异，而是显式白名单：目前只允许 Rust 将敏感 Agent 配置写成 `0600`、TS 写成 `0644`，且路径和内容必须完全一致。其他新增差异仍会令进程以非零状态退出。
