# Plan：实现与迁移设计

## 1. 已核对的源码事实

2026-09-17 原生扩展见 [native-desktop-runtime/plan.md](../native-desktop-runtime/plan.md)：包含 app-cli 与 file-server-proxy 的自包含依赖、原生布局/端口/实例协议和当前缺口。下面源码表仍保留首次方案的历史基线；当前开发同时核对 [R01–R11 审查](../development-review-2026-09-17/review.md)，不能将旧描述当作 HEAD 现状。

基线见 README；下表是定位入口，开发时重新核对符号，不将行号当作固定补丁坐标。

| 路径 / 符号 | 当前事实及影响 |
|---|---|
| `crates/file-server-userapp/src/handlers/userapp_dev_server.rs` / `START_DONE_WAIT_MAX_SECS`、任务执行段 | 等待常量 3600；调用方保留 evt_tx 和捕获 clone 的 on_event；source 与 artifact 两种构建路径 |
| `crates/file-server-userapp/src/service/userapp/start_events.rs` / `StartEventPipe` | 有序服务事件与 Done 同队列；超时只 abort 消费者，没有停止启动进程的证明 |
| `crates/file-server/src/service/dev_server/start.rs` / `start_dev_manifest` | legacy spawn、注入 dev profile、Child 句柄被 drop、3010/9080 固定、宽松探活 |
| `crates/file-server/src/service/dev_server/log.rs` / `spawn_log_pipe_with_events` | EOF/读取异常仅结束管道，不提供启动终结信号 |
| `crates/file-server/src/service/dev_server/stop.rs` / `stop_dev` | 登记加 ps 扫描，不是 supervisor program 所有权协议 |
| `crates/app-cli/src/main.rs` | legacy API 后台 bind 失败只记日志；supervisor Err 记录后返回 Ok |
| `crates/app-cli/src/supervisor.rs` / `start_pingap`、启动块 | 代理失败 `?` 跳过 Done；有清理；devrun 选择及探针策略存在于 builtin |
| `crates/app-cli/src/proxy/admin_probe.rs` | 当前确认预算 25 秒；配置 hash 确认不应替代入口就绪证明 |
| `crates/app-cli/src/server.rs` / `serve`、`try_accept_deploy_with_id` | 先恢复后 API bind；有串行受理、持久化、恢复保护，但不能仅凭有 ID 就认为支持重放 |
| `crates/app-cli/src/server_journal.rs` / `Journal::open` | 状态根由 workspace.parent 推导，source/.run 可能分裂锁域 |
| `crates/app-cli/src/supervisord_host.rs` | spec 用 run.command；固定动态命名空间；热切换前 stop_all，非增量服务部署 |
| `crates/file-server-userapp/src/service/userapp/run_dir.rs` | `.dev-prepare.lock` 与 serve journal 分离；file-server 激活 `.run` |
| `crates/app-cli/src/api/mod.rs` | `/v1/deploy` + status，token 鉴权、可选代次校验；无完整运行生命周期操作接口 |

## 2. 阶段一：独立错误传播修复

### 2.1 API bind 与 main 返回值

将 API 构造分为预绑定 listener/router 与服务 future。legacy、serve 先完成必要的只读校验/锁和 listener bind，再进行部署清理、恢复、migrate、业务启动。不能只调整 tokio::spawn 的位置：必须在主流程真正 await bind 结果。

serve 的旧 journal/ownership 保护继续保留。未取得所有权或绑定失败，不得标记旧 owner 为 Quiescent，不得执行 host.stop_all。运行中 API 服务任务异常也进入统一停止/保护流程，不能让不可控制的业务继续被新调用方误认成不存在。

“先 bind”不等于“先开放写受理”：初始化期管理 API 可返回 Initializing/存活状态，但修改端点在恢复检查和执行 worker 就绪前明确拒绝。legacy 不能宣告自己具备没有消费者的 hot deploy 能力。复核现有 LivenessHold/idle listener，避免自己重复绑定 3010。

legacy main 保存 supervisor Result，完成 API 与日志收尾后返回原始错误；组合清理错误保留原始 cause。成功启动后常驻不变。build/gen-lock 不因此绑定 API 或要求 daemon。

### 2.2 一个启动结果出口

