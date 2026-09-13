# AGENTS.md

本文件是跨 provider 的项目级单一事实源。Claude Code 通过 CLAUDE.md 中的导入读取本文件。

## 开始工作

1. 用 `git status --short` 和相关路径的 diff 确认工作树；保留其他并行开发改动，提交时只暂存本任务文件或改动块。
2. 按下表定位模块，先读相关实现及 `specs/` 下的 Spec、Plan、Tasks，再修改代码。
3. 先做聚焦验证，再按影响范围执行部署 E2E。跨 Docker/K8s 运行时的修改要分别验证两种模式。
4. 交付时说明改动、验证命令和退出码、实际通过范围及剩余问题；不能把旧报告视为本轮结果。

## 项目与代码导航

- **RCoder**: 基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，Rust Cargo workspace。
- **Rust edition**: 2024（见 `Cargo.toml` `[workspace.package]`）。
- **运行时**: 本地 `cargo run`（8087）、Docker Compose `make dev-up`（8090）、K8s DevSpace `devspace dev`（8290）；远端 K8s 开发测试使用 `make remote-k8s-*`，访问地址由 `.env.local` 配置。
- **请求链路**: HTTP/SSE 客户端 → RCoder → Tonic gRPC → agent_runner → ACP agent；UserApp 由应用管理层调用 Docker/K8s 运行时管理。
- **容器编排**: Docker 动态容器；K8s agent 使用 STS（StatefulSet），停止计算资源时保留 PVC。

| 要处理的问题 | 优先查看 |
|---|---|
| HTTP 路由、应用装配、配置 | `crates/rcoder/src/`，尤其 `app_state.rs`、`config/`、`router_docs/api_doc.rs` |
| agent 执行、gRPC 服务端 | `crates/agent_runner/` |
| ACP 连接与协议适配 | `crates/agent_abstraction/` |
| UserApp 生命周期、存储接口 | `crates/app_manager/`；builder 接入在 `crates/rcoder/src/userapp_builder/` |
| Docker/K8s 运行时实现 | `crates/docker_manager/src/runtime/`；接口在 `crates/container-runtime-api/` |
| 持久化 | `crates/rcoder-storage/` |
| 跨 crate 共享契约 | `crates/shared_types/` |
| gRPC proto | `crates/shared_types_grpc/proto/agent.proto` |
| E2E 场景与启动器 | `tests-e2e/src/`、`tests-e2e/tools/run.py`、`make/test.mk` |
| 远端 K8s 工作流与部署配置 | `tools/remote_k8s/`、`docker/remote-k8s/`、`make/remote-k8s.mk` |

## 验证命令

```bash
# 格式化检查
cargo fmt --all -- --check

# Lint
cargo clippy --workspace --all-targets

# 全量测试
cargo test --workspace

# 通过 Makefile（推荐，封装了分级测试）
make test              # 全量测试
make test-unit         # 仅单元测试
make test-integration  # 仅集成测试

# 聚焦验证单个 crate
cargo test -p rcoder
cargo test -p docker_manager

# 构建
cargo build --release --workspace
```

需要格式化时用 `cargo fmt --all`。不要让多个 Cargo 任务共用同一 `CARGO_TARGET_DIR` 并发运行。`cargo test --workspace` 中的环境门控用例可能跳过，不能替代显式 E2E 验收。

## 本地开发的部署模式测试

- **Docker Compose 模式**：沿用 `make dev-build`、`make dev-up`，通过 `make test-e2e-compose` 验证；部署相关套件使用 `make test-e2e-compose-deploy`。原有 `make test-e2e` 等入口保持不变。
- **K8s 模式**：今后默认使用下述 `remote-k8s-*` 工作流。Mac 编辑源码，Mutagen 同步到个人 Linux 主机，远端构建 amd64 镜像并部署专属 namespace，再由 Mac 执行 E2E。涉及 K8s STS、动态 agent/UserApp、PVC、RBAC、Gateway 或多副本行为的修改，必须选择对应 K8s 套件验证；Compose 测试不能证明 K8s 行为正确。现有 DevSpace 和 `test-e2e-k8s` 入口仍保留。

| 验证目的 | 入口 |
|---|---|
| Compose 日常 Rust 更新 | `make dev-hot`；镜像或依赖变更时按需 `make dev-restart` |
| Compose 共享 chat/SSE 场景 | `make test-e2e-compose` |
| UserApp 基础与生命周期回归 | `make test-e2e` |
| Compose 制品部署链 | `make test-e2e-compose-deploy` |
| K8s 双副本与 API 基础健康 | `make remote-k8s-verify SUITE=smoke` |
| K8s UserApp workspace、构建与部署链 | `make remote-k8s-verify SUITE=userapp` |
| K8s 真实 AI、多入口会话与续传 | `make remote-k8s-verify SUITE=chat` |
| K8s Gateway 实际 HTTP 请求 | `make remote-k8s-verify SUITE=gateway` |
| 远端工作流支持的全部套件 | `make remote-k8s-verify SUITE=all` |

Compose 可用 `E2E_SUITE=<套件名>`、`E2E_FILTER=<场景名>` 聚焦；远端入口使用上表的 `SUITE`。两者参数含义不同。严格启动器将缺少前置、空筛选、skip、aborted 或缺报告判为失败，详见 [E2E 说明](tests-e2e/tools/README.md)。

