# UserApp 生命周期收敛规范

状态：用户已批准实施；任务完成与验证证据分别登记在 tasks.md。

## 目标

解决真实 K8s 三副本验收中的首次 PVC claim 冲突、错误分类丢失、purge 元数据假冲突与操作锁残留，并使同类跨进程问题可恢复、可验证。

## 行为不变量

1. app_id 是业务定位键；lifecycle_id 是应用存在周期的身份，仅彻底删除后显式重建才改变；metadata_revision 只用于业务字段并发更新。均不复用 release_id、K8s resourceVersion 或 app-cli deployment_generation_id。
2. 同 owner 的重复 workspace ensure 不创建新生命周期，不覆盖不属于该命令的元数据字段。不同 owner 的普通 ensure 拒绝；归属迁移需要独立命令。
3. 归属、生命周期状态、删除许可由权威存储判定。缓存不能否决已确认提交的删除，也不能允许 Deleting/Deleted 状态接受旧写入。
4. 同一应用的冲突生命周期操作统一受理；重复同操作可查询/等待原操作，不重复执行。长任务的取消/结束协议仍需与 purge 协作。
5. 远端创建、更新、删除绑定真实资源身份。resourceVersion 冲突仅在 UID/生命周期/归属不变时有限重试。
6. HTTP 超时不代表操作失败或取消。操作状态必须区分失败、处理中、结果待核实、完成；既有同步接口不得提前返回完全成功。
7. purge 持久化意图后再产生副作用；逐步记录进度，重复推进幂等；不能确认远端写入结果时阻止竞争的新操作。
8. 权威存储不能确认安全前置条件时，破坏性操作在产生副作用前拒绝。普通展示元数据可保留显式降级，但降级补写也受生命周期前置条件约束。
9. Deleted 保留旧生命周期墓碑，普通旧 ensure/补写不能恢复它；显式新建生成新 lifecycle_id。墓碑不按任意短 TTL 清除。
10. 跨 crate 的领域类型、命令和结果定义在 shared_types；生产实现无 unsafe、无 unwrap/expect，不持 DashMap guard 跨 await。

## 范围

统一覆盖 builder ensure、owner 登记、生产 create/update/start/stop/hot 控制受理、分级 delete/storage destroy 和整应用 purge。开始实施前固定每个入口的实际删除范围矩阵；普通 prod delete 不得误升级为整应用 purge，agent PVC 永不进入删除路径。

保留当前正式 HTTP 路由、参数位置、HTTP 200 + HttpResult 和同步成功含义；允许增加操作状态查询与可选显式 request_id，但不强迫现有调用方立即迁移。保留容器内 hot 行为、现有部署身份协议与已删除的容量/条目限额。

## 非目标

本轮不引入 CRD/独立 Operator、Kafka/Redis/Temporal、跨 K8s/PG 两阶段提交或完整事件溯源。复制控制器的持久化意图和幂等推进思想，不复制整套平台。不能承诺任意未知远端写都能自动、安全地接管。

## 已批准的存储与首开契约

- Compose 单 rcoder 使用 SQLx SQLite，仅持久化 userApp；K8s 使用 PG，Agent 存储配置独立。
- SQLite WAL/FULL/foreign_keys，5 秒忙等待，初版单连接；事务内不执行 runtime I/O，失败不降级为内存成功。
- 同身份同配置 ensure 等待同一操作，默认 90 秒总预算；类型冲突、删除和不确定操作不能当创建重试。
- 锁消失或探活通过不能代替操作结果、资源归属和生命周期校验。
- 三份 Compose 挂载独立数据目录，数据库目录永不进入 userApp purge；正常上传无限额，错误收尾至多 1 MiB/1 秒。

## 配置操作与回收策略一致性

- 创建、配置更新和单独策略修改均以已成功提交的策略为查询与重启恢复依据；旧策略不得覆盖已成功应用的新配置。
- 创建或更新受理后、远端执行期间，以及失败或结果不确定时，目标策略不提前成为已生效策略。目标策略与操作记录一起持久化，仅在同一成功终态事务中合并生效。
- 配置更新省略的策略字段保留已有值；目标策略记录不能替代资源命令和物理身份检查，不能单凭该记录授权重放远端写操作。
- 成功提交更新后释放本操作的资源标记；结果不确定时仍保留保护边界。

