# AGENTS.md

本文件是 RCoder 的跨工具项目开发规范。`CLAUDE.md` 仅保留 `@AGENTS.md` 导入，不重复维护规则。所有面向用户的说明使用中文。

## 1. 开始工作与任务范围

1. 执行 `git status --short`，读取相关路径的 diff，区分本任务已有改动与无关改动。不覆盖无关内容；提交时只暂存本任务文件或改动块，不默认使用 `git add -A`。
2. 按模块导航阅读实际调用链及相关 Spec、Plan、Tasks。源码说明当前行为，Spec 与用户已确认的需求说明目标；发现不一致要明确记录，不能用当前实现反向缩减需求。
3. 对明确的任务持续完成实现和验证。常规实现选择自行处理；只有影响需求、兼容性或破坏性操作且现有要求无法确定时，才需要澄清。
4. 先做聚焦验证，再按影响范围扩大。文档修改核对内容、链接和命令即可，不要求运行无关构建或部署。

## 2. 项目与模块导航

RCoder 是基于 ACP 的 AI 开发平台，主体为 Rust 2024 Cargo workspace。请求主链为 HTTP/SSE → RCoder → Tonic gRPC → agent_runner → ACP agent。UserApp 由应用管理层调用 Docker/K8s 运行时管理。

| 问题 | 优先查看 |
|---|---|
| HTTP 路由、应用装配、配置 | `crates/rcoder/src/`，尤其 `app_state.rs`、`config/`、`router_docs/api_doc.rs` |
| agent 执行、gRPC 服务端 | `crates/agent_runner/` |
| ACP 连接与协议适配 | `crates/agent_abstraction/` |
| UserApp 生命周期、存储接口 | `crates/app_manager/`；builder 接入在 `crates/rcoder/src/userapp_builder/` |
| UserApp 容器内编排、启动、部署与控制协议 | `crates/app-cli/`（独立 Cargo 项目） |
| 文件、工作区、Git、技能与开发服务 | `crates/file-server/` |
| UserApp 文件服务接口、构建任务与启动事件 | `crates/file-server-userapp/` |
| 代理、预览和终端转发 | `crates/rcoder-proxy/` |
| Docker/K8s 运行时 | `crates/docker_manager/src/runtime/`；接口在 `crates/container-runtime-api/` |
| 持久化 | `crates/rcoder-storage/` |
| 跨 crate 业务契约 | `crates/shared_types/` |
| gRPC proto | `crates/shared_types_grpc/proto/agent.proto` |
| E2E 场景与启动器 | `tests-e2e/src/`、`tests-e2e/tools/run.py`、`make/test.mk` |
| 远端 K8s 工作流 | `tools/remote_k8s/`、`docker/remote-k8s/`、`make/remote-k8s.mk` |

## 3. 设计与编码约束

- **SOLID 与 Fail Fast**：职责清晰，尽早校验输入、配置和能力；错误主动传播，必要时用 `context()` 补充操作、对象和阶段。不得吞错后返回成功。
- **Rust 安全**：生产代码禁止新增 `unsafe`、`unwrap()`、`expect()`。测试可使用 `unwrap()`/`expect()`；已有仅限测试的 unsafe lint 豁免不得扩展到生产。
- **锁与异步**：DashMap 有锁，优先使用 entry API，但 entry/guard 同样需要及时释放；禁止持有 DashMap guard 跨 await 或嵌套访问导致死锁。维护一致锁序，避免在临界区执行耗时 I/O。单线程场景不需要 DashMap。
- **契约集中**：跨 crate 业务契约放 `shared_types`，避免消费方和实现方重复定义。已有运行时接口和 gRPC proto 保持其既有模块边界。
- **HTTP 文档**：使用 utoipa 描述请求、响应和错误，并核对文档注册入口。字段、默认值和兼容行为变化时同步 OpenAPI、示例及测试。
- **ACP 变更**：`shared_types` 嵌套 `schema::v1` 类型；升级 SDK/schema 后检查调用点，完成全量编译和相关测试。
- **兼容性必须有依据**：不得未经需求授权增加旧字段回退、忽略有效输入或静默切换执行路径。暂不支持的能力明确拒绝，不虚报支持、不返回假成功。
- **持久化升级**：变更字段和 schema 时检查实际数据库及持久 JSON。用旧版本数据验证升级，保留无关字段、生命周期和物理资源保护；不能用清库或全面放宽解析代替兼容处理。

