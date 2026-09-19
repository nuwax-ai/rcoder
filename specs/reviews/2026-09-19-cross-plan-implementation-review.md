# 近期方案实施复核：运行内核、存储关机、Dev 销毁与代理生命周期

## 1. 结论与审查边界

**当前实现仍有 9 项需要处理的问题，不能仅凭此前组件测试或某轮 Compose 通过宣布相关方案完成。** 最优先的是：Stop 被提前写成 Cancelled、排队执行丢失身份、Dev 销毁重复申请租约、存储和业务关机未可靠收束。

- 审查日期：2026-09-19。
- RCoder 源码基线：`97915c47b65113fe6905c46b377af53b503c31a3`。重点比较 `166c3579..97915c47` 的近期修复，并沿实际消费者追踪。
- 配套 `build-agent-docker` 抽查基线：`2aaad89`。本轮只检查部分配置、版本约束与说明，未重新构建镜像、渲染完整 Chart 或部署集群。
- 对照方案：`specs/userapp-runtime-ownership/`、`specs/native-desktop-runtime/`、`specs/userapp-turso-local-storage/`，以及近期批 8/批 9 交付记录；补充抽查错误类型改造、ServiceType 归一化、Pingap 构建入口。
- 本轮不修改业务代码、测试代码、运行配置或数据库，不执行远端操作。只新增本审查文档；临时验证程序放在系统临时目录。
- 工作区已有 Toasty 规划及其他未跟踪审查文档。审查末尾还观察到 `tests-e2e/tests/compose_userapp_deploy.rs` 出现未提交修改，未修改或覆盖该文件，也未将其视为本轮验证基线。
- 下文行号均对应上述源码基线；后续改动后应按函数名重新定位。

### 1.1 用户已确认的行为，不得反向修改

**`FILE_SERVER_PROXY_PUBLIC_BIND` 默认允许，不恢复默认限制。** RCoder 由上游项目调用，不直接作为面向公网的服务提供方；不能为了泛化的安全建议，使正常的跨项目调用必须再增加声明才能工作。

当前 `crates/file-server-proxy/src/config.rs:58` 的默认值与 `instance.rs:109` 的判断已经表达这一方向。本报告不将该默认值列为缺陷，不要求强制 token、强制 loopback 或强制设置环境变量。调用方显式提供配置时，仍应准确执行该配置。下文对代理 stop 的意见是生命周期正确性问题，与是否默认开放访问无关。

同样，未来 Toasty 迁移不改变操作身份、未知结果保护、事务原子性和关机顺序等业务约束；不能以即将更换数据库为理由绕开本报告发现的边界。

## 2. 问题总表

| 编号 | 优先级 | 问题 | 本轮证据 |
|---|---|---|---|
| CR01 | P1 | 排队操作派发后未接任 active，正常提交被判 NotActive | 直接引用当前 RuntimeKernel 源码的探针复现 |
| CR02 | P1 | Stop 被放入启动排队槽，提前 Cancelled，未知停止结果无法保护 | 当前 RuntimeKernel 探针复现 |
| CR03 | P1 | supersede 落盘失败后，新请求残留 Accepted，重试只重放 | 当前 RuntimeKernel + 真实临时文件故障注入 |
| CR04 | P2 | 排队操作从脱敏记录重建，PG 密码丢失 | 当前 RuntimeKernel 探针复现 |
| CR05 | P1 | Turso shutdown 关闭通道报错丢失 join；Notify 等待存在丢唤醒窗口 | 原方法提取探针 + Notify 时序探针；未开真实数据库 |
| CR06 | P1 | RCoder 关机门闸漏计未调度任务，且未封闭全部生产者 | 当前门闸源码探针 + 服务调用链核对 |
| CR07 | P1 | Dev destroy 在 K8s 自身租约冲突；Docker 落盘回执与执行租约不一致 | 双运行时调用链及租约恢复源码确认；未跑部署 E2E |
| CR08 | P2 | file-server-proxy stop 只等待监听循环，旧连接仍可继续处理请求 | 实例与连接任务生命周期源码确认 |
| CR09 | P2 | 事件序号重复，游标重放漏掉操作终态 | 当前 RuntimeKernel 探针复现 |

