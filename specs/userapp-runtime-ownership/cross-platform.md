# 跨平台实施补充：Linux / macOS / Windows

日期：2026-09-17。性质：已确认的设计要求与待开发任务，**不是实现或验收完成记录**。

本补充与 spec.md、plan.md、tasks.md 一起实施。跨平台核心能力属于本轮所有权改造范围，不能只保留 Windows 编译通过而将运行正确性延期。系统服务安装、零停机升级、增量部署不属于本补充范围。

2026-09-17 新增要求：**app-cli 和 file-server-proxy 均需三平台支持，并尽量自包含，不要求其他后台服务作为基础前置。** 无容器宿主机的需求、源码缺口、依赖分发与验收见 [native-desktop-runtime](../native-desktop-runtime/README.md)。Electron 仅为使用场景，本轮不开发客户端。本文平台锁/进程/恢复约束继续适用；只有平台适配 helper 或发布构建矩阵不能视为实现完成。

## 1. 支持范围与保证

| 平台 | 基础执行引擎 | 目标架构 |
|---|---|---|
| Linux | builtin；容器内保留 supervisord | x64、ARM64 |
| macOS | builtin | x64、ARM64 |
| Windows | builtin，不要求 supervisord | x64 |

目标架构对齐现有发布工作流；Windows ARM64 不在本轮承诺中。每个操作系统必须有原生生命周期测试，各发布架构分别记录构建及实际运行覆盖；交叉编译不能代替原生行为验证。

保证的是“同一应用运行环境最多一个有效 owner”，不是进程列表只能出现一个 app-cli。客户端、附着进程和内部服务载体可以并存，但不能各自编排业务或改写 active 目录。

同机不同项目允许各自运行一个 owner。同一项目改端口、相对路径、符号链接或 junction 不能绕过锁域。跨 Pod/主机共享目录仍遵守平台租约和物理身份保护，本地文件锁不替代分布式协调。

## 2. 统一内核与平台适配

保留 clap、Axum、reqwest、Tokio 及现有 runtime kernel。身份、受理、幂等、revision、待执行目标、停止屏障、事件和恢复规则三平台共用，不复制三套业务状态机。

平台差异收敛为小型内部模块，具体名称可按现有结构调整：

- `OwnerGuard`：本机跨进程排他锁，生命周期覆盖 owner。
- `ManagedProcessTree`：启动、优雅停止、强制停止、等待及清理确认。
- `StateStore`：复用现有存储接口，封装平台持久化与文件替换差异。
- 启动命令与路径解析适配：复用现有 win_cmd 等代码。

不引入大型 Actor 框架，不要求 PG/K8s 依赖，不为跨平台重写现有协议。内部适配可以使用条件编译；上层停止成功、结果未知等业务语义不得因平台静默降级。

## 3. 唯一 owner 与管理地址

### 文件锁

按项目 MSRV 核验 Rust 标准库文件锁可用性；不足时选维护可靠的跨平台安全封装。不能只用进程内 Mutex、PID 文件、端口空闲检查或锁文件是否存在作为排他依据。

锁根位于部署替换范围之外。先统一应用身份及 source/.run/别名映射，再定位唯一锁文件；不能将用户输入路径字符串直接散列成身份，也不能简单 lower-case 所有路径。

锁文件打开并加锁后保留句柄；不删除重建，不泄漏或继承给业务子进程。锁竞争与权限/文件系统故障分别处理。初期明确支持本地文件系统；网络盘/共享卷需专项验证或明确拒绝未经验证的 standalone 模式，不影响既有 K8s 平台保护。

### 启动顺序

读取身份 → 尝试排他 → 成功绑定并持有 listener → 检查恢复状态 → 开放修改受理 → 执行业务副作用。

竞争失败者有界等待 owner 初始化，然后核验身份并转交；连接失败不意味着可以抢锁、删锁或启动旧编排。拿到锁也不意味着遗留进程已经退出。Initializing 期间只开放诊断/健康信息，写受理明确拒绝。listener 从初始化到运行连续持有，不关闭后重新抢占。

### 端口与认证

- 容器 managed 模式保留平台约定管理地址和端口（当前为 3010），不自动换端口规避冲突。
- 桌面 standalone 默认绑定 loopback。不同项目允许配置不同端口；可实现显式自动分配模式：获得项目锁后 bind loopback:0，记录系统实际分配端口。
- endpoint 发现记录包含协议版本、应用/工作区身份、runtime instance 和实际地址，原子发布；它是发现线索，不是所有权证明。客户端连接后再次核验实例。
- 同一项目改端口仍竞争同一把锁；未知 listener 不抢杀。客户端连接本机 owner 时避免系统 HTTP proxy 干扰，不将认证请求重定向到未知地址。
- loopback 仍需认证；本地凭据文件采用 Unix 权限或 Windows ACL 等对应保护，不在命令行参数、日志或公开 identity 响应中泄露。复用现有认证协议，明确凭据生成/复用/轮换规则。

## 4. 进程树与停止语义

### Linux / macOS

复用 builtin 的独立进程组、组信号及退出确认。正常受管程序不能 daemonize 脱离管理；对需要脱离的程序必须显式适配或拒绝，不声称任意后代都必然受进程组约束。supervisord 模式维护 program 归属及停止意图，防止 autorestart 违背 Stopped。

### Windows

使用 Job Object 或具备等价保证的安全封装管理业务进程树。仅 `Child::start_kill()` 加直接子进程 wait 不足以证明 npm/node 等后代退出。

