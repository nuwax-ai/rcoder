# UserApp：控制版本 CAS、短事务与外键边界

日期：2026-09-19。属于 Toasty 迁移中的设计修订，不代表整轮迁移已验收。

## 1. 设计结论

移除 UserApp 的 `SELECT ... FOR UPDATE` 依赖，以根控制版本 CAS 建立跨表共同竞争点。它仍有短事务写锁；不能宣传成无锁数据库，也不能仅靠替换语法解决连接池耗尽。

外键不一刀切删除。业务互斥由根 CAS、槽位 CAS、operation CAS 和领域状态机实现；外键只负责引用身份完整性。保持显式清理，不引入 ON DELETE/UPDATE CASCADE。历史 operation 不应随当前 lifecycle 换代而自动改绑。

## 2. 原子竞争点

`userapps.control_revision` 为存储内部版本，初始 1，每次成功竞争递增；独立于公开 metadata_revision。即使 lifecycle 重建也不能重置。

事务流程：

1. 读取根的 lifecycle、state、control_revision。
2. `UPDATE userapps SET control_revision=next WHERE app_id=... AND lifecycle_id=... AND lifecycle_state=... AND control_revision=expected`。
3. 影响行数不是 1：不得继续读改子表；整笔回滚。
4. 成功后重新读取当前根、槽位、operation、幂等映射，执行领域校验。
5. 原子写入 operation、私有输入、配置捕获、请求映射、槽位；提交之后才执行外部资源操作。

受理、删除状态推进、重建、配置应用等写路径共用这个根。不能只在 operation 和 slots 各自增加 revision。根的短事务串行化不意味着 dev/prod 外部编排串行；两个环境可以同时持有自己的业务槽位。

槽位 UPDATE 同时核对旧 dev/prod/Application operation_id（包括 NULL），因此旧完成通知不能清除新操作。CHECK 保证 Application 与两个环境槽不能共存。状态推进继续核对 app/lifecycle、revision、state、executor。

## 3. 重试与结果未知

目前只对已经成功回滚的 VersionConflict 进行完整事务重试，最多三次，间隔 5/10ms。请求身份、预期版本与输入不变，每次重新读并校验。业务预期版本已经过期时不会通过重试变成有效版本；最终仍返回冲突。

锁超时、连接错误、提交或回滚错误不盲目自动重放。特别是提交响应丢失，调用方须按原 operation/request 身份查询结果。事务只能包含数据库工作，禁止 K8s、Docker、HTTP、进程等待和用户输入。

数据库事务结束不代表外部副作用已收束，RecoveryRequired、operation 槽位和外部租约继续按原身份协议保留。

## 4. 超时与取消

每条 PG 池连接初始化设置：

- statement_timeout：现有配置。
- lock_timeout：statement_timeout 毫秒值夹在 1–2000ms 之间。
- idle_in_transaction_session_timeout：夹在 1–30000ms 之间，覆盖 SQL 语句之间的挂起。

这两个上限目前是存储连接策略，适用于该 PolicyDriver 建立的 PG 连接，不能只评估 UserApp 路径；Project/Preview/初始化的回归也需要检查。PG 16 可用，不直接添加只在较新 PG 提供的 transaction_timeout。

DatabaseOwner 中已受理的数据库任务不因 HTTP future 取消而中途丢弃。事务控制命令失败后连接标记失效，不可回池。池等待/建连预算、队列有界以及 shutdown 排空规则仍有效。

这些机制不能保证整个业务操作在固定时间完成，也不授权按时间强制释放业务租约。

## 5. 外键保留与审查准则

| 约束 | 本轮选择 | 原因 |
|---|---|---|
| inputs/leases/deadlines → operation + app + lifecycle | 保留复合 FK | 阻止记录引用与其声明的 app/lifecycle 不一致的 operation；当前代次授权仍由 CAS 检查 |
| 配置执行记录 → operation/scope 与不可变配置版本 | 保留 | 防止执行捕获引用错误版本或环境 |
| 当前 slots/activity → 当前根代次 | 保留 | 配合显式换代清理，不自动转移旧身份 |
| 历史 operation → app_id | 保留，不引用当前 lifecycle 复合键 | 历史必须跨重建保留 |
| 自动级联删除或换代 | 不采用 | 不能让持久证据随父记录隐式消失或改绑 |

外键不是权限校验，也不能代替互斥状态机。检查引用列索引、父键更新频率、清理批量大小、统一写入顺序；不要机械为每条 FK 增加重复索引。将来删除冗余 FK 应单独证明替代不变量，并用独立连接反例验证，不以“可能有锁”为唯一理由。

## 6. 验证与边界

新反例在 `common/concurrency_tests.rs`：

- 旧全槽位快照不能清除新 prod 操作。
- 两个独立 PG owner 并行 dev/prod 受理后两个槽位均保留。
- DeleteApplication 与 dev 受理竞争只有一个提交，失败方无 operation 残留。
- 真实事务持有根写入时，另一连接受理被 lock_timeout 限定；释放后原请求可以成功。
- PG 事务语句间 idle 超时后不能成功提交；单连接池随后可正常重建连接读取。

还须补充/确认删除成功与重建、旧代次迟到写入、同请求并发重放、跨表故障回滚等完整契约；新增测试不代替原组件契约、整个 workspace、Compose 或 K8s 验证。实际执行证据记录在 verification.md。
