# UserApp 存储 trait 设计补充

日期：2026-09-19。配合 [spec.md](spec.md)、[plan.md](plan.md)、[tasks.md](tasks.md) 实施。本文只规定接口和实现边界，不新增部署能力。

## 1. 已确认范围

- 保留现有 `#[cfg(feature = "kubernetes")]` PG 初始化门控及 `rcoder-pg -> kubernetes` 关系。
- 默认本地实现由 SQLite 换为 Turso，PG 实现保留。
- 不新增 userapp-pg、不增加 Compose PG 部署，不提前开发多副本，不修改 Agent 存储抽象。
- 未来切换后端需要配置/装配与运行时安全性验收；trait 只保证业务存储契约，不保证不同数据库间自动数据迁移或运行中热切换。

## 2. 复用已有接口，不另造通用 Repository

当前 `crates/shared_types/src/userapp/lifecycle.rs:707` 已定义 `UserAppLifecycleStore: Send + Sync`，`AppState` 和 app_manager 已注入 `Arc<dyn UserAppLifecycleStore>`。以这个接口为唯一业务入口，保持 async_trait 的 dyn 使用方式。

```text
handler / application service / recovery
                   ↓
      Arc<dyn UserAppLifecycleStore>
         ↙                      ↘
TursoUserAppStore             PgUserAppStore
worker + connection          SQLx + transaction
         ↘                      ↙
    共用 domain 身份校验和状态转换
```

- trait、请求、结果、业务错误放 shared_types；SQL 和驱动类型留在 rcoder-storage。
- Turso/PG 类型和配置只允许存储实现、装配工厂及后端专用测试使用。
- 不向业务层提供 execute_sql、begin、commit、连接池或数据库 Transaction，不增加 backend_name 来引导业务分支。
- 保留一个完整业务 trait。暂不拆分多个读写子 trait、泛型 DbDriver 或 capability registry；只有实际消费者需要更小权限面时另行评估。
- 现有传引用签名可保留：Turso 适配器将必要参数 clone 为内部有所有权的任务。不得为适配 worker 把 SQL closure/驱动类型塞入公共接口。

## 3. 方法契约与事务边界

每个变更方法的成功响应必须发生在其事务提交之后。数据库事务中禁止运行 Docker/K8s/HTTP 请求。以下是现有方法的契约补全，不要求为改名重写全部调用点。

| 方法组 | 对外保证 |
|---|---|
| ensure_identity / import_application | 创建或读取在同一事务；重复调用不覆盖已有身份和 tombstone |
| patch_metadata | 校验生命周期与 metadata revision 后原子更新，不整条覆盖并发变更 |
| admit / admit_with_input | 身份、请求幂等、作用域冲突检查、operation、request 映射、active slot、私有执行输入一次提交；失败全回滚 |
| advance | 校验 app/lifecycle/operation/executor/revision 与正确 scope 槽位；原子推进记录与槽位，只释放当前操作自己的槽位 |
| commit_resource_binding | 物理资源绑定和成功收养终态一次提交，不先单独写绑定再 advance |
| bind_operation_deadline | 原子 bind-once，重复受理返回已存值；必须在运行时副作用前完成 |
| bind_operation_lease | 校验执行身份和 receipt 归属；不能覆盖不同所有者的有效凭据 |
| reserve_completed_operation | 只按已确认终局证据条件更新，不能变成通用强制接管或清锁入口 |
| forget_operation_lease | 在外部已经完成条件释放后，仍核对终态与确切 receipt 再删除；不能按 app_id 批量清理 |
| recreate | 按预期旧 lifecycle 和 request_id 原子建立新代次；重复请求保持既有幂等语义 |

外部资源释放与数据库删除不可能靠单个数据库事务一起原子提交。继续使用“证据/receipt 持久化 → 条件释放 → 精确忘记凭据”的可恢复流程；禁止弱化成删除数据库记录即视为外部释放成功。

### 查询一致性

- `list_control_snapshots` 每页的 application 与各 scope 操作必须来自同一语句快照或同一一致性事务；分页之间允许并发变化，恢复动作必须重新 CAS，不能把整轮扫描当一个全局快照。
- `get_application`、`get_operation`、`get_operation_by_request` 等查询只有确实无匹配记录时才返回 Ok(None)。解析错误、数据库不可用、记录关系损坏必须 Err。
- `read_execution_input` 继续验证当前执行身份，不通过公共 HTTP 接口暴露私有输入。
- `unfinished_operations` / `terminal_operation_leases` 保留稳定游标、有界页大小及既有筛选规则，不能退化成无界全表加载。
- 修正当前 trait 的注释归属：位于 get_resource_binding 上的“每页单语句快照”说明应归到 list_control_snapshots。

## 4. 完整后端必须在编译时实现完整契约

现有 trait 某些方法默认返回 InvalidOperation("... unsupported")，admit_with_input 对 None 默认转发 admit。这会让新增后端漏实现后仍能编译。

