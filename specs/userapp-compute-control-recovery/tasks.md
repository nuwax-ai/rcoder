# 实施任务

- [x] T1 增量 schema 迁移，旧 baseline 无损升级（Turso 与独立 PG v1 升级反例通过）
- [ ] T2 存储原子控制受理、执行代次、停止意图、幂等及优先级
- [ ] T3 执行者撤销与在途运行时写核验，旧结果隔离
- [ ] T4 dev/prod stop/restart 协调器与后台恢复
- [ ] T5 现存资源发现与自动恢复登记
- [ ] T6 HTTP 202、操作查询、恢复入口、OpenAPI、Java 接入说明
- [ ] T7 默认/全 features nextest、Clippy、受影响独立项目检查
- [ ] T8 Compose、真实 PG、多副本 K8s、数据保留验证
- [ ] T9 发布验证后的恢复入口收尾 app 129，真实聊天及 stop/restart

当前整体未验收。T1 已实现并通过 Turso/PG 存量 v1 升级验证。T2 已实现受理、认领、冲突、代次、独立回执和终态状态转换，运行时证据仍需协调器接入。T3 已接入部分真实写前边界，远端在途写与所有复合写阶段仍未完成。T4 仅完成控制终态回执清理扫描，T5–T6 尚未交付；T7 仅完成本轮受影响模块验证；T8 仅完成存储真实 PG，T9 未执行。不得把基础设施变更或局部测试视为业务功能完成。

## 2026-09-20 继续实施

- T2：新增独立控制租约回执、进度/失败诊断、阶段顺序及终态提交；Stop 完成后的新 Restart 通过正式存储接口验证，不再用直接修改状态的测试 fixture。
- T3：新增被 Stop 取代的执行者确认收束接口；只有原 executor 可提交，不能变成成功；未知写继续保护。尚需实际运行时发出收束证明。
- T4：新增控制终态租约后台扫描，已用真实 Turso 与文件租约适配器验证；Pending/Running/RecoveryRequired/Superseded 不自动释放。dev/prod 控制执行器仍未交付。
- T1/T8：独立 PG v1→v2 升级、双连接竞争/终态/回执清理通过；未使用业务 PG。Compose/K8s 业务验收仍未执行。

下一步必须完成：普通被中断任务的收束、远端在途写核验、控制执行器、202/状态/恢复接口、自动登记恢复及 E2E。不得把上述账本和清理扫描称为高优先级 Stop/Restart 已上线。


## 2026-09-20：手动 owner 停止与 Pending 收束

- [x] file-server 无登记 owner 发现、Stop 专用真实路径归属核验、凭据恢复及终态确认。
- [x] 旧版工作区内 `.local-deploy` 无记录可停止；外部构建目录由 app-cli 写明确来源记录，锁/token/journal 不迁移。
- [x] 修正 owner 服务已停却返回 No running process found 的消息。
- [x] 认领控制操作时原子收束从未获执行权的 Pending 业务操作；Turso 与独立 PG 双连接验证。
- [ ] Compose/K8s 实际手动启动→dev/stop→再次启动验收；本轮未升级线上镜像。

上述 dev/stop 是容器内应用服务控制，不代表 T4 的容器控制执行器已完成。T3 的 Running/RecoveryRequired 运行时核验、T4–T6 和 T8–T9 仍按前文推进。详细设计见 [dev owner 修复](dev-owner-stop-2026-09-20.md)。

## 本轮 Qoder 复核与 Compose 证据

手动 `.local-deploy` owner → RCoder dev/stop 已真实复现修复前失败、修复后通过；容器、owner 与工作区保留。尚未完成跨目录再次启动及 K8s 验收，因此上方组合任务继续保持未完成。Qoder 的恢复入口 Running 边界与验收代码遗漏已修正，完整部署链仍有业务恢复失败。详见 [本轮复核报告](qoder-review-and-compose-2026-09-20.md)。

## 2026-09-20：优先完成生产调用链

按用户要求暂停新增测试代码，先接生产逻辑。本节覆盖前文的实现进度，不覆盖历史验证结论。

- T3：新增已持久化最终副作用回执的旧业务收束。核对控制身份、旧记录全量快照及最终证据后，原子记录 Failed/COMPUTE_INTERRUPTED，保留回执；不允许旧任务重报成功。普通恢复扫描也不能重新认领被控制操作中断的旧任务。
- T4：新增脱离 HTTP 生命周期的 dev/prod 控制执行器，Pending 后台发现、独立租约、先停后起、K8s 旧 Pod 退出确认。Stop 覆盖 Restart 时，原执行者在已完成本次运行时调用后提交收束证明。尚未覆盖所有未知远端写恢复。
- T4：显式启动与被动访问分开；Stop 进行中 Start/Restart 冲突，Stop 完成后显式启动允许恢复。prod 停止意图禁止流量唤醒。
- T6：UserApp stop/restart 接入 202，新增操作查询；新增仅用于尚未获得运行时租约、仍处于 draining_previous 的原操作恢复入口。具有运行时写入证据的恢复阶段尚需实现，不算完整恢复交付。

当前剩余生产逻辑：未知在途写的分阶段核验与恢复、已持有租约的控制操作崩溃恢复、prod 控制器缺失时的幂等 Stop（需排除孤立 Pod）、自动恢复旧 dev/prod 登记、app-cli 中断编排的 journal 恢复、显式热部署 PG 对齐与 Ready 等待顺序、手动 owner 跨目录切换。测试代码后置，不能因此把这些逻辑标为完成。

## 2026-09-20：已确认计算操作的恢复

- T4：`Stop/stopped` 与 `Restart/verifying` 增加终态补交，不重放容器写。保留原 executor、generation、operation 与 checkpoint；全记录 CAS 拒绝旧观察、并发 Stop 或记录更新。
- prod Restart 终态补交前读取当前物理身份、就绪状态，再次核对同一 UID/resourceVersion；dev 的 verifying 仅在原执行者确认 file-server 就绪后写入。
- 202 恢复入口及后台发现共用补交流程，原执行者和恢复任务争用时只有一方提交。已终结租约释放失败留给扫描器，不把确认完成改回未知。
- 控制执行器直接清理已经终结的普通/控制操作租约，避免全部恢复槽位被等待旧租约的控制任务占满。
- 控制执行器捕获 panic 并尝试保存恢复阶段；开发容器确认就绪后再核对执行身份，避免过期 Restart 发布登记。

本轮仍未允许 stopping/starting 等未知写阶段自动补交；这些阶段需要运行时写入回执或明确排除迟到写的协议。自动登记恢复与 app-cli 中断恢复仍未实现。没有新增测试用例。

## 2026-09-20：原生命周期发现与缺失根恢复

