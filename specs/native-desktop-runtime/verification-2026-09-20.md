# 2026-09-20 原生前端增量验证

## 范围与初始环境

用户提供的 macOS ARM64、Linux x86_64、Windows 主机均已通过 SSH 连接。本文不保存连接账号、密码或数据库凭据。使用独立测试目录，不覆盖系统安装，也不操作 K8s 资源。

> 下列三项是安装前记录，当前环境见文末“工具安装后复核”，不能当作仍未安装的阻塞项。

- Mac Mini：Rust、Node、pnpm 可用；非交互 PATH 与 Homebrew 路径不同，需显式配置。
- Linux：现有 app-cli 为 0.3.0-beta.2，Pingap 为 0.14.1，不能用于证明当前 0.3.6 源码通过；当前源码要求匹配 Pingap 0.14.3。
- Windows：Rust、Node、pnpm、Git、protoc、Python 可用；未在 PATH 找到 Pingap。尚未构建并运行当前源码。

## 实际反例与修复

使用本地模板仓库的 `cli/dist/index.js init <独立目录> --frontend vue` 创建纯前端项目。

修复前实际执行 `app-cli build --workspace <目录> --dev`：pnpm 安装退出 0，但 app-cli 随后因缺少生产 `dist` 退出 1。`workspace-manifest` 的 DevbuildSection 明确允许准备命令不产生 artifact；此处误用了生产检查。

修复：显式 devbuild 成功后不检查生产 artifact；生产 build 及 dev 回落普通 build 仍检查。没有制造空 dist，也没有放宽生产断言。

- 新增 `build::tests::dev_preparation_does_not_require_production_artifact`，覆盖 dev 准备成功、生产缺产物失败、dev 普通构建缺产物失败。
- `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(dev_preparation_does_not_require_production_artifact)'`：退出 0，1/1 通过，239 未选择；run ID `37b3afc6-5fc3-4def-b7b9-100921b38f5d`。
- 修复后实际 Vue 项目相同 dev 构建命令退出 0。
- 独立 app-cli build、fmt 与 `git diff --check` 退出 0。本记录不代表全量 nextest/Clippy 已通过。

## Mac Mini 实际 owner 检查（空工作区，非业务服务验收）

复制当前源码构建的 macOS ARM64 app-cli 0.3.6 至独立目录。二进制 SHA256：`1a340254ecbc893751554eb4af78d638166ede2dafa68249aab2390f882a85a5`。

1. 确认 3010 空闲，启动 `serve --workspace <独立空目录> --admin-addr 127.0.0.1:3010`。
2. 等待 runtime identity 可查询，再执行相同 serve 命令。
3. 第二次调用退出 1，错误为 exclusive owner lock；原 owner 存活且 identity 未改变。
4. 向本次启动的 owner 发送 TERM，15 秒内退出，3010 已关闭；未停止其他进程。

结果：排他保护与正常退出检查通过，但重复 serve **没有转交启动请求**。这不能作为“最后一个请求生效”的验收通过。`run` 中有转交逻辑，`serve` 目前直接获取 OwnerGuard；`--attach` 等待旧 owner 退出，也不等价于请求转交。

## 下一轮必须覆盖

- 匹配 Pingap 的完整纯前端构建、启动和真实 HTTP 响应；不要用旧 Pingap 产生的配置 hash 不匹配归因当前业务逻辑。
- Windows 模板生产构建调用 `sh scripts/build-standalone.sh` 的可移植性；有 Git 不等于 sh 已在进程 PATH，不能隐藏依赖。
- 同项目并发 serve/run：操作身份、受理顺序、终态、实际页面版本与唯一业务进程树；明确冲突拒绝与已受理请求的区别。
- 服务启动途中再次请求，停止后无残留监听；杀 owner 后业务后代如何收束与恢复。
- 端口被外部程序占用时不修改项目、不杀占用者；多个项目端口隔离。
- Linux、Windows 当前源码构建和同样的真实场景，以及 file-server-proxy 相关矩阵。

完整三平台、完整前端业务、Compose、K8s、发布均未由本轮增量检查证明。

## 后续增量：重复 serve 转交

上述 Mac 二进制实测后继续修改源码，不能把其旧 SHA256 实测结果算作以下改动的原生验证：

- OwnerGuard 新增结构化 `try_acquire`：仅 WouldBlock 返回占用，文件系统与锁 I/O 错误直接传播。
- 重复 serve 与 run 在占用时转交现有 owner，run 不再泄漏锁句柄，也不在 NoOwner 分支无锁进入编排。
- 转交核对规范化项目根，不能因两个目录同名而操作另一项目；同项目 `.run` 别名沿用统一布局规则。
- 控制客户端禁重定向；状态和操作响应核对实例身份，轮询核对 operation_id/kind，HTTP 错误不能当作成功数据。
- 带 APP_DEPLOY_URL 的重复入口明确要求部署 API，防止把制品部署参数静默丢弃成 Source Start。

