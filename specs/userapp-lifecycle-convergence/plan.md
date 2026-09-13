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

purge：短事务设置 Deleting并登记目标范围 → 获取当前身份快照 → 阻止新控制面写入并按现有取消协议结束开发任务 → 删除 prod 计算面 → 删除选定存储/dev 资源 → 确认各目标身份消失 → 提交 Deleted 墓碑和操作终态。

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