抽出启动结果汇总职责，覆盖校验、API、PG/migrate、服务、静态监听和 pingap。启动尝试至多输出一个 Done；已有服务失败清单必须保留。清理之前可先发 Log/ServiceStartFail，最终错误附清理结果；清理未确认不能伪装为已停止。

对非常早期错误、SIGKILL、stdout 失效不承诺一定发事件；父进程监督独立兜底。不要只在 pingap 的 `?` 周围补一个 emit，遗漏其他失败路径。

现有 `orchestration_done {failed:[...]}` 保持可解析；新增结构化字段/类型须放 shared_types，检查旧 reader 是否容忍新增字段。若不兼容，阶段一使用已有 failed/error 字段传达诊断，并以独立内部类型保存完整信息，不强推不兼容 wire。

### 2.3 Child 生命周期和事件流监督

仅为 UserApp manifest 启动路径增加可监督的句柄，例如包含启动身份、子进程退出结果、stdout 完成结果、stderr 尾部、受控停止能力的内部 handle。具体命名由代码组织决定。不要把通用 Web/Vite 的所有行为顺便重写。

- 一个 worker 持有并 wait/reap Child；stop 通过该 worker 或同一登记协作，避免双 wait/双 kill。
- stdout worker 返回正常 EOF/读错误，并在返回前完成已读取事件的入队。
- caller 将回调移交管道，或显式释放 evt_tx 和原始 on_event；只 drop evt_tx 不足。
- stderr/log writer 有界收尾；错误尾部不能读取无界日志，也不能记录原始 token/环境变量。
- startup 消费器序列化服务进度、Done 与流结束；终态不得越过已入队进度。
- Child 退出与 stdout 末尾处理不在同一时序：收到退出后给管道短且有界的排空机会，再判定；后代继承 stdout 不能无限拖住等待。
- 已观察到编排进程在启动完成提交前退出，不能仅凭缓存 Done 成功。成功提交之后的服务崩溃属于运行健康变化，不追改已完成任务。
- 退出后无有效 Done：即便退出码 0 也失败，附退出码、最后阶段、脱敏 stderr 尾部。
- EOF/读错但进程仍活：启动通道异常；停止本轮已证明归属的进程，或明确保留未确认状态。不能忽略继续等 3600 秒。
- 取消消费者不等于取消启动。受理后取消要由 worker 处理停止/清理并给出证据，不能丢 handle。

终态至少区分：ConfirmedSuccess、StartupFailed、ProducerExited、EventStreamFailed、DeadlineExceeded、Cancelled、CleanupUnconfirmed（这是内部结果设计，具体 wire 保持兼容）。对最后一类，本实例不再允许新 start/restart 越过保护。

### 2.4 超时设计

不要把 finish 调用时刻误当成启动开始。记录 monotonic launch deadline，poll_alive、事件等待共享剩余预算，不在每层重新获得完整时长。

区分构建、启动执行、清理、HTTP 观察四种预算；构建仍受现有 build/dev command 配置控制。启动预算必须列出 PG 等待、串行 migrate/准备、并行服务 readiness（取最大值而非总和）、代理校验/确认与小幅调度余量。每个子步骤本身必须有界。

300 秒是候选默认底线，不是无条件硬上限：启动有效预算至少覆盖 manifest 显式声明的合法慢启动与其他阶段。建议 `max(300s, 已计算启动阶段预算)`，使用 checked arithmetic 和明确配置上限；显式配置小于必要阶段预算时在副作用前报配置错误。不能截断合法 600 秒服务为 300 秒，也不能悄悄恢复无限等待。

阶段一任务必须先提交预算清单和配置测试，再替换 3600 常量。若某步骤预算尚未理清，保留可解释的旧上限并标记该子任务未完成，不能凭“退出有监督”直接缩短。deadline 到期后进入有界清理；清理未确认继续保护该运行身份。禁止按端口清理外部实例。

## 3. 阶段二：单一所有者与共享内核

```text
平台：构建 + 任务/SSE       CLI：start/restart/stop/status
             \                 /
                 运行控制 API
                      |
        app-cli serve：受理 / journal / 执行 / 恢复
                      |
        source 运行计划 或 artifact 准备与激活
                      |
             builtin / supervisord host
```

### 3.1 身份和稳定状态根

由平台向 builder 提供 application_id、service_family、builder 物理实例身份和稳定 state_root。逻辑 workspace_id 单独持久化并绑定规范化 source_root，不使用 manifest.name 当唯一身份。run_root 根据 profile 选择，不用于推导锁位置。