第一轮 owner 聚焦测试 7/7 通过（run `b7c0d629-1152-4133-9bb0-4c39732c2353`，退出 0），覆盖 serve 公开入口受锁时调用 mock owner，以及跨进程排他。随后增加项目路径和操作身份核验，需以下一轮结果为准。

仍未完成：无显式 token 的原生自动凭据、owner 初始化中的等待/重试、并发 revision 冲突处理、制品部署转交、变更后二进制的三平台业务实测。不得仅凭 mock 转交通过宣称多 agent 场景全部完成。

最终聚焦复跑：`cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(owner_dispatch) | test(platform::owner_guard) | test(server::journal)'` 退出 0，15/15 通过、226 未选择；run `e15a6b26-34e0-4082-8611-e6205281704e`。包含路径核验、serve 转交和 StartupFailed journal 反例。

## 发布包缺失 Pingap 的修复与环境准备

用户授权安装和更新三台测试机环境。使用各自主目录下独立 bin/packages，不替换现有 K8s 服务或数据。

实际 `npm pack @nuwax-ai/app-cli-darwin-arm64@0.3.6 --ignore-scripts --json` 返回的文件仅有 app-cli/package.json，确认已发布包缺失 Pingap。源码 CI 虽复制 Unix Pingap，三个 Unix 平台包 files 清单却未列入；启动器仅 Windows 自动选择配套 Pingap。修复三个包清单与跨平台选择；显式 APP_CLI_PINGAP_BIN 仍优先。另补 npm 发布任务对 build-pingap-unix 的依赖，避免下载制品早于构建完成。

新增 `node --test npm/app-cli/test/launcher.test.cjs`：四个平台分别验证启动器选择/显式覆盖及真实 npm pack 文件清单，8/8 通过、退出 0。pack 测试使用临时占位二进制，仅验证包装规则，不证明二进制运行。CI 已接入该检查。node --check 与 git diff --check 退出 0。未发布修复后的 npm 包。

macOS ARM64 与 Linux x86_64 从上游 v0.14.3 release 安装 Pingap，实际 --version 均输出 commit `cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8`；前者 tls=openssl，后者 tls=rustls。Windows 的已发布 0.3.6 平台包包含 pingap.exe，提取到独立工具目录；运行结果另行记录。

注意：Windows SSH 默认 shell 中 HOME 为 MSYS 路径，直接插值给 PowerShell 会产生错误目录。后续采用 EncodedCommand + USERPROFILE 构造路径，避免 shell 之间再次展开。

Windows 安装后的实际核验：已发布 `@nuwax-ai/app-cli-windows-x64@0.3.6` 中 pingap.exe 输出 **0.14.1 / c74e4eaa44e64958cffa18c33e8bbf5995b6844f**，与当前 0.14.3 pin 不符，不能用于当前业务验收。正在测试机独立目录 checkout v0.14.3 并严格核对 commit 后原生构建 tls-rustls debug 二进制（两编译线程）。尚未宣称构建完成。

## Mac Mini 纯前端启动与停机预算

当前源码生成 release.lock，并通过 app-cli build 将模板 Vue 项目编译为静态产物；构建退出 0。Mac Mini 使用匹配 Pingap 实际启动，`/vue/` HTTP 200 并返回 HTML，无 PG/Redis/容器参与。

第一轮脚本仅给 owner 15 秒退出时间，短于实现的默认 30 秒 grace，错误地提前强杀 owner，并且曾只按页面/端口判断 passed。已将远端结果纠正为未完整通过，记录脚本错误，确认 PID、PGID、完整命令和测试配置路径后清理残留的本轮 Pingap 进程组。下一轮使用独立工作区与 45 秒总停机观察预算；必须 owner 正常退出且端口释放才能通过，不能只看页面或端口。

正确预算重跑（独立 case `frontend-1fa8ec35`）：页面成功，但 owner 30.03 秒后退出 1（server shutdown confirmation timed out），三个端口关闭而 Pingap 进程仍存活。确认是产品缺陷：server 外层固定 30 秒与 supervisor 默认 30 秒 grace 相撞，driver 被 abort 前未完成强制收束。实测结果 passed=false。已核验测试进程完整命令/配置目录/PGID 并清理残留。需修正外层总预算对业务 grace、强制收束确认、剩余 writer 的覆盖关系，并复跑同一真实流程；不得仅放宽测试断言。

## 停机预算修复闭环