本次完整 Turso/PG 后端必须覆盖所有方法。将以下关键方法改为必需实现，移除 unsupported 默认方法体：

- get_resource_binding、commit_resource_binding。
- admit_with_input、read_execution_input。
- bind_operation_lease、get_operation_lease、terminal_operation_leases、forget_operation_lease。
- reserve_completed_operation。

保留现有返回类型和调用语义。为避免 admit 与 admit_with_input 分叉，可在具体后端私有实现中统一受理逻辑；不引入互相调用的递归默认实现。全仓查找所有实现（包括宏生成、测试替身），一起更新；测试替身不得用假成功补齐未实现功能。

## 5. 错误与取消语义

沿用 `UserAppStoreError` 的 OwnershipConflict、LifecycleConflict、OperationInProgress、VersionConflict、NotFound、InvalidOperation、Storage。禁止用驱动 message 字符串分类，也不把所有唯一键错误泛化成 VersionConflict；应结合实际约束及操作上下文映射。

- 同一业务失败在两个后端得到相同业务变体；OperationInProgress 保留原阻塞操作身份与 scope。
- Storage 保留底层 source 和动作上下文，对外日志注意隐藏 DSN、凭据、执行输入。
- `Storage` 不能被理解为“绝对没有提交”。提交成功回包丢失、commit 错误等情况下，调用方按原操作身份重新查询/幂等重试，不分配新身份盲重试。
- 本次不为没有消费者的分类新增一套大错误框架。若现有调用方需要区分“明确未执行”和“提交未知”来做自动重试，必须增加后端中性的 typed outcome 分类并同步两实现、映射及测试；不能靠字符串或 driver downcast 决策。
- Turso 队列满/后端关闭明确返回错误。请求取消不表示已开始事务被取消；实现遵守 plan 的完整事务执行策略。
- 不改变 HTTP 错误码和业务终态映射，除非为修复确切契约缺陷并有对应回归证据。

## 6. 存储生命周期与业务接口分离

业务服务只持有 `Arc<dyn UserAppLifecycleStore>`；只有启动/关机装配层拥有关闭权限。不要为了 Turso worker 给所有 handler 添加 shutdown() 能力。

可在 rcoder-storage 使用小型独立控制接口（示意，命名可随现有代码调整）：

```rust
#[async_trait::async_trait]
pub trait UserAppStoreControl: Send + Sync {
    async fn shutdown(&self) -> Result<(), UserAppStoreError>;
}

pub struct OpenedUserAppStore {
    pub store: Arc<dyn UserAppLifecycleStore>,
    pub control: Arc<dyn UserAppStoreControl>,
}
```

配置工厂返回装配结果，业务层仅注入 store；control 留给 shutdown coordinator。Turso 实现负责停止接单、完成已接收事务、关闭连接、异步等待线程结束和释放锁；PG 实现负责等待使用方停止并关闭自身池。若池共享，控制句柄不得关闭其他业务拥有的池，必须先明确资源所有权。

shutdown 重复调用应安全，共享实际关闭结果；并发入队与关闭要有明确接收边界。后端已关闭后，遗留 store 引用调用返回错误，不挂起、不成功。Drop 只能作兜底，不充当已完成 flush 的证据。

重启 quarantine、文件锁和 schema migration 是后端启动实现，不属于业务 trait；PG 不能照搬 Turso 的“独占进程重启”逻辑去隔离其他仍运行副本。

## 7. 共同契约测试

建立普通测试支持模块/函数，接收 `&dyn UserAppLifecycleStore` 或 Arc；Turso 临时目录与真实 PG 隔离 schema 各调用同一套行为断言。不新增复杂测试框架，不让测试结果依赖具体 SQL 或数据库类型。

必测：全部方法的成功路径和关键拒绝路径；同 request 幂等及不同指纹拒绝；同域单胜者、跨域互不覆盖、Application 冲突；旧 revision/executor/lifecycle 拒绝；受理输入与槽位原子提交；绑定/清理准确 receipt；bind-once deadline；分页与 tombstone；存储故障不能等价 None。

PG 并发测试必须使用独立连接/独立 store 实例，不能靠单实例 mutex 把竞态藏掉。Turso 使用多个任务共享真实适配器验证业务原子性，并另测双进程目录排他。

事务中途故障、迁移失败、取消、worker 关闭、SIGKILL 与查询快照并发一致性做后端专用测试，行为断言保持一致；断言具体时序，不只最终记录数量。

## 8. 提交与验收

同步更新 trait 文档、所有实现、所有消费者的错误匹配和装配关机链；按 T2/T5 完成 nextest、真实 PG 与 Compose 测试。记录默认 Turso 与 rcoder-pg（仍含 Kubernetes）构建。不得因 trait 接口统一就声称 Compose 多副本已支持。