## 恢复扫描公平性

- 单个应用恢复缓慢或失败不得阻止其他应用恢复；本进程同时执行的恢复数量有上限，同一操作不得重复进入执行队列。
- 正在执行和结果不确定的操作不得因为扫描、超时或本进程队列空位而获得新执行权；授权仍以持久化条件认领和资源身份为准。

## Builder 等待总预算

- 公共普通 ensure 与探活 ensure 的总等待预算覆盖锁等待、归属/元数据查询、注册表与 runtime 校验、操作受理、完成轮询及返回前的身份复核。嵌套步骤不得重新计算预算。
- 截止时间已过时不再开始新的等待工作；等待者结束不撤销已经受理的共享创建或其执行租约。

## Builder 等待错误协议

等待预算耗尽使用稳定错误码 `ERR_USERAPP_WAIT_TIMEOUT`，不意味着共享创建已取消或失败。已知操作身份时返回可查询的 operation_id。互斥冲突仅通过结构化错误识别，不能根据字符串或任意 HTTP 409 推断为可加入的创建。旧 TS 转发入口保留网关状态，正式 userApp 路由继续使用 HTTP 200 信封。

## Purge 开发资源凭据

任何包含开发资源清理的 purge 在产生副作用前，必须持久化生产资源快照、开发工作负载/存储资源快照，以及当时注册表的代次与物理容器身份。捕获对象必须属于目标应用。缺少旧凭据不能授权按逻辑名称重新选择资源删除。

## 删除检查点身份绑定

完整删除检查点必须同时记录格式版本、控制操作上下文、删除范围以及对应资源凭据。缺少生命周期、执行者或必要资源版本时不得作为自动恢复依据；旧检查点禁止以当前状态或默认值补齐身份。

## 删除完成边界

计算资源消失、生产存储消失、开发清理完成分别确认并持久化，不能凭 DELETE 受理成功记录资源消失。查询失败或物理 UID 变化阻止后续清理。后续步骤失败须保留原始凭据和最后已确认阶段。

K8s 删除工作负载采用前台级联，避免仅确认 Deployment 对象消失就继续清理仍被旧 Pod 使用的存储。UID/resourceVersion 条件仍须随实际 DELETE 发送。

删除阶段顺序、证据不可替换和终态前置条件由持久化层同时强制执行；任何调用方都不能跳过确认阶段直接提交成功。失败或结果不确定的记录必须保留上一份证据。

## 删除期间的流量保护

删除开始后，计算面消失不代表整个 purge 已完成。唤醒阻断保留到完整成功终态提交；不确定删除失败不能根据旧状态恢复流量入口。删除通知须保留给已取得唤醒句柄但尚未订阅的调用方。

重启恢复活动状态时，持久化 Deleting/Deleted 身份和未完成的删除操作优先于运行时列表与唤醒注解。计算资源不在列表中也不能丢失未完成 purge 的流量阻断；当前操作记录缺失或不一致时启动失败，不伪装正常状态。

生命周期及其当前操作用于恢复判定时，必须来自同一数据库快照。跨副本的正常终态提交不能因两次读取的时间差被误判为存储损坏；真正的断链仍应报错。

本地唤醒阻断不能永久否决其他副本已提交的显式启动或策略变更。唤醒必须在操作锁内验证持久化生命周期和当前策略；只有受理及物理身份检查成功后才能清除旧本地标记。已提交的主动停止不能被流量覆盖。

## 唤醒成功不能由缓存授权

`ensure_running` 即使观察到运行中的副本，也必须经过权威应用身份、当前生命周期和资源身份校验。远端副本数的 TTL 缓存仅用于路由探测，不能证明手动停止已解除或应用仍属于有效生命周期。

恢复并发预算覆盖已交给后台的 builder 执行任务，直到执行及结果提交结束才释放调度槽位。观察者取消不得取消已经受理的创建任务。

## 转发定位与错误收敛