- ServerState 保留本进程历代 release 的停机宽限期和组数最大值；新代次不会压缩尚未清理的旧代次预算。builtin 预算覆盖业务宽限、5 秒强制树清理确认及 30 秒剩余 writer/journal；supervisord 另按顺序 stop/remove RPC 和组查询计算预算，不再固定 30 秒先行中止。预算溢出 clock 范围明确报错。
- 聚焦命令：`cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(shutdown_budget) | test(platform::process_tree) | test(shutdown_cannot) | test(supervisor_join_failure)'`，退出 0，7/7 通过、235 未选择；run `48f39be7-27bc-4b67-b213-1bb1261dde12`。
- Mac Mini 真机 case `shutdownfix-fcc7b2f0`：当前 app-cli SHA256 `308bb8962f5eb309597201b04d5a335fac6bfeeb3f58dd9df666815ae41ef4ab`，真实 Vue HTML 返回，TERM 后 30.92 秒 owner 退出 0，3010/3018/9080 关闭，ps 中无本 case 进程。passed=true。此结果证明 builtin 纯前端启动/停机，不替代 supervisord、多 agent 并发或其他平台验证。
- Windows Pingap 构建首次失败为已安装 CMake 不在 SSH PATH；加入 CMake/bin 后继续使用缓存构建。目前尚未宣称完成。

## 2026-09-20 自动 owner 凭据与平台构建进展

- 源码快照包含 1127 个文件（不含 .env、target、node_modules、测试报告），压缩包 SHA256 `719327bf44cc7a438b262c259938999aa482fd69cc78e589b8137db9038f720c`。Linux/Windows 在独立源码目录构建 app-cli，两条编译线程；无需提交代码，不覆盖 K8s 或系统安装。此快照包含停机修复，**不包含以下随后新增的自动凭据修改**。
- Windows Pingap 原生构建完成，实际 `--version` 为 `0.14.3 (cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8, tls=rustls)`，退出 0；此前 CMake PATH 问题已解决。该结果不是 Windows app-cli 业务验收。
- serve 获得 owner 后初始化实例凭据，显式 APP_CLI_DEPLOY_TOKEN 优先，否则生成随机凭据，不修改进程全局 env；runtime/deploy/配置激活 API 统一使用该实例凭据。legacy 未初始化 owner 的入口保留显式 token 行为。
- token 使用临时文件，先完成 Unix 权限/Windows ACL，再写入同步并原子替换，避免暴露未保护内容或被客户端读到部分 token；发布失败明确返回错误。
- `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(token) | test(owner_dispatch)'`：退出 0，6/6 通过、237 未选择；run `9c759777-5bc8-43e3-9a75-8d336cd220bd`。覆盖实例凭据稳定性/不改 env、已有 token 文件权限、legacy 无 token 拒绝与 mock owner 转交。新自动凭据的真实宿主机 API 验收仍待补。

Linux 当前快照原生构建退出 0（3 分钟）。case `linuxsmoke-5950f2da` 实机验证：二进制 SHA256 `458c349b3a5289906faa5bb638d7294c08dfa47beb7b66d5bb79d7b075ca0acc`，Vue 页面成功，TERM 后 30.67 秒 owner 退出 0，3010/3018/9080 释放，无本 case 残留进程。passed=true。这是源码快照 719327bf 的 builtin 产物态业务验证，自动凭据变更仍需补验。

自动凭据发布随后增加失败清理：在 kernel/endpoint 发布前完成 token 写入；失败关闭 admission 并 abort 本实例 API/signal 任务，避免返回后 listener task 仍存活。事件桥和 endpoint 直接复用已获得的 Arc kernel，去掉两处生产 expect。此补充已 fmt/diff 检查，需下一轮编译验证。Windows app-cli 快照构建仍在执行。

## 自动凭据真实 API 回归

新增跨平台集成测试 `crates/app-cli/tests/owner_credentials.rs`：真实启动独立 app-cli owner，清除 APP_CLI_DEPLOY_TOKEN，等待 identity、原子 token 文件及 /ready；错误 token 的 Stop 返回 403，文件中的 token 提交同请求返回 202，按 operation_id 与 runtime_instance_id 观察到 Succeeded，并确认 desired=Stopped。空工作区不启动 Pingap 或业务进程，结束时回收本测试 owner；不把此测试称为完整业务或优雅进程树停机验证。

命令：`cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(owner_publishes_usable_credentials)'`；最终 run `f4c8e30f-c68e-436b-a2c6-dca43f97511d`，退出 0，1/1 通过，243 未选择。包含上轮自动凭据失败清理改动的编译。等待 /ready 避免把尚在初始化的身份发现当作写端点已开放。

## Owner 初始化窗口与 Windows 编译

- 重复 CLI 的 owner 身份探测增加 10 秒有界初始化等待，每次请求包含在同一 deadline 内；仍需随后通过协议、项目路径和实例身份核验，等待期间不取得运行态所有权或启动业务。未获得可信身份时保持拒绝，不通过端口猜测接管。
- wildcard 监听地址 `0.0.0.0`/`::` 在本机客户端转换为 IPv4/IPv6 loopback，避免 Windows 无法连接 unspecified 地址。
- `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast -E 'test(owner_dispatch)'` 退出 0，3/3 通过，243 未选择；run `1eede54e-4e16-4860-a683-59df7989ea6c`。包括延迟发布 identity 后转交、无 HTTP 应答严格超时与 wildcard 转换。
- Windows 源码快照 719327bf 的 app-cli 原生构建退出 0（8m25s）。发现的 Windows 条件编译告警已在本地调整（archive_links 参数、storage_contents 显式 drop、XML-RPC 错误携带 socket 上下文），这些调整不在该快照里，需增量补验。正在执行纯前端/API Stop 实机流程。

