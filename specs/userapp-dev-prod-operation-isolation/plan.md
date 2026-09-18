# UserApp dev/prod 操作隔离实施方案

## 1. 现场依据与现状

2026-09-18 只读取证确认：app 104 的 prod 流量唤醒操作 `a49b2fbd-fad4-4d85-97a2-cf740145fe1c` 于北京时间14:14创建，后端 PostgreSQL 认证失败导致应用启动失败，14:15唤醒超时进入 RecoveryRequired；16:20 dev builder 被空闲回收，17:04 dev restart 收到409。两个 PVC 独立且保留。此处为历史取证，不代表实施时现场仍相同；不得据此直接操作现场。

源码锚点（实施前重新核对）：

- `crates/shared_types/src/userapp/lifecycle.rs`：UserAppLifecycleRecord 只有 current_operation_id；UserAppControlSnapshot 只有一个 operation。
- `crates/rcoder-storage/src/userapp_lifecycle/domain.rs:107,144,160,242`：受理检查、占位、advance 校验、终态清理均依赖单指针。
- `crates/rcoder-storage/src/userapp_lifecycle/postgres.rs:46–47`：应用行锁和单 current_operation_id JOIN。
- `crates/rcoder/src/userapp_builder/control.rs:55–90`：RestartBuilder 经统一 admission；169行拒绝不存在的 builder。
- `crates/rcoder/src/userapp_builder/mod.rs:150–166`：已有 builder 可在无关 prod 操作期间服务，但重新创建仍被统一 admission 拦截。
- `crates/app_manager/src/lifecycle/wake.rs:54–120`：prod 唤醒受理、checkpoint、失败收束。
- `crates/rcoder/src/userapp_builder/recovery.rs:185–213`：仅具备最终证据的 Running/RecoveryRequired 自动收束；不能把新范围隔离变为放宽恢复证明。
- `crates/rcoder/src/cleanup_task/container/destroyer.rs:99–105`：通用清理直接调用运行时停止；需追踪 builder 上层及运行时保护，统一纳入 dev 协调。
- Java `ComputerPodClient.java:274–294` 的 parseClientErr 只保留 code/message，丢失 operation_id；后续领域层、响应模型也需核对。

**不能只改 admission 的 if。** 否则两个操作覆盖一个 current_operation_id，完成时互相清除或无法推进，是新的数据一致性缺陷。

## 2. 持久模型：共享生命周期 + 三个操作槽位

建议最小可落地模型：新增 `UserAppOperationScope { Dev, Prod, Application }`，操作记录持久化 scope；应用记录使用明确的 `active_operations { dev, prod, application }` 三个可空 operation ID，替代单 current_operation_id。名字可按项目风格调整，语义不可缩减。

- active_operations 不应含可随意扩展的字符串键，避免未知环境被默认接受。
- 同一应用的短数据库事务仍串行锁住应用行，原子检查冲突矩阵并写入操作记录/槽位；外部执行在事务外并行。
- PG 复用 SELECT FOR UPDATE；SQLite 使用既有写事务串行保证；内存实现同样保持原子性。不能拿进程内锁代替 PG 协调。
- advance 以 app_id+lifecycle_id+scope+operation_id+revision+executor 校验所有权；终态只清自己的槽位，且在同一事务内提交。不得覆盖另一环境同时提交的状态。
- application 操作要求三个槽位均无活动操作；其自身占 application 槽。DeleteApplication 受理与 Deleting 状态同事务提交，继续阻止两边新操作。
- 状态查询返回两个环境和整体范围的操作快照。不要在“当前操作”接口任意选最新一条；旧单值接口增加明确环境或新增多槽快照，并同步调用方。
- 聚合字段（metadata、运行策略、应用状态）按实际所有者合并更新，禁止旧应用对象整体覆盖新字段。prod runtime_policy 只由符合条件的 prod 成功操作提交。

### 请求幂等

本轮优先保持现有 request_id 唯一性范围，不因引入 scope 静默复用旧 key。fingerprint 与 duplicate 校验包含 scope；相同 request_id 换环境/换命令明确拒绝。相同环境同请求重放返回原操作，不能重复产生物理写。以后需要按环境复用 request_id 再做独立契约升级。