state_root 必须位于保留卷、在有效运行目录之外。建议目录结构如下，实际挂载位置复用现有存储配置，不硬编码生产主机路径：

```text
<state_root>/identity.json
<state_root>/owner.lock
<state_root>/coordinator.json
<state_root>/desired.json
<state_root>/operations/<operation_id>.json
<state_root>/events/<operation_id>.jsonl
```

以上是逻辑布局，可复用现有 journal 文件实现，避免双份权威。锁文件永不 unlink；canonical 路径校验、目录身份绑定和 app 级单一配置阻止 source/.run/别名分裂。Linux 协作锁与当前文件锁机制优先复用，不新增 unsafe。

单靠持久卷锁不能证明网络分区时旧远端写入停止。继承平台已有 builder 物理身份和恢复保护；跨 Pod 接管必须有旧执行停止证据，不做 TTL 抢占。保持 file-server kube-free，由 rcoder 传入或适配 shared_types 契约。

进程每次启动生成新的 runtime_instance_id；持久运行代次 deployment_generation_id 可跨正常重建延续。旧实例请求一律不能修改新实例。不能将两种 ID 合并。

### 3.2 协议建议

新增端点名称为本计划设计，当前尚不存在。共享请求/响应/错误放 `shared_types::userapp`，HTTP 用 utoipa 注册完整 OpenAPI。

| 端点 | 行为 |
|---|---|
| `GET /v1/runtime/identity` | application/workspace/family、物理绑定、runtime_instance、generation、协议版本和 capabilities |
| `GET /v1/runtime/status` | desired、observed、当前有效目标、revision、active operation、恢复保护；管理存活不等于业务 ready |
| `POST /v1/runtime/operations` | 异步受理 start/restart/deploy/stop，返回 202、操作 ID、查询地址 |
| `GET /v1/runtime/operations/{id}` | 读取指定操作，不能用“最近一次操作”代替 |
| `GET /v1/runtime/operations/{id}/events?after_seq=N` | SSE 或分页事件重放，游标带实例/操作上下文 |
| `POST /v1/runtime/operations/{id}/cancel` | 请求取消指定操作，返回受理状态，不伪称立即取消完成 |

修改请求必须带 operation_id、expected_runtime_instance_id、expected_revision、workspace_id、kind、profile、目标输入；服务端规范化后计算 request_digest。平台生成的 ID 在网络重试前固定，不能每次重发换 UUID。

原 `/v1/deploy` 保留兼容适配，最终调用同一内核。保持现有部署段成功与业务 Running 的差异；不要让冷部署原本的阶段等待偷偷改成完整启动等待。现有 generation/operation/persisted 契约继续有效。

管理写接口保留 token 鉴权，identity/status 不回显 secrets。端口监听成功或返回一个自报 workspace 不足以认领；绑定配置、实例身份和能力须一起核验。root 可读取 token 是现有信任边界，不能夸大安全保证。

### 3.3 原子受理、重放、并发与 stop

在短受理锁内：鉴权/身份 → 查 operation 历史 → 对比请求摘要 → 检查 revision/恢复保护 → 持久化记录与意图 → 排队；实际 I/O 执行不持 map guard 跨 await。

- 同 ID 同摘要：返回既有记录，不再执行。先查重放再拒绝 busy。
- 同 ID 不同摘要：409 `OPERATION_ID_CONFLICT`。
- 新的 start/restart/deploy 遇 active operation：409，携带进行中操作 ID。
- 记录落盘失败：不入执行队列。落盘成功但回复丢失：查询原 ID。
- 事件和完整日志可以后续归档，但操作去重摘要不能随意淘汰。首版不自动删除操作去重记录；未来做 GC 必须同时定义拒绝旧请求的 epoch/tombstone 机制。
- 进程重建后可只读查询旧操作；任何继续/恢复执行必须重新验证新实例所有权，不能重放旧实例的写请求。

操作受理、desired/revision、结果和事件记录跨文件时不能假装多次 rename 是原子事务。优先用现有 journal 扩展为单一可恢复提交记录：记录提交序号和操作，再从它派生快照/事件；启动重放能修复已提交但未发布的事件。若采用多文件布局，必须定义提交标记、fsync/目录同步顺序和截断尾部恢复规则，并在每个崩溃点测试；没有提交证据的记录不授权副作用。

