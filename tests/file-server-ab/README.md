# File-server A/B 对照

在本机用独立 Docker Compose project 并行运行 Rust `file-server-proxy --embed --policy all_rust` 和指定提交的 TypeScript `nuwax-file-server`，对两边发送同一组 HTTP 请求并比较响应、文件状态。它不经过 RCoder 代理或 TS 上游，避免路由转发掩盖实现差异。

## 运行

```bash
make file-server-ab
```

可选参数：

```bash
make file-server-ab \
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
- `state/`：请求执行前与结束后的两侧工作区树、文件内容摘要、权限与软链接。
- `diff.json`、`summary.md`：机器可读差异及人工可读总览；错误响应只归一化精确路径 `/error/requestId` 和 `/error/timestamp`，两侧原值仍保存在正文证据中。
- `logs/compose.log`：Compose 服务日志。

镜像解析、构建或容器启动失败时不会生成比较通过结果；启动器会写 `runner-failure.json`，包含失败阶段和类别，并在 `summary.md` 中明确说明没有产生对照结果。HTTP 传输错误保存在 `diff.json`，会使命令失败。

报告继承启动环境的 `umask 077` 权限，不应手工加入凭据。当前 `core` 场景不读取任何真实凭据。

## 差异规则

默认所有差异都视为未分类并使命令失败。经人工确认的预期差异可写入 `diff-rules.json`，规则必须精确匹配 `case`、JSON Pointer `path`、`kind`、Rust 值和 TypeScript 值，并附原因、审核者和未过期日期；不接受通配路径，也不允许把传输错误归为预期差异。值缺失用 `{"$missing":true}` 表示。命中规则只改变分类，不会删除原始响应或 diff。

## 当前覆盖边界

目前只执行离线 `core` 子集：健康探针、React/Vue 模板项目初始化与读取、项目文件更新和静态读取、Computer 文件列表边界、文件系统浏览/创建/重命名。Git 操作与依赖安装、构建及 dev server 生命周期还未纳入本轮。