## Windows 纯前端/API Stop 实机结果

case `win-smoke-003b83fb` 使用源码快照 719327bf 构建，app-cli SHA256 `33d8f8dfa65590bdc8835f1ec8be6d59e836e8bc507f867ce82e8237fc5a60ec`，匹配的本机 Pingap 0.14.3。实际 Vue HTML 返回，日志确认 config_hash matched。随后经携带测试专用 token 的 runtime API 提交 Stop（核验 operation_id/runtime_instance_id），30.19 秒后 Succeeded，owner 仍存活。测试收尾终止已无业务进程的 owner；此 TerminateProcess 退出 1 是测试清理行为，不是 Unix 式优雅 owner 关机证据。最终 3010/3018/9080 关闭，Win32_Process 无本 case 命令行进程，passed=true。测试 token 仅保存在该测试目录受控状态文件与进程内存，不写入本报告。

三平台至此各有一轮纯静态前端启动/业务停止证据；不是 NT01–NT16 完整矩阵，Windows 仍需当前增量（自动凭据/初始化等待/告警修复）编译与回归，及多 agent 并发、路径边界、重试/失败恢复、file-server-proxy 等验收。

## 测试工具安装与版本复核

依据用户对三台个人测试机安装/更新工具的授权，从 nextest 官方 `https://get.nexte.st/0.9/{platform}` 下载 mac、linux-musl、windows-tar 预构建包，安装到各用户 `.cargo/bin`。三台实际运行 `cargo-nextest nextest --version` 均为 0.9.145（commit 00af4550ec3b3b9f0e574b897b06acb95d325ba2），安装脚本退出 0。未修改系统服务或集群配置。

再次通过 SSH 实测版本（整轮检查退出 0）：

| 平台 | Rust | Node | pnpm | 独立测试目录 Pingap |
|---|---|---|---|---|
| macOS ARM64 | 1.97.1 | 26.8.2 | 12.4.1 | 0.14.3，cd74a461，OpenSSL |
| Linux x86_64 | 1.98.1 | 22.22.1 | 10.33.0 | 0.14.3，cd74a461，rustls |
| Windows x86_64 | 1.98.1 | 24.13.1 | 10.33.0 | 0.14.3，cd74a461，rustls |

Mac 非交互命令显式前置 `.cargo/bin` 与 `/opt/homebrew/bin`；Windows 构建显式前置 `.cargo/bin` 与 `C:/Program Files/CMake/bin`（CMake 4.4.3）。Pingap 使用独立测试目录中的绝对路径，不能误调用 PATH 中的旧版本。pnpm 主版本尚未统一，后续模板构建须记录实际选用版本，不能认为三机依赖环境完全相同。

本节仅证明工具已安装并可执行；没有新增业务测试通过结论，也未将旧快照测试结果升级为当前工作树验证结果。

## 2026-09-20 工具安装后复核

用户授权三台个人测试机器按需安装/升级开发工具。再次通过 SSH 实际运行版本命令，三台均成功：

| 平台 | Rust | Node | pnpm | nextest | Pingap |
|---|---|---|---|---|---|
| macOS ARM64 | 1.97.1 | 26.8.2 | 12.4.1 | 0.9.145 | 0.14.3 / openssl |
| Linux x86_64 | 1.98.1 | 22.22.1 | 10.33.0 | 0.9.145 | 0.14.3 / rustls |
| Windows x64 | 1.98.1 | 24.13.1 | 10.33.0 | 0.9.145 | 0.14.3 / rustls |

三个 Pingap 的实际版本输出均包含提交 `cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8`，版本命令退出 0。测试使用个人目录 `rcoder-native-20260920/bin` 内的匹配工具，不以 Linux 系统旧 app-cli/Pingap 验证新源码。Mac 非交互命令显式加入 Cargo/Homebrew 路径；Windows 使用 PowerShell EncodedCommand、USERPROFILE 路径与 pnpm.cmd，避免 MSYS 路径展开和脚本执行策略影响。

环境安装可用不等于所有业务验收完成。前述三平台纯前端结果仍只对应各自记录的源码快照；自动 owner 凭据、重复启动/转交、file-server-proxy 完整矩阵及后续增量代码仍需复验。未执行 npm 发布。

## 重复 CLI 的初始化窗口（后续增量）

owner_dispatch 已增加提交前初始化等待：/ready 返回 initializing 时有界等待，ready 或业务 not_ready 均允许进入后续 status 身份核验和提交。它不会等待业务 Ready，也不会因初始化未结束启动第二个 owner。请求无响应受同一初始化 deadline 限制，非协议响应明确报错。

新增 fixture 覆盖两轮 initializing 后转为 not_ready，确认继续提交前流程；无 HTTP 应答时验证预算收束。已有 owner 分派测试也使用业务 not_ready 响应，保留操作/实例身份与源目录核验断言。