P1 表示正常控制流或故障恢复会被破坏，应先修复再验收；P2 也属于应修问题，但触发条件或影响范围较窄。

## 3. 逐项问题与修复建议

### CR01：派发排队请求时没有交接执行身份

**触发过程：** A 正在执行，B 被受理进入 `pending_restart`；A 成功后派发 B。

源码依据：

- `crates/app-cli/src/runtime_kernel.rs:1064` 的 `finish` 清空 `active_operation_id`，随后在 `1071–1077` 取出并派发 B，没有设置 `active_operation_id = B`。
- `runtime_kernel.rs:1206–1219` 的 `write_terminal_locked` 重复了同一缺口，两条终态路径都要修。
- `runtime_kernel.rs:1260–1263` 的 `commit_execution` 要求传入 ID 等于 active；`1304–1307` 的取消入口也依赖此条件；`1463–1468` 的事件桥接在没有 active 时直接丢弃事件。
- `crates/app-cli/src/server.rs:1983–2009` 会将未终态的 NotActive 操作转为 RecoveryRequired。
- 还有第二层身份交接：`server.rs:1605` 的 dispatch 回调尝试设置执行 ID 时，A 的 server 执行槽尚未在 `1971–1975` 清空；`324–338` 又禁止覆盖非空槽。不能只补一个内核字段就结束检查。

本轮探针：`admit(A) → admit(B) → finish(A, Succeeded) → commit_execution(B)`，实际 `active=None`、`commit_b=NotActive`。

**影响：** B 虽已发往执行队列，却没有执行者身份；其取消、事件与提交异常。同时内核可能把新请求 C 当作空闲请求受理，破坏串行执行约束。

**建议修复：**

1. 合并两处“收束当前操作并提升排队操作”的状态变更，确保在同一 admission 临界区内原子完成 A→B 交接。
2. 在派发前确认 B 仍是可执行的非终态记录，并安装 B 的 active 身份；加载/派发失败要保留可恢复记录，不能落入无 active 的假空闲状态。
3. 将 server 执行身份与真正消费动作的边界绑定，或显式增加带 ID 的交接协议；不能靠 dispatch 回调覆盖仍在执行的 A。
4. 取消、事件和终态都使用显式操作 ID；避免从“最近一次请求”推测正在执行者。

**必须补的反例：** A 成功后 B 能真实执行并提交 Succeeded；B 执行期间 C 只能排队；B 的取消与事件属于 B；Source 和 Artifact 两个派发分支都覆盖。现有只断言 dispatch 列表含 B 的测试不足以证明这些条件。

### CR02：Stop 错占 pending_restart，导致停止结果被提前终态化

源码依据：

- `runtime_kernel.rs:921–925` 注释写“清空排队启动”，实际使用 `pending_restart.replace(stop_id)`，把 Stop 本身放进启动排队槽。
- `runtime_kernel.rs:1086–1090` / `1226–1230` 在 active 失败、取消或未知时沉降排队者；`1140–1155` 不区分 kind，直接将该槽中的操作写成 Cancelled。
- `runtime_kernel.rs:1114–1117` 拒绝覆盖已有终态。因此 Stop 后续实际停止失败时，要求写 RecoveryRequired 也会返回原 Cancelled；`1061–1062` 不会开启恢复保护。
- 这不是只能手工构造的状态：`server.rs:2279–2305` 在启动被 Stop 意图取代且旧业务清理完成后，确实把 A 收束为 Cancelled；Stop 自身随后由 `2101–2146` 处理。

本轮探针：A 启动期间受理 Stop S，先 `finish(A, Cancelled)`。S 在执行前已变为 Cancelled；随后 `finish(S, RecoveryRequired)`，仍然是 Cancelled，`recovery_protection=false`。

