# Claude Code 开发任务：审查修复与三平台原生运行

在 `/Users/soddy/Documents/git-workspace/rcoder` 完成审查修复及原生运行需求；镜像契约变化同步到 `/Users/soddy/Documents/git-workspace/build-agent-docker`。

目标：app-cli 与 file-server-proxy 在 Windows、macOS、Linux 宿主机独立运行。基础功能无需预装或启动 Docker、K8s、supervisord、RCoder 服务端、PostgreSQL、Redis 等后台服务；必要产品组件随包携带并自行管理。项目自己的工具链/数据库按真实需求配置，不以默认等待 PG 阻断纯前端或文件服务。

**Electron 仅是未来使用场景。本任务不开发 Electron 客户端、IPC SDK、示例工程、安装器、签名或自动更新。** 完成两个组件自身及通用调用契约，不把客户端集成当作前置。

## 一、必读与基线

1. 两仓适用的 AGENTS.md；分别检查 status/diff/log，保留无关改动，不默认 git add -A。
2. `specs/development-review-2026-09-17/review.md`：R01–R11。
3. `specs/development-review-2026-09-17/build-agent-docker-review.md`：B01–B05。
4. `specs/native-desktop-runtime/` 的 README、spec、plan、tasks、verification：原生问题 N01–N10、任务 ND01–ND12、测试 NT01–NT16。
5. `specs/userapp-runtime-ownership/` 的 spec、plan、tasks、cross-platform、verification。
6. `specs/userapp-deploy-budget-and-recovery/plan.md`。
7. `specs/error-string-matching-elimination/plan.md`；先修正 review.md R11 点名的计划缺陷再实施。

审查基线：RCoder `feature-userapp` / `19dfd381`，配套仓库 `a4a4522`。这是定位依据，不是要求 reset 的版本。当前可能已有新改动和审查文档，重新核实；已修复项提供提交及真实反例证据，不按旧行号机械改代码。

## 二、必须处理的行为

### 原有审查问题

- R01–R09：真实业务进程接入跨平台进程树；CLI/平台激活服从唯一 owner；Cancelled 不当成功；Stop 未确认前保留身份和恢复信息；操作/SSE 终态正确、有序、可重放；构建前身份/revision 绑定具体任务；合法 PG 凭据更新不丢弃；项目状态根、凭据及发现契约一致。
- R10：同一持久 deadline 贯穿首次执行、恢复、热/冷部署、状态读取及退避，不局部重置或使用固定 1800 秒掩盖预算缺口；未知写继续受保护。
- R11：错误类型化和经核实的 lockfile 边界需求真正落地；修订不符合实际的完成勾选，保留历史 verification。
- B01–B05：修复 managed CLI 参数顺序、supervisor 关闭模式、动态 builder 的真实 workspace/模式/token/state root 注入、凭据路径及 Secret 轮换 rollout。两仓分别交付并验证新镜像。保留环境 values 烘焙进 Chart 后 `--reset-values` 的既有部署方式，不擅自要求额外个人 values。

### 三平台原生能力

1. app-cli 不依赖 file-server-proxy/Electron 常驻；proxy 基础文件功能不依赖 app-cli 或其他后台服务。UserApp 运行通过 app-cli 唯一 owner 控制。
2. 原生显式 builtin，共享 RuntimeLayout 解析资源、状态、日志、工作区、工具路径和项目身份。修正 `/app`、`/run`、Unix 等于容器、Windows 跳过路径保护的假设；不同项目并存，同项目 source/.run/别名共锁。
3. 无数据库需求不等 PG；有需求按真实配置和预算检查。先做可完成的前置校验再改变旧运行态。基础原生包不依赖全局 Node；具体 JS 项目使用配套或明确配置的 Node/pnpm。
4. 各平台随包携带版本锁定的 Pingap，由 app-cli 私有托管，保留已有代理配置语义；不要求用户另装 Pingap 服务，不为单 exe 目标重写完整代理。
5. proxy 原生标准模式明确 `embed + all_rust`，不自动拉 TS。先列必需 API 覆盖矩阵并补齐缺口；TS 为显式兼容模式，不静默回退或删功能。产物没有 embed 能力时不能 warn 后假装成功。
6. 原生 loopback、认证、真实动态端口和身份握手；Pingap/admin/项目服务端口统一规划。`/health` 200、PID 存活、端口可连不等于实例归属；保留 managed 固定端口契约。
7. proxy 补跨进程锁、受保护状态和真正关停，npm 不再以全局 tmp PID 文件抢占/清理。Windows taskkill 失败不得假成功，内嵌文件服务创建的子进程也要处理，不误停 app-cli 业务。
8. 原生分发清单覆盖组件/Pingap 版本、协议、OS/arch/ABI、哈希和源码身份。完整包离线、只读资源目录可启动；修复下载器安全解压和错误目标映射。组件版本切换不覆盖 Windows 活动 exe、不清空 journal。

外部程序调用仅要求明确的二进制/资源路径、cwd/env、结构化身份/就绪/能力、HTTP/SSE 操作与事件、Stop/组件退出语义。客户端断连不等于 Stop；不引入新总控服务，不重新引入业务 user_id/x-user-id。

## 三、实施与验证

按原生 tasks.md 分批推进，合并 R/B/N 的相同根因，复用现有 runtime kernel、协议及小型 OS 适配。先补真实调用链反例，修复前失败、修复后通过；helper、HTTP 202、mock 手写成功事件不能代替业务验收。

覆盖 NT01–NT16 及原审查反例，重点包括：无 PG/TS/全局 Node 的基础功能、CLI 重复启动、同机双项目、伪造健康端口、PID 复用、孙进程停止、proxy 重启后任务恢复、Windows 路径/.cmd、SSE 续传、只读安装目录及离线包。必需功能缺失不能用 unsupported 或切回 TS 标记完成。

优先 nextest，先聚焦再检查受影响默认/all-features；app-cli 独立 Cargo 项目单独 fmt/clippy/nextest；npm 包运行自身测试。同一 target 的 Cargo 任务串行。失败逐项区分代码、已批准需求造成的测试偏移、环境前置；声称基线失败必须相同条件复现。

用本机 macOS 和用户提供的 Linux/Windows 原生机器测试真实二进制及配套文件。连接参数从未提交配置或 SSH 别名读取，不写账号、密码或 token 到文档/日志。只用专用目录、独立端口及归属可证明的进程，不改系统服务，不为无依赖测试停止已有 K8s/数据库。缺少连接配置先完成其他可做工作，具体记录未验证项，不能冒称三平台通过。

共享/核心业务继续运行本地 Compose test-e2e、新 managed 镜像及 `.env.local` + `make remote-k8s-*` 的 K8s UserApp，按影响补 Gateway；环境串行。原生、Compose、K8s 报告互不替代。

## 四、交付

追加本轮 verification，逐 R01–R11、B01–B05、N01–N10/ND 任务记录修复、反例、源码/包/镜像身份、命令与退出码、平台、失败归因和剩余项。同步 Spec/Plan/Tasks，保留历史记录；分开报告实现、测试、部署验收和发布。

只暂存本次文件/改动块，不自动 push/tag/npm 发布/生产部署，不覆盖无关改动。不因测试困难删断言、放松身份验证、清空状态或自动 legacy 回退。最终中文报告，不用总测试数代替需求完成证据。