stop 是特殊意图屏障：允许在 active 启动/部署期间受理一个持久化 stop 意图，并增加 revision；执行层仍只有一个 worker。它请求取消当前操作，等待实际准备写入/子进程停止后，再完成 stop。重复 stop 幂等；其他修改在 pending stop 时拒绝。不允许另起并发 stop worker 与激活抢写。

平台构建开始前捕获 runtime_instance_id/revision；构建后提交携带同一预期值。stop 或其他控制操作修改 revision 后，旧构建提交被拒绝。revision 检查失败不能自动获取新 revision 后重发原启动，否则会绕过用户 stop。构建可产出未部署包，但不获启动授权。

cancel：准备期等待真实 writer 结束后取消；切换期请求取消进入受控收束，不假定回滚；已成功操作取消返回既有结果，不等价于 stop。明确 stop 成功后才确认业务停止。

### 3.4 source 与 artifact 共用运行计划

抽出共享解析器，生成 ResolvedRunPlan：命令、cwd、env、端口、依赖顺序、static/dev server 选择、migrate 策略、readiness 策略、proxy 计划、profile 与计划摘要。

- source：file-server 保留 `[devbuild]` / `[devrun]` / `[build]` 分派；serve 使用源码根；devrun 优先、run 兜底；不以生产 static hosting 遮蔽 devrun。
- artifact：build 输出不可变 zip；serve 接受经校验的 artifact 引用，自己准备、停止确认、激活 `.run` 并启动。
- source 模式的 release lock 准备/运行计划验证最终由所有者执行，避免 file-server 绕过修改运行契约。普通 CLI build/gen-lock 与运行目录写入需明确边界，不得写 serve 管理的 active artifact。
- builtin 与 supervisord 都消费同一计划，不复制 devrun 条件判断。
- 业务端口占用不得用旧 TCP/HTTP 响应判新服务成功。就绪须绑定该轮启动进程和配置；至少验证受管服务存活、绑定归属及入口，不能只探 localhost。
- 开发源码会被用户随时编辑：配置与运行计划在操作内固定并记录摘要；需要写入的共享构建/配置步骤通过共同 workspace 锁串行。任意 root 外部写入不在一致性保证内。
- migrate 是否运行保留现有 restart/deploy 语义并显式记录；不顺便改成跳过或无条件重复。失败不宣称数据库已回滚。

本地制品传递优先增加受限 artifact_id 解析器：只解析登记的构建输出，打开后复制/校验到所有者持有的 staging，避免 hash 校验后源文件再被替换。不暴露任意路径激活接口。生产 URL 下载作为另一 input adapter，复用准备/激活事务。

### 3.5 状态切换与目录所有权

正常：Accepted → Preparing → Stopping → Activating → Starting → Succeeded。

准备失败保持旧运行态；停止未确认进入 RecoveryRequired，不激活；激活后启动失败保留证据，不自动恢复旧业务。与现有 D 系列 journal 边界对齐，不新增一套冲突的自动回滚逻辑。预提交的原子目录 rename 失败可回退文件交换，但不能把它等同于业务/数据库回滚。

file-server 的 prepare_run_dir/activate 最终退役或变成只产出独立 staging 的适配器，不能写有效 `.run`。hygiene、workspace clear、模板解压、导入、构建 deploy_dir、proxy reload、gen-lock 等旁路要逐一清点：影响受管运行态者进入同一内核/工作区排他协调，或对受管目录明确拒绝；不能留下第二个 writer。

### 3.6 事件和任务关联

事件记录包含 operation_id、sequence、runtime_instance_id、stage、service、payload。每操作单序列，先落盘/有序记录再发布；终态和结果持久化之后才让订阅者看到完成。日志不是事实记录，日志丢失不能消除终态。

file-server 保存 task_id 到 operation_id 的关联，重启可恢复观察；不必引入 PG，复用允许持久化的本地状态根与已有任务设施。慢订阅不阻塞执行 worker；断线后从 cursor 重放，禁止静默丢事件还声称完整。越界/过期 cursor 给出明确错误和状态查询路径。

前端保留 service_starting/service_start_ok/service_start_fail/Completed/Failed 等事件名。已运行 start 返回当前 active target，本轮未部署包另列，不能把新构建 ID 当生效 ID。成功必须匹配操作、持久化、目标配置及健康；只有 `/ready=200` 不够。