**影响：** 停止操作的最终结果不真实，且“停止结果未知必须保持写保护”的要求被绕过。

**建议修复：** Stop 受理只 `take()` 启动排队槽，取消真正被替代的 Start/Restart；Stop 自身只进入 `pending_stop`。排队沉降和派发都验证 kind/状态。S 的终态只能由 S 的停止结果决定，不能被 A 的终态连带覆盖。不要通过允许随意改写已有终态来掩盖错误的提前终态化。

**必须补的反例：** A 被 Stop 取代后，S 仍可从 Accepted 进入 Succeeded；S 清理失败则为 RecoveryRequired 并阻断后续写；A Failed/Cancelled/RecoveryRequired 分别覆盖。另测空闲时 Stop 完成后再 Start，不得从残留启动排队槽再次派发旧 Stop。

### CR03：新受理记录已落盘，旧排队者落盘失败却按普通拒绝返回

源码依据：

- `runtime_kernel.rs:872–893` 先写新请求 C 的 Accepted，`894–919` 再写 desired。
- `runtime_kernel.rs:948–967` 若取消旧排队者 B 的写入失败，只恢复内存中的 B，然后返回 ERR_BACKEND_ERROR；没有收束 C，也没有开启恢复保护。
- Stop 分支 `921–943` 有相同问题，且此时可能已持久化 Stopped 并推进 revision。
- 空闲分支 `973` 的滞留排队者沉降错误，同样发生在 C 已落盘之后。
- `runtime_kernel.rs:795–816` 对同一 C 的重试优先重放原记录，不再派发。现有部分提交处理 `1319–1344` 只在前面的 desired 写失败路径使用，未覆盖上述情况。

本轮探针先受理 A/B，再将 B 的临时落盘路径建成目录以注入真实文件写失败，随后受理 C。结果：调用报错，但 C 留为 Accepted，恢复保护为 false，同 ID 重试仅返回 `Replayed(Accepted)`。

**建议修复：**

1. 在首次持久写之前完成所有可提前完成的校验。
2. 把“写新请求、改 desired、替代旧排队者、更新执行槽”作为一个明确的受理提交协议；可采用可恢复的 admission journal，或对所有部分提交分支统一进入持久 RecoveryRequired 与内存保护。
3. 不要简单删除 C：desired、revision 或 B 的状态可能已发生变化，必须有明确补偿/恢复顺序。
4. API 应能告诉调用方这次请求已经部分持久化及其真实 operation_id；不能表现为“完全未受理”。

**必须补的反例：** 新 Accepted 写、desired 写、旧 B 终态写、排队加载分别故障；重试和重启后均不能出现永久 Accepted、丢失已受理意图或无保护地继续写。Start/Restart 与 Stop 两类都要覆盖。

### CR04：排队请求使用脱敏后的 PG 密码执行

源码依据：

- `runtime_kernel.rs:411–423` 的 `store_operation` 克隆请求并清空 `run_config.pg.password` 后落盘，这是合理的敏感信息处理。
- 立即执行使用尚在内存的原 `stored`，而排队请求在 `1073` / `1215` 从文件重新加载。
- `runtime_kernel.rs:1393–1400` 直接把重新加载记录中的 PG 凭据交给 Source 编排。

本轮探针给 B 提交非空合成密码，A 成功后捕获 B 的派发参数，密码长度为 0。日志未打印密码内容。

**建议修复：** 同一 owner 存活期间，给排队请求保留不落盘的执行参数，按 operation_id 绑定，替代/取消/终态时清理；持久记录继续脱敏。owner 崩溃后不能将脱敏记录冒充完整可执行命令，需要沿已批准的恢复协议处理或明确要求重新提供凭据。不要以明文持久化密码作为修复。

**必须补的反例：** 立即执行和排队执行收到相同凭据；落盘仍无明文；B 被 C 替代后不复用 B 的凭据；重启不以空密码自动重放 B。对应原生矩阵 NT11。

