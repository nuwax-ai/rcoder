# Verification：UserApp 运行态单一所有者

本文件记录每轮实际执行的命令、退出码、证据与未运行项。勾选 tasks.md 前必须有本轮证据；历史报告不作为本轮结果。

## 实际基线（T00，2026-09-15）

- 开发起点 HEAD：`71a1ab6e96e814540a40db97d7189e4a4c30fbaee`（feature-userapp）。
  - 阅读基线 `a4380b37` 之后新增两个并行提交：`7576cfd0`（dev builder 复合键定位）、`71a1ab6e`（preview E2E 批次 4）。两者均未触及 `crates/app-cli`、`crates/file-server/src/service/dev_server`、`crates/file-server-userapp`，与本任务无文件交集。
- app-cli 版本：`0.3.5`（阶段一完成后按版本纪律评估 bump；默认不发布）。
- 分支策略：feature-userapp 直接 pathspec 提交（用户拍板）；验证深度=组件级 + Compose + remote-k8s userapp（用户拍板）。
- 工作树并行状态：`tests-e2e/tools/*.json` 三文件为并行 e2e 会话占用（不动）；未跟踪 `specs/kube-runtime-adoption/`（他人，不动）。
- Cargo 纪律：与并行 e2e 会话串行使用 target；聚焦单 crate 优先。

## 启动阶段预算清单（P1-06 前置，先记录现状）

| 阶段 | 现状预算 | 源码依据 |
|---|---|---|
| PG 等待（pg_isready） | 30 次 × 2s = 60s | app-cli `supervisor.rs` wait_for_pg（30×2s 轮询） |
| 逐服务 migrate / 准备命令 | 受现有 dev command 超时控制（`dev_command_timeout_secs`） | file-server config / build_manager |
| 服务 readiness 探测 | 各服务 `[health].startup_timeout_seconds`，并行取 max（java 模板 60s 量级；显式慢启动合法） | manifest [health] 段 |
| pingap 编译 + 配置校验 + admin 确认 | 确认预算 25s（`CONFIRM_BUDGET`）+ 编译/校验时间 | app-cli `proxy/admin_probe.rs:31` |
| 启动排空 / 收尾余量 | 本任务新增（有界排空窗 2s + 调度余量 30s） | P1-04 设计 |

计算式（P1-06 实现）：`launch_budget_secs = min(1200, dev_command_timeout_secs + 150)`，
单调共享 deadline 自 spawn 前起算；`START_DONE_WAIT_MAX_SECS` 从 3600s 降至 1200s
仅作兜底默认。显式配置小于必要阶段预算时仍可能被截断——本轮先落地上限与配置感知，
未增加独立 `dev_launch_budget_max_secs` 配置键。

## 逐任务记录