- 进程开始执行前完成 Job 归属，避免 spawn 后才加入产生逃逸窗口；优先使用支持这一保证的安全依赖。
- 不开放不必要的 breakaway；Job 句柄不继承给业务进程。
- 正确处理 CI/终端已有 Job、嵌套 Job 约束；归属失败必须 fail-fast，不能静默退化成只管父 PID。
- builtin 推荐明确启用 owner Job 句柄关闭时清理业务的策略；supervisord 外部托管语义单独保留。即使进程被清理，SQL/远端副作用未知仍保持恢复保护。
- 优雅停止优先使用业务支持的应用协议；控制台信号仅在满足适用条件时使用，不将其宣称为通用 SIGTERM。宽限期后终止受管 Job，并确认成员收束。

统一停止结果区分正常收尾、强制停止、清理未确认。取消通知或 future abort 不是完成证据；无法确认原执行停止时不得激活新版本。

遵守生产 Rust 不新增 unsafe 的工程约束；对候选依赖核验安全 API、创建时归属保证和句柄生命周期。若依赖不能满足，不以临时裸 Win32 FFI 或降级行为假装完成。

## 5. 文件、命令及恢复

- 使用 Path/PathBuf；覆盖空格、中文、盘符、符号链接/junction、平台文件系统大小写差异。
- 制品准备在独立 staging，跨卷 rename 不假设原子；在目标卷准备最终激活内容。
- Windows 文件占用可能阻止目录替换；失败保留阶段与证据，不能先删 active 再碰运气。旧进程退出、日志句柄释放等前置必须确认。
- 持久化验证同目录临时文件、同步、替换、读回以及重启恢复；Unix 目录 fsync 不能照搬成 Windows 已具有同等断电保证。明确保证边界，损坏或提交未知保持保护。
- 多个 JSON 文件各自原子替换不等于多记录事务；替代操作与新待执行目标必须有一致、可恢复的提交协议。
- 启动优先使用程序与参数数组；shell 命令明确指定可用 shell。Windows .cmd/PowerShell 不假设等价于 /bin/sh，不能未经授权改写用户命令语义。
- 首版三平台支持前台 owner + 多客户端。launchd、systemd、Windows Service 安装为后续可选项，不是核心功能前置。

## 6. 当前源码核验入口

以下为 2026-09-17 静态核验，开发前重读，不作为通过证据：

- `.github/workflows/release-app-cli.yml` 已有 Linux/macOS 双架构、Windows x64 构建配置及 Windows 配置配对检查；不能代替生命周期验收。
- `crates/app-cli/src/supervisor.rs` 的 Unix 分支已有 process_group；非 Unix 的 send_term 使用 start_kill，wait_for_quiescence 不检查进程组，需补进程树保证。
- `crates/app-cli/src/run_service.rs` 明确 Windows 不使用 supervisord 引擎。
- `crates/app-cli/src/server_journal.rs` 的目录同步包含 Unix 条件编译，Windows 持久化边界需核验。

## 7. 原生测试矩阵与交付门禁

下列测试使用真实子进程和受控 fixture，在 Linux、macOS、Windows 原生环境运行；它们不等同于真实 AI E2E。

| ID | 场景 | 必须断言 |
|---|---|---|
| XP01 | 两个 CLI 并发首次启动 | 一个 owner；另一请求正确转交；无第二业务树 |
| XP02 | 重复调用、响应丢失和同 ID 重试 | 操作幂等，事件和终态可查询，无重复迁移 |
| XP03 | 两个不同项目；同项目改端口/路径别名 | 不同项目可并存，同项目不能分裂锁域 |
| XP04 | 未知程序占用指定端口；初始化期竞争 | 无业务/目录副作用，不杀对方，不任意换端口 |
| XP05 | 父进程产生持续运行的子孙后停止 | 原受管进程树退出，端口释放，强杀与优雅结果可区分 |
| XP06 | owner 非正常退出、业务仍有子孙 | 锁不被后代保活；重启先恢复，不盲目重放或接管 |
| XP07 | A/B 构建乱序、Stop 交错、替代中崩溃 | 旧 revision 不覆盖新目标，停止屏障与待执行记录一致 |
| XP08 | 空格/中文路径、shell/.cmd、别名 | 命令参数正确，身份稳定；所需前置缺失不能算通过 |
| XP09 | 激活遇文件占用、跨卷、落盘失败 | 无假成功，原内容或可恢复记录保留，未知保持保护 |
| XP10 | 本机认证、旧发现记录、错误实例 | 错误目标拒绝，凭据不泄露，代理不劫持本地请求 |

XP05/XP06 必须覆盖 Windows Job 后代；Linux/macOS 测真实进程组，不只 mock helper。Windows 强制结束 owner 的测试要与 Unix kill 场景分别实现。

组件验证优先 nextest，app-cli 独立 fmt/clippy/test；默认及受影响 feature 分别覆盖。CI 新增三平台生命周期门禁，发布架构运行覆盖不足须列明。Linux Compose/remote-k8s 继续按主 Tasks 执行，不能替代 macOS/Windows 原生测试，反之亦然。

验收记录包括 OS、架构、Rust 版本、源码身份、执行引擎、实际命令/退出码与证据。缺少某平台 runner 时标为未验收，不能把构建成功写成三平台功能完成。

## 8. 技术参考

- [Rust File 文件锁](https://doc.rust-lang.org/std/fs/struct.File.html)：平台锁实现与版本要求。
- [Tokio process](https://docs.rs/tokio/latest/tokio/process/)：Child、退出与进程管理边界；开发时对照锁定版本。
- [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)：Job 归属、子进程及终止语义。

本补充未运行测试、未修改 Rust 代码、未发布任何平台包。实际完成情况追加到本轮 verification，不覆盖历史记录。
