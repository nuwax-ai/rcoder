# Tasks：原生运行时实施与验收

日期：2026-09-17。以下全部为待办，不继承历史“跨平台完成”勾选。实施前核对实际代码，已有修复用提交和真实反例结果证明。

## 1. 执行顺序

### A. 基线、依赖与真实控制链

- [ ] ND01：读取 AGENTS、两仓 status/diff、本目录及 R01–R11/B01–B05；建立 N01–N10 的“反例→修复→验证”对应表，合并重复根因。
- [ ] ND02：完成 runtime mode/RuntimeLayout、项目身份与 proxy 配置空间契约，CLI/npm/平台使用同源解析。补两兄弟项目和路径别名反例，禁止依赖默认 unknown-app 或 parent 猜根。（N02，R02/R09，B03/B04）
- [ ] ND03：接通三平台真实 ManagedProcessTree；覆盖 app-cli 业务/migrate/Pingap 及 file-server 自有构建/dev 子进程，保持 UserApp 唯一 owner。停止失败不宣告成功。（N08/N10，R01/R04/R05）
- [ ] ND04：数据库前置按运行计划需求决定；纯静态/无数据库项目不探测 PG，必需数据库缺配置或不可达有界失败。新配置预检不先破坏旧运行版本。（N01，R08）

### B. 原生标准模式和安全复用

- [ ] ND05：原生管理口和 proxy 入口 loopback + 动态端口 + 真实地址发布；Pingap/admin/业务端口同一计划。固定端口冲突不杀他人、不静默改约定。（N03）
- [ ] ND06：proxy 的跨进程锁、身份/凭据、状态持久化和关停由 Rust 实例负责；npm 不再持有独立的全局 PID 权威。损坏、响应丢失、PID 复用、并发启动按实例协议处理。（N06/N07/N10）
- [ ] ND07：原生 file-server-proxy 标准模式内嵌 Rust file-server + userapp，明确启用 all_rust；列出必需 API 覆盖，补齐缺口。TS 是显式兼容模式，不强制安装或启动。请求 embed 而产物不具备能力要 fail-fast。（N05）
- [ ] ND08：UserApp API、构建、staging→owner、task-operation、SSE 和取消沿真实链修复；proxy 重启不丢 owner 关联，不以 EOF/Cancelled 当成功。（R02–R08，与原生模式一起验收）

### C. 分发和兼容

- [ ] ND09：各支持平台提供 Pingap 配套产物及版本校验；修复 native 路径白名单，资源只读、状态独立可写；不存在系统安装时仍可运行。（N02/N04）
- [ ] ND10：统一支持的 OS/arch/ABI 清单、版本清单、哈希校验和安全解压；原生完整包离线可启动；修复 Windows ARM64 错误映射及跨架构缓存混用。npm 独立入口与直接 exe 入口分别测试。（N09）
- [ ] ND11：实现组件自身的正常关停/版本接管边界及通用调用说明；Windows 活动 exe 不被覆盖、协议不兼容不抢占。无需开发 Electron 项目或其安装/更新系统。
- [ ] ND12：三平台原生验收及两容器模式回归；R10/R11、B01–B05 按各自审查文档完成。追加本轮 verification、更新实际任务状态；不将本地源码或 npm 构建写成已发布。

## 2. 原生反例矩阵

优先使用跨平台 Rust fixture 可执行文件及实际 app-cli/proxy，避免测试依赖本来要移除的 sh、系统 PG、Docker 或全局 Node。涉及 Node 项目时另测受控工具链。