### CR05：Turso shutdown 的错误路径与通知时序仍可能永久等待

源码依据均在 `crates/rcoder-storage/src/userapp_lifecycle/turso/mod.rs`。

**缺口 A：取走 join 后发送失败，未发布关闭结果。** `286–291` 先 `guard.take()`，再 `shutdown.send(true)?`。worker 已异常结束且 receiver 已关闭时，发送返回错误；join 句柄被丢弃，没有启动 join 任务，也没有设置 outcome。下一次 shutdown 看不到 join，也等不到 outcome。

本轮提取原 `WorkerHandle` 和原 shutdown 方法，保留其控制流，用已经异常退出的测试线程替代数据库 worker：第一次返回 `shutdown channel closed`，第二次在 50ms 限定内无法完成。此证据验证的是该方法，不等于真实数据库集成测试。

**缺口 B：检查结果与创建等待 future 之间丢唤醒。** `309–319` 先检查 outcome，再创建 `notified()`；另一个线程可恰好在这之间执行 `301–304`，设置 outcome 并 `notify_waiters()`。该通知不会为尚未创建的 waiter 保存许可，等待者随后睡眠且不再检查结果。同步构造该时序的 Notify 探针已复现。

**建议修复：**

1. stop signal 发送失败也必须继续收束已取走的 join，发布唯一共享结果，区分线程 panic 与通知通道已关闭；不能让 `?` 跳过收束。
2. 使用携带完成状态的 watch / 共享完成 future，或正确安排 waiter 注册和条件复查，避免“先看条件、后创建 waiter”。
3. 关闭动作继续独立于任何调用方 future；所有调用者应拿到同一逻辑结果。目录锁仍覆盖数据库连接和 worker 的完整生命周期。
4. 顺带修正 `267` 的 `worker dropped reply without executing` 表述：reply 丢失不能证明操作未执行，可能已写入后异常。不得据此授权重放。

**必须补的反例：** worker panic 后连续两次及并发 shutdown 都有限完成且返回错误；正常关闭结果发布恰好发生在检查/等待之间时不挂起；取消首个 shutdown 调用不影响后续等待；关闭完成后才允许重新独占打开目录。

### CR06：RCoder 的关机门闸没有封闭受理，且漏计 spawn 尚未开始的任务

源码依据：

- `crates/rcoder/src/userapp_builder/shutdown_gate.rs:18–20` 只有计数，没有 Closing 状态；`33–37` 在任何时刻都允许增加；`46–50` 看到 0 即宣布排空。
- `userapp_builder/creation.rs:91–94`、`control.rs:25–29` 在 `tokio::spawn` 内才拿 guard。任务已创建但尚未 poll 时，不在计数中。
- `crates/rcoder/src/server.rs:28–40` 的关闭只退出 accept；连接任务在 `40–72` 独立 spawn，无连接 drain，已建立的 keep-alive 连接和正在处理的 handler 仍可能进入业务。
- `crates/rcoder/src/shutdown.rs:120–149` 等待恢复任务及该 gate 后就关 store，gate 没覆盖完整 HTTP/Prod 操作链；超时分支直接继续关库。
- `crates/rcoder/src/main.rs:362–375` 在上述关机处理返回后才停止 Pingora 代理，其可能触发的业务入口也需纳入关闭边界。

本轮使用原 gate 源码，在 current-thread runtime 中 spawn 一个内部才获取 guard 的任务，然后立即 `wait_idle`：返回 0，而该任务尚未完成。这准确复现当前调用方的注册窗口。

**影响：** “数据库队列已排空”不等于业务不再产生数据库操作；容器调用完成后可能因库已关闭而无法提交终态。新增 gate 尚未实现声明的“先停生产者，再排空业务，最后关库”。

**建议修复：**

