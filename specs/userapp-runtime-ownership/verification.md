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
| XP01/XP09 | `2a660ed7` 双 CLI 真实进程竞争 + 激活失败无假成功 | 三平台原生 | 0 | macOS 199/199；Linux 189+10；Windows 全目标（含 XP01 Windows 双进程版） | 三台真机 | — | XP09 跨卷 EXDEV 需真实多卷环境 |
| P3-02 | `3caebc0b` owner 复用（平台 start 经运行 API 路由既有 serve owner）+ token 凭据契约 | cargo nextest -p file-server -p file-server-userapp + app-cli 三平台 | 0 | file-server 401/401（owner_probe_branches 四分支）；app-cli 200/200（macOS）/191+10（Linux）/全目标（Windows） | 本机 + 两台真机 | — | 复用路径 SSE 事件流（P3-03）；构建前 revision 捕获；Windows token ACL；P3-04~07（镜像/灰度/Compose/K8s） |
| P3-03 | `801b4ef5` SSE 事件兼容 + 构建前 owner 期望捕获 + `d3e99549` typed mock/Windows ACL | 同上 + mock 扩展 | 0 | owner_probe_branches 含 SSE 转发断言；Windows icacls ACL 实测（token 无继承条目） | 本机 + Windows 真机 | — | 服务级事件仍走 stdout（kernel 事件为 operation 级） |
| **P3-04/05** | build-agent-docker `a4a4522`：rcoder-app-runtime 固定程序 + supervisor conf（managed 灰度默认关） | 镜像配置审查（env 优先：APP_CLI_MANAGED/APP_CLI_RUNTIME_WORKSPACE/APP_CLI_LOG_DIR/APP_CLI_ADMIN_ADDR） | — | 灰度关闭=exit 0+autorestart=unexpected 零行为变化；启用=serve 常驻 owner | — | — | 镜像实机重建验证待用户执行 |
| **P3-06 Compose** | head `71911c77`（含 P3-02/03 全部改动） | make dev-hot && make test-e2e | 0/2 | **38 pass / 3 fail，3 个失败全为环境归因**：①sqlite_compose_recreation ②docker_runtime_crash_recovery（均缺 E2E_SQLITE_BINARY_SHA256 冻结二进制前置）③userapp_agent_dispatch_anthropic（ERR_MODEL_UNAVAILABLE 模型端点不可用）。**本轮改动相关全过**：userapp_dev 全套（server_lifecycle、ttyd WS、SSE cursor、owner lazy ensure）、hot_deployment builtin+supervisord 双引擎、deploy_full_chain、并发、native crash | 报告 `tests-e2e/reports/904c4e697be1431ba9b2d0cf57bd19e9`（首跑 e59dbe4c 有 source-drift——中途 clippy 提交所致，干净重跑已消除） | — | K8s remote-k8s verify 未运行（环境/时间预算；compose 已覆盖 file-server 改动回归） |
| 依赖升级 | `3af45f7d`+`7ebd0423` pingap 0.14.3 + pingora 0.9（rcoder）+ build-agent-docker `61416bf` | 三平台 + workspace | 0 | app-cli 200/200；workspace 2154/2155（1 预存 env 失败）；**ttyd WS 经 pingora 0.9 WebSocketOnly 策略实测通过**（compose e2e） | 三台真机 | — | 镜像内 pingap 二进制随下次重建生效 |
| clippy 清零 | `71911c77` | cargo clippy --all-targets --workspace --all-features | 0 | 零告警；678/678 相关 crate | — | — | — |

## 跨平台原生验证矩阵（XP-T 基础层，2026-09-17）