### 生命周期、取消与恢复

- 操作身份从受理、排队、执行到终态完整传递；重试按同一操作身份处理，不能用最近一次请求替代正在执行的操作。
- 停止/取消受理与成功提交应有明确顺序和同步边界。取消请求已受理不等于清理已完成；进程、服务和目录切换的结果必须有证据。
- 结果未知、持久状态损坏或清理未确认时，保持对应运行态的写保护。新旧 API、自动恢复及后台任务遵守同一所有权和恢复约束，不得旁路执行。
- watch/cache、端口连通或同名资源只提供观察信息，不能替代生命周期、物理 UID、代次、租约等身份核验，也不能自动授权接管或删除。
- 总 deadline 应覆盖各阶段及重试/退避；不能通过放大超时掩盖错误传播问题，也不能任意缩短预算截断合法任务。

## 4. Spec 与完成标准

| 文档 | 职责 |
|---|---|
| `spec.md` | 做什么：需求、行为、范围及非目标 |
| `plan.md` | 如何实现：架构、模块、协议、迁移与技术方案 |
| `tasks.md` | 执行步骤、依赖、完成标准与验收证据 |
| `verification.md` | 每轮源码基线、实际命令、退出码、证据与未完成项 |

- 按原需求逐项推进，不能自行把必做项改为可选。分批交付时明确本批范围和剩余工作。
- 临时关闭能力、显式拒绝未支持请求属于止血，不等于该功能开发完成。
- Tasks 与实际进展同步；验证未通过的项目不得仅凭代码存在就勾为完成。历史报告保留其原基线，不改写成新一轮结果。
- 分开报告 **实现完成、测试通过、部署验收、发布完成**。接口存在、组件测试通过或 smoke 通过均不能替代完整功能验收。

## 5. 测试有效性与本地检查

### 测试要求

- 修复逻辑缺陷时，优先补能在修复前暴露错误的反例，并记录修复后结果。禁止空测试或无行为断言的“回归”。
- 状态机、并发、取消、恢复和迁移要覆盖真实调用链及关键时序；不能只测 helper、内存标记、HTTP 202 或最终通过数量。
- 协议和故障测试可使用 fixture、受控服务及故障注入。识别底层客户端重试、缓存等行为，避免它们掩盖被测层缺陷。
- 生产业务与真实 AI E2E 禁止伪造 AI 响应；单元/协议测试的替身结果不得宣称为真实端到端验证。真实 AI 测试缺少 LLM 配置应明确失败。
- 不删失败断言、不降低身份校验、不以 skip 或空筛选制造通过。声称基线失败时提供基线版本及相同条件下的复现证据；未复现则标为待归因。
- Rust 测试优先使用 `cargo nextest run`，以 `--no-fail-fast` 收集本轮全部失败。先聚焦，再用 `--all-features` 验证完整功能组合；全 feature 通过不能替代受影响的默认 feature 路径验证。
- 对失败用例逐项查看断言、日志和实际行为，判断是测试预期随已批准需求发生偏移、代码逻辑错误，还是环境/前置问题。测试偏移须依据需求更新用例并保留有效保护；逻辑错误修代码，不能迎合错误实现修改预期。修复后重跑失败用例及受影响回归，保留归因和结果。

### 根 workspace

```bash
# 聚焦测试（按修改范围选择 crate）
cargo nextest run -p rcoder --no-fail-fast --all-features
cargo nextest run -p docker_manager --no-fail-fast --all-features

# 根 workspace 检查
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
# 根 workspace 全 feature 测试，收集全部失败
cargo nextest run --workspace --no-fail-fast --all-features
```

日常可用 `cargo nextest run --no-fail-fast --all-features`，但本仓配置了 `default-members`；需要验证整个根 workspace 时必须显式加 `--workspace`。推荐 Make 入口：`make test`（workspace 全 features）、`make test-default`（默认 features）、`make test-unit`（库单元）、`make test-integration`（Rust 集成测试目标）、`make test-app-cli`（独立项目），均使用 nextest。用 `NEXTEST_ARGS='-p rcoder'` 聚焦单个目标，`TEST_FEATURES=` 选择默认 features。

`make test-all` 串行执行 workspace、app-cli 和文档测试，收集各部分失败，不包含部署 E2E；crate 专用筛选不要用于该跨项目入口。`make test-doc` 保留 `cargo test --doc`，不接收 nextest 筛选参数。严格 E2E 保留专用启动器，不机械替换其内部测试调用。

