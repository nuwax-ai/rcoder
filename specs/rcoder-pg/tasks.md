# rcoder-pg 任务清单（Phase 1 实施记录，2026-08-14）

## U 线：UserApp OpenAPI 注释补全（独立阶段，已完成）

- [x] U1 handlers/releases.rs 6 端点（doc 注释 + 全错误码 + 描述，对齐 service 错误分支）
- [x] U1 handlers/logs.rs 3 端点（同上；404/409/400/500 对应 get_app/无就绪 IP/app-cli 拒绝/后端）
- [x] U1 models/release.rs + models/logs.rs 字段级注释 + example
- [x] U2 query_apps/list_app_runtimes 补 500；ensure_builder/stream_task 补错误码
- [x] U2 防回归测试 `userapp_openapi_annotations_are_complete`
      （router.rs：38 端点强制"有描述 + 200 有说明 + ≥1 错误码"，已抓到 stream_task 漏网）

## M 线：rcoder-pg

- [x] M1 建 `crates/rcoder-storage`，storage 模块整体迁出（git mv，42 测试随迁全绿）；
      CleanupRequest/StorageStats/IdleContainerInfo 历史重复定义合并入 shared_types
- [x] M2 `ProjectStore` trait 入 shared_types（23 方法）+ `ProjectStoreBackend`
      枚举静态分发 + AppState/projects 接线 + 10 个调用方 trait 引入 +
      deprecated delegate 清理（update_session_atomic 零调用删除）
- [x] M3 migrations/0001_init.sql（5 表全 COMMENT ON）；PostgresConfig（to_dsn
      percent-encode/校验/脱敏日志，7 单测）；PersistOp 模型（结构性/幂等分类，
      快照 Box 抹平枚举尺寸差）；writer（批 200/退避重试/超深丢幂等/优雅排空）；
      load（containers→projects→sessions，latest 最后回放复原 latest_session）
- [x] M4 StorageConfig/StorageBackend + `RCODER_STORAGE_*`/`RCODER_PG_*` env 覆盖
      （env > yml > 默认）+ fail fast 校验；main 构造分叉（cfg-on-arm）；
      startup_cleanup/graceful 关停 PG 模式跳过 + flush；rcoder_default.yml storage 段；
      yml 解析测试（含旧配置零改动静兼容）
- [x] M5 ActivityPersistence 契约 + ActivityRow（shared_types）；registry
      Instant→DateTime 化 + dirty/deleted set + apply_loaded/collect_dirty；
      rebuild_stopped_apps 仅对未加载 app seed；PgActivityPersistence（5s flusher +
      forget 删除，background_tasks pg_shadow task）；PublishTaskPersistence 契约 +
      PgPublishTaskPersistence（唯一索引 Busy 映射**按约束名**区分——测试抓到主键
      冲突误判 Busy 的 bug 并修复）；task 终态/阶段钩子；store 双模式 +
      lookup_snapshot PG 回退；启动恢复 recover_running("rcoder restarted")；
      TTL 1h 清理
- [x] M6a specs 文档（本目录）+ AGENTS.md "跨 crate 契约进 shared_types" 约定
- [ ] M6b build-agent-docker：helm `storage/postgres.yaml`（单实例 STS+SVC+Secret，
      照 pxc.yaml 模式）+ values（PG env/secretKeyRef）+ make/k8s.mk CARGO_FLAGS
- [ ] M6c 集群端到端：192.168.1.20 部署 PG → PG 模式部署 rcoder →
      ①kill -9 后 resolve 200/容器在 ②滚动更新容器不清 ③backend=memory AB 回归
      ④build 中途重启 → failed("rcoder restarted")

## Phase 2/3 备忘（本阶段不做）

- Phase 2：后台任务 leader election（PG advisory lock 心跳）；启动对账（PG↔K8s diff
  清孤儿）；agent_runner 多订阅者（SSE 跨副本互踢根修，单槽 current_connection →
  conn_id 注册表）；Cilium Gateway SNAT 对 ClientIP affinity 影响验证；projects
  version 列跨副本冲突检测。
- Phase 3：CNPG operator 替换单实例 PG（对照 PXC 范式：operator manifest + helm CR
  + values mode 开关 + 私仓镜像重写 + offline bundle）。