- 新增共享 `UserAppDiscoveredIdentity` 契约，Docker/K8s 实现只读发现。K8s 同时枚举 dev STS 与 prod Deployment，读取原生命周期并经现有捕获接口核对物理 UID；Docker 检查两个受管理容器并复核生命周期与容器 ID。
- 两个作用域生命周期不一致、同作用域多个控制器、删除中资源、身份字段缺失均明确报错，不随意选择资源。
- 存储在同事务恢复缺失 root 与停止意图；用插入结果区分创建胜者，其他副本不能用过期观察追加停止标记。已有 root、墓碑和不同生命周期不覆盖，不创建历史成功操作。
- dev ensure/probed ensure、计算 Stop/Restart、prod 显式启动和部署接入共享发现流程。停止状态在被动访问前恢复，显式启动按现有规则处理。
- 后台容器/PVC 清单仅用于发现 appId；每个 app 再通过相同流程实时核对后登记。普通控制任务优先，发现任务每轮最多两个。

T5 尚未全部完成：缺少生命周期标记的历史资源绑定、仅失败 ensure 新根与旧身份的原子归并、PVC UID 持久见证仍需补全。未知运行时写恢复、app-cli 中断恢复、热部署凭据顺序、跨目录 owner 切换等原任务继续保留。

## 2026-09-20：app-cli journal 边界修正

- 恢复已确认制品时不再写 Switching：没有目录交换，保留原 Active/Preparing 等证据。数据库迁移仍由独立迁移 journal 判断，不因为普通启动而重复执行已确认迁移。
- 热部署准备阶段不提前写 Switching；仅在执行循环确认停服、进入 activate 前写入。准备完成后遇到 Stop 不再制造虚假的切换中断记录。
- 原历史 Switching/Failed 没有额外证据时不删除、不自动改写。启动编排中控制信号即时消费和旧失败记录的分阶段恢复仍须继续完成。

## 2026-09-20：builtin 启动期间的控制消费

- builtin 在等待 Running 通知期间同时消费控制信号，不再只能等待服务 Ready 后处理 Stop。
- 先取消并 join 原编排任务，再按原启动 ID 收束 Cancelled；Stop 保留自己的操作 ID，由既有 StopBusiness 分支确认停止完成。
- 进程关闭未知或任务 panic 保持恢复；若进程已确认停止但迁移结果未知，原启动记录 RecoveryRequired，Stop 仍可确认计算停止，新的编排不能越过迁移保护。
- Running 路径释放启动期 control 接收锁后再进入原等待环，避免同一 mutex 重入。

supervisord 启动阶段的控制消费尚未补齐；历史未知写核验等整体剩余项不变。本轮未新增测试。

## 2026-09-20：supervisord 启动期协作取消

- supervisord 编排接入取消 token。启动期收到控制信号时停止后续步骤，等待当前编排返回后再 stop_all；没有丢弃正在执行的 RPC future。
- PG 等待、pingap hash 及 bridge 就绪等待可取消；迁移命令通过受管进程树取消并确认退出，未确认数据库结果保留原迁移回执。
- 原启动和 Stop 使用与 builtin 相同的分离收尾逻辑。迁移未知不伪装成取消成功，计算已停仍可单独确认 Stop。
- RPC 变更调用区分明确 fault 和响应不明；响应不明使用结构化 ShutdownUnconfirmed，不能在确认关闭前发布可恢复终态。关机也保留该保护。
- 迁移进程树关闭未知统一使用结构化错误，移除重复的字符串式关闭判断。

本轮没有新增测试。下一步仍需完成跨目录 owner 切换、热部署凭据顺序、历史资源绑定与控制器未知写恢复；编译通过不替代这些剩余实现。

## 2026-09-20：已记录来源的本地构建目录复用 owner

- 平台构建前观察、启动提交和恢复重放按已核验 owner 的 source_root 查 token，不再假定 token 位于调用方源码目录对应的状态根。
- app-cli 在初始化时固定执行项目来源；Source 和本地 ArtifactId 分别选择该项目源码目录、builds 制品与 .run 激活目录。锁、journal、token 和 workspace_id 保留原 owner 身份。
- 平台仅在来源相同且 owner 声明 project-origin-execution 能力时允许跨目录启动。没有来源记录的任意子目录仍不作为源码别名；Stop 的既有工作区内控制语义不变。
- owner 在发送可能停服的控制信号前核验来源，执行准备及恢复时再次检查。来源损坏或与初始化时不同则报错，不能静默切换项目。
- prod Restart 的 Ready 观察增加前后物理资源一致性核验，提交前再核对当前计算执行者。

尚未完成：无来源记录的历史构建目录启动接管；热部署凭据在业务就绪前应用及完整回执链；历史无生命周期资源绑定、失败新根与旧资源归并、PVC UID 持久见证、未知远端写核验。未新增测试。

## 2026-09-20：热部署 PG 顺序与操作边界

- 热部署拆分只读能力预检与提交执行。带 PG 的热部署先确认 app-cli 支持 deployment_run_pg，再应用数据库凭据并进行 TCP 验证，之后才发送部署请求、等待业务 Ready。
- app-cli `/v1/deploy` 接收可选 pg，先验证角色与密码格式，将凭据交给既有迁移/builtin/supervisord 配置通道；不在普通状态响应或部署 journal 请求中写出密码。
- hot_execution 检查点嵌套保留此前数据库证据。从此阶段开始，数据库专用恢复不能把整个部署当成已完成，部署重放也不能重新发起未知写。
- 冷部署及控制启动保留创建/唤醒后、业务等待前应用 PG 的顺序；带 PG 热部署不再在 Ready 后才执行改密。

剩余：hot_execution 阶段的完整自动核验、owner 进程重启后操作凭据的私有恢复来源仍需接通，不能将本轮的请求内凭据传递当成持久恢复完成。原资源接管及未知远端写剩余项继续保留。

## 2026-09-20：app-cli 私有运行凭据恢复

- 独立 run-credentials.json 保存在现有 owner 状态根，绑定 application_id 与已核验项目来源；不包含在普通操作记录、事件、制品或状态响应中。
- 复用 token 的私有文件写入通道：先设置权限再写入、文件同步、原子替换；Unix 同步目录。Windows 使用现有 icacls 权限路径。
- /v1/deploy 通过受理检查后持久化显式配置；运行协议派发在控制信号/部署发送前持久化，取消请求先收束，避免迟到的取消操作覆盖配置。
- 编排选择顺序为显式请求配置、私有已保存配置、原环境变量。进程重启后的同项目启动能恢复此前已传入的凭据；已有运行进程的环境不被原地改写。
- 私有配置纳入状态根权威域发现，不作为可忽略残留。记录损坏或项目身份不匹配明确报错。

边界：该文件保存 app-cli 已接收的运行配置，不主动执行 PG 改密，也不能观察用户在数据库外部单独更改的密码。平台直接改密之后的显式启动仍需从平台配置传递对应凭据；运行配置同步全链、hot_execution 恢复和其他资源恢复剩余项尚需继续核对。