需要格式化时使用 `cargo fmt --all`。按受影响 feature 追加编译、Clippy 和测试；涉及 K8s 时必须验证 `kubernetes` feature，也检查默认 Docker 模式（例如 `cargo nextest run -p docker_manager --no-fail-fast`）。发布构建按需执行 `cargo build --release --workspace`。

### app-cli 独立检查

根 `Cargo.toml` **排除了 `crates/app-cli`**，它有独立 `Cargo.lock`。根 workspace 的 fmt/clippy/test/build 和上述 Make 测试不能代替 app-cli 检查。修改 app-cli 或影响它的共享依赖时，额外执行：

```bash
cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check
cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features
```

不要让多个 Cargo 任务共用同一 `CARGO_TARGET_DIR` 并发运行。先聚焦、后扩大，必要检查通过后不无理由重复全量测试。workspace 中环境门控用例可能跳过，不能代替显式部署 E2E。

## 6. Compose 与 K8s 部署验证

**本地 `test-e2e` 是防止核心业务逻辑偏移的集成回归门禁**：以本地 Docker Compose 环境验证 RCoder 的实际接口、状态转换、构建部署及持久化等核心业务链路，不只是检查容器是否启动或端口是否可达。修改核心业务逻辑时运行对应场景，交付前按影响范围运行完整套件；新增或调整已批准的业务行为时同步维护场景和必测断言。nextest 的组件测试不能替代该门禁。

按影响范围选套件。跨 Docker/K8s 的运行时改动分别验证两种模式；K8s STS、动态 agent/UserApp、PVC、RBAC、Gateway、多副本行为必须用对应 K8s 场景验证。

| 验证目的 | 入口 |
|---|---|
| Compose 构建与启动 | `make dev-build`、`make dev-up` |
| Compose 日常 Rust 更新 | `make dev-hot`；镜像/依赖变化按需 `make dev-restart` |
| Compose chat/SSE | `make test-e2e-compose` |
| 本地 Compose 核心业务回归（UserApp 基础、生命周期、构建部署等） | `make test-e2e` |
| Compose 制品部署 | `make test-e2e-compose-deploy` |
| K8s 基础部署健康 | `make remote-k8s-verify SUITE=smoke` |
| K8s UserApp workspace、构建与部署 | `make remote-k8s-verify SUITE=userapp` |
| K8s 真实 AI、会话与续传 | `make remote-k8s-verify SUITE=chat` |
| K8s Gateway 实际 HTTP 请求 | `make remote-k8s-verify SUITE=gateway` |
| 远端全部套件 | `make remote-k8s-verify SUITE=all` |

- K8s 默认使用远端工作流；原 DevSpace 与 `test-e2e-k8s` 入口保留。先读 [远端 K8s 使用说明](specs/remote-k8s-dev/README.md)，工具版本、资源预算、同步/构建/诊断步骤以该文档及配置为准。
- `specs/` 是仅本地保留的内部工作目录（spec/plan/tasks/verification、问题记录等，已 gitignore，不随开源仓库发布）：本地开发照常读写；对公众可见的说明一律放 `docs/`。
- 复用未提交的 `.env.local`，缺项参考 [配置示例](tools/remote_k8s/env.example)，不覆盖已有无关配置。主机、目录、context、namespace、registry、访问地址从配置读取；不把真实凭据写入源码、文档或日志。
- `remote-k8s-verify` 校验并构建当前源码、部署后测试；`remote-k8s-test` 仅测试已部署版本，不能证明本地新修改有效。同步不会自动构建或部署；远端接收目录不是编辑源。
- 同一环境构建、部署、测试串行；测试时不替换被测部署。`userapp/chat/all` 期间保持本地源码稳定，遵守启动器的源码漂移检查。
- Compose 筛选使用 `E2E_SUITE`/`E2E_FILTER`，远端套件使用 `SUITE`，不可混用。严格验收规则见 [E2E 说明](tests-e2e/tools/README.md)；缺前置、skip、aborted、空筛选或缺报告不能算通过。
- smoke 只证明基础健康。Gateway 检查实际 HTTP 路径；UserApp/Chat 检查整轮报告，不能用部分场景通过替代整轮成功。
- 证据关联源码摘要、镜像 digest、namespace 和报告；远端记录在 `.remote-k8s/<环境ID>/`，E2E 明细在 `tests-e2e/reports/`。环境阻塞记录具体错误及未覆盖项。

