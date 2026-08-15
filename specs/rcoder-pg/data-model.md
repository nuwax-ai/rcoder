# rcoder-pg 数据模型

迁移文件：`crates/rcoder-storage/migrations/0001_init.sql`（sqlx::migrate! 启动执行，
advisory lock 保护多副本并发；全字段 COMMENT ON，`psql \d+` 可见）。

## 内存结构 → PG 映射

| 内存结构 | PG 归宿 |
|---|---|
| ProjectAdapter.projects（ProjectCoreState+ExtendedState） | `projects` 表 |
| ProjectAdapter.containers（ContainerEntry） | `containers` 表（运行态真源仍在 K8s/Docker API） |
| session_index + project.sessions 集合 | `sessions` 一张表双向覆盖；container_name 冗余列支撑 resolve 单查 |
| 反向索引 ×3（user_id/pod_id/container_id） | 普通索引（含部分索引） |
| ContainerEntry.ref_count | 派生：count(projects where container_name=X)，不落库 |
| AppActivityRegistry.last_accessed/stopped/wake_blocked | `userapp_activity` 表 |
| PublishTaskStore.map 的任务状态 | `publish_tasks` 表（进度事件流留内存） |
| waking/recycling/session_stream_registry/grpc_pool 等瞬态 | 不迁移 |

## 表清单（5 张）

- **containers**：PK container_name（无容器时 logical_id 占位，container_id 可空）；
  version 乐观锁列（Phase 2 用）。
- **projects**：PK project_id；container_name FK→containers ON DELETE SET NULL；
  model_provider/agent_status JSONB（api_key 明文——运维排查决策）；部分索引
  user_id/pod_id/(tenant_id,space_id)。
- **sessions**：PK session_id（resolve 热路径）；project_id FK 级联删；
  last_seen_at 节流写。
- **userapp_activity**：PK app_id；last_accessed（Pingora touch 5s 节流 + 批量 flush）。
- **publish_tasks**：PK task_id（uuid v7）；**部分唯一索引
  `UNIQUE(app_id) WHERE terminal_at IS NULL` = 同 app 单活跃任务**（跨进程/跨副本
  的 AppBusy 409；约束名 `idx_publish_one_active_per_app`——错误映射按约束名区分，
  主键冲突不得误判为 Busy）。

## 写侧（write-behind）

PersistOp（FIFO 保序）：UpsertProject/UpsertContainer/RemoveProject/AddSession/
RemoveSession/ClearSessions/DeleteContainerWithProjects（结构性，永不丢）；
TouchProject/TouchContainer/TouchSession/UpdateAgentStatus（幂等，队列 >10k 可丢）。
Touch 入队前 5s 节流。批 200 op 单事务，整批失败指数退避重试（全部幂等可重放）。

## 崩溃窗口

结构性 op 顺序持久化、毫秒级典型延迟；kill -9 损失尾部 Touch（闲置判据秒级误差，
可接受）。优雅关停 flush 有界 5s。