## 2026-09-20：凭据持久化约定复核与修正

上一节的私有明文运行凭据文件实现已撤回，未部署执行，也未生成运行凭据文件。复核 `specs/reviews/2026-09-19-cross-plan-implementation-review.md` 的 R08 要求后，继续遵守操作凭据不落盘、崩溃后不能把脱敏输入当成完整命令的约定。

- 保留请求内凭据到迁移/业务进程的传递及取消前置判断。
- DeployRequest 落盘仅保留用户名与空密码标记；恢复已确认版本时识别必须重新提供凭据，不能因为旧 serde(skip) 丢失该事实而静默采用旧环境密码。
- 显式新环境部署继续走其已有受理与恢复流程；不自动改数据库密码。
- token 原子写入的目录同步改进保留，仅服务 token 文件。

尚需实现平台与 owner 的明确恢复交互，在原操作证据下重新提供配置；本轮不把需要重新提供凭据的报错当作完整业务恢复。旧资源绑定、未知远端写和 hot_execution 核验仍为必做项。

## 2026-09-20：prod 计算资源缺失时的幂等 Stop

- 新增只读运行时能力 app_compute_absent；未实现后端默认不提供缺失证明，不能把未知状态当作已停止。
- Docker 仅确认 inspect 的结构化 404；连接失败和其他响应照常报错。
- K8s 先读取目标 Deployment，再确认应用范围内无 ReplicaSet、Pod 和 Deployment；控制器已删除但旧计算仍残留时不能提前成功，RBAC/网络错误不当作缺失。
- 协调器只在旧执行完成收束、物理租约已绑定后使用缺失证明，并再次核验当前执行者；按 Stopping→Stopped→Completed 记录证据和清理租约。
- 该分支不创建容器、不修改 Service/PVC、不唤醒业务，Restart 不使用缺失证明假装已启动。

本轮补齐 prod 已缺失计算资源的 Stop 幂等行为；未知旧写收束、旧生命周期接管及凭据恢复交互仍需继续完成。

## 2026-09-20：K8s prod Stop 响应丢失的回执恢复

- 缩容与 `rcoder.io/compute-stop-receipt` 原执行上下文写入同一次 UID/resourceVersion 条件更新；不使用单独写回执造成第二个不确定窗口。
- 新增只读恢复能力：确认控制器 UID、原 lifecycle/operation/executor 回执和 replicas=0，等待旧 Pod 全部退出，再复核回执。未实现该证明的后端返回不能确认。
- 显式恢复与后台扫描可处理 prod Stop 的 stopping 阶段：仅观察原写入，不重发 stop、不重建容器、不按超时或租约年龄解锁。
- 存储以完整原快照、当前控制头、生命周期、执行代次及租约核验，在同事务完成 stopping→stopped→completed。竞争失败整体回滚，原租约仍按既有终态扫描精确释放。
- 并发原执行者回调遇到 revision/终态变化即停止推进；不能用原回调覆盖新终态。

范围仍未全完：旧版本没有回执的停止、Docker 未确认调用、Restart 中断续行、被高优先级控制打断的未知普通写仍需各自的证据核验，不能沿用本回执凭空宣布已完成。

## 2026-09-20：K8s prod Restart 启动响应丢失恢复

- 启动条件更新原子写入 compute-start-receipt，与 replicas=1 使用同一 UID/resourceVersion 前置条件。
- 启动回执绑定原 lifecycle/operation/executor；核验控制器 UID、目标副本数、当前代次 observedGeneration、updatedReplicas 和 readyReplicas，不用旧代次 Ready 代替新启动完成。
- 显式恢复与后台扫描支持 prod Restart/starting；就绪观察前后再次确认原启动回执及物理身份，全程只读运行时，不重新启动。
- 存储复用完整快照条件提交，在同事务经过 verifying 并收束终态。Stop 替代 Restart 后旧快照不能越过新的控制头。
- Java 接入说明同步上述两类原子回执恢复；没有回执的历史操作仍不能凭年龄或 Ready 猜测成功。

本轮只补原启动写已生效的恢复；Restart 在停止完成后尚未发启动的续行、旧未知写、历史资源接管及配置恢复交互仍需继续实现。

## 2026-09-20：K8s prod Restart 停止后续行

- 新增原物理租约只读验证：核对 namespace/name、UID、resourceVersion、token 和应用/服务族；不释放或替换租约。
- prod Restart/stopped 可由显式恢复或后台扫描续行。先复核停止回执、旧 Pod 退出和原控制器 UID，再用完整数据库快照原子认领 Starting；不产生新的 operation_id，不再次停止容器。
- 正常执行者和恢复执行者争用同一 revision，只有一个能进入剩余启动。新的 Stop 改变控制头后，旧启动不能通过存储执行者检查。
- 已确认启动转 Verifying，业务尚未就绪时由既有扫描继续观察；启动响应未知保留 Starting 和原租约，沿原启动回执恢复。
- 被 Stop 替代且不存在未知写时记录原执行已收束，终态扫描精确释放原租约；未知写不伪装为已收束。

仍需补充：Starting 持久化后尚未发出请求就崩溃的重试协议、dev/Docker 的中断续行、旧版本未知写、历史资源身份恢复及配置恢复交互。未新增测试。

## 2026-09-20：计算重启专用启动与卷身份见证

- 新增 UserAppComputeStartTarget，保留原控制目标形状，追加单写能力标识与 PVC 物理身份；旧记录缺失字段不能冒充新协议。
- K8s 计算重启不再复用含 PVC 认领写的普通启动链。捕获和核验现有 PVC 为只读，实际启动仅执行一次控制器 UID/resourceVersion 条件更新，并原子写启动回执。
- Restart 在停止前捕获卷 UID，停止完成后的启动及中断续行再次比较卷数量、名称、种类和 UID；同名 PVC 被替换时拒绝继续使用。
- 专用启动还需确认旧计算已停止、Pod 已退出，不创建控制器/PVC，不修改存储使用标记。正常部署/普通启动的原存储认领行为保留。
- 中断续行存储更新控制器 resourceVersion 时保留原卷见证，进入 Verifying 后继续保留见证。

尚需将单写协议接入 Starting 崩溃窗口的条件重试，并覆盖 superseded 的未知写收束；旧记录、dev 续行、历史资源接管与配置恢复交互仍未全部完成。未新增测试。

## 2026-09-20：Starting 条件重试及已替代 Restart 收束