1. 统一关闭状态与受理计数；开始关闭和获取执行令牌必须互斥，Closing 后拒绝新业务受理。
2. 在 spawn 之前取得 guard 并移入任务；同时覆盖已受理的内联 Prod 操作、恢复任务和代理可触发的创建链，不能只数 builder 的两个 spawn 点。
3. 停止新连接并让已有连接进入 graceful shutdown，不允许 keep-alive 继续提交新操作；等待已经受理的请求/独立协调任务完成。
4. 使用统一关机 deadline。预算耗尽时明确记录并保留未确认操作的恢复身份，禁止再启动新工作；不能仅写日志后任由未受控任务继续使用已关闭 store。
5. 不得为关机强行把未知物理操作写成 Failed 或释放其保护租约。

**必须补的反例：** spawn 尚未 poll 即收到 SIGTERM；持有 keep-alive 后关机并发新请求；Prod 部署等待远端结果时关机；恢复扫描器退出超时；跨远端副作用和终态落盘的暂停点。既测组件时序，也做真实 Compose SIGTERM，PG/K8s 路径单独验证。

### CR07：Dev destroy 的文件锁交接没有形成跨运行时的一次性租约交接

这是 `2786632a` 修复同进程 flock 冲突后仍存在的两部分问题。

**K8s 确定性自冲突：**

1. `crates/app_manager/src/service/operation_lock.rs:164–175` 为 Dev 操作获取 builder-family 租约。
2. `crates/app_manager/src/ops/storage.rs:614–624` 绑定该回执，调用 hand_over，然后 `capture_dev_deletion`。
3. `operation_lock.rs:88–91` 的 hand_over 只清 `_file`，并没有转移 K8s `runtime` 租约。
4. `crates/rcoder/src/userapp_builder/dev_cleanup.rs:125–133` 再次申请 builder operation。
5. 外层 `crates/docker_manager/src/runtime/k8s_userapp_impl.rs:44–49` 与内层 `kubernetes_runtime.rs:212–217` 最终都调用 `acquire_application_operation(app_id, UserappBuilder)`。
6. `k8s_app_operation.rs:156,189–226` 使用相同 ConfigMap 名称，通过 create 排他；第二次 create 返回 409，不是可重入锁。

因此在执行到实际内层捕获的正常前提下，Dev destroy 会被自己持有的租约拒绝。该结论来自实际调用链，本轮未连接集群复现。

**Docker 回执身份不一致：**

- `operation_lock.rs:21–38,264` 把外层 marker A 的 token/device/inode 作为 durable receipt；`storage.rs:614` 先持久化它。
- hand_over 释放外层 fd；内层 `docker_builder_deletion.rs:63–70,331–351` 创建 marker B 并写入同一锁文件。
- `dev_cleanup.rs:250–262` 的删除回执保存资源快照与 registry 身份，没有把内层 B 的租约回执替换进控制存储。
- 清理成功时 `dev_cleanup.rs:329–330` 正常释放 B，成功路径可通过；中途失败留下 B 时，恢复仍用 A。`docker_builder_deletion.rs:406–410` 按 token 检查并拒绝清除另一所有者的 marker。

**建议修复：** 将本次操作实际持有的租约作为有类型的执行上下文传给删除器，避免第二次 acquire。外层与内层共同使用同一物理租约、同一 durable receipt，并明确唯一 release 责任。确需两把不同资源锁时，分别记录每把租约及恢复责任；不要把释放再重新获取称为无缝交接。

不要通过删除现存 ConfigMap、放宽 marker 检查、仅把 K8s lease 提前 drop 再重抢来“修好”冲突，这会改变多副本互斥及未知写保护。Dev/Prod scope 仍保持分离。

**必须补的反例：** 同一 Dev destroy 在 Docker 与 K8s 都只取得一次相应物理租约；真实执行成功；获取/捕获/删除/终态提交各边界失败后，durable receipt 能验证当前租约；另一副本无法在交接过程中进入；恢复不清理后来实例。现有 Fake runtime 若每次 acquire 都成功，会掩盖本缺陷。