本轮正在执行独立 app-cli nextest owner_dispatch 筛选，日志 /tmp/rcoder-owner-initialization-tests.log；尚未得到退出码，不能宣称通过。根 workspace 全 features 回归同时使用另一个构建目录，日志 /tmp/rcoder-workspace-all-features-final.log。两项均仍在编译，不是 native 三平台实机结果。

### 初始化等待组件验证结果

上述独立 app-cli 筛选现已结束：退出 0，run ID `c9975f3e-e5aa-4eda-a7a0-bbcf8a921731`，4/4 通过，243 个未选中。覆盖 wildcard 地址转换、身份等待预算、初始化等待/业务 NotReady 放行、已有 owner 请求分派；日志 `/tmp/rcoder-owner-initialization-tests.log`。该结果不替代当前源码的三平台实机与完整业务矩阵。

### 独立 app-cli 全 features 回归

命令 `CARGO_TARGET_DIR=crates/app-cli/target cargo nextest run --manifest-path crates/app-cli/Cargo.toml --all-features --no-fail-fast`（实际使用同一目录的绝对路径），退出 0，run ID `a80079c0-653f-4b27-bd76-99121bdcc276`，247/247 通过，0 skipped，测试阶段 26.097 秒。日志 `/tmp/rcoder-app-cli-all-features-current.log`。包括 owner 凭据发布、并发首启、管理口冲突副作用保护、重复调用分派、失败后重试、正常关停和进程树清理。本轮在本地 macOS 执行，不能替代远端三平台当前快照的模板构建/代理/真实服务矩阵。

## 并发登记与动态管理口发现修正

后续源码核查确认两处遗漏：standalone 首次登记 registry.lock 使用一次 try_lock，正常并发竞争会失败；serve 将配置的管理地址写入 endpoint.json，配置端口 0 时无法发现实际 listener。已修改：

- 登记锁仅对 WouldBlock 有界重试（2 秒），其他文件系统错误立即返回；拿锁后仍重读登记表，同一项目只生成一个状态根。
- serve 从 listener.local_addr() 发布实际地址；重复 CLI 在统一发现预算内重新读取记录、探测无认证身份，验证 application/workspace/instance/protocol 后才允许后续凭据请求。陈旧实例记录不授权转交；项目规范路径验证保留。
- 新增 12 路并发首次登记与持续锁竞争预算断言；现有真实 owner 凭据用例改用 `127.0.0.1:0`，断言发布非零端口并实际执行认证 Stop；分派用例新增动态端口发现和陈旧记录拒绝断言。

registry 聚焦 nextest 日志 `/tmp/rcoder-registry-contention-tests.log`；app-cli 聚焦验证已启动，日志 `/tmp/rcoder-dynamic-owner-tests.log`。当前增量结果不得沿用上一轮 247/247 全通过结论。

本增量聚焦结果：registry nextest 退出 0，6/6 通过（run `e39755a1-f274-4b69-98db-00d9c042e81e`）；app-cli nextest 退出 0，5/5 通过、242 未选中（run `6edefde6-0d1f-43d0-a6be-ab3689612339`）。真实 owner 的动态地址发布和认证 Stop 已在本地进程验证，重复请求分派及陈旧记录拒绝由 HTTP fixture 验证。未将此范围扩称为三平台完整验收。

### attach 项目身份核验增量

serve --attach 原先只比较 application_id 与目录 leaf name，无法区分同名的不同项目。现改为解析完整 RuntimeIdentityView、核验协议版本和 canonical 项目路径；source/.run 别名仍匹配。re-exec 只去除 --attach 参数，不再误删值为 attach 的普通路径参数。新增同名项目拒绝、.run 接受、协议不兼容拒绝用例，并执行现有 attach 二进制测试。日志 `/tmp/rcoder-attach-identity-tests.log`，结果待回填。

attach 聚焦验证结束，退出 0：3/3 通过，245 未选中。覆盖新增规范路径/协议规则、真实子进程身份不匹配非零退出，以及无既有 owner 时正常进入 serve。日志 `/tmp/rcoder-attach-identity-tests.log`。独立 app-cli Clippy 已启动，日志 `/tmp/rcoder-app-cli-clippy-current.log`。

独立 app-cli Clippy 清理后退出 0、无 warning/error：`CARGO_TARGET_DIR=crates/app-cli/target cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets --all-features`，日志 `/tmp/rcoder-app-cli-clippy-cleanup.log`。仅去除三个不必要借用；没有改变凭据或 PG 调用语义。fmt check 与 git diff --check 通过。

## 当前快照三平台重新构建

冻结源码内容 SHA256 `4e4e49267c505b75a77b142b6024d87c2bfcf03b6fc2f0c8d1cabc698cbfb521`（1136 文件）；归档 SHA256 `3f23ed3ec8599ff7d352d7dc554484aa2585881095a4fcdc67f48a6f4cb8bfe4`。仅打包 Cargo manifests/locks、crates 与 tests-e2e 源文件，不包含环境凭据、target、node_modules 或报告。使用独立源码目录，不覆盖系统安装。