- Starting 新协议通过原租约、停止回执、PVC UID 和控制器条件版本核验后，按原操作身份进行单写重试；启动回执已存在时只观察就绪，不重复提交。旧记录缺少单写见证时不进入重试。
- Stop 等待被替代 Restart 时，可依据 stopped/verifying 的持久化完成边界收束旧执行。存储同时检查原执行身份、revision、Superseded 状态、租约及原检查点，竞争失败不修改记录。
- 该路径将旧 Restart 标记为 cancelled/failed 并保存原检查点，不伪造成功；后续仍按原物理回执释放租约。Starting/Stopping 的未知写不使用此证明。
- 新证据类型为 PersistedMutationBoundary，与原执行者返回确定运行时结果的 MutationCompleted 分开。Java 接入说明同步条件重试与已替代执行的收束边界。

剩余必做：未知普通写及被替代 Starting/Stopping 的核验、dev/Docker 中断续行、历史资源身份和 PVC 登记恢复、凭据恢复交互及 hot_execution 闭环。当前不是整体实现完成。

## 2026-09-20：Stop 隔离旧 Restart 的条件启动写

- K8s prod 新协议 Restart/Starting 被 Stop 替代时，核验原租约、原执行身份与单写见证，再处理旧 UID/resourceVersion 前置条件。
- 同 UID 的当前版本已变化，说明旧条件写不能迟到提交；版本未变化则以原条件提交仅修改注解的隔离写，并复核版本已经变化。隔离失败或响应未知保留原操作与租约。
- 隔离不修改 replicas、Pod template 或 PVC；它只证明旧请求不能再提交，不证明业务停止。新 Stop 仍须获取自己的物理租约、缩容并确认旧 Pod 退出。
- 存储增加 ConditionalStartupFenced 证据，限制 Prod/Restart/Starting、单写见证、原上下文、检查点和 revision。收束记录保留旧证据，不伪造 Restart 成功。

未覆盖：旧版本多写启动、Restart/Stopping、dev 和 Docker 未知调用、未知普通业务写、历史身份恢复及凭据恢复交互。上述仍属于整体必做范围。未增加测试。

## 2026-09-20：阻塞解除后的后台续行

- 后台扫描接入被替代 Restart 的完成边界/条件启动隔离，与 Stop 同步执行链复用同一生产函数；不再依赖原执行进程仍存活。
- 原租约仍交给精确回执释放流程处理。draining_previous 的 RecoveryRequired 只有在未绑定租约、未留下计算写检查点且所有旧操作终结、租约释放后才自动恢复。
- 先只读检查阻塞条件；未解除直接结束本轮观察，不再次占用工作槽等待 120 秒。恢复以完整原快照 CAS，保留 operation_id；执行前继续复核当前控制头和已收束条件。
- 多副本竞争中的版本冲突视为其他执行者已推进，不能重置或覆盖其记录。缺失历史证据和生命周期冲突仍报错。

本轮未增加测试。未知普通写、未支持的 runtime 恢复分支、历史身份接管及配置恢复仍须继续完成。

## 2026-09-20：发现登记的卷身份核验与持久证据

- K8s dev/prod 发现结果加入 PVC 名称、UID；读取真实 Pod 模板引用，STS 模板卷按单工作区 ordinal 计算，不靠 PVC 列表任意选取。多个 builder ordinal 明确进入恢复处理。
- PVC 必须存在、未删除且 app/lifecycle 元数据一致；缺失身份不猜测接管。两次库存复核同时比较卷 UID，忽略无关 resourceVersion 更新，卷名排序去重。
- 新增版本化迁移 userapp-recovery-witness-v3.sql；恢复根登记的同一事务保存不可变发现证据与停止意图，不创造历史成功操作。既有 baseline/v2 不修改、不清库。
- Docker 继续使用容器物理身份，其挂载证据尚需补充，不能把此次 PVC 核验表述为 Docker 卷身份已完成。

剩余：后续访问对持久卷见证的复核、无生命周期历史资源、失败新根与旧资源的原子恢复、dev/Docker 中断续行及未知普通写、凭据恢复交互等，仍未完成整体功能。

## 2026-09-20：恢复卷证据接入使用前检查

- 存储 trait 增加 get_recovery_witness，按 app/lifecycle 读取并严格校验持久 JSON，损坏证据不当作不存在。
- dev ensure/probe、prod 显式启动/发布及计算 Restart 在访问或启动前读取本 scope 的恢复证据，K8s 实时核验原 PVC UID、生命周期、未删除状态。dev/prod 不互相读取另一环境卷。
- Stop 不增加存储可用性前置条件；故障卷不能成为阻止停止计算资源的原因。正常未经过恢复登记的应用没有该证据时沿原物理捕获流程执行。
- 该检查证明原卷仍存在且身份未替换；尚不能单独证明当前 Pod 模板没有改挂其他卷，后续仍需把挂载引用与见证比较接入受租约保护的最终捕获。Docker 挂载证据和未标记旧资源接管仍未完成。

## 2026-09-20：恢复卷与当前挂载引用比较

- 恢复卷校验接口显式传递 dev/prod scope，不跨环境读取另一个工作负载。
- dev 重新读取已捕获 UID/resourceVersion 的 StatefulSet，解析直接 PVC 和模板卷引用；prod 检查当前 Deployment 生命周期及 Pod 模板引用。
- 比较挂载 PVC 的名称/UID 集合与不可变恢复见证，旧卷仍存在但模板改挂其他卷时明确报错。缺控制器只保留原卷存在证明，不冒充已恢复运行实例。
- 此检查仍为使用前观察；受租约保护的最终变更前复核、缺失控制器重建时的模板校验以及历史无标记资源恢复仍需继续完成。Stop 不调用此检查。未新增测试。

## 2026-09-20：Stop 接管旧 Restart 缩容未知阶段

- 新协议 K8s prod Restart/Stopping 的原缩容只有一次 UID/resourceVersion 条件写。接管方核验原租约与单写见证后隔离该前置条件，旧缩容请求不能迟到提交。
- 新增 ConditionalStopFenced 证据并严格匹配 stopping 阶段；与 Starting 的证据不能混用。同步 Stop 与后台恢复共用收束入口，随后仍执行新 Stop 自身的缩容、旧 Pod 退出确认。
- 未扩大到缺乏单写见证的旧记录，也未把已隔离原写等同于物理停止成功。dev/Docker 与未知普通业务写仍需继续。

## 2026-09-20：K8s dev Stop 原子回执恢复

- builder 缩容更新同时写入原执行上下文回执，与 StatefulSet UID/resourceVersion 前置条件共用一次 patch。
- 新增只读恢复：核验原 workload UID、生命周期/物理绑定、replicas=0、原回执；确认该 builder 范围 Pod 全部退出后再次读取工作负载与回执。
- 显式恢复和后台扫描均支持 dev Stop/stopping；存储按 BuilderControlTarget 验证上下文与 STS 类型，完整快照 CAS 后收束终态并按原回执清理租约。
- 缺回执、被替换或尚有 Pod 时保留恢复状态。Docker 默认不提供此证明。未新增测试。