### CR08：file-server-proxy 停止后旧连接任务仍存活

源码依据：

- `crates/file-server-proxy/src/proxy.rs:82–123` 收到取消信号只退出 accept；每个连接在 `97–113` 单独 spawn，没有收到该取消信号，也没有被主任务 join。
- `crates/file-server-proxy/src/instance.rs:183–204` 取出全局实例后，等待的只是 listener 所在任务，随后返回停止成功。

**影响：** 监听端口可以重新绑定，但已有 keep-alive/流式连接仍可能使用旧配置转发后续请求；嵌入式宿主持续存活时尤为明显。第二个并发 stop 还可能在第一次停止尚未完成时看到 None 并提前成功。端口释放不能证明代理实例已经停止。

**建议修复：** 实例持有连接任务集合及共享 Stopping 完成状态；关闭受理后对连接发 graceful shutdown，拒绝新的 keep-alive 请求，并有界等待已受理请求结束。必要的强制断开应明确体现在 stop 结果中。并发 stop 等待同一结果，start 按实际停止完成状态处理。代理关停不应顺带停止独立 app-cli owner 的业务。

**必须补的反例：** 保持一条真实 HTTP keep-alive，stop 返回后不能再沿旧连接完成新的转发；长响应按约定完成或有界中止；两个 stop 共享完成结果；同端口重启后的请求只进入新实例；app-cli 业务不被 proxy stop 误停。

本项不改变默认 public bind，也不增加强制 token 前置条件。

### CR09：事件序号计算与排队事件固定序号破坏游标重放

源码依据：

- `runtime_kernel.rs:443–464` 的 `replay_events(id, after_seq)` 明确只返回 `sequence > after_seq`。
- `runtime_kernel.rs:1254–1258` 和 `server.rs:363–369` 却用 `after_seq=u64::MAX` 获取记录数量，再计算下一序号。这必然读到空集；`.max(2)` 使终态固定使用 2。
- 排队派发/替代在 `runtime_kernel.rs:1076,1157,1218` 又固定使用 1，与 accepted 重复。
- 编排事件在 `runtime_kernel.rs:1470–1474` 采用读历史最大值再加 1，但读与追加不是统一的原子序号分配；只替换上述 MAX 参数仍不足以覆盖并发事件写入。
- 消费端 `crates/app-cli/src/api/runtime.rs:279,350` 使用同样的 after_seq 语义，客户端已推进的游标不会倒退读取终态。

本轮探针：Accepted(1) → build(2) → ready(3) → commit；实际重放得到 `(1,accepted),(2,build),(2,terminal),(3,ready)`，查询 `after_seq=3` 返回空。

**建议修复：** 将每操作事件序号分配与追加收口到一个串行、可恢复的写入入口；accepted、dispatch、superseded、服务事件、terminal 全部使用它。序号按持久最大值推进，不以筛选后的条数计算。持久化失败时保留可诊断状态，不伪造成功事件；幂等 finish 不重复产生新的相同终态事件。注意锁序，不能新增 admission 与事件锁反向持有。

**必须补的反例：** 至少两条服务事件后成功/失败终态仍能由最后游标续读；排队替代/派发序号严格递增；并发追加不重复；重启后续写不回退；重复 finish 不重复通知；通过真实 HTTP 重放接口核对，而不只手工插入固定序号测试查询 helper。

## 4. 已有修复与完成度校正