## 4. supervisord、CLI 和恢复

- builder 镜像提供固定控制 program（如 `rcoder-app-runtime`，仅建议名），以稳定 state_root 启动；不以 manifest.name 命名控制者。
- 空 workspace 进入管理 Idle，不因未创建应用而 crash loop。supervisord 仅重启控制进程，业务 autostart=false，由 serve 服从 desired 重新生成配置并启停。
- 固定 program、动态业务组及 spec 文件必须有明确 owner/generation 归属；不能只凭 `app-svc-` 前缀就误停其他实例。首版每 builder 一个受管应用，显式拒绝第二身份。
- `serve` 默认竞争唯一控制权；已存在同身份时明确退出“已有实例”，不修改现有运行态。新增显式 `serve --attach` 供自启脚本前台附着：验证身份、等待既有控制者，控制者断开后重新验证，不能自行抢占未知新实例。附着进程收到信号不替代业务 stop。
- 固定控制 program 使用真正 owner 模式；重复用户脚本可改为 attach 或不对“已有实例”状态 autorestart。迁移不能自动改任意用户脚本。
- legacy 无子命令在阶段一保持兼容。阶段三在已启用统一模式的 builder 中转为控制客户端/附着兼容入口；显式 standalone 保留于未托管环境，同样竞争稳定锁且冲突 fail-fast。不允许无子命令绕过模式锁启动第二套。
- 启动恢复先获得所有权并 bind，再依据 journal 证明旧执行停止；Stopped 保持 Idle；Running 恢复有效目标；不明确的切换/损坏记录保持 RecoveryRequired。控制进程正常退出不自动写 Stopped。
- 旧 journal 从 parent 推导迁移到显式根必须一次性、有版本、持有旧新必要锁并确认无旧 writer；不能复制后让旧程序继续写旧根。跨 Pod 恢复延用已有平台保护，不凭文件时间戳接管。

## 5. 阶段三：平台迁移和灰度

1. 先发布阶段一：app-cli 和 file-server 两侧分别升级验证，不能只更新 npm 就宣称 builder 已修复。
2. 阶段二完成两引擎契约验证，先在全新隔离 builder 启用控制 program。
3. 灰度模式是应用环境级持久配置：legacy 或 managed，禁止按单请求失败动态切换。
4. 迁移前暂停该应用变更受理，核验 legacy 进程/用户 supervisor program/有效目录归属；只停止已授权且已确认归属的旧 writer。未知 program 返回迁移阻塞，不按端口强杀。
5. 停止确认后迁移状态根和有效版本，明确 desired state，启用控制者；验证身份、业务内容和查询，再开放受理。
6. 平台 start/restart/stop/list、任务 cancel、清理、恢复、保活若存在都切同一协议。对每个入口建立调用图并证明旧 spawn 不再可达。
7. managed 环境 403/超时/缺能力/身份冲突返回准确错误；不能当不存在，也不能旧 spawn fallback。
8. 回退需维护窗口：停新受理、收束操作、停止并确认新控制者/业务退出，核验旧版能理解的目录和记录后显式切模式。未知执行或新 journal 不兼容时禁止自动回退。

## 6. 风险与设计取舍

| 风险/代价 | 处理 |
|---|---|
| 常驻控制者成为关键进程 | supervisord 恢复 + 持久化结果；API 故障受控收束，不启动竞争实例 |
| 协议/记录增加复杂度 | 复用当前 journal/admission，分模块；不引入新分布式锁或数据库 |
| hot 更新有短暂不可用 | 首版整组切换，明确停服窗口；增量/零停机后续独立设计 |
| 旧脚本依赖 legacy 阻塞行为 | 显式托管模式和 attach；灰度迁移，不直接将所有 serve 改成打印退出 |
| 原地 source 内容可变 | 运行计划固定、受管写锁、当前源码恢复说明；不虚构制品级可重现性 |
| 记录持久化失败/磁盘问题 | Fail Fast，接受和终态不谎报；恢复保护保留 |
| 管理端口与其他 root 服务冲突 | 提前识别、明确报错；没有按端口驱逐策略 |
| 跨仓内并行修改 | 小范围 diff 与独立提交；不覆盖 builder/preview 现有工作 |

验收和分阶段执行以 tasks.md 为准。新增参数/协议字段须在实现提交中补入对应配置文档和 OpenAPI；本计划中的拟议接口不代表已经存在。