未完：dev Restart 的中断续行/写隔离、Docker 未知调用、普通业务未知写、历史资源接管与凭据恢复交互等。

## 2026-09-20：dev Restart 启动已生效的回执恢复

- builder 从 replicas=0 启动时，原启动上下文与 replicas=1 在同一 UID/resourceVersion 条件 patch 中写入。
- dev Restart/Starting 支持原子回执观察：核对原上下文、工作负载 UID、当前 observedGeneration、Ready 副本、Pod Ready 与控制器修订标签，再次读取 Pod 和 STS 排除观察期间换代。
- 显式恢复与后台扫描共用该路径，存储扩展原快照终态提交到 BuilderControlTarget。观察分支不发启动写，也不在终态 CAS 前改写本地 builder 登记。
- 本轮解决原启动已经提交的响应丢失；未提交启动的 stopped/starting 条件续行、dev 被 Stop 抢占的未知写、Docker 与普通业务未知写仍需继续。未新增测试。

## 2026-09-20：dev Restart 停止后续行

- 显式恢复与后台扫描支持 dev Restart/stopped。核验原运行时租约、停止回执与旧 Pod 退出，读取恢复卷证据，并重新捕获同一 STS 的条件版本。
- 存储以原快照、当前控制头、生命周期、代次与原物理绑定检查后 CAS 到 Starting；新目标不得有 Pod，不得切换 STS 名称/UID，不生成新操作身份。
- dev/prod 启动提交后的进度、异常和抢占收束共用 continuation 执行器，避免两条链错误处理不同。未知写保留 Starting，已确认写转 Verifying 后交既有终态恢复。
- 尚未覆盖 dev Starting 写未提交的条件重试、被 Stop 抢占时的写隔离以及完整卷见证。Docker 恢复、历史资源接管与普通业务未知写仍需继续。未新增测试。

## 2026-09-20：dev Restart 抢占隔离

- K8s builder 运行时声明单次条件写能力，新的停止/启动检查点保存 builder_compute_single_write。默认后端不声明，旧记录缺标记不自动获得该能力。
- 被 Stop 替代的 dev Restart/Starting 或 Stopping 核验原租约后，用同 STS UID/resourceVersion 的注解写隔离旧前置条件；不修改 replicas、Pod 模板或 PVC。
- 数据库按原上下文、阶段、检查点与 revision 接受隔离证据；同步控制与后台扫描共用入口，之后新 Stop 仍自行缩容并确认退出。
- BuilderControlTarget 严格反序列化仅额外识别协议标记，其他未知字段仍拒绝；恢复续行保留旧检查点的能力标记。
- 未完成 dev Starting 未提交写的条件重试及完整卷捕获、Docker 和普通业务未知写、历史身份接管、凭据恢复。未新增测试。

## 2026-09-20：dev Starting 条件重试

- Starting 新记录沿原租约和上下文观察：启动已确认则补交终态；启动回执已存在但未 Ready 只观察，不重新启动。
- 原启动尚未提交时，确认原停止回执与无 Pod；刷新捕获必须仍等于停止观察前读取的 STS resourceVersion，防止把并发迟到启动静默覆盖。
- 存储允许具有单写标记的 dev Starting 完整快照 CAS 重试；原物理 UID、绑定、代次和租约约束保留。显式与后台入口一致。
- 旧版本缺标记不获得重试资格。完整卷捕获、Docker 恢复、普通业务未知写与历史资源接管/凭据恢复仍未全部完成。未新增测试。

## 2026-09-20：dev Restart 计算检查点卷见证

- dev Restart 在停止前读取同 STS UID/resourceVersion 的 Pod 模板及实际 PVC 名称/UID；停止后再次捕获，比对原见证后才进入 Starting。
- stopped 续行和 starting 条件重试均对比原 builder_volumes，数据库更新工作负载条件版本时保留原卷见证，不以当前卷反向覆盖原证据。
- Stop 不要求卷见证；legacy 记录缺原卷证据明确要求恢复，不能默认采用现在的卷。BuilderControlTarget 严格解析仅识别明确增加的见证字段。
- K8s 卷捕获验证原卷生命周期及未删除状态；Docker 默认空见证仅保留原容器 ID 约束，尚不代表 Docker 挂载恢复完成。
- 未新增测试；旧记录恢复、Docker、未知普通写与历史资源接管/凭据恢复仍需继续。

## 2026-09-20：Docker 启动语义与恢复卷核验时序

- Docker `start_builder_control` 改为调用仅启动分支；分阶段 Restart 已停止旧实例后不再误调用 Docker restart。真正 restart 入口保持原行为。
- Restart 恢复卷核验移到取得物理租约之后、绑定执行收据之前；失败保持只读租约清理，不发起容器写入。等待租约前的观察不再充当最终依据。
- 正常启动和原操作恢复续行均在启动提交前再次核验恢复卷，随后检查当前执行者；Stop 不增加卷健康前置条件。
- 这些检查约束 RCoder 协调写入，不声称能够排除外部管理员在检查后删除 PVC 的竞态。Docker 未知写收束、历史资源接管和凭据恢复仍未全部完成。

## 2026-09-20：dev 控制器缺失不再等价于停止完成

- K8s builder 捕获在 STS 404 时核验 app/family 标签下的全部 Pod，同时核验规范 Pod 名称，避免标签丢失隐藏残留实例；最后重新读取 STS，排除观察期间控制器重建。
- 无工作负载的 Stop 在执行边界重复只读核验；Start/Restart 对空目标提前明确拒绝，不返回空成功。所有检查不修改 Service、PVC 或残留 Pod。
- 孤儿 Pod 仍存在时不虚报停止完成。缺控制器情况下如何凭原物理证据收束孤儿 Pod，以及无写停止的崩溃恢复仍需补齐，不能把本次保护修正算作完整恢复实现。

## 2026-09-20：无运行时写入的停止检查点恢复

- 新增严格解析的 `ComputeAbsenceCheckpoint`，记录原执行上下文及已确认不存在；dev/prod Stop 在旧执行已收束并取得物理租约后均使用该检查点。
- Stop/stopping 恢复对该检查点先核对原物理租约，再实时复核计算资源不存在；K8s dev 复用控制器、标签 Pod、规范名 Pod 的检查。不会把普通停止的未知写检查点转换成“原本不存在”。
- 存储仅对 Stop 和 dev/prod 接受该证据，保留当前控制头、生命周期、代次及完整快照 CAS，原操作进入终态后沿现有流程释放原租约。
- Docker 缺少只读物理租约核验实现，因此 Stopping 恢复仍明确保留保护；不能把本轮称为 Docker 完整恢复。孤儿 Pod 收束与历史资源恢复仍待完成。

## 2026-09-20：Docker 原租约只读核验