| 主题 | 本轮结论 |
|---|---|
| Pingap 配对版本 | 已执行 `python3 k8s/scripts/pingap_version_gate.py`，退出码 0；四个脚本入口均为 0.14.3 / cd74a461a3e7。只证明构建声明一致，不证明已运行镜像中的二进制一致。 |
| Prod 锁冲突结构化 blocker | `crates/app_manager/src/service/mod.rs:178–243` 已加入持久 blocker 查询和 admission 窗口表达，不能再沿用“该进程锁分支始终 data=null”的旧结论。本轮未重新跑接口 E2E。 |
| app-cli 失败后排队者沉降 | 新分支已经存在，但 CR01–CR04/CR09 表明排队协议仍未闭环，不能标记整体完成。 |
| Turso worker 持锁与退出 | worker 持有目录锁、watch 退出等已有修复；CR05/CR06 是仍需处理的关机缺口，不能因旧缺陷修过就略过。 |
| 错误字符串改造 | 抽查 DownloadError 已携带状态码，AgentDownloadError 复用 Download 类型；本轮没有重新逐项验收该方案全部八项及所有调用者。 |
| Dev/Prod 分域与物理类型归一化 | 近期已改 ServiceType/registry/cache 等路径；CR07 是分域后的资源销毁执行链缺口，不应退回 app_id 单槽来解决。 |
| N07 与默认访问行为 | 当前代理请求层已有可选 token 校验；“独立入口完全没有令牌校验”的旧描述不能继续作为现状。默认 public bind 保持用户确认的允许行为，不列为问题。 |
| 三平台原生支持 | `specs/native-desktop-runtime/tasks.md` 的 ND/NT 完整验收仍未闭环；源码存在不等于 Windows/macOS/Linux 无容器实机均通过。本轮不发布三平台通过结论。 |
| Toasty 与首版表结构 | `specs/toasty-storage-unification/` 是另行编写的规划，本轮不能把它当作已实现。迁移后仍需保留本报告要求的业务契约测试。 |

批 9 报告 `specs/userapp-turso-local-storage/reviews/2026-09-19-batch9-fixes.md` 的基线是 `c46a2b94`，其 Compose 记录为 **41/43**，并明确遗留 SIGTERM、remote K8s、原生完整矩阵等项目。本轮不改写历史记录，也不把后续提交的存在推导为这些测试已经重新通过。

配套仓库还有一项文档偏差：`build-agent-docker/docker/TURSO.md:23` 仍称旧 `userapp.sqlite3` 会被忽略，而当前 RCoder 在“新库不存在、旧库存在”时会 fail-fast（`turso/mod.rs:95–101`）。后续同步修正文案，说明使用独立目录及实际拒绝条件；本轮不修改该仓库。

## 5. 本轮实际验证及其限度

### 5.1 临时源码探针

为不与现有 Cargo 任务争用构建目录，使用 `rustc` 链接已有依赖缓存，临时程序通过 `#[path]` 引用当前 `runtime_kernel.rs` 和 `shutdown_gate.rs`。只在系统临时目录写文件，没有改动仓库测试或生产代码，也没有运行真实容器。

- 临时目录：`/var/folders/y6/g5lk3d750833hz_rn5h3y6nh0000gn/T/rcoder-review-20260919-dc57o7pt/`。
- 完整 rustc argv：该目录 `compile-command.json`；程序 `probe.rs`；最终编译日志 `compile-final.log`；最终运行日志 `result-final.log`。
- RuntimeKernel 源文件 SHA-256：`28c3f659bd709a6426af6c85d80a2f2536a4a9e507b650cd744f4f4be7c43a47`。
- 最终编译退出码 **0**；探针程序退出码 **1**，表示成功暴露缺陷，绝不是产品测试通过。最初探针搭建出现依赖缓存版本不配对，调整为同一组 rlib 后编译通过，该搭建错误不计为项目缺陷。

持久保留的关键结果：

```text
queued promotion: active=None; commit_b=NotActive
stop isolation: before_stop_execution=Cancelled; after_unknown=Cancelled; protected=false
queued credential: dispatched_password_length=Some(0)
partial admission: rejected=true; c_state=Some(Accepted); protection=false; retry=Replayed(Accepted)
terminal event replay: [(1,accepted),(2,build),(2,terminal),(3,ready)]; replay_after_3_count=0
gate spawn gap: drain_remaining=0; spawned_job_finished=false
shutdown Notify mechanism: completed_now=true; next_wait_timed_out=true
extracted shutdown method: first_error=shutdown channel closed; second_call_timeout=true
confirmed failing invariants=8/8
probe_exit=1
```