macOS 本地当前源码 app-cli build 退出 0（日志 `/tmp/rcoder-native-mac-current-build.log`），产物复制到个人 Mac 测试目录；Linux 在已核验归档的独立目录 build -j2 退出 0。Windows 初次 PowerShell ErrorAction Stop 将 Cargo 正常 stderr 编译输出错误提升为异常，进程已确认退出；改用 Start-Process 独立 stdout/stderr 文件后重新构建，仍在执行。此为测试启动器问题，不能记成 Windows 代码编译失败，也不能尚未完成就记为成功。

### 当前快照真实重复启动反例（未通过）

Mac case `nativeowner9afb77b3bc48` 与 Linux case `nativeownerbf737d1cf350`：真实模板 HTML 首次启动通过、自动 token 与动态 endpoint 可读，但第二次 serve 均退出 1，原文 `decode accepted operation: missing field kind`。两轮清理后 owner 退出 0，管理/Pingap 端口关闭；整例 passed=false。日志分别为 `/tmp/rcoder-native-mac-owner-current.log`、`/tmp/rcoder-native-linux-owner-current.log`。Mac 二进制 SHA256 `c392072fbbc7efe2d5b73051f49a26462b4bdc5b1a7bf491e20a1b9194235d03`；Linux `20a1c84c036e8f09ae4a294116f48c6c5a163ca279df85bba5921f12980174ee`。

根因：真实 POST 返回 RuntimeOperationAccepted（operation_id/state/poll），owner_dispatch 却直接反序列化成 RuntimeOperationView；原 mock 也错误返回完整 view，导致组件通过。现改为解析并验证受理回执的 operation_id/poll，再 GET 完整 view 校验实例、kind 和终态；即使受理回执声称终态也必须读取完整视图。轮询请求使用统一 deadline。mock 改为真实 202 DTO，不增加兼容回退或弱化身份断言。复验日志 `/tmp/rcoder-owner-accepted-receipt-tests.log`。

因此刚构建的 4e4e49267c50 快照二进制与 Docker 镜像不能作为最终验收版本；完成修复后必须重新构建受影响的 app-cli 产物。Windows 旧快照构建仍需收取实际结果，不因旧快照失效而误报编译失败。

受理回执修复的 owner_dispatch 聚焦测试退出 0，4/4 通过，244 未选中，run `a8cd1d93-d85f-40e5-8747-a02343126f75`。真实 Mac/Linux 重复调用反例仍需用修复后产物复跑，不能仅以此 mock 修正宣布闭环。

Windows 旧快照构建补充：编译日志显示 Finished（1m07s），但 Start-Process -Wait 控制器未退出。独立进程检查确认 cargo/rustc 已不存在，控制器无子进程；核验其 EncodedCommand 确为本轮唯一构建脚本后终止该控制器，SSH 退出 127。不能将此 SSH 结果写作构建命令退出 0。下一轮用 Process.WaitForExit 等待直接进程并单独记录 ExitCode，避免 Job 等待问题；未终止任何业务或无关进程。

### 受理回执修复后的真实复跑

当前源码 SHA256 `5d37d04086948f9f1b919ec450da95258e76965c86ee6cb75091db02b8b6d85c`，归档 SHA256 `f2b80c7f114bfae38d361125b6f1337086dc258e73b9220dfddad0b2d7c0dd51`。使用 `tools/native_owner_smoke.py`，无 Docker/PG：真实模板静态 HTML → 自动凭据/动态管理地址 → 重复 serve → 同实例提供更新 HTML → API Stop Succeeded、owner 仍活着且 desired=stopped → owner 退出、端口释放。

- Mac case `nativeowner5dd9d774af9a`，退出 0、passed=true，binary SHA256 `50573abbd758479a78fda6d0bc6cc5de71e7c2d88b139ef08882342f7e89f8eb`，日志 `/tmp/rcoder-native-mac-owner-receipt.log`。
- Linux case `nativeownerf4c03549b4c0`，退出 0、passed=true，binary SHA256 `a436c082a163fbb90cee85ba737892633c8a8125725bb0353a925a32b71e925a`，日志 `/tmp/rcoder-native-linux-owner-receipt.log`。

fixture 复用已构建 Vue 模板产物，刻意删除 devbuild/devrun 段以验证纯静态服务，保留路由和页面内容；不是源项目 pnpm/Vite 开发模式或三平台完整 NT 矩阵，也不包含 file-server-proxy。端口释放不宣称为完整后代进程证明。Windows 当前快照编译控制器 ExitCode 最初为 null，不能以 SSH 0 冒充执行过业务测试；已改成 cmd 原生重定向并显式记录 $LASTEXITCODE 后继续实际 smoke。