- Docker 实现 `validate_app_operation_receipt`，复用释放租约的文件身份检查：原 device/inode、普通文件、禁止最终路径符号链接、非阻塞取得独占锁后再次核对路径和 fd 身份。
- 读取并精确比较原 token，限制读取长度；空标记不授权恢复。仅临时取得/释放文件锁，不修改文件标记，不把原执行者仍持锁视为可接管。
- 无写入 Stop 的原检查点现可在 Docker Unix 后端核验租约并继续恢复。非 Unix 后端仍返回不支持核验；该限制不涉及 app-cli 独立宿主机能力。
- 文件锁可取得只证明原本地执行者已离开临界区，不能证明 Docker daemon 的未知写已结束；未因此放开普通超时写入恢复。

## 2026-09-20：Docker dev Stop 因果回执

- Docker stop 返回成功、同容器 ID 再次检查明确 running=false 后，原子发布原 BuilderControlTarget 回执；缺 running 字段不再按停止解释。
- 回执使用操作身份命名，包含原生命周期、执行者、容器 ID 与绑定；临时文件先同步再以 hard link 不覆盖发布，重复内容可幂等、不同内容拒绝。Unix 同步目录；读取有长度限制和严格目标解析。
- Stop/stopping 恢复先核验原物理租约，再核验回执与实时原容器状态，按原快照 CAS 完成。存储仅在 Stop 分支允许 Docker Container 目标，不扩展未知 Restart 恢复。
- 回执位于控制工作区 `.app-operation-receipts`，不写业务 PVC、不含 PG 密码，暂随操作历史保留。API 返回前断连、成功返回后但回执落盘前崩溃仍缺证明，不以 stopped 状态自动解锁；后续仍需补全这些恢复边界及回执历史清理。

## 2026-09-20：Docker prod Stop 回执恢复

- prod 的身份绑定停止入口在 stop 返回及原容器明确停止后保存 UserAppMutationTarget 回执；恢复要求回执完全匹配并重新确认原容器已停止/不存在。
- dev/prod 共用原子发布与有界读取，文件名按 builder/prod 隔离；保持各自强类型目标，不把 prod 身份转换成 builder。
- 存储对 prod Stop 允许 Container 目标，仍校验完整操作快照及当前控制头；没有放宽 prod Restart 的恢复判据。
- 回执未产生的响应丢失、Docker Restart 续行和未知写抢占仍待实现，不能以两种 Stop 回执存在宣称 Docker 控制恢复全部完成。

## 2026-09-20：Docker Restart/stopped 续行

- dev/prod 存储续行允许 stopped 阶段的 Container 目标，必须保留原类型、容器 ID、名称和上下文；已有租约与停止回执核验通过后，完整快照 CAS 选出唯一启动者。
- 不开放 Docker starting 的条件重试；Docker 未提供 K8s resourceVersion 写隔离能力，不能因启动超时重新提交启动。
- dev 共用续行执行器补齐管理通道就绪确认和身份检查后的注册，再写 Verifying；避免 Docker 仅返回 running 时提前报告恢复完成。
- 同一 Docker 容器的创建配置保留，不重建容器或改密。外部卷内容变化、起动响应丢失及停止后但回执缺失的恢复仍需进一步处理。

## 2026-09-20：Docker 启动确认回执

- dev 的仅启动分支及 prod 的 compute 启动入口，在 Docker start 返回且原容器明确 running 后保存启动回执；与停止回执按动作隔离，保留同一操作内两个阶段的证据。
- Docker starting 恢复只读取回执和原容器，不提交 start；必须验证原租约。dev 追加文件服务就绪检查并再次确认运行身份，prod 沿既有业务 Ready 及身份前后复核，再按原快照提交终态。
- 存储允许 Container 的已观察启动终态；没有允许 Docker starting 条件重试或把运行状态单独当作成功依据。
- API 响应丢失/回执未落盘的窗口、被 Stop 抢占的未知启动，以及历史资源接管仍待继续。未新增测试。

## 2026-09-20：Docker 已确认写入的抢占交接

- 同步 Stop 与后台扫描允许检查被替代的 Docker Restart/starting、stopping；原文件租约必须仍匹配且旧执行者不再持锁，运行时再核对同阶段、同目标的持久回执。
- 新的 `PersistedRuntimeAcknowledgement` 仅用于收束旧执行；存储验证原上下文、Docker 目标、阶段、lease、完整 checkpoint 和 revision，随后记录取消，不伪造旧 Restart 成功。
- 回执读取不要求当前业务 Ready；新 Stop 仍独立捕获资源、停止并确认。未知写没有回执时继续保留保护，不使用运行状态或超时推定已结束。
- 旧执行者仍存活但陷于就绪等待、响应丢失未产出回执、历史资源恢复与凭据链仍需继续。

## 2026-09-20：dev 外层就绪等待响应抢占

- 正常 Restart、stopped 续行和 starting 回执恢复共用 `confirm_compute_builder_ready`。在原就绪 deadline 内，每 250ms 读取原操作并核对完整快照；被 Stop 替代或 revision 改变时退出只读探测。
- 外层 timeout 同时约束探测与数据库核验，数据库核验失败不继续报告成功。正常执行者沿现有 settled 分支记录已确认写完成，交给新 Stop 收束。
- 不取消或丢弃尚未返回的 runtime start/stop future；K8s runtime 内部等待和其他未知远端写仍需进一步拆分处理。本轮只解决 runtime 返回后外层文件服务就绪等待的交接延迟。

## 2026-09-20：空生命周期根恢复旧身份

- 首次访问与扫描不再因数据库已有初始空根就跳过运行时发现；原生命周期一致直接复用，缺少计算资源时保留现有根。
- 已两次实时核验的旧资源遇到新空根时，存储先竞争根 control_revision，再检查 epoch/revision 为初始值、槽位为空，且不存在任何操作、请求、活动、配置、计算意图/记录或恢复见证。
- 同一事务内移除空槽位行、以旧生命周期/metadata revision 条件更新根、递增 epoch/revision、建立原生命周期的空槽位，并保存原 PVC 见证和停止意图。任一步失败整体回滚；不改运行时资源，不删除业务数据。
- 已有失败 Ensure/RecoveryRequired、删除重建历史或配置的新根仍需独立的证据恢复流程。本轮不将空根恢复等同于该场景完成。

## 2026-09-20：确认拒绝的 Ensure 历史随身份恢复保留

- 扩展空根恢复的资格检查：仅允许原新生命周期下的 EnsureBuilder/dev、Failed、creation_result、null checkpoint。creation.rs 的该组合来自结构化 RequestRejected；其他错误当前进入 RecoveryRequired，不符合条件。
- 原失败操作、输入、请求幂等记录均保留，不改状态或生命周期。新根恢复原运行时身份后，新请求使用恢复身份；旧请求仍保留原失败结果，不能伪造成功。
- 同事务继续排除任何 operation lease、资源绑定、活动、凭据配置、compute 意图/操作、恢复见证和 recreate 请求；epoch/revision 初始值、空槽位、双次运行时身份核验不变。
- RecoveryRequired/运行中/其他失败阶段仍不参与该替换路径；其未知写证据收束仍待完成。没有清库、清锁或改写历史。

