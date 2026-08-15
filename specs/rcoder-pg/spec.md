# rcoder-pg：PostgreSQL 持久化存储后端（Phase 1）

## 背景与目标

rcoder 主服务此前以纯内存（DashMap）承载 project/session/container 映射、UserApp
活动状态与发布任务表——单节点假设：重启即丢（session resolve 404）、启动/关停全量
清容器。本特性引入可选 PostgreSQL 持久化（`rcoder-pg` feature + `[storage]` 配置），
为 k8s 多副本铺路：

- **Phase 1（本 spec）**：单副本 PG 模式。rcoder 重启（含 kill -9、滚动更新）后
  session resolve 不 404、用户容器不被清理。
- Phase 2：多副本解锁（leader election、启动对账、agent_runner 多订阅者、SSE 粘性验证）。
- Phase 3：CNPG HA（3 节点挂 1 仍可服务）。

## 模块划分

```
shared_types（领域 + 跨 crate 契约，单一事实源）
  · ProjectStore trait（23 方法，与 ContainerLookup 并列）
  · ActivityPersistence trait + ActivityRow
  · CleanupRequest / StorageStats / IdleContainerInfo（历史重复定义已合并于此）
        ↑
rcoder-storage（存储层独立 crate，default=纯内存零 sqlx）
  · adapter：ProjectAdapter 内存实现（自 rcoder/src/storage 迁出）
  · backend：ProjectStoreBackend 枚举（静态分发：Memory | Postgres）
  · publish_repo：PublishTaskPersistence 契约（原语字段边界）
  · config：PostgresConfig（[storage.postgres] 数据模型 + to_dsn）
  · pg（feature="pg"）：PgStore（内存镜像+write-behind）+ writer + load
    + activity/publish 两个 PG 实现
        ↑
rcoder（AppState.projects: Arc<ProjectStoreBackend>；main 按配置分叉）
```

**不做 dyn、不做泛型**：后端启动时决定、运行期不变 → 枚举 match 静态分发；
Pingora 的 `Arc<dyn ContainerLookup>` 为既有 crate 边界，非本次引入。

## 核心设计决策

| 决策 | 内容 | 依据 |
|---|---|---|
| 读路径 | 全部走内存镜像（同步） | ContainerLookup 是同步 trait；session resolve 每消息级热路径 |
| 写路径 | 先应用镜像（复用唯一一份业务逻辑）→ 同步 enqueue → writer 批量落 PG | write-behind 毫秒级延迟；结构性 op FIFO 保序永不丢 |
| 启动 | connect → migrate（advisory lock，多副本安全）→ 全量 load 重建镜像 | PG 为跨重启真源；经 inner 直写天然旁路持久化 |
| 行为分叉 | PG 模式跳过 startup_cleanup / graceful 清容器（flush writer 后退出） | 否则每次重启清光容器+库，持久化无意义；滚动更新不打断用户容器即此副产品 |
| Activity | 影子持久化（dirty set + 5s flusher），不做双实现 | wake single-flight 等是进程内协调机制；Instant→DateTime 换 wall-clock |
| PublishTask | 内存对象保留；任务行入库（唯一索引=跨副本 AppBusy；启动恢复标记僵尸 failed） | 进度流本质进程内；状态跨重启可查 |
| compose 路径 | 默认编译不进 sqlx，行为字节级不变 | feature 在编译层而非约定层保证 |

## 验证方式

- 单测/集成：`cargo test -p rcoder-storage [--features pg]`（60 用例；
  PG-gated 需 `RCODER_PG_TEST_DSN`，未设跳过）。
- 本地端到端：`docker run postgres:17` + `cargo run --features rcoder-pg` +
  `RCODER_STORAGE_BACKEND=postgres`。
- 集群：单实例 PG StatefulSet（Phase 3 换 CNPG）→ 部署 → kill -9/滚动更新场景验证。

详细任务拆解见 [plan.md](plan.md)；表结构见 [data-model.md](data-model.md)。
