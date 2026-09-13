# UserApp 生命周期收敛实施方案（调研提案）

状态：已批准实施，实际证据见 tasks.md。2026-09-13 核对当前代码与官方资料。

## 1. 社区依据与选择

- Kubernetes client-go `RetryOnConflict` 每次重新读取对象，只对明确冲突按退避策略重试，非冲突错误原样返回：[官方 API](https://pkg.go.dev/k8s.io/client-go/util/retry)。本项目使用 Rust kube 4.2，采用等价的局部 helper，不引入 Go 依赖。
- Kubernetes 控制器通过反复观察/推进状态工作；删除 finalizer 在清理结束前保留对象：[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/)、[Finalizers](https://kubernetes.io/docs/concepts/overview/working-with-objects/finalizers/)。[kube-rs](https://kube.rs/controllers/intro/) 也采用 reconcile/error_policy。此处借鉴流程，不把 PG 元数据清理简单挂到现有 PVC finalizer。
- client-go 明确说明 leader election 不保证 fencing：[官方说明](https://pkg.go.dev/k8s.io/client-go/tools/leaderelection)。Lease 超时或 PG 锁断开，不证明旧执行者已停止远端写。
- PG 行锁/事务级 advisory lock 随事务结束释放：[PG17](https://www.postgresql.org/docs/17/explicit-locking.html)。使用短事务处理受理和提交，不在 SQL 事务中等待容器启动或删除。
- 幂等调用使用调用意图 ID；同 ID 不同参数应拒绝，而非仅以内容 hash 推定同一请求：[AWS Builders' Library](https://aws.amazon.com/builders-library/making-retries-safe-with-idempotent-APIs/)。operation_id 与 request_id、制品 ID 各有独立含义。
- outbox 将业务状态和待执行事项放同一事务，消费端仍需幂等：[AWS 官方模式](https://docs.aws.amazon.com/prescriptive-guidance/latest/cloud-design-patterns/transactional-outbox.html)。本方案操作表本身就是待办，无须额外队列；不能把 SQL+K8s 宣称为同一个原子事务。

推荐：权威 UserAppStore + 持久化操作表 + rcoder 内部推进任务。K8s 资源配置/运行状态仍以 runtime 为事实源，数据库不镜像整个 Deployment spec；只存应用身份、操作意图、必要不可变输入/引用及进度。

## 2. 当前必须调整的边界

- `userapp_forward/workspace.rs` 先登记元数据、再取得 builder 操作锁；`handlers/lifecycle.rs` 删除前还会调用 record_dev_registration。改成协调器受理后执行有语义的登记/校验，删除不再偷偷 upsert owner。
- `AppMetadataStore::record` 每次制造新 generation 并整行 upsert；改为原子 ensure_identity / patch_metadata 等命令，相同 owner 登记幂等，字段更新不依赖旧缓存合并。
- `record_deleted` 在 PG 条件删除后比较本地 cache；权威层应返回明确结果，删除路径不再使用缓存 CAS。
- 当前 prod/builder 两把分布式锁各自拥有部分语义；跨两族 purge 需要共同的应用级受理边界。第一版保守串行同 app 的控制面变更，不串行无关 app 或只读查询。
- 锁的嵌套必须收敛：入口只受理一次，下层传 AppOperationContext，不在 start→create→ensure 等调用中重复获取同一非重入锁。

## 3. 数据与模块

建议 shared_types 定义：

- AppLifecycleId（UUID newtype）、AppMetadataRevision、AppOperationId，避免字符串混用。
- AppLifecycleRecord：app_id、lifecycle_id、metadata_revision、owner/业务字段、Active/Deleting/Deleted。
- AppOperationRecord：operation_id、lifecycle_id、kind、scope、request_id（可选）、request_fingerprint、state、step、checkpoint、typed error、result。
- AdmissionOutcome：Accepted / JoinedExisting / AlreadySatisfied / Conflict。
- MetadataDeleteOutcome：Deleted / AlreadyDeleted / LifecycleConflict。
- UserAppStore：受理命令、按当前版本更新/提交操作、权威查询、扫描未完成操作；不继续暴露无条件整行 upsert。

`rcoder-storage`：PG 表和事务实现。应用行与操作登记在同一短事务，已有行 FOR UPDATE；不存在时用唯一键 INSERT ON CONFLICT 后重读。删除结果在同一事务中分类，不做 bool 返回后另一次 GET 猜测结果。

`app_manager`：AppLifecycleCoordinator、操作步骤执行器和查询投影。每一步调用 runtime 后提交 checkpoint，提交携带 operation_id/version；本地 spawn 不是唯一任务来源，启动扫描和周期扫描负责遗漏恢复。

`docker_manager`：保留实际 SDK 错误，实现身份化读写与分类结果；不负责业务元数据或 HTTP 错误码。

`rcoder`：HTTP 命令适配、依赖注入和运行任务；创建/删除入口复用同一协调器，不在 handler 中多写一份 owner。

## 4. 存储与缓存取舍

K8s 多副本使用 PG 权威 UserAppStore。userApp 控制存储与 Agent ProjectStore 的配置解耦，不改变此前 Agent 内存受理/异步补写策略。

Compose 改用 SQLx SQLite 的权威 UserAppStore，与 PG 实现同一语义契约和操作表。SQLite WAL/FULL/foreign_keys、5 秒 busy_timeout、max_connections=1；独立迁移，数据库事务不跨 runtime I/O。禁止 JSON 文件操作日志或失败后退回内存。默认挂载 ${RCODER_DATA_DIR:-./data/rcoder}:/app/data，数据库 /app/data/userapp.sqlite3；三份 Compose 同步，Agent 存储不变。

第一版移除 userApp 元数据全量 DashMap 镜像的权威用途。owner/删除/受理直接访问 store；列表使用批量读取，避免 N+1。小表低频控制面不值得优先维护跨副本缓存失效协议。若后续测出热点，只对展示投影增加有明确过期语义的缓存。

## 5. PVC 重试与错误分类

claim operation ID 在循环外生成，每次 GET 使用权威 K8s API；最多 4 次尝试和 3 秒总预算（初始建议参数），短退避带 jitter。只有 PATCH 明确 409 且重读 UID 不变、owner/type 相同、未删除才重试；UID 变化立即 LifecycleConflict。重复读取同 operation 标记可视为已提交。

默认保留完整 UID/resourceVersion 条件，不以省掉 RV 避免冲突；它与旧 purge 快照的删除保护有关。若以后改成字段级 JSON Patch test，应另行证明删除竞争契约，不和本轮混做。

原生错误在 runtime 边界分类为 Conflict / Rejected / OutcomeUnknown，保留必要源信息；HTTP 集中映射稳定码，所有 userApp 消费者复用，不以字符串判断。明确 403 可结束请求；408/499/断连/5xx 不能直接证明远端未执行。

## 6. 操作推进、幂等与删除

外部 HTTP → 短事务受理 → 执行器推进 → 短事务保存进度 → 同步等待已定义的成功边界。

同 app、同生命周期、相同 ensure 意图可以合并为一个进行中的操作；不同 owner/冲突意图拒绝。部署不能仅用 artifact hash 合并：同制品重新部署也可能是新意图。可选 request_id 通过显式 body/query 传递；相同 request_id 不同参数返回参数冲突。无 request_id 的调用保持当前语义，不宣称跨网络重试严格去重。

完整 delete/app：短事务设置 Deleting 并登记目标范围 → 获取当前身份快照 → 阻止新控制面写入并按现有取消协议结束开发任务 → 删除 prod 计算面 → 删除选定存储/dev 资源 → 确认各目标身份消失 → 提交 Deleted 墓碑和操作终态。prod/delete 的 purge=true 与单独 storage/destroy 保留应用生命周期，由当前操作字段阻止冲突控制请求，不提交整个应用的 Deleted 墓碑。

普通 prod delete/stop/dev storage destroy 使用明确 scope，不统一变成整应用 Deleted；范围矩阵按当前实际接口行为固定，修正文档与代码不一致之处。

HTTP 等待超时时保持现有错误信封，返回/关联 operation_id 供查询；后台记录不得因 HTTP future 取消而丢失。完成结果重复查询一致。

## 7. 恢复安全边界

操作记录与执行权分开。生命周期 Deleting 会阻止新建，即使执行进程死亡也不会因进程锁释放而重新开放资源。

自动恢复以同一 operation_id、相同输入和保存的 UID/checkpoint 推进已证明幂等的步骤。K8s 更新/删除使用真实 UID/resourceVersion；Docker 使用容器 ID与共享文件锁。仅凭 lease 过期、Pod Ready 或一次 GET 不存在，都不能证明旧写已停止。

必须审计 STS/Deployment/Service/PVC 创建、扩缩容、配置更新全部写面。未完成远端防旧写能力前，不能安全判断的超时操作进入 RecoveryRequired，展示具体步骤和资源证据，阻止冲突新操作，不实现“到时自动删锁”。需清楚区分可自动重读确认的操作与必须人工核实的未知创建。

PG/SQLite 提交失败但远端步骤已完成：保留原 operation 和 checkpoint，恢复完成提交；不能再由本地缓存制造假冲突。释放资源保护只基于真实完成状态，不做全局 Err→release。

## 8. 开发与迁移

先直接修 K01/K03 并补确定性测试；再替换 metadata 命令和存储契约；最后接入统一受理、持久化 purge和恢复任务。开发可拆提交，部署必须按依赖整体升级。

userApp 未上线，不保留旧 writer/new writer 混跑或无条件 upsert 兼容层；但现有测试数据不自动丢弃。切换时停写并排空任务，迁移现有记录，为每个当前应用分配一次 lifecycle_id、metadata_revision=1，核对运行资源身份后恢复。无法确定的在途操作登记 RecoveryRequired，不能根据旧锁存在与否直接宣告成功。SQLite 进行同等一次性初始化。

HTTP 路由和当前成功边界保持；新增操作查询、错误码与三语资源同步 OpenAPI。每批检查 Helm/RBAC/build-agent-docker 配套；未增加权限则不空改 chart。不因仅平台控制逻辑变化而空发 app-cli。

## 首开与 HTTP 收尾

结构化 OperationInProgress 贯通 ensure/转发/代理。协调器合并同 app/owner/lifecycle/config 创建；本地共享带快照通知，跨副本查询持久化结果。总预算 90 秒，200ms→2s 抖动退避；取消单个等待不取消共享创建。不对遗留锁或不确定操作自动接管。未转发请求错误收尾按流最多丢弃 1MiB/1s，不改正常上传限制。

## 配置与验收

主仓 docker/docker-compose.yml 与构建仓 docker-userapp-computer/docker-compose.yml、docker/docker-compose.yml 同步数据目录和后端配置。检查真实运行 SQLite 版本及本地 SQLx 0.9 源码；不引入 path 依赖。先组件/真实 PG 与 SQLite 契约，再 dev-restart/dev-hot 和严格 E2E；K8s 运行验收待用户部署镜像。

## 业务接入实现落点

AppState 在构造 AppService 前打开独立 userApp store，初始化/迁移失败阻断启动。AppMetadataStore 只提供异步权威投影和字段补丁，不保留全量 owner 缓存。同步响应构造函数显式接收已读取的 owner，避免闭包内阻塞数据库访问。完整删除持有资源锁直到操作终态提交；Deleted 行作为墓碑保留，单独存储销毁不结束生命周期。

Compose 的 SQL 数据目录独立于应用卷，每个项目使用自己的目录或 project-scoped named volume。启动脚本只创建并检查目录可写性，数据库工厂继续验证文件系统/单实例/迁移；不递归修改权限。主镜像与 dev-hot 都依赖 rcoder 默认的 userapp-sqlite feature。配置解析、组件重开、真实容器重建持久化是三个不同验收层级。

## 集中实现与门禁节奏

按用户最新要求，先完成剩余功能和固定回归用例，再统一运行 Cargo 门禁。期间保留源代码检查和格式整理，不将未运行的用例计为通过。首开创建工作任务持有本地执行 lease，HTTP 等待者只观察持久化结果；进程内 watch 是通知优化，跨副本和晚订阅均以操作表为准。资源锁的 409 只暴露结构化占用信息，不直接授权重试或释放旧锁。

## 操作查询和恢复接口实现

生命周期查询与操作查询按 owner 读取持久化状态，对外只返回操作身份、阶段、状态、结果错误和时间；内部执行者及恢复检查点不直接进入响应。显式 recreate 以旧生命周期和 request_id 为条件，精确重复请求返回第一次生成的生命周期。

启动和周期恢复先覆盖未认领的 Pending builder：输入可由固定创建参数及存储 owner 重建，必须匹配已保存配置指纹；认领仍走操作 revision CAS。进程内使用非阻塞 lease 检查，页游标避免一个阻塞操作饿死后续应用。不把 Running 或未知远端写当成安全重放来源。

转发前拒绝请求时，以有界流式读取收尾，读完的小请求保留原 HTTP 错误响应；大体积/停滞请求到预算即退出，不承诺避免所有传输层错误，不重放 multipart 请求。

## 运行时身份与 StatefulSet 条件写入

持久化认领后构造 shared_types::UserAppExecutionContext，经 ContainerCreateParams 传递；资源元数据键由共享契约生成。Builder StatefulSet 顶层及 Pod 模板记录同一生命周期和配置身份，Docker 写入相同语义的 labels。操作 ID 记录创建来源，复用要求 owner/lifecycle/config 相同，不要求每次 ensure 的新操作 ID 等于原创建操作。

有上下文的 builder 不使用 Agent 的漂移删除重建分支：POST 冲突只读查询赢家，验证身份、模板和挂载，再以该次读取的 UID/resourceVersion 条件扩容。所有扩缩容共用捕获身份后的 PATCH，不在校验和写入之间重新按名称选择对象。缺身份的旧资源须走显式接纳流程，不能仅凭健康或同名自动盖章；接纳实现仍需完成后才能整体验收。

### Full-delete admission boundary

The application guard precedes the authoritative identity read. The caller supplies registered owner and, after recreation, lifecycle_id; request_id binds the canonical deletion intent. Exact successful replay does not repeat runtime cleanup. Full deletion retains a tombstone; unknown identity is rejected without touching resources. E2E cleanup must use the owner/lifecycle captured in its run receipt, rather than reconstructing current identity during cleanup.


### Production automatic wake implementation

The activity registry retains watch-based request merging and advisory runtime reads. A weak AppService reference delegates mutations to durable lifecycle admission under the existing application operation guard, avoiding nested runtime leases and reference cycles. Wake checkpoints the captured resource identity before starting it and checks the same UID around readiness observations. Manual-stop policy is checked under the guard. Timeouts and uncertain writes retain recovery records and never trigger an unguarded retry. Runtime query failure is an explicit failed ensure outcome. This implementation is source-only until the consolidated gate phase.


### SQLite startup file identity

Canonicalize the configured parent directory once and use that same canonical path for both the instance lease and SQLx database opening. Reject file-level symlink/hard-link aliases for the database, sidecars and lease to prevent split WAL paths or bypassing directory ownership. Directory-level aliases are supported and share one lease. This is a startup consistency check for private deployment data directories; external processes must not replace files during runtime.


### Unclaimed control replay

Startup/periodic scans may resume original Pending Start/Restart/Stop commands only after an application operation guard, an authoritative lifecycle/current-operation read and a SQL revision claim. They never call a public control wrapper that would admit a second operation. Runtime targets are captured and checkpointed before effects. Traffic wake uses the same UID-checked readiness observer as live wake; intentional stops remain authoritative. Running, RecoveryRequired, missing-input and retained runtime ownership cases are not automatically taken over.

### Create/Update policy projection

`UserAppAdmission` and the stored operation retain a typed optional `runtime_policy_on_success`. Only Create/Update can supply this projection; command-based controls derive their policy from the original command. Duplicate admission compares the projection as part of the exact intent. The common domain transition merges it into applied lifecycle policy only alongside Succeeded, with revision changes only for actual field changes. This adds no resource-replay permission and stores no environment values or secrets.

Create captures resolved recycle/idle fields and enables traffic wake; Update captures the effective merged runtime parameters and preserves current wake policy. The successful update clears the owned mutation marker only after the SQL terminal transaction commits. Existing records omit the optional field and retain their previous semantics.

### Bounded recovery discovery

Recovery runs at most eight independent futures per process, tracks active operation IDs, and retains the discovery cursor across five-second ticks. Each tick scans at most four 128-row pages; each storage read has a five-second bound and continues polling active recovery futures. Only replayable Pending records are scheduled. Panic isolation affects only the failed task; it never clears a retained resource lease or fabricates a successful operation result.

### Ensure waiter deadline boundary

Both public builder ensure variants compute one absolute deadline and wrap their entire internal path in `within_builder_deadline`. Internal functions accept that deadline and forward it to durable creation waiting. The helper refuses to poll newly submitted work when already expired. The independently spawned creation worker is outside this waiter cancellation boundary; a cancellation at durable admission may leave Pending for the existing recovery scanner and must not be interpreted as remote failure.

### Typed builder wait failures

`shared_types::UserAppWaitTimeout` carries an optional operation ID through anyhow context. A common builder HTTP mapper preserves this timeout and typed store OperationInProgress identities; unrelated text remains an ordinary backend error. Pod ensure/keepalive and the initial dev-forward ensure use this mapper. The error code has three-language resources; the final formal-route middleware still owns HTTP normalization. A timeout before the waiter has a known operation ID is returned without inventing one.

### Development deletion receipts

The shared deletion ticket exposes an immutable serializable receipt containing its runtime snapshot and optional registry generation/container ID. Production purge and full lifecycle deletion persist this receipt alongside production resources before the first deletion. The coordinator rejects a receipt for another application. Captured tickets still own their original locks; serialization neither transfers those locks nor authorizes replay of an uncertain write. Older checkpoints without development evidence require explicit reconciliation.

### Compose persistence configuration validation

The read-only `sqlite_compose_contract.py` consumes Docker Compose's normalized JSON to verify mount and backend semantics, including merged named-volume overrides. Run it against all three configurations after source freeze, both with default paths and an explicit absolute bind override. It intentionally records only sanitized persistence fields and labels evidence as configuration-only. Actual container startup/recreation, SQLite durability, binary identity and isolated cleanup remain separate required gates.

### Versioned deletion checkpoint contract

`shared_types::UserAppDeletionCheckpoint` replaces consumer-defined JSON objects. Version 1 binds app, owner, lifecycle, operation, executor, intent fingerprint and deletion kind to production/development receipts. Validate before persistence and before any runtime deletion. `validate_operation` checks evidence against the authoritative stored operation; it does not grant a resource lease. Compute-only deletion excludes development receipts, while purge/full deletion requires them. Kubernetes identities require UID and resourceVersion; old/missing-context records fail decoding rather than being reconstructed by logical name.

### Confirmed deletion progress

Versioned deletion checkpoints carry Captured → ComputeRemoved → ProductionStorageRemoved → DevelopmentRemoved. OwnedOperation checks the prior stored evidence and allows only the next boundary; context/receipts cannot change. SQL commits precede publishing the updated local checkpoint. Both purge entry points share the captured storage/dev cleanup implementation. K8s now polls every deleted resource kind until 404 under a 60-second absolute deadline, rejecting query errors and replacement UIDs. An absent Deployment does not skip captured residual compute resources.

### Kubernetes deletion wire coverage

All identity-bound application DELETE requests use Foreground propagation. The API contract fixture checks physical preconditions and propagation, returns a still-terminating object before 404, and asserts that subsequent resource deletion occurs only after that 404. Alternate responses cover 503 observation failure and replacement UID; both must terminate the sequence at the original observation error.

### Storage-level deletion transition guard

The common SQL domain validator enforces typed deletion evidence, owner/operation binding, immutable target snapshots, ordered stage changes and a previously committed final stage before Succeeded. Failed/RecoveryRequired must preserve evidence. Missing or legacy checkpoints cannot bypass this validation. SQLite and PostgreSQL share the validator and the direct-storage contract suite; coordinator-only validation is insufficient.

### Deletion activity-state completion

Compute teardown removes routing and blocks wake, retaining both on runtime failure because a multi-resource deletion can already have partially succeeded. It no longer clears activity at compute completion. Both controlled deletion and full purge call forget_app only after SQL Succeeded commits. Its watch notification uses send_replace so late subscribers see deletion. Known preflight failures still happen before routing mutation; uncertain outcomes require reconciliation.

### Durable deletion wake fences on restart

Activity reconstruction first loads lifecycle deletion fences using paginated identities and authoritative current-operation reads. Non-Active lifecycles and unfinished deletion kinds remain wake-blocked, including absent compute. Running/Stopped runtime status cannot override those fences. All storage reads finish before publishing reconstructed flags; inconsistent operation links fail startup. Successful operation paths remain responsible for removing their own fences after terminal commit.

### Atomic control snapshots

Added `UserAppControlSnapshot` and paginated `list_control_snapshots` to the shared store contract. Each backend executes one LEFT JOIN statement between lifecycle JSON's current-operation pointer and the matching app/operation record. SQLite uses json_extract and PostgreSQL uses a JSONB cast/extraction; both validate lifecycle/operation linkage before returning the page. Activity restoration consumes these snapshots instead of N+1 independent reads. No SQL transaction spans runtime I/O.

### Revalidate stale local wake blocks

The activity registry delegates blocked requests to the coordinator instead of deciding intentional-stop state from memory alone. The coordinator checks current runtime/SQL policy under the operation guard, admits the durable operation and validates/checkpoints its captured target before refreshing local activity. Recovery uses the same order for traffic Start. Retained resource leases and current operation conflicts still prevent activation; completion checks continue to respect a newly observed stop.

## 唤醒缓存与控制入口边界

保留 `remote_stopped` 的 TTL 探测与本地标记回填；移除 `ensure_running` 基于该缓存直接返回 `AlreadyRunning` 的分支。所有显式唤醒请求经现有 single-flight 合流进入生命周期协调器，由其读取权威策略、条件受理并捕获资源身份后判断无需启动或执行启动。普通健康流量的路由探测不因此增加控制操作。

Builder 创建函数返回可观察的 JoinHandle；普通 ensure 脱离该 handle 后继续按持久化状态等待，恢复调用则等待 handle 完成，将实际执行期纳入 RecoveryTasks 上限。执行 panic 与检查点失败在保留原有持久化恢复标记及通知清理后返回给恢复调度器。

转发层统一使用 builder_control_response 转换首次及再次 ensure 错误，保留 code/operation_id，旧 TS 状态仍为 502。完整 resolve_dev_addr 外层截止时间约束内部各步骤；超时退出仅取消观察，不取消已受理 worker。注册表清理由协调入口校验最新状态，转发层不执行无条件覆写或关闭项目流。生产详情沿用 AppOperationError 错误码，runtime 地址查询错误直接上抛，地址过渡态使用 503。

新增 POST /api/v1/userapp/{app_id}/operations/{operation_id}/retry，请求契约 UserAppRetryRequest 放 shared_types，含 user_id/lifecycle_id/expected_revision。先校验归属及生命周期，未完成操作再校验 revision；Pending 原命令通过现有 SQL 条件认领和 runtime lease 执行。HTTP 观察者取消不打断 worker。当前仅基本控制命令具备原输入；其余操作的恢复命令及未知结果对账仍为后续实施项，不能据此宣称全部恢复完成。

UserAppControlCommand 增加 DeleteResources { purge, expected_resource_version }，分别映射 DeleteCompute/PurgeResources。首次删除与 Pending 恢复复用 execute_resource_deletion，在已持有应用锁和原操作认领的上下文内读取版本、捕获生产/开发凭据并执行逐步清理；不嵌套受理或获取锁。Running 与 RecoveryRequired 的断点恢复仍需另行完成证据对账。

完整删除保存独立 DeleteApplication 命令，Pending 恢复复用 purge_app_resources。恢复前置校验按操作类型要求 Active 或 Deleting，并同时匹配生命周期和 current_operation_id；最终成功事务才结束生命周期。删除失败沿用检查点专属 fail 语义，不使用普通控制的 reject_without_mutation。

DestroyStorageRequest 增加可选 lifecycle_id/request_id；HTTP 使用 controlled 入口并返回 operation_id，取消响应观察者不取消 worker。共享 DestroyStorage 命令保存生产/开发范围，UserAppStorageDestruction 保存执行身份及原始生产、开发、注册表凭据。首次执行和 Pending 恢复共用 execute_storage_destruction；SQL 校验 scope、executor、生命周期、不可变资源凭据及 storage_captured → production_storage_removed（仅 prod）→ development_storage_removed 顺序。移除旧的未受理销毁分支及已无调用方的重复执行 helper。clear 内容接口仍需单独收敛，不能等同销毁卷。

shared_types::storage_contents::clear_directory_contents 统一 Docker 生产目录和 Rust file-server 开发工作区的逐项清理：symlink_metadata 校验根目录，DirEntry::file_type 不跟随子链接，只对根 NotFound 返回成功。去掉两端 try_exists/metadata 的 unwrap_or(false) 和重复删除循环。调用方的完整持久化受理、停止任务和目录替换保护仍需独立接入。

BuildTaskStore 按应用维护弱引用工作区 RwLock，构建与 dev start/restart worker 持有 owned read guard 到任务退出；app-files 上传、下载导入、查询、删除也持读 guard。reset 先在启动代次锁内失效旧代并取消任务，然后释放代次锁再等待写 guard（最多 90 秒），避免等待 commit_start 的 worker 与 reset 互锁。持写 guard 后再次推进代次、停止开发进程并检查全部 kill 结果，再清内容。reset 独立 worker 持有资源到完成，不随 HTTP 观察者取消。其他开发写入口和平台持久化 clear 协调仍需继续收敛。

UserApp file mirror 的 files-update、upload-file(s)、generate-file、import-project 与项目确认、ensure-workspace 在解析工作区并产生文件副作用前取得同应用读租约。execute-command/install-project 将执行与读租约一起交给独立 worker，响应观察者取消不释放正在执行的进程租约。容器外部写入、其他路由的覆盖范围及平台 clear 操作记录仍需独立核对。

ClearStorageRequest 增加 lifecycle_id/request_id，新增 ClearDevStorage/ClearProdStorage 操作类型及 shared UserAppStorageClear 目标凭据。开发定位 ensure 在 clear 受理之前完成以避免嵌套等待；受理后持有开发资源票据至结果提交，调用容器 reset 并检查明确 success。生产 K8s 用捕获快照条件删除存储，Docker 保留四目录本身；目标检查点和完成步骤由 SQL 校验。HTTP 执行由独立 worker 持有，错误关联原 request。开发 HTTP 目标的物理身份确认、Docker 目录替换边界及 clear Pending/RecoveryRequired 恢复仍需继续完成，当前凭据不能授权未知结果重放。

UserappDevDeletion 增加 begin_external_mutation/finish_external_mutation，默认拒绝不具备该能力的适配器。CapturedDeletion 委托 BuilderOperation 状态管理：写入前转换 mutating，失败或取消 Drop 保留分布式/文件标记；确认成功后显式 release，release 错误保留 uncertain 状态。平台开发 clear 在发送前转换票据，解析成功确认后完成释放，再提交 clear 完成检查点。

共享 workspace_clear 协议定义只读 probe、target 和 clear request；file-server 每进程生成独立 UUID，GET app-files/clear-target 返回实例身份，POST app-files/clear 强制 expected_instance_id 校验。平台在只读捕获期间读取身份，将其写入 UserAppStorageClear 证据，发送时回传并核对完成回显。无旧镜像降级重发；缺能力/无字段失败。该实例 fence 防止观察后替换，初始 endpoint 与 runtime UID 的绑定仍是单独未完成的核验项。

### Internal reset wire contract and remaining workspace writers

Use UserAppWorkspaceClearResult from shared_types on both the file-server response and platform decoder, with one confirms predicate and no defaulted identity fields. Exercise the registered router, including envelope middleware, to distinguish internal app-files responses from public HttpResult APIs. Template initialization and skill push parse uploads first, then acquire the owned workspace read lease and move it, input temporary files and execution into an independent worker. This keeps reset exclusive through extraction/install completion without holding a workspace lease over the network upload.

### Runtime-backed workspace endpoint inspection

Add the shared UserAppBuilderWorkspaceEndpoint and pass inspection through the existing captured builder ticket/runtime trait. Docker inspects the captured container ID and checks labels and running state. Kubernetes reads the captured StatefulSet UID and lifecycle annotations, then reads its exact Pod and checks controller owner UID, family, lifecycle, readiness and Pod UID/IP. Persist the endpoint beside the process-instance nonce in the SQL clear checkpoint; bracket the nonce probe with two uncached observations. Construct a direct reqwest client with no redirects or environment proxy. No registration cache or Service DNS supplies deletion/clear authority.

### Directory receipts and live handles

Use StorageDirectoryLease with an Arc-owned directory descriptor and a serializable CapturedStorageDirectory receipt. Keep live leases through SQL checkpoint and worker completion; compare all receipts and roots before Docker clear starts. Traverse iteratively using rustix directory-relative open/stat/unlink APIs with NOFOLLOW, and validate root identity again afterward. File-server reset captures before invalidating tasks and revalidates after obtaining exclusive workspace access. Blocking workers retain their own descriptor clone through observer cancellation.