### 2026-09-20：创建超时的迟到成功证据

- 已实现：保持原创建 future 的观察，迟到成功后只读核验控制器/Pod 身份与管理端点，再保存 BuilderCreationEvidence。
- 存储使用根行 CAS 竞争点、完整操作快照和原 executor 校验；仅允许 creation_confirmation_timed_out/null checkpoint 转为 builder_ready_confirmed，状态仍为 RecoveryRequired，槽位不释放。
- 后续走既有完成恢复协调器，重新核验业务执行权与物理身份；Stop 已接管时不能凭迟到结果发布成功或登记缓存。
- 本批未处理：迟到错误的分类收束、管理端点尚未就绪时的持久待核验结果，以及进程在结果回执落盘前崩溃的恢复。没有重试创建或用超时直接释放槽位。
- 未新增或执行测试。

### 2026-09-20：迟到创建结果分离“返回”与“就绪”

- 替代上一批先等待管理就绪再存证：先保存 builder_created_observed，再由扫描器只读核验当前物理 UID、当前地址与管理端点；核验通过晋升 builder_ready_confirmed，始终保持 RecoveryRequired，最终完成另走执行权核验。
- 恢复核验不再调用会刷新项目缓存的 cross_verify_registration，也不在终态 CAS 前登记项目；补上原完成恢复遗漏的管理端点实际探测。
- Stop 的 drain evidence 独立于成功 evidence：创建 API 已返回、运行时租约已释放时，业务尚未 Ready 不再阻止停止；存储按原快照及当前控制执行者收束为取消失败。此证据不能用于业务成功提交。
- 若 Stop 的原收束预算已到，扫描器发现迟到证据后恢复原控制操作，让新选举的执行者完成收束，不创建新的用户请求或排队。
- 仍未完成：迟到错误结果和创建结果持久化前崩溃的恢复；完整目标继续推进。未新增或执行测试。

### 2026-09-20：迟到明确拒绝与租约清理失败

- 迟到创建结果统一分派：成功继续保存观察证据；外层类型明确为 RuntimeRequestRejection 时，以完整快照、原 executor/lifecycle/fingerprint 收束原 EnsureBuilder 为 Failed，并在同事务释放其槽位。未知错误不进入该路径。
- 修复 builder_completion：请求被拒绝但运行时租约释放失败时，返回包含原错误与清理错误的 ConnectionError，保持 RecoveryRequired，不能继续把 RequestRejected 传给上层误判可终结。
- 没有新增或执行测试。现有 release_failure_preserves_original_error_and_rejects_success 中拒绝分支仍断言旧 RequestRejected 类型，与批准后的清理失败语义不一致；按当前要求暂不改测试，后续验证阶段需调整该断言，而不是恢复错误生产行为。
- 剩余：未知写入结果及回执落盘前崩溃的现场核验；整体功能尚未完成。

### 2026-09-20：dev Stop 收束残留孤儿 Pod

- 新增独立 BuilderOrphanStopTarget：原生命周期、Pod UID/resourceVersion、原 StatefulSet 名称/UID；不扩展普通 BuilderControlTarget，不允许普通 ensure 因该证据创建新控制器。
- K8s 在 StatefulSet 实时不存在时核验 canonical Pod 的应用/family 标签、生命周期注解与 ownerReference，二次检查控制器仍不存在，再捕获。
- Stop 在既有控制执行权与物理租约下持久化该检查点，删除请求携带 Pod UID+resourceVersion，等待原 Pod 消失并确认控制器/其他目标 Pod 均不存在，再提交 Stopped/Completed。PVC、Service 不变。
- 响应丢失恢复只读查询原 Pod 和控制器，没有无条件删除重试；新 UID 或新控制器不会被按名称清理。
- 尚未覆盖：无原控制器 ownerReference/生命周期注解的历史孤儿，以及 Restart 的控制器重建。这些仍需按原资源恢复方案继续开发，不声称孤儿全部可自动恢复。
- 未新增、修改或运行测试。

### 2026-09-20：已绑定历史孤儿 Pod 的 Stop

- 在孤儿捕获中接入现有 UserAppResourceBinding：先只读获取 Pod 的原 StatefulSet UID，再读取登记，以当前生命周期重新核验 Pod/控制器身份；没有写注解或新建绑定。
- 历史资源可以缺少生命周期注解，但必须有同 app/lifecycle/family/物理 UID 的既有登记。原注解明确冲突时，即使存在登记也拒绝覆盖。
- 孤儿检查点携带可选 binding（旧检查点省略仍可读），执行删除前重新核验同一登记；登记不能替代 Pod UID/resourceVersion 前置条件。
- 候选查询与最终捕获之间 Pod 或控制器身份改变时终止本次捕获，不悄悄改为停止另一个资源。
- 仍未完成：既无注解又无历史登记的归属恢复、ownerReference 已丢失的孤儿以及 Restart 重建。没有新增或执行测试。

### 2026-09-20：Docker 创建完成回执跨进程恢复

- Docker UserappBuilder 创建成功后，先核验物理容器身份，原子保存原 context、容器信息、物理目标和原文件租约回执，再释放租约。复用现有原子发布/有界读取设施，不保存业务密码。
- 后台对 EnsureBuilder 的 claimed/creation_confirmation_timed_out/worker_interrupted/creation_result 空检查点查询原运行时回执；只允许 Running/RecoveryRequired 且 executor 已存在的操作。
- 回执必须匹配 app/lifecycle/operation/executor/fingerprint。清理仅使用原 inode/token，活跃 flock 或替换锁会拒绝；释放完成后以完整快照 CAS 保存 builder_created_observed，后续仍需独立就绪核验或由 Stop 收束。
- 关闭的窗口是“运行时回执已保存，但数据库检查点未保存”；运行时回执自身写入前崩溃、Docker 返回结果丢失，以及 K8s 同等回执仍需继续处理。不能按现存容器名称补造回执。
- 未新增或执行测试。

### 2026-09-20：K8s 创建完成回执

- Docker/K8s 共用内部 BuilderCreationReceipt 校验模型，校验运行时类型、原租约 family、控制器与实际容器/Pod UID。
- K8s 创建返回成功后，在释放原 ConfigMap 租约前创建独立 immutable ConfigMap 回执；按原 operation ID 可重复定位，409 只允许完整载荷相同，不覆盖已有证据。
- 回执不挂 Pod/控制器 ownerReference，防止实例删除时丢失恢复证据；不保存 PG 密码或业务环境变量。读取有载荷大小上限、context/namespace/资源类型核验。
- 后台恢复复用上一批协调器：读回执、按 UID/resourceVersion/token 释放原租约，再 CAS 保存 builder_created_observed。API 结果丢失但回执已提交时可重新读取；没有回执时不猜测完成。
- 尚未闭环：创建成功但运行时回执本身尚未提交时崩溃、终态历史回执的回收策略，以及其他原计划残留。未新增或执行测试。

