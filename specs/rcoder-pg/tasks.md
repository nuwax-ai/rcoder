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
- [x] M6b build-agent-docker：helm `storage/postgres.yaml`（单实例 STS+SVC+Secret，
      照 pxc.yaml 模式）+ values（PG env/secretKeyRef）+ make/k8s.mk CARGO_FLAGS
      （已提交 build-agent-docker 仓库）
- [x] M6c 集群端到端：192.168.1.20 + 192.168.32.229 双集群验证
      ①kill -9 后 resolve 200/容器在 ②SIGTERM flush 完成 ③backend=memory AB 回归
      （PG STS 保留在 .20 nuwax-k8s-test ns）

## Phase 2（多副本解锁，2026-08-15 完成）

- [x] P2-M1 跨副本读感知：pending_ops 原子计数 + wait_drained 排空屏障 +
      pg/sync.rs 5s 全量 diff 同步（排空屏障保证删除判定安全；
      签名比对跳过无变化行；应用走 inner 直写旁路持久化）
- [x] P2-M2 agent_runner 多订阅者（SSE 互踢根修）：单槽 ArcSwapOption →
      DashMap<u64, ConnectionState> 注册表 + AtomicU64 conn_id；
      close_connection(id) 关自己 / close_all_connections 清场；
      MAX_SUBSCRIBERS=8 超限逐最旧；Push 遍历投递 Closed 按 id 移除
- [x] P2-M3 leader election（pg/leader.rs）：session 级 advisory lock
      （LEADER_LOCK_KEY=0x7263_6f64_6572）+ 5s 保活探测 + 让位；
      run_leader_supervisor 代际 channel 拉起/停止 4 个单实例任务
- [x] P2-M4 双进程验证（.20 PG）：leader 互斥（仅 B 获主）+ A 写 B 经 sync 5s
      内 resolve 200 + kill leader B → A 4s 接任 + 后台任务重启

## SSE 修复（main 分支既有缺陷，2026-08-15 定位+修复）

- [x] 根因：SharedStream ring 跨 turn 残留 + 前端 last_seq=0 重连全量 replay
- [x] 修复：终端事件（SessionPromptEnd）广播后 clear_ring；last_seq 保持单调
- [x] 边界审查：epoch 哨兵被连带清但不构成缺陷（僵尸流 is_alive 检测保证
      重连创建全新流，被清 ring 无人读取）
- [x] 验证：单元测试 13 绿 + docker 实机 ring cleared 生产日志 + 重连 0 重复

## 待开发

- [ ] **Phase 3：CNPG HA**（3 节点挂 1 仍可服务）——operator manifest（对照
      PXC 范式）+ helm CR 模板 + values mode 开关 + 私仓镜像重写 + offline bundle
- [ ] **多副本正式部署**：helm `replicaCount: 2` + PDB 开启 → 集群内 rcoder
      双副本运行（本地双进程已验证，集群内需构建正式镜像 ~40min）
- [ ] **Cilium Gateway SNAT 验证**：Gateway API 流量经 envoy SNAT 后
      ClientIP affinity 是否仍生效（影响多副本 SSE 粘性）
- [ ] **projects version 列跨副本冲突检测**（Phase 2 预留，单副本无需）
- [ ] **ACP SDK 行为回归**：agent-client-protocol 依赖升级后需集群内真实
      chat 链路验证（AGENTS.md 风险约束 #3）

## Phase 2/3 备忘（本阶段不做）

- Phase 2：后台任务 leader election（PG advisory lock 心跳）；启动对账（PG↔K8s diff
  清孤儿）；agent_runner 多订阅者（SSE 跨副本互踢根修，单槽 current_connection →
  conn_id 注册表）；Cilium Gateway SNAT 对 ClientIP affinity 影响验证；projects
  version 列跨副本冲突检测。
- Phase 3：CNPG operator 替换单实例 PG（对照 PXC 范式：operator manifest + helm CR
  + values mode 开关 + 私仓镜像重写 + offline bundle）。