Windows case `nativeowner22e660fee98e` 同样退出 0、passed=true：binary SHA256 `79919178417a9d44e74f3c9a7b36470cd988185154a4362902b4066204e8da53`；实际 HTML、重复调用保持 runtime_instance_id、更新 HTML、API Stop 后空闲 owner、最终管理/Pingap 端口释放全部通过。编译确认日志 Finished 0.92s（缓存命中），显式记录 exit_code=0 后才启动 Python smoke。测试最后对已停止业务的 owner 调用 Windows TerminateProcess，owner_exit=1 是清理行为，不能称为优雅进程退出；测试脚本本身退出 0。证据位于个人测试目录内该 case/result.json。

至此本轮 202 回执解析缺陷的三平台真实重复启动反例闭环；不代表所有 NT01–NT16、源项目开发模式、并发最后请求语义或 file-server-proxy 完成。

### file-server-proxy 原生入口增量

源码核查发现 Windows 的 `main() -> ()` 在非 Unix 信号分支使用 `?`，无法编译；状态目录仅识别 HOME，Windows 未设置时落到当前目录；IPv6 host 直接拼接端口和文件名也不具备跨平台有效性。现分离 `shutdown_signal() -> Result`，信号错误仍先收束代理，停止失败返回非零；状态目录支持 USERPROFILE，缺失明确拒绝并提示显式目录；IP 地址使用 SocketAddr 格式、锁名使用 host 字节十六进制。没有更改 public bind 默认值。

- `cargo nextest run -p file-server-proxy --all-features --no-fail-fast`：退出 0，24/24 通过，日志 `/tmp/rcoder-proxy-native-fixes-tests.log`。
- `cargo clippy -p file-server-proxy --all-targets --all-features`：退出 0，无 warning，日志 `/tmp/rcoder-proxy-native-clippy.log`。
- `cargo build -p file-server-proxy`：退出 0，日志 `/tmp/rcoder-proxy-native-build.log`。
- 新增 `tools/native_proxy_smoke.py`：真实内嵌 Router 文件根浏览、缺少 token 拒绝、含空格路径的 HTTP 写文件和列文件、重复固定端口进程失败且原服务继续响应、退出后端口关闭。
- 本地 macOS case `proxybedb669bbe71`：退出 0、passed=true，binary SHA256 `aec0011b5578bec6b0983baba8389f9059b36fe50d6ea60fcf56b9ebcb71e5f6`。报告在 `/tmp/rcoder-native-20260920/proxy-cases/proxybedb669bbe71/result.json`。

上述 smoke 不是完整 NT02（尚未覆盖搜索/上传/下载/Git），也不证明所有后代进程、npm 入口或离线完整包。Windows 已启动实际 native build，结果不能提前计入。Windows 源码基于 `5d37d0408694` 快照叠加本轮 main.rs/instance.rs，须保留这一增量身份，不能称原归档包含最新修复。

file-server-proxy Linux / Windows 实测补充：

- Linux 原生 build 与 smoke 均退出 0，case `proxyf41fb0df6902`，binary SHA256 `b43a17da519955da06d24c103fe8c52979c8a6e19f766e120f458ab63fdbd97e`，四组 HTTP/冲突断言通过，代理 SIGTERM 退出 0、端口释放。
- Windows 原生 build 退出 0（6m41s），case `proxy38e5e6043a80`，binary SHA256 `8a61418cd5d9785afd9866573be1cfe8dc31e4e41fc42fbedac9c11118fc58df`，smoke 退出 0、passed=true。测试最后用 TerminateProcess 收束代理，进程退出 1、端口关闭；不是 Windows 优雅关停的证据。
- Windows 编译暴露 file-server Git regular_file_mode 的 Unix 专用 metadata 参数警告；本地随后改名为 `_metadata`，不改变代码行为。以上二进制仍是该无行为改名之前的产物。

两台远端报告各自在本轮个人测试目录 `proxy-cases/<case>/result.json`。这些结果继续仅限上述 smoke 范围，未冒称完整原生矩阵。

本轮 native 修复合并后，独立 app-cli 完整 nextest：`CARGO_TARGET_DIR=crates/app-cli/target cargo nextest run --manifest-path crates/app-cli/Cargo.toml --all-features --no-fail-fast` 退出 0，248/248 通过、0 skipped；日志 `/tmp/rcoder-app-cli-native-final-tests.log`。仍非完整三平台矩阵。

## Checkpoint 后三平台追加实测

本段记录并行任务的实际结果，不能替代尚未完成的 NT01–NT16 全矩阵或 npm 发布验证。三台宿主机均使用独立测试目录，未操作 K8s 资源。