### 2026-09-20：创建链 Service 条件写入

- 源码确认 K8s builder 在 Pod Ready 后仍执行 create_agent_service，随后才获取返回信息。因此 Pod Ready 本身不能替代创建完成回执。
- 修复 builder Service 端口收敛的无条件 SSA：先核验应用/family 标签、selector、Service 形态与未删除状态，再按捕获 UID/resourceVersion 使用 Merge 只更新期望端口；不覆盖 ClusterIP 或未知字段。
- headless Service 检查不再把所有 GET 失败当成不存在：仅 404 允许创建，权限/网络错误直接传播；已有 builder headless Service 也核验归属与路由。
- 本批是迟到 Service 写入保护，不是“回执提交前崩溃”完整恢复；该窗口仍需操作绑定的最后写入证据与重建协议。未新增或执行测试。

### 2026-09-20：新建 builder 的最后 Service 写入携带完成证据

- K8s 受管理 builder 新建路径将原租约回执传入编排，在 Pod Ready/物理捕获后，把创建完成载荷与最后一次 Service create/UID+version patch 原子提交；普通 agent 保留先创建 Service 再取返回信息的原顺序。
- 原 operation/executor/lifecycle/fingerprint、Pod/控制器身份和租约均在载荷中，未保存业务密码。之后只有独立回执归档及只读检查，不再发出计算资源/Service 写入。
- 后台先读不可变 ConfigMap 回执；不存在时读取 Service 原子完成标记。其他 operation 的标记返回无证据，原 operation 却身份不同则报错；不拼装或覆盖不匹配证据。
- 关闭“最后 Service 已提交，但独立回执未提交”的窗口。最后 Service 尚未提交时、旧绑定资源 resume_bound_builder 分支、Docker 回执前窗口仍需各自的阶段恢复，不声称创建所有崩溃点闭环。
- 未新增或执行测试。内部 managed builder 创建现要求原 lease receipt；后续测试若绕开 runtime 直接调用创建 helper，需按真实租约入口验证，不能为测试取消该约束。

### 2026-09-20：K8s 创建等待响应高优先级控制

- RCoder 创建观察者按持久 compute head 检查当前 Stop/Restart 是否明确将原 operation 列为 interrupted；仅此证据触发进程内取消信号。信号不序列化、不替代多副本数据库判定。
- K8s 新建 builder 在已确认请求之间检查取消；Pod Ready 的只读 watch 可立即结束。任何已经发出的资源写入仍完整 await 后才进入取消边界，不能 drop 后声明取消完成。
- 增加 CreationCancelled 结构化结果，只有安全边界产生；builder completion 显式释放原租约后传播。清理失败继续返回未知结果，不能按取消终结。
- 正常和迟到取消都保留 creation_cancelled 检查点，按原 executor/生命周期/快照终结原创建槽位；不会伪装成“零副作用拒绝”，残留资源交由 Stop 处理。
- 数据库暂态错误不触发取消，也不会通过 `?` 丢弃在途创建观察。仍待补：Docker 长创建阶段、K8s 已绑定实例的长等待、取消确认后数据库落盘前崩溃的持久回执。
- 未新增、修改或运行测试。

### 2026-09-20 生产逻辑进度：取消部署模式覆盖

- 已实现 K8s 绑定 builder 恢复启动的只读等待取消。
- 已实现 Docker 新建 builder 的健康等待取消及绑定容器启动边界取消；保留已创建资源，由 Stop 处理。
- 最终全 features 编译通过；按用户要求未新增或运行测试。
- 未完成：取消确认的持久化崩溃窗口，以及原计划其余恢复链路。不能将编译通过视为运行时验收。

### 2026-09-20 生产逻辑进度：取消持久回执

- Docker/K8s 已接入取消回执，保存后释放原租约；后台按原 execution context 恢复收束。
- 存储取消收束支持已确认回执对应的 Running/RecoveryRequired 创建阶段；仍检查完整快照和原执行者，不授权再次写运行时。
- 回执落盘前退出、未知写入及其他剩余恢复路径未完成；未执行测试。

### 2026-09-20 生产逻辑进度：Service 修复所有权

- UserApp 查询剥离 Service 创建副作用。缺失 Service 通过注册交叉验证触发受理式 ensure，复用旧绑定资源。
- K8s 绑定实例启动补齐最终 Service 完成回执；原操作租约内完成，普通 agent 行为不变。
- 全 features 编译通过，未新增或执行测试；其余恢复缺口仍按原范围继续。

### 2026-09-20 生产逻辑进度：创建完成登记

- 创建 worker 不在终态前登记；就绪探测只读。成功等待方重新核对物理身份与 compute 状态后登记。
- 创建成功后的健康观察响应对应 Stop/Restart 接管，结束只读等待。
- 未新增或执行测试；整体恢复范围尚未完成。

### 2026-09-20 生产逻辑进度：Docker 复合创建分类

- 创建已返回物理 ID 后的启动/观察失败，保留 ContainerCreationIncomplete 结构化阶段信息，不再按最后的 4xx 判断整个操作无副作用。
- 缺失控制器 Restart 仍未完成：需要持久化原模板/卷绑定与条件重建，不能仅移除 absent 拒绝分支。
- 未新增或执行测试。

### 2026-09-20 生产逻辑进度：部分创建恢复

- Docker 新建 builder 的已确认部分结果可保存物理资源回执，交由后台恢复及高优先级控制收束。
- 故障阶段使用枚举；未知 start 写入保持保护，不把只读 inspect 当执行结束证据。
- 未新增或运行测试，缺失控制器重建及其他原任务仍未完成。

### 2026-09-20 生产逻辑进度：app-cli 控制循环与凭据恢复保护

- 缺少脱敏凭据阻止自动业务启动，不再让已确认目录的控制循环永久等待进程退出。
- owner 恢复保护在持久化前拒绝新业务操作，保留原操作重放及 Stop 内核核验；派发再检查保护。
- 原操作补交凭据接口仍未完成。未新增或执行测试。

### 2026-09-20 生产逻辑进度：Restart 原模板重建

- K8s dev Restart 在停止前私有归档原模板与卷身份；公开操作只持有引用。
- 停止后控制器丢失时，普通执行与 stopped 恢复入口可重建零副本控制器，检查 PVC、按原控制操作 CAS 继续启动。
- 尚缺：请求前已缺控制器且无历史归档、Docker 与 prod 对等路径、归档回收。未新增或执行测试。