| 阶段/任务 | HEAD/源码差异 | 命令或场景 | 退出码 | 通过范围 | 日志/报告 | 镜像/实例/操作 | 未运行项 |
|---|---|---|---|---|---|---|---|
| T00 基线核对 | 71a1ab6e vs a4380b37 = 2 并行提交无交集 | git log/status/show --stat | 0 | 基线确认 | 本文件 | — | — |
| P1-01/02 | `d2eb64a0` app-cli bind/Done/退出码 | cargo test -p app-cli（146 通过）；bin_startup 4/4 | 0 | API 预绑定 fail-fast + 失败 Done + 非零退出码 | app-cli targeted tests | — | — |
| P1-03/04 | `9a895bda` SupervisedChild/DevEventHooks/start_events | cargo test -p file-server -p file-server-userapp（294+76 通过） | 0 | manifest spawn 可监督；ProducerExited/StreamEnded 受理 | targeted tests | — | — |
| P1-06 | `38d8407b` 启动等待窗与预算 | cargo test -p file-server -p file-server-userapp（296+76 通过） | 0 | 等待窗 3600s→1200s；预算 min(1200, dev_command_timeout_secs+150) | targeted tests | — | — |
| P1-05 | cleanup-state + stop 确认 + start 拒绝未清理残留 | cargo test -p file-server（stop tests 2/2）；cargo test -p file-server -p file-server-userapp（296+76 通过） | 0 | stop 设置 Cleaning/Cleaned；has_uncleaned_cleanup 守卫；start_dev_manifest 拒绝残留 | targeted tests | — | Compose / remote-k8s / 全量 workspace 门禁 |
| P1-01 强化 | legacy main startup order fix: bind before deploy_stage + remove LivenessHold | cargo nextest run --manifest-path crates/app-cli/Cargo.toml --all-features（188 通过，含新测试 legacy_deploy_url_with_port_conflict_has_no_deploy_side_effects / serve_attach_*） | 0 | API bind 在 deploy_stage 前 fail-fast；APP_DEPLOY_URL + 端口冲突无部署副作用；LivenessHold 退役；--attach 模式（身份核验+等待+re-exec）；clippy 零 warning | targeted tests | — | Compose / remote-k8s / 全量 workspace 门禁 |
| XP 三平台验证 | `429144f6` std 文件锁 + env 污染消除 + AV 重试 + Job Object 修正 | 三平台原生 cargo test（见下矩阵） | 0 | macOS 193/193；Linux 193/193；Windows 172/172×3（12 个 Unix 专属测试 cfg 门控） | 三台真机 SSH 执行 | — | Compose/K8s 回归 |
| XP 质量修复 | `2e1c2ba6` force_kill 树杀修正 + CREATE_SUSPENDED 归属 + Windows 锁对端 | 同矩阵复验 | 0 | macOS 193/193；Linux 193/193；Windows **173/173×3**（+PowerShell/.NET Lock 对端，跨进程互斥实测通过） | 三台真机 | — | Compose/K8s 回归；pingap 0.14.1→0.14.3 协同升级（11 处版本同步点，另行任务） |
| XP-T02/06/07 | `a217a88a` endpoint 发现 + process-wrap + XP03/06/08/10 测试 + 三平台 CI | 三平台原生 cargo test --all-features | 0 | macOS 197/197；Linux 197/197；Windows 全测试目标 ok×3（含 XP06 owner-kill/XP08 中文路径/win_lifecycle 生命周期） | 三台真机 + 新增 .github/workflows/app-cli-cross-platform.yml | — | CI 首跑待 push 后验证；P3 平台迁移（file-server 入口/灰度/Compose/K8s）未开始 |

## 跨平台原生验证矩阵（XP-T 基础层，2026-09-17）

| 平台 | 机器 | Rust | 命令 | 结果 |
|---|---|---|---|---|
| macOS ARM64 | 本地 | 1.98.1 | cargo nextest run --all-features | 193/193 ✅ |
| Linux x86_64 (Ubuntu 26.04) | 192.168.32.131 | 1.98.1 | cargo test --all-features（lib + bin_startup + serve_restart） | 193/193 ✅（含 flock 跨进程互斥实测） |
| Windows x64 (MSVC) | 192.168.32.53 | 1.98.1（1.93→1.98 升级） | cargo test --lib ×3 连跑 | 172/172 ×3 ✅（含 Job Object spawn/terminate、std 锁同进程互斥；12 个 Unix 专属测试按设计门控：跨进程锁对端×2、supervisord 渲染、proxy 路径护栏×2 等） |

Windows 环境准备：choco 安装 cmake 4.4.3 + VS2022 BuildTools (VCTools)；rustup 升级 1.93→1.98（kdl/sysinfo 依赖要求 ≥1.95）。
发现的真机问题与修复：①APP_CLI_STATE_ROOT set_var 与并行测试竞争（os error 2/3/183 随机失败）→ resolve_root 参数化消除 env 变异；②Defender 独占 .tmp（os error 5）→ write_json 有界重试；③fs2 锁域与 Python lockf 在 Linux 不互斥 → 改 std::fs::File::try_lock（flock 语义官方背书）；④force_kill Windows 未杀进程树 → TerminateJobObject；⑤spawn→Assign 逃逸窗口 → CREATE_SUSPENDED + ResumeThread。
未验证：Windows 集成测试（bin_startup/serve_restart 为 #![cfg(unix)]）；Windows 跨进程锁已于 `2e1c2ba6` 以 PowerShell/.NET Lock 对端实测补齐。
