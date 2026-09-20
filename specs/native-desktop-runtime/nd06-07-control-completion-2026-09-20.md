# ND06/ND07 独立宿主机控制补全

## 范围

保留原二进制前台启动 flags 与业务监听默认值。新增显式 native owner 模式，由 Rust 在同一 scope 稳定目录持有排他文件锁、原子写入版本化实例回执，并通过 loopback 私有管理通道提供身份校验的状态和停止。管理通道采用有界 JSON 行协议，不挂到业务 HTTP 路由；实例 ID 与随机令牌必须同时匹配。

复用现有 `instance::try_start/stop` 与连接 drain，不建立业务状态机副本。停止只有 drain 完成且 Stopped 回执持久化后成功；损坏、未知版本、控制断连不清记录、不按 PID 杀进程。显式 restart 必须先确认原实例停止。JS 仅解析参数、解析已安装二进制、启动并调用控制命令。

## 实施边界

- Rust：file-server-proxy main、native_control，必要依赖；容器不启用 native owner 时原行为不变。
- npm：bin、daemon、orchestrate；不得维护全局 PID 权威。
- package 的 TS 强制依赖移除与产物准备修改协调，标准 all_rust 不依赖 TS。
- 共享进程原语从 app-cli 抽取至 process_utils；app-cli 保留薄导出和原测试。TS 进程树由 Rust owner 持有，JS 不接管全局 PID。

## 验证

覆盖同时启动单胜者、错误实例/令牌拒绝、损坏/版本/未知回执保留、stop 已完成 drain、重启仅承接已确认 Stopped、原容器 flags 不变。三平台宿主机验收与本地组件证据分开记录。

## 共享进程原语实施

已获准将 app-cli 私有 ManagedChild/StopOutcome/spawn_managed 的实现原样抽入 process_utils::managed_tree，app-cli 保留薄导出和原测试。Windows JobObject/KillOnDrop、Unix 进程组及既有收束判据保持；不新建生命周期状态机。显式TS兼容无外部端口时，JS仅解析已安装Node/TS入口交给Rust，owner直接spawn并持真实进程树句柄，stop完成屏障同时等待业务drain和TS全树收束。显式外部TS端口不接管。不会读写TS全局PID文件。标准路径不解析TS依赖。

以上已完成本机组件验证；尚未完成独立 app-cli 检查和三平台宿主机部署验收。

## 命令与边界

- 原有 `file-server-proxy --embed --policy all_rust ...` 前台/容器形式不变。
- `file-server-proxy start --embed --policy all_rust --port N` 启用原生 owner；`stop/status/restart --port N` 使用同一 host、端口和状态根的控制通道。`restart` 在确认旧实例已停止并释放锁后以前台方式启动；npm `--detached` 由薄启动器处理。
- `FILE_SERVER_PROXY_STATE_DIR` 是显式稳定根；否则取 HOME/USERPROFILE 下 `.file-server-proxy`。使用相同 host 与请求端口 0 的调用也属于同一 scope，多实例应显式分配不同状态根。
- 控制输出不包含随机认证令牌。业务 public bind 的默认行为保持不变。
- SIGKILL/异常退出留下非 Stopped 回执时不自动清理、不重新按 PID 接管。此版保留未知结果保护，完整人工核验恢复流程尚未提供。
- TS 健康探测只用于就绪观察，归属始终由 Rust spawn 获得的进程树句柄证明，外部 TS 永不回收。

## 本进程工作收束

`full_router_with_lifecycle` 返回与路由共享的生命周期句柄；原 `full_router` 保留兼容调用形式。原生 owner 使用该句柄关闭受理并等待本进程的 HTTP handler、后台构建与工作区重置。登记与关闭使用同一门闩，HTTP 连接断开只结束观察，不取消已登记的 worker，因此解包等 `spawn_blocking` 工作仍须完成后才能收束。

受管命令在 spawn 前记录 SpawnPending，持有实际树句柄后记录 Running；确认整树退出后才记录 Quiescent。PID 只用于诊断。停止或用户取消通过 CancellationToken 传递，不能把已发送取消事件当成清理完成。kill 的 deadline 同时覆盖发起和确认；超时保留原 kill future 与实际句柄继续观察，不能取消后重新创建一个可能误判已完成的 future。