- macOS：当前独立快照83f5ab52对应二进制cb03859a；真实Vue源码devbuild/Vite、重复serve同owner更新、Stop保持idle、SIGTERM正常退出及端口释放通过；生产build与产物serve链通过；伪造health占管理口11ms非零退出且工作区未变；原生针对性回归9/9通过。详细报告 `/tmp/rcoder-native-macos-followup.md`。
- Linux：含最新PG环境修复二进制cacc162e，Vue生产build、源码devrun、重复serve、失败后显式重试实际恢复、未知管理口占用保护通过。随后空格/中文workspace反例修复前Stop409，修复后2/2及真实Vue链通过（binary1f664359，case nativepath4713045378e3）。报告 `/tmp/rcoder-native-linux-followup.md`。
- Windows：实际目录含空格、无Git Bash PATH的Vue源码启动/重复serve同owner/Stop/再次serve通过，最终进程与5756/3010/3018/9080监听全部清理。Job强杀反例修复前失败，增加KillOnDrop后通过；既有process_tree三例通过；workspace身份+owner崩溃两集成目标2/2通过、0skip。组合binary beaa33d29e99165f5bd04057ce5c596ae954790bdb2773ce3f1c486fa12a23ff，case nativeownerad74266b6b36。报告 `/tmp/rcoder-native-windows-followup.md`。

原生测试发现并修复的目录身份和Job关闭问题已包含在4e51f476检查点。之后的本地ArtifactId部署目标和file-server持久意图仍在修改，以上结果不是这些后续改动的验收。Windows Ctrl+C优雅关闭、完整离线/打包环境及全部原生矩阵仍未完成。

### Windows file-server-proxy 路径读写追加验证

checkpoint原生HTTP复现两个缺陷：../写被跳过却返回成功；junction指向同测试目录之外时可真实读写外部哨兵。修复为整个写入批次在备份/创建目录前预检实际祖先、执行前复检；文件解析与静态读取也核验解析后的归属。调用方仍可选择合法workspace根，不增加根目录白名单。边界为路径预检，并非对恶意本机进程并发置换链接提供句柄级隔离。

Windows新增5条路径组件测试5/5通过（run07d0cb0b-5d08-419a-bbbd-8d8c55759d50），最终真实HTTP12项通过（case proxye28d5c2f5318，binary72d5a3ca27c93168be7dbb3a059f7601b4a38e675162bde752a8c47eeb4aa905）。含中文空格根、二进制上传下载、搜索、Git、../拒绝、junction外写拒绝、resolve外部链接exists=false、static外链404、ZIP不泄漏；内部链接读取仍通过。报告 `/tmp/rcoder-native-windows-proxy-followup.md`，测试资源已清理。并不替代npm发行包或完整桌面矩阵验收。

Mac/Linux proxy追加核心套件的委派任务在执行前被平台自动安全拦截终止，未取得这两平台的新结果；不把Windows结果或此前基础smoke替代该追加套件。该任务未自行提交/发布，其他Compose与源码修复继续。

### retire/摘要校验集中验证（2026-09-20 深夜补跑）

ND06/07 收尾文档标注"retire 与摘要校验已实现但尚未编译/实测，等待集中验证"，本段补齐该验证。基线：HEAD `604fd2b6` 工作树（仅存在未跟踪交接文档），全部在本机 macOS 串行执行。

- 四组件（process_utils/file-server/file-server-userapp/file-server-proxy）nextest 默认 features：488/488 通过、0 skipped，退出 0。
- 同四组件 nextest all-features：488/488 通过、0 skipped，退出 0。
- 同四组件 clippy `--all-targets -- -D warnings`：退出 0。
- proxy 纯转发形态 clippy（`--no-default-features --all-targets -- -D warnings`）：退出 0。
- `cargo build -p file-server-proxy --bin file-server-proxy`：退出 0。
- 真实二进制脚本 `python3 crates/file-server-proxy/tools/test_native_guardian.py --binary target/debug/file-server-proxy --report /tmp/rcoder-native-retire-guardian-20260920.json`：**10 项断言全部通过，退出 0**；二进制 SHA-256 `4380ab53d455e46c3f2e68177bf83dd4578df006d4eafe27bbfde64d4135bd63`。覆盖：真实 owner SIGKILL 的父进程 wait 见证；pipe EOF 收束真实 TS 树后才 Quiescent；同 boot recover 需原实例+真实退出/清理见证；旧实例迟到 stop 不误停后继；存活 owner 授权拒绝变更命令 spec（无副作用）；未消费 guardian 拒绝并持久 Revoked；Pending 撤销后真实迟到 guardian 二进制被拒；内嵌 execute-command 在途崩溃恢复命令与中断 worker 且不冒充 HTTP 成功；显式 retire 关闭长连接 owner 后 recover 消费其退出见证、重试不误伤后继；guardian+owner 双崩溃后即使 fixture 自行结束仍保持未知保护。
- npm 启动器测试：24/24 通过，退出 0。根 workspace `cargo fmt --all -- --check`：退出 0。

边界：以上为 macOS 本机组件与真实二进制验证。guardian/retire 链的 Linux/Windows 宿主机执行、NT13–16（离线只读包、跨 ABI 拒绝、活动文件升级、TS 兼容模式）、Mac/Linux proxy 追加核心套件（此前委派任务被拦截未取得结果）仍未验证，不以此段替代。