## 3. 服务端操作归类

先对每个 kind 的全部调用点、runtime 写集合进行审计，形成可执行的穷尽映射，禁止通配默认 Prod。

| 候选范围 | 操作 |
|---|---|
| Dev | EnsureBuilder、AdoptBuilder、StopBuilder、RestartBuilder、DestroyDevStorage、ClearDevStorage |
| Prod | StartDeployment、RestartDeployment、Start（含流量唤醒）、Restart、Stop、SetRecyclePolicy、HotDeploy；Create/Update 在确认只改变生产资源后归入此域 |
| 需根据实际资源明确 | DeleteCompute、PurgeResources：名称不能证明只影响 prod；逐调用链核实，不明确则 Application |
| Application | DeleteApplication、生命周期换代及任何同时更改两环境/共享权威资源的操作 |

纯创建应用身份仍是短事务操作，不需要变成长时间环境锁；Create 若还创建生产计算资源，拆清身份初始化与资源执行的职责。

检查部署是否在线读取 dev 工作区或控制 builder：若会，应先在 dev 范围内生成不可变制品，再在 prod 范围部署；不能标成 Prod 后让 dev restart 破坏其正在读取的数据。真正跨域变更应使用 Application 范围。共享名称/元数据的短事务更新不自动等于长期跨域资源写。

现有 affects_builder 漏含/独立维护的概念要统一：包含 ClearDevStorage 与整体操作的保护语义，不要继续用互相不一致的硬编码名单。

## 4. 运行时、本地锁与后台路径

1. 排查 `userapp_builder/lifecycle.rs`、app_manager 本地锁、runtime operation receipt 的键。环境内执行锁应按 app+scope；全局保护由数据库 admission 原子保证，不能用两把进程锁伪造跨副本整体排他。
2. 核实既有 prod/builder runtime 租约确实不同，receipt 的资源家族、操作身份与 scope 一致。重放/释放必须验证完整 receipt；不因改了 scope 删除旧锁。
3. 所有变更入口用同一协调：HTTP restart/stop/ensure、流量唤醒、闲置回收、cleaner/reaper、启动扫描、存储清理。builder 回收应受 Dev 操作保护；Application 删除保护仍优先。
4. RecoveryRequired 仅阻断它实际拥有的资源域；Application 的未知结果阻断两域。恢复扫描按 operation ID 遍历，不能依赖“每 app 仅一个”假设；同 app 两环境都要被扫描且不能饥饿。
5. 完成证据、身份校验和未知写入保护保持原标准。时间过去、Pod 不存在、另一环境正常都不是释放未知操作租约的证据。
6. restart 请求绑定的物理 UID/代次、生命周期和操作者身份继续贯穿；不能因为有 app_stage 就按名称盲删容器。

## 5. 迁移与部署约束

本轮推荐**维护窗口内一次性迁移，禁止旧新协调器混跑**，不为此引入昂贵的双写协议。

1. 盘点所有持久后端、JSON记录、唯一索引、快照接口及历史缺command记录；备份必须可恢复且不泄露私密部署输入。
2. 先停止接收新的变更请求，排空可完成操作；停止全部旧 RCoder 执行者/后台扫描器后迁移。停止进程不证明在途 K8s 写已经结束，未知记录继续保留保护。
3. 旧记录缺scope时按已核实的 kind/command/资源家族推导。旧current指向操作填入相应槽位；历史终态不占槽。active指针悬空、身份冲突、未知kind、无法确定范围时阻止迁移或显式保守Application保护并输出诊断，不能当作无锁。
4. 旧prod RecoveryRequired 必须保持其operation ID、revision、checkpoint及runtime receipt，仅归属到Prod槽。dev的新操作不能清掉它。
5. 校验新增模型不变量，标记schema版本；新程序拒绝无法识别/混合损坏数据。不要给新scope直接 serde default=Prod。
6. 全部部署新协调器后才恢复流量。同步修改开发和生产镜像来源、数据迁移工具与部署手册；本任务不授权执行现场迁移。
7. 回滚不能直接把双槽数据交给旧程序。只有所有活跃槽安全收束、没有并行活动记录且完成显式降级转换后才可回旧版本；否则保持新schema并向前修复。备份恢复不能在外部资源已变化后盲目执行。