### 远端 K8s 配置与命令

先阅读 [远端 K8s 使用说明](specs/remote-k8s-dev/README.md)。复用仓库已有的未提交 `.env.local`；缺少配置时参考 [配置示例](tools/remote_k8s/env.example) 补充，不覆盖其他本地配置。主机、远端目录、Kubernetes context、namespace、registry 和访问地址都从配置读取，禁止在 Makefile、源码或提交的文档中写入真实凭据。SSH 使用密钥或已有 SSH config。

Mutagen 固定 **0.18.1**；默认 namespace 为 `rcoder-e2e-soddy`，构建预算为 4 核、16 GiB、4 个编译任务。远端工具、仓库和 Ceph 存储前置由 doctor 检查。

```bash
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-sync-status

# 验证当前源码：串行同步校验、构建、推送、部署、测试
make remote-k8s-verify SUITE=smoke
# 按修改范围选择 smoke|userapp|chat|gateway|all
make remote-k8s-verify SUITE=userapp

# 仅对当前已部署版本追加测试，不会构建或部署本地修改
make remote-k8s-test SUITE=chat
make remote-k8s-test SUITE=gateway

# 诊断与停机
make remote-k8s-logs
make remote-k8s-down       # 停止本环境工作负载，保留 PVC
make remote-k8s-sync-stop  # 单独停止本项目同步会话
```

需要分步排查时可使用 `remote-k8s-build`、`remote-k8s-deploy`、`remote-k8s-test`。同步是 Mac 到 Linux 的 `one-way-replica`，远端接收目录不可作为编辑源；实时同步不会自动触发构建或部署。工作流从校验后的独立快照构建，按镜像 digest 部署。同一环境的构建、部署、测试必须串行；测试期间禁止替换被测部署，运行 `userapp/chat/all` 时保持本地源码稳定，避免严格漂移校验失败。

### 验收与隔离要求

- 优先通过 Make 入口运行，工具会向现有 E2E 启动器传入 `RCODER_URL`、`TEST_K8S_SSH`、`TEST_K8S_NS` 等运行上下文，不修改旧测试的默认值。AI 场景必须真实执行；缺少 LLM 配置应明确失败。
- 每轮以 `.remote-k8s/<环境ID>/` 下的构建、部署和测试记录关联源码摘要、镜像 digest、namespace 与测试结果；E2E 明细在 `tests-e2e/reports/`。失败时执行 `remote-k8s-logs`，检查 RCoder 文件日志、agent stdout、Pod 状态及事件。
- smoke 通过只证明基础部署可用；Gateway 必须验证实际请求路径，UserApp/Chat 必须检查整轮报告和退出码。历史未通过项见使用说明，不能将旧记录或部分场景通过当成本轮验收，也不能跳过失败断言。
- 只操作本环境拥有的资源、同步会话和镜像标签，不改动已有测试服务或共享 Gateway。`down` 保留 namespace 和数据卷；agent PVC 永不删除。CephFS 的 namespace 不构成共享存储安全边界，禁止删除共享 CephFS 根数据。

## 开发约束

1. **禁止 unsafe 代码** —— 项目要求内存安全。
2. **禁止模拟响应逻辑** —— 所有 AI 调用必须真实执行，不得返回 mock 数据。
3. **ACP schema 变更须谨慎** —— `shared_types` 直接嵌套 `schema::v1` 类型，升级 SDK 后务必全量编译 + 测试。
4. **agent 侧 PVC 永不删除** —— `destroy_workspace_pvc` 仅 UserApp 独立 REST 接口调用，agent 停止流程不碰 PVC。
5. **Always Response in 中文** —— 所有面向用户的响应必须使用中文。
6. **SOLID 与 Fail Fast** —— 保持职责边界；错误主动传播，必要时用 `context()` 补充场景。生产代码禁止 `unwrap()` / `expect()`，测试用例可用。
7. **DashMap 有锁** —— 并发操作优先使用 entry API，及时释放 guard，避免持锁跨 await 或嵌套访问导致死锁；单线程场景不需要 DashMap。
8. **HTTP 接口必须有 OpenAPI** —— 使用 utoipa 完整描述接口及请求/响应，并检查文档注册入口。
9. **跨 crate 业务契约统一放 shared_types** —— 如 ContainerLookup、ProjectStore、ActivityPersistence、PublishTaskPersistence、CleanupRequest；避免消费方和实现方各定义一套。既有运行时接口和 gRPC proto 按上面的模块导航维护。

Spec 开发产物分为三个层级，分别维护：`spec.md` 说明做什么与边界，`plan.md` 说明技术方案与模块设计，`tasks.md` 拆分执行步骤、完成标准和验收证据。

## AI 调试路由

- **日志框架**: `tracing`，文件日志 JSON 格式按天滚动（`logs/` 目录）。
- **K8s 下 rcoder 日志写文件不写 stdout** —— `kubectl logs` 几乎为空，须 `kubectl exec ... grep /app/logs/rcoder.$(date +%Y-%m-%d)` 查询。
- **agent_runner Pod 日志在 stdout** —— `kubectl logs` 可直接查看。
- **可观测性栈**: OTLP 分布式追踪 + Prometheus 指标（`/metrics`）+ Pyroscope 持续剖析。
- 远端 K8s 调试和验收记录见 [使用说明](specs/remote-k8s-dev/README.md)。