开发转发的总截止时间覆盖初次 ensure、探活、真实状态验证与必要的再次 ensure。过去一次不存在的观察不能授权清空新注册及其连接。生产查询失败不得报告为权威不存在；已经确认应用存在但地址暂不可用时返回暂不可用。

## 显式重试的前置条件

重试必须提供应用归属、生命周期及预期操作版本，复用原操作和原输入。已成功操作返回已有结果；执行中或结果不确定不能由重试授权接管。没有持久化可执行输入的操作不得靠当前配置推测重放。

删除类 Pending 操作必须保留原始删除范围和预期资源版本。恢复不得把仅删计算扩大为清理存储或结束生命周期；恢复执行同样先持久化身份化资源快照，再推进删除检查点。

完整删除受理后应用为 Deleting；只有同生命周期、当前关联的原 DeleteApplication 操作可以继续执行删除。开发资源清理失败时必须保留删除状态和最后确认的检查点，不能转成 Deleted 或因普通重试再次执行。

## 独立存储销毁

独立 storage/destroy 校验调用者归属、生命周期和确认字段，先持久化操作与资源凭据再清理。dev 销毁整个开发环境；prod 保留现有契约，清生产存储及开发资源，不删除运行中的生产计算，也不结束应用生命周期。重复请求保留原操作身份且不重复清理。SQL 不允许跳过确认步骤提交成功。

内容清空仅把根目录明确不存在视为幂等成功；权限、遍历或删除错误必须上抛。根目录不得是符号链接，子链接仅删除链接本身，保留根目录和链接指向的外部内容。本地文件操作助手不替代生命周期租约及拥有者路径校验。

开发工作区清空必须等待构建/启动 worker 实际退出，不能用 Cancelled 事件代替执行结束。清空期间禁止该工作区的新构建、启动和 app-files 写入；停止进程失败时不删除内容。HTTP 观察者取消不释放仍在执行清空的工作区租约。

清空协调覆盖正式文件写入、项目导入/确认、目录初始化及受控命令和安装执行。命令/安装 HTTP 观察者退出不视为进程执行结束，工作区租约由实际执行 worker 持有。

平台 storage/clear 与 destroy 使用不同操作类型；保留清空内容的范围，不因统一受理而扩大删除范围。clear 必须验证归属和生命周期、记录目标与完成检查点、关联 request_id/operation_id；不完整或失败的容器响应不能当作完成。

开发资源票据一旦用于外部写入，不能在结果不确定时按只读捕获自动释放。必须收到明确完成确认并成功释放租约；释放失败也要作为操作失败报告，不能返回完全成功。

开发工作区 clear 内部请求必须携带预先观察到的 file-server 实例身份。容器进程重启或地址切换导致实例身份变化时，在任务停止和文件删除之前拒绝。成功响应回显相同实例身份，缺失字段不能当作兼容成功。

### Workspace reset acknowledgement and concurrent writers

The internal app-files reset protocol requires a shared, explicit success acknowledgement containing the observed process instance. Missing identity, a negative result, an unrelated instance or an HTTP error envelope cannot confirm reset. Template extraction and skill installation join the same per-application workspace activity lease as build and development tasks; accepted writes retain that lease until actual worker completion even if the request observer disconnects.

### Physical workspace endpoint binding

Development storage clear must inspect its captured builder under the resource lease, verify application/owner/lifecycle and actual workload identity, and connect to a direct container or Pod IP. An HTTP target probe is accepted only between matching uncached runtime observations. Missing identity, another service family, replacement workload, non-ready Pod, query error or changed physical endpoint prevents the clear write. HTTP redirects and environment proxies cannot change this target. The process-instance echo still fences replacement after observation.

### Captured filesystem roots

Storage clear persists each selected directory device/inode or its absence before effects. Execution separately retains an open directory handle, preventing inode reuse and path replacement from redirecting the write. A deserialized receipt does not grant execution authority. A newly created root after captured absence is a conflict. Unix traversal uses safe descriptor-relative operations, does not follow child symlinks, and retains the root; non-Unix identity-bound clearing fails explicitly until a matching implementation exists.