可使用新增版本化active_operations字段承接JSON演进；是否增加独立SQL列/表由现有存储封装决定，但必须同时覆盖 PG、SQLite 和测试后端，不能只改Rust结构体。

## 6. API 与 Java

- `/computer/pod/restart` dev分支保持现有正常语义，不改为普通agent路径。app_stage用于目标定位，服务端校验其与kind一致。
- 冲突响应增加结构化 blocker 信息（scope、operation_id、kind、state、step），错误message保持英文。没有权限读取的内部信息不泄露。
- 保留既有operation_id；Java RPC→领域→统一响应全过程透传。Java工程位于 `/Users/soddy/Documents/git-workspace/agent-platform`，若本轮仅负责Rust，写清Java待办，不能声称端到端提示已完成。
- OpenAPI、GET当前操作/按request查询、SDK/示例/测试同步。只读接口返回全部活动范围，明确单环境筛选规则。
- 不存在builder的strict restart返回明确不存在；ensure可在Prod未知时独立创建。此时frontend先恢复环境的交互可单独实现，不能把它伪装成重启成功。

## 7. 必须覆盖的反例和验证

先新增能在旧实现上失败的用例，再改实现。

1. Prod RecoveryRequired + Dev RestartBuilder受理执行：Prod记录/租约不变，只重启捕获的builder UID，PVC不删。
2. Prod RecoveryRequired + builder不存在：Dev EnsureBuilder创建成功；strict restart仍明确不存在。复现本次“prod失败→dev回收→再次进入”的完整链。
3. Dev RecoveryRequired 不阻塞独立Prod启动/停止；同域第二个操作冲突。
4. 两个独立PG store/执行者同时请求：跨域各一成功；同域单胜者。不能只测试同进程共享Mutex。
5. 两域并发终态提交分别清各自槽位，不丢metadata/runtime_policy更新、不误释放对方receipt。
6. DeleteApplication与dev/prod受理竞态：整体操作与环境操作不可同时持有；整体RecoveryRequired阻止两域。
7. 请求ID重放、跨环境误复用、生命周期换代、旧UID/旧revision回调均按契约处理。
8. 旧JSON迁移含Pending/Running/RecoveryRequired/终态、缺command、悬空指针、未知kind；重复迁移幂等且失败不半提交。
9. 执行者退出后扫描恢复：同app两域均可发现；无最终证据不自动释放；终态租约扫描仅释放对应租约。
10. cleaner与Dev restart并发、整体删除与回收并发，不得旁路修改或删除新实例。
11. Java透传409结构化详情；无操作冲突的原dev/prod正常流程不退化。

组件测试优先 nextest：先聚焦 shared_types/rcoder-storage/app_manager/rcoder/docker_manager 的实际受影响包，再覆盖默认Docker和kubernetes feature；按AGENTS完成fmt/clippy。实际支持的feature名从Cargo.toml核验，不抄不存在的参数。

Compose及真实K8s分别验证独立资源并发、回收后恢复、整体删除阻断和PVC保留。使用独立测试应用，不动现场app104。K8s使用项目remote-k8s入口，PG多执行者验证不能被smoke替代。人为制造的prod失败可用受控应用，不需要伪造AI回应。

## 8. 实施顺序与交付

1. 调用点/操作资源矩阵、存储迁移设计与反例；发现跨域依赖先修订归类。
2. 类型/存储/状态机/迁移；随后消费者和运行时/后台路径，同一发布整体交付。
3. 状态查询/错误契约/Java透传；再组件、Compose、K8s验收。
4. verification.md记录每仓基线、命令退出码、实际覆盖与阻塞，分别说明实现/验证/部署状态。

本轮设计不授权push、发布镜像或修改远端数据库；实施者保留无关工作树改动。原 `.zcode/plans/pod-restart-design-issue.md` 的“删除app_id/force绕过”的建议应标记为已被此方案取代。