worker panic、目录读取失败、持久写失败或命令清理未确认均保持保护；没有持久根的兼容调用也保留内存 pending 状态。目录新建包含 Unix 父目录 fsync。关闭同时尝试代理 drain、内嵌工作和 TS 清理，聚合错误；只有全部确认且 Stopped 已持久化才返回停止成功。这里不停止独立 app-cli 业务 owner。

### 当前验证（工作树，非发布证明）

- `cargo nextest run -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --no-fail-fast`：482/482，退出 0；日志 `/tmp/rcoder-native-owner-nextest5.log`。
- `cargo clippy -p process_utils -p file-server -p file-server-userapp -p file-server-proxy --all-targets -- -D warnings`：退出 0；日志 `/tmp/rcoder-native-owner-clippy4.log`。
- npm daemon 测试：7/7；涵盖显式 TS 工具链、Electron 不推断 Node、旧 launch ID 不以新实例就绪作为自身成功。
- 未运行：本轮独立 app-cli、完整 feature 组合、三平台宿主机和部署 E2E。本轮新原生代码尚未进入被测生产镜像。

### 未完成，不能据此勾完 ND06/ND07

1. 非正常 owner 死亡后的同 boot 显式恢复尚未提供。活动命令不能凭端口/PID或文件锁空闲推断树已清理；需要独立持真实句柄的监督者，或同等级可验证收束证据。现有未知状态保护不是功能完成。
2. TS 意外退出或管理 listener accept 失败后的清理错误仍可能结束 owner；应与信号关闭保持同样的持句柄保护和重试语义。
3. 当前工作日志关联原生 instance 目录，但还未完整记录业务 task/外部 app-cli 操作身份。异常恢复需保留外部提交的原身份和未知结果，不能据本地 worker 退出推断业务操作已完成。
4. admission 临界区仍包含同步持久 I/O；deadline 无法抢占单次阻塞系统调用。后续应明确延迟边界，不能把超时包装当作 I/O 已被取消。

## 后续实现节点：同 boot 恢复（待编译与实测）

已新增同一二进制的两个私有模式，不依赖额外安装服务：

1. `native_supervisor` 持有实际原 owner 子进程句柄；只观察该进程退出，不建立覆盖独立 app-cli 业务的进程树 Job。得到 wait 结果后持久 `OwnerExited`，绑定 supervisor UUID 与原 instance UUID。信号通过同实例管理协议转发；日志继承，退出码透传。
2. `process_utils::guardian` 持有本地命令/TS 的实际 ManagedChild。父子 stdin 管道是存活租约，父进程死亡或停止关闭管道后，guardian 自行收束整树。父进程绝不通过 kill guardian 代替业务清理。guardian 在 spawn 之前锁住授权记录并将 Pending 改为 Running；全树确认后才写 Quiescent。命令的原 task/app 与外部 operation/owner/workspace 关联记录保留。
3. 显式 `recover --instance-id <原实例>` 必须先取得 scope 锁，匹配真实 `OwnerExited` 证据，再逐项核验 guardian。未消费 Pending 在原 guardian 锁内改 Revoked，迟到的 guardian 不能再 spawn；Running 且无确认回执仍拒绝。全部命令收束后，原本进程 worker 可登记 `OwnerExitedInterrupted`，不把外部 app-cli 操作宣称成功、不停止其业务。

该实现节点尚未编译或进行真实子进程故障验收；前文 482/482 仅属于 guardian 加入前的阶段基线。guardian 自身也崩溃而缺少清理证据属于剩余未知状态，不能凭 PID、端口、超时或文件锁空闲解锁。

### 同 boot 恢复实际验证