这些是 **5 个真实 RuntimeKernel 源码反例、1 个原门闸调用时序反例、2 个 shutdown 控制流/通知机制反例**。后两项未打开 Turso 数据库；CR07/CR08 为源码分析。临时目录会被系统清理，正式修复应将上述最小场景纳入仓库测试，不能依赖临时程序永久存在。

### 5.2 未执行项

本轮未执行 workspace/app-cli 全量 nextest、Clippy、Compose、remote K8s、三平台实机、镜像构建或 npm 发布。未访问或清理个人集群 PG。已有交付报告的通过数量只作为历史证据，不算本轮结果。

## 6. 推荐修复顺序与验收任务

1. **先修 CR02、CR01、CR03**：统一受理、队列、Stop 与执行身份的状态变更。随后在同一模块补 CR04、CR09，避免重复调整队列与事件设计。先证明修复前的反例，再证明真实 serve 控制链通过。
2. **独立修 CR07**：设计一次性租约传递及 durable receipt，Docker/K8s 共用契约；不要继续逐分支 drop 锁规避冲突。
3. **修 CR05、CR06**：先确保 store.shutdown 必然发布共享结果，再封闭业务受理并真实排空。若 Toasty 迁移同时进行，把关机契约移到新的 store 实现与公共装配层，不丢弃测试。
4. **修 CR08**：代理连接任务与实例 stop 统一收束；保留默认 public bind 允许。
5. **最后做组合回归并更新完成度**：每项注明修复提交、反例和部署证据，历史报告保留原基线。

### 6.1 组件验证

按改动范围先聚焦 nextest，再执行受影响的默认/all-features 组合；Cargo 任务串行使用各自 target 目录。

```bash
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features
cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast
cargo nextest run -p app_manager -p docker_manager -p rcoder -p file-server-proxy --no-fail-fast
cargo nextest run -p app_manager -p docker_manager -p rcoder -p file-server-proxy --all-features --no-fail-fast
```

随后按 AGENTS.md 完成受影响 fmt/Clippy 和交付范围对应的全 workspace 检查；app-cli 独立检查不能遗漏。测试中真实 PG/镜像等必要前置缺失时应记录受阻，不将 skip 或空筛选当通过。

### 6.2 集成验证

- Compose：补并发 Start/Restart/Stop、排队凭据、终态游标、Dev destroy 失败恢复、SIGTERM 与 proxy keep-alive 停止场景；重新回归原 `compose_regression` 和 `deploy_full_chain`，取得当前提交对应的完整结果。
- K8s：经配置化 `remote-k8s` 工作流验证当前源码；重点覆盖 Dev destroy、多副本排他、物理 UID/租约恢复、关机与在途操作。`smoke` 不能替代 UserApp 业务用例。先部署一次已确认快照，再用 `remote-k8s-test` 追加对应套件，测试期间不替换部署。
- 原生：R02 队列、凭据、事件和代理停止修复纳入 NT03/08/09/11/12 等场景；Windows/macOS/Linux 使用无容器宿主机形态分别验收。只要求组件与业务边界，不扩大到 Electron 项目开发。
- 发布：本审查不授权额外发版；后续发布任务按用户安排执行，组件通过、部署通过、npm/镜像发布分别报告。

### 6.3 修复交付必须回答

每项 CR 编号分别提供：是否确认、最终改动位置、修复前反例、修复后结果、真实集成覆盖或未运行原因。若判定某项不成立，应给出完整调用链或反例证据，不能仅以现有测试总数全绿否定。

仍需保留的设计原则：结果未知不能当作未执行；Stop 的受理不能替代停止完成；观察缓存不能替代物理身份；释放端口不能替代任务收束；ORM/数据库替换不能削弱这些约束。