### 远端 K8s 实际操作顺序

Mac 编辑源码，经 Mutagen 同步并生成独立快照，在 Linux 原生构建 amd64 镜像、推送仓库，再按 digest 部署到个人测试 namespace；测试由 Mac 发起、作用于真实 K8s。远端镜像构建不等于执行了 nextest，组件测试仍按第 5 节完成。

```bash
# 首次使用或环境变化时检查前置和同步
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-sync-status

# 部署当前源码快照，先确认基础环境可用
make remote-k8s-verify SUITE=smoke

# 对同一已部署快照追加受影响套件，按上表选择，不必重复构建
make remote-k8s-test SUITE=userapp
make remote-k8s-test SUITE=chat
make remote-k8s-test SUITE=gateway

# 失败时收集诊断
make remote-k8s-logs
```

上面的追加套件按修改范围执行；共享运行时或跨链路变更需要组合验证，必要时用 `SUITE=all`。源码再次修改后，重新运行 `verify`，不能只运行 `test`。已有套件未覆盖的新行为须补场景和断言；套件名称不能替代覆盖范围核对。

### IP + NodePort 与验收边界

- 个人集群支持 IP + NodePort 测试。直连入口从 `.env.local` 的 `REMOTE_K8S_URL` 读取；实际部署的直连与 Gateway 地址查看 `.remote-k8s/<环境ID>/deployment.json` 的 `url`、`gateway_url`，不要写死地址或自动分配的端口。
- `smoke` 核查部署身份、双副本就绪、PVC Bound 和直连健康接口，不能证明应用生命周期、数据保留或跨副本业务正确。
- `gateway` 套件检查 Gateway/HTTPRoute 条件并请求 Gateway 健康接口，仍不能替代动态 UserApp 路由、预览、WebSocket/SSE 等业务场景。直连 RCoder 与经过 Gateway 是两条不同路径，须分别记录结果。
- Gateway 不可用时，可以继续不依赖它的直连测试；依赖 Gateway 的场景必须记为失败或受阻，不能改成直连、跳过断言后宣称通过。控制面超时、存储异常等环境故障应与业务逻辑失败分开归因。
- `doctor` 只检查前置，不证明网络与存储全链路可用；历史部署记录也不证明本轮通过。当前故障与修复进展放在 `specs/remote-k8s-dev/` 的日期化验证记录中，不将临时故障固化为测试豁免。

### 资源保护

- 只操作本环境拥有的资源、同步会话和镜像标签，不修改已有测试服务或共享 Gateway。
- **agent PVC 永不删除**；停止 agent 计算资源保留 PVC。`destroy_workspace_pvc` 仅用于 UserApp 独立 REST 接口的既有流程，不接入 agent 停止链。
- `remote-k8s-down` 保留 namespace 和数据卷。CephFS 的 namespace 不是共享存储隔离边界，禁止删除共享 CephFS 根数据。

## 7. 诊断与发布

- 使用 `tracing`，RCoder 文件日志为 `logs/` 下按天滚动的 JSON。K8s 排查须检查容器内 `/app/logs/`，不能因 `kubectl logs` 为空就认定无错误；agent_runner 日志在 stdout。
- 远端故障优先 `make remote-k8s-logs`，结合 RCoder 文件日志、agent stdout、Pod 状态与事件。操作步骤见远端使用说明。
- 本地性能诊断可用 `hotpath` feature，按需叠加 `hotpath-mcp`。不启用 `hotpath-alloc`，避免与 Turso 的全局分配器冲突。Docker dev 的启用方式检查当前 Makefile/Dockerfile；容器内仅绑定 loopback 的诊断端点需从容器内访问，不假定端口映射有效。
- app-cli 版本源为 `crates/app-cli/Cargo.toml`；发布规则见 [release-app-cli.yml](.github/workflows/release-app-cli.yml)。发布时版本、提交及 `app-cli-v<VERSION>` tag 一致，核验主包及平台子包实际发布结果。npm 发布和容器镜像更新分别验证，不把源码修改称为已发布。

## 8. 交付说明

按任务复杂度说明：修改了什么及原因、需求完成范围、实际验证命令与退出码、证据路径、失败归因、未运行项和剩余问题。部署/发布任务补充提交与镜像/包版本身份。

先说明影响用户的行为和结论；源码分析、组件测试、真实部署及发布证据分开列明。不要把历史报告、部分实现或局部测试通过表述为整个任务完成。