- 四组件 `cargo check ... --all-targets`：退出 0，`/tmp/rcoder-native-guardian-check2.log`。
- 四组件 nextest：**484/484，0 skip，退出 0**，`/tmp/rcoder-native-guardian-nextest.log`。新增实际 TS 退出叠加持久化失败保留 owner 锁、task/外部身份冲突反例。
- `cargo build -p file-server-proxy --bin file-server-proxy`：退出 0，`/tmp/rcoder-native-guardian-build.log`。
- `python3 crates/file-server-proxy/tools/test_native_guardian.py --binary target/debug/file-server-proxy --report /tmp/rcoder-native-guardian-contract3.json`：**7 项真实链路断言通过，退出 0**，报告含二进制 SHA-256 及独立 fixture 路径。该脚本限 Unix；本轮在 macOS 执行，不代替 Linux/Windows 验收。

真实二进制覆盖：实际 owner SIGKILL 的 Child wait 见证；TS 根及后代随 pipe EOF 清理；错误原实例 recover 拒绝、正确原实例恢复；旧实例迟到 stop 不停止新实例；实际内嵌 execute-command 在途崩溃后命令收束与 worker 中断登记；Pending 撤销后实际迟到 guardian binary 拒绝；guardian 和 owner 双崩溃后保持未知保护，即使本轮 fixture 自行结束也不凭观察信号放行。测试仅操作本轮 Popen 子树，原回执与日志保留，未删除未知状态。

恢复命令：`file-server-proxy recover --port <原请求端口> --instance-id <受保护原实例 UUID>`，使用原 host 与 `FILE_SERVER_PROXY_STATE_DIR`。原实例 UUID 来自原启动结果或 `owner.json` 的 `instance_id` 字段；不要输出该文件中的管理令牌。无真实 `OwnerExited`、guardian 自身清理未知、损坏/不兼容记录均明确失败，不能删除记录重试。

本轮 guardian 加入后的 Clippy、完整 feature 组合、Linux/Windows 宿主机验证仍待完成；此前 Clippy 结果只属于 guardian 加入前的阶段。

### 后续组件节点及显式退出接口

Pending 启动与停止竞态已经补齐：原 owner 已停止受理导致 guardian 在尚未跨 Running 时拒绝，须在原授权锁内写 Revoked，证明未 spawn。Running 未知不能走该撤销路径。execute-command 的工作/命令记录现关联 app_id。

该节点组件验证：default **486/486**（`/tmp/rcoder-native-guardian-nextest2.log`）、all-features **486/486**（`/tmp/rcoder-native-guardian-allfeatures.log`），均 0 skip、退出 0；all-features Clippy `-D warnings` 与 proxy no-default-features Clippy 均退出 0。这些结果发生在下面 retire/spec 摘要加入之前，不能用作新增接口的验证结果。

新增待验证接口：`retire --port <原端口> --instance-id <原实例>`。它要求原管理令牌与 instance 匹配、真实 supervisor 尚在，关闭受理后持久 `Stopping + retirement_requested`，尝试写出并冲刷 `RetirementAccepted` 后退出原 owner（退出码 75）。Accepted 不表示退出完成；CLI 必须看到同一 supervisor 对同一 owner 的实际 wait 见证，才返回 `OwnerExited`。响应丢失可用同一实例重试观察。它不写 Stopped、不把 worker 或外部业务操作改成功。随后显式 `recover` 才依据原 guardian 与退出见证收束。这为仍存活但本地工作结果未知的 owner 提供可执行通路，避免要求用户手动按 PID 杀进程。

Guardian 授权同时新增命令 spec SHA-256：父进程在 Pending 落盘摘要，guardian 对收到的完整 spec 重算，匹配后才进入 Running/spawn；只持久摘要，不保存命令参数或环境中的凭据。错摘要在未消费阶段持锁撤销，不能执行替换命令。

retire 与摘要校验当前已实现但尚未编译/实测；独立真实脚本已增加对应反例，等待集中验证。

> 2026-09-20 深夜补记：retire 与摘要校验已完成集中验证（四组件 nextest 488/488 默认与 all-features、两形态 Clippy、真实二进制脚本 10 项断言全过，macOS），证据见 `verification-2026-09-20.md` 尾部"retire/摘要校验集中验证"段。Linux/Windows 宿主机执行仍待验证。
