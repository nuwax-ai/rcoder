# File-server A/B 对照

在本机用独立 Docker Compose project 并行运行 Rust `file-server-proxy --embed --policy all_rust` 和指定提交的 TypeScript `nuwax-file-server`，对两边发送同一组 HTTP 请求并比较响应、文件状态。它不经过 RCoder 代理或 TS 上游，避免路由转发掩盖实现差异。

## 运行

```bash
make file-server-ab
```

默认执行离线 `core` 套件。需要比较 Git 或模板依赖安装/构建/开发服务时显式选套件：

```bash
make file-server-ab AB_SUITE=git
make file-server-ab AB_SUITE=build
make file-server-ab AB_SUITE=all
```

`build` / `all` 会从两份 ZIP 模板创建项目，访问 npm registry 安装依赖并构建，再验证 dev server 的启动、HTTP 可达、日志分页、日志缓存接口、端口池登记、keep-alive、重启和停止；也会对照构建错误解析。项目创建包含模板解压和初始 Git 提交，驱动为这一步单独留出 120 秒，避免慢磁盘上的 30 秒通用请求预算过早中断后继续请求尚未初始化完成的项目。首次运行较慢；失败会保留证据并区分比较差异与环境错误。

可选参数：

```bash
make file-server-ab \
  AB_SUITE=core \
  AB_TS_SOURCE=/path/to/nuwax-file-server \
  AB_TS_REF=main \
  AB_PNPM_VERSION=10.34.5 \
  AB_RUST_PORT=61101 AB_TS_PORT=61100 \
  AB_KEEP=1
```

默认宿主机端口由 Docker 动态分配并只绑定到 `127.0.0.1`。指定 `AB_RUST_PORT` / `AB_TS_PORT` 后使用固定端口。每轮都有唯一 Compose project、容器镜像 tag、宿主机工作区和报告目录。正常结束会删除本轮容器、镜像与临时工作区；对照差异或服务请求失败时会保留工作区以便复查。`AB_KEEP=1` 保留容器与工作区。

Rust 与 TypeScript 镜像按顺序构建，避免两个依赖安装/编译任务同时争用本机内存和磁盘。

## 报告

报告位于 `tests-e2e/reports/file-server-ab/<run-id>/`，包括：

- `manifest.json`：Rust/TS 源码身份、镜像 ID、Node/pnpm/Git 版本、运行架构、模板 ZIP 哈希、配置 profile 和规则文件哈希。
- `requests.jsonl`：每个场景、每一侧的请求摘要、状态、选定响应头、耗时、完整响应哈希、原文引用或传输错误。
- `bodies/`：请求与响应原文；单个文件最多保存 2 MiB，截断状态、完整字节数和完整 SHA-256 仍记录在 JSONL。
- `state/`：请求执行前与结束后的两侧工作区树、文件内容摘要、权限与软链接；`git`/`all` 另保存每个 fixture repo 的 HEAD、refs 对应 tree、index entries 和 porcelain 状态。
- `diff.json`、`summary.md`：机器可读差异及人工可读总览；错误响应只归一化精确路径 `/error/requestId` 和 `/error/timestamp`，两侧原值仍保存在正文证据中。
- `route-coverage.json`：TS/Rust 路由交集、已覆盖/待覆盖状态，以及本次选择的套件是否实际执行了对应场景。
- `logs/compose.log`：Compose 服务日志。

镜像解析、构建或容器启动失败时不会生成比较通过结果；启动器会写 `runner-failure.json`，包含失败阶段和类别，并在 `summary.md` 中明确说明没有产生对照结果。HTTP 传输错误保存在 `diff.json`，会使命令失败。

报告继承启动环境的 `umask 077` 权限，不应手工加入凭据。当前 `core` 场景不读取任何真实凭据。

## 差异规则

默认所有差异都视为未分类并使命令失败。经人工确认的预期差异可写入 `diff-rules.json`，规则必须精确匹配 `case`、JSON Pointer `path`、`kind`、Rust 值和 TypeScript 值，并附原因、审核者和未过期日期；不接受通配路径，也不允许把传输错误归为预期差异。值缺失用 `{"$missing":true}` 表示。命中规则只改变分类，不会删除原始响应或 diff。

## 套件与覆盖边界

- `core`：健康/API 版本、React/Vue 模板初始化与读取、项目文件更新、静态普通/Range 读取、Computer 文件列表/resolve/search/metadata 边界和基础文件系统操作。无 npm 外网依赖。
- `git`：通过 HTTP 对照 init、status、add、commit、file-content、branch create/delete、tag、log、worktree/staged diff、unstage、checkout、discard、revert，以及 mixed/hard/soft reset。另用系统 Git 为两侧独立 fixture 准备相同的真实 merge-conflict index，再通过 HTTP 对照 `status.conflicted`；当前 API 没有 merge 操作端点，因此不把 fixture 准备命令当成被测 API。每个会改变历史或工作树的流程使用独立 pageApp fixture，避免一个实现的失败污染其他场景；最终比较 refs 对应 tree、HEAD tree、index entries、工作区状态和文件树。Rust 服务使用 gix，TS 服务使用镜像内系统 Git；驱动只用系统 Git读取最终仓库状态及准备对称 fixture，不参与被测 API 操作。
- `build`：分别用两份模板走项目初始化、依赖安装、production build、产物静态读取、start-dev、真实页面 HTTP、开发日志分页、日志缓存查询/清理、端口池状态、keep-alive、restart-dev 和 stop-dev，并对照构建错误解析。依赖 registry 网络；报告记下环境版本与错误。
- `all`：顺序执行以上套件。路由清单按当前 TypeScript 基线快照维护；没有 A/B 场景的共同路由明确标为 pending。

路由清单的 `typescript_revision` 必须与本次准备的 TypeScript Git 提交一致；基线变化时 Make 运行会在发请求前失败，要求先复核并更新路由清单。

这不是“所有路由都已覆盖”的声明。真实结果以报告中的 `route-coverage.json` 与 `requests.jsonl` 为准；Rust-only `/api/v1/userapp` 由既有 UserApp 测试单独覆盖。