| ID | 场景 | 必须断言 |
|---|---|---|
| NT01 | 普通用户、私有可写目录、无容器/PG/Redis/supervisord | app-cli 实际启动无数据库夹具并可访问业务；没有 pg_isready 轮询；无需 APP_CLI_SKIP_PG_WAIT 逃生开关 |
| NT02 | 无 app-cli/TS/全局 Node 服务时启动原生 proxy | 文件树、读取/写入、搜索、上传/下载、本地 Git 必需接口实际通过；没有连 8086/60001/5432；启停不额外拉 TS |
| NT03 | CLI 和模拟外部调用进程并发启动同项目 | 一个 owner，重复请求转交或幂等；运行目录与业务树不重复；无需 Electron 夹具 |
| NT04 | 两个不同项目；同项目 source/.run/符号链接/junction | 不同项目并存，同项目共锁；所有监听及配置归属一致；冲突的硬编码业务端口明确报错 |
| NT05 | 固定端口被未知服务占用，含伪造 /health 200 | 不将对方认作自己，不 kill、不修改 active；自动端口返回真实地址，不能返回 :0 |
| NT06 | proxy/owner 错误 token、实例 ID、项目/配置空间，路径越界 | 文件/构建/控制请求拒绝；凭据不进日志/URL；不同组件 token 不混用；对 symlink/junction 有断言 |
| NT07 | proxy 状态损坏、旧 PID 指向无关存活 fixture、两个 start 同时执行 | 不因坏 JSON 当无实例后覆盖，不杀无关 fixture；跨进程单胜者，失败者不清理胜者的记录/服务 |
| NT08 | 真实业务/构建产生持端口的孙进程 | stop、restart、准备阶段超时、proxy 正常退出后，各自归属的树收束；Windows 杀进程失败不返回成功；proxy 不误停 app-cli 业务 |
| NT09 | 客户端断连、后台 owner 崩溃、proxy 重启 | 断连不取消已受理操作；原 operation 可恢复查询；Stopped 不复活；未知结果继续受保护 |
| NT10 | 空格/中文/引号/特殊字符路径，Windows .cmd，精简 PATH | argv/cwd 和数据不损坏；工具缺失说明具体依赖；不是通过预装 Git Bash 或借用开发机 PATH 才通过 |
| NT11 | 必需数据库不可达/凭据变化；纯前端无数据库 | 两类行为区分；必需前置失败有预算和原因；合法新凭据实际生效、日志脱敏；失败预检不先停止旧实例 |
| NT12 | 成功/失败终态、超过 10 秒的 SSE、断线重放、取消竞争 | owner→文件服务任务→调用方结果一致，服务事件不丢，Cancelled 不冒充成功 |
| NT13 | 完整原生包从含空格目录解包，离线且资源目录只读 | 两组件及 Pingap可运行；状态/日志在可写目录；不启动时联网下载或写入包内；缺少组件/哈希错误提前拒绝 |
| NT14 | 不支持架构、不同版本/ABI 缓存、Linux 制品交给 Windows | 结构化拒绝错误目标或不兼容制品，不下载不存在的 ARM64 包，不复用错误缓存 |
| NT15 | 新旧组件协议不兼容、升级时活动 exe/文件被占用 | 不清空状态、不覆盖活动文件、不按旧 PID 抢杀；旧实例仍可诊断，版本切换结果可核验 |
| NT16 | 显式 TS 兼容模式及未编译 embed 的产物 | TS 仅在显式选择且工具齐全时启动；作用域/身份与清理不波及其他实例；缺少声明能力不能降级后假成功 |

NT01/NT02 的“无服务前置”可使用隔离目录、受控 env 和访问记录证明。测试机器本身即使装有 K8s 或数据库，也不得为了造环境卸载或停止这些服务；不得借用它们让反例假通过。

Windows、macOS、Linux 原生行为分别记录。用户已有三类机器可用于组件测试；SSH 连接信息保存在已忽略的本地配置或 SSH 别名，不写入本目录、命令样例或共享日志。远端原生测试用专用普通用户目录、独立端口和有归属的进程；不改系统服务、不与远端 K8s 测试混用部署。

## 3. 检查命令和验证层级

先聚焦相关反例，再扩展默认/all-features；同一 target 的 Cargo 任务串行。

```bash
# app-cli 是独立项目
cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check
cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features

# 文件服务链的默认形态、全 features 和纯转发形态按影响分别验证
cargo nextest run -p file-server-proxy -p file-server -p file-server-userapp --no-fail-fast
cargo nextest run -p file-server-proxy -p file-server -p file-server-userapp --no-fail-fast --all-features
cargo nextest run -p file-server-proxy --no-default-features --no-fail-fast
cargo fmt --all -- --check
cargo clippy -p file-server-proxy -p file-server -p file-server-userapp --all-targets

# npm 包现有测试入口；app-cli wrapper 的新增测试补进可重复入口
npm --prefix crates/file-server-proxy/npm test
```

新增原生进程测试提供可重复、三平台兼容的入口，不强制 Windows 安装 Make/bash。测试要走发布形态的真实二进制和配套文件；交叉编译与测试 helper 都不能代替它。

共享业务逻辑变化继续按根 AGENTS 执行 Compose `test-e2e` 和 `make remote-k8s-*` 对应套件，使用本轮新镜像；纯转发容器形态与原生 embed 形态分开记录。原生端口动态化不得破坏 managed 固定端口契约。远端 K8s 用既有配置，Helm 保留环境配置烘焙进 Chart 后 `--reset-values` 的部署方式。

## 4. 完成门禁

- 每项说明实际交付：实现、组件测试、原生运行、容器回归、发布；不能合并写“全绿”。
- 新功能未实现、平台缺少 runner、环境受阻、断言失败分别列出；尚未验证的额外架构不宣称完成。
- 必需 Rust API 缺口不能以“可切回 TS”关闭；TS 兼容模式又不能因 native 默认改变而悄悄损坏。
- 不开发 Electron 客户端。本轮只核验外部普通进程能按公开契约调用组件。
- npm/tag/远端发布按单独授权执行；本轮开发提示词不自动授权生产发布或升级系统组件。