| 平台 | 机器 | Rust | 命令 | 结果 |
|---|---|---|---|---|
| macOS ARM64 | 本地 | 1.98.1 | cargo nextest run --all-features | 193/193 ✅ |
| Linux x86_64 (Ubuntu 26.04) | 192.168.32.131 | 1.98.1 | cargo test --all-features（lib + bin_startup + serve_restart） | 193/193 ✅（含 flock 跨进程互斥实测） |
| Windows x64 (MSVC) | 192.168.32.53 | 1.98.1（1.93→1.98 升级） | cargo test --lib ×3 连跑 | 172/172 ×3 ✅（含 Job Object spawn/terminate、std 锁同进程互斥；12 个 Unix 专属测试按设计门控：跨进程锁对端×2、supervisord 渲染、proxy 路径护栏×2 等） |

Windows 环境准备：choco 安装 cmake 4.4.3 + VS2022 BuildTools (VCTools)；rustup 升级 1.93→1.98（kdl/sysinfo 依赖要求 ≥1.95）。
发现的真机问题与修复：①APP_CLI_STATE_ROOT set_var 与并行测试竞争（os error 2/3/183 随机失败）→ resolve_root 参数化消除 env 变异；②Defender 独占 .tmp（os error 5）→ write_json 有界重试；③fs2 锁域与 Python lockf 在 Linux 不互斥 → 改 std::fs::File::try_lock（flock 语义官方背书）；④force_kill Windows 未杀进程树 → TerminateJobObject；⑤spawn→Assign 逃逸窗口 → CREATE_SUSPENDED + ResumeThread。
未验证：Windows 集成测试（bin_startup/serve_restart 为 #![cfg(unix)]）；Windows 跨进程锁已于 `2e1c2ba6` 以 PowerShell/.NET Lock 对端实测补齐。

## 2026-09-18 轮：development-review R01/R04–R10/B01–B04 修复

| 项 | 提交 | 验证 | 未运行项 |
|---|---|---|---|
| R01 | `2574f8c5` | app-cli 202→207（含 tree_lifecycle 真实编排链反例 ×2、supervisor 树测试 12 连跑稳定） | Windows/Linux 真机矩阵本轮复验；Compose/K8s |
| R04/R05/R06 | `7e74cd0b` | file-server+userapp 405→412（external_stop 4 反例 + owner_probe 新契约 + observation/pg wire）；app-cli kernel 桥 2 反例 | Compose/K8s 真实链路 |
| R07/R08 | `578ac9f6` | 664/664（三 crate）；supervisor env 注入/kernel 脱敏/wire 三层反例 | 改密后真实 PG 连通（容器态） |
| R09+N01 | `1fcc3860` | layout crate 5 反例 + fs token 契约；legacy 迁移 EINVAL 修复回归 | Windows junction |
| B01/B02 | 镜像仓 `59b7a0f` | supervisord 容器态双场景（EXITED 预期/RUNNING+health）；B01 本机二进制复现 | 新镜像实机构建 |
| B03/B04 | `936b8cf6`/`1fcc3860` | docker_manager 85+207（双 feature）；注入链单元反例 | managed E2E（镜像重建后 Compose） |
| R10 | `3b931eb8` | app_manager 194/195（1 既有环境项） | K8s 部署预算实测 |
| 全 workspace | `3b931eb8` | 见下方本轮汇总行 | — |

### 2026-09-18 三平台矩阵（本轮改动复验）

| 平台 | 机器 | 命令 | 结果 |
|---|---|---|---|
| macOS ARM64 | 本地 | cargo nextest --all-features | 207/207 ✅ |
| Linux x86_64 | 192.168.32.131 | cargo test --all-features（源码 rsync 同步；首跑为陈旧指纹误报，touch 重建后全绿） | 207/207 ✅ |
| Windows x64 MSVC | 192.168.32.53 | cargo test --all-features（tar-over-ssh 同步） | **179 pass / 4 fail——四项全部为既有 N02 平台缺口**（zip symlink 需 Unix 运行时、proxy 路径校验对 Windows 整体跳过、svc_spec `\` 分隔符拼接），本轮未触碰这三文件（git diff 归因为空），与本轮改动无关 |

本轮 R01 树测试/编排链/进程组收束/R09 布局契约在 Linux 全过；Windows 的进程树/生命周期测试（win 专属集合）全过。
