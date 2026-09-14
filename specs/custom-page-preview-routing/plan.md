# Custom Page 多副本预览路由与生命周期 — 技术方案（已按用户约束修订）

对应 spec.md 的实现设计。修订要点（2026-09-14 计划评审后）：**执行器纯 Rust（TS 仓 nuwax-file-server 零改动）**；存储 **K8s=PG / Compose=进程内**（用户澄清 Compose 单节点）；发现层选型论证见 §11。

## 1. 架构总览

```
                     ┌────────────────────── rcoder Pod A（宿主） ──────────────────────┐
Java ──► Service:60000 ──► file-server-proxy（Rust,60000 纯分流，零 PG）                  │
  /api/build/start-dev 等 7 路径（所有策略）→ Rust 8086 协调 handler                      │
  /api/build/build 等其余路径              → 按策略 → TS 60001 / Rust 8086（不变）        │
                              ▼                                                          │
                    preview-coordinator（新 crate，rcoder 主进程内，每 Pod 一份）          │
                    ├── 状态机 + 条件受理（operation/instance/revision CAS）               │
                    ├── 存储：K8s=PG（rcoder-storage::preview_lifecycle）                 │
                    │          Compose=进程内实现（同一 trait，无 SQL 面）                  │
                    ├── 端口全局分配 + 路由缓存（port→instance 正/负缓存，短 TTL）         │
                    └── 执行器=进程内 Rust DevServerManager（spawn vite，--strictPort）    │
                                                        ▼                                │
                     ┌── rcoder Pod B（非宿主）──┐    Vite（detached，本 Pod 网络栈）      │
Java ──► Service:8088 ──► Pingora /proxy/{port} │      ▲                                  │
                          预览解析（协调器缓存→PG）│      │ /internal/preview-forward/...   │
                          命中宿主→跨 Pod 转发 ──┼──────┘ （Pod B 8088 校验令牌+instance+  │
                          未命中→原 localhost 行为│          host+port+本地登记→127.0.0.1）│
                     └───────────────────────────┘                                       │
```

组件落点：

| 组件 | 位置 | 职责 |
|---|---|---|
| 契约类型 + `PreviewLifecycleStore` trait | `crates/shared_types/src/preview.rs`（新） | kube-free/storage-free 纯契约：身份、状态、记录、信封、错误、trait |
| PG 存储 | `crates/rcoder-storage/src/preview_lifecycle/`（新）+ `migrations-preview-pg/` | 迁移 + 全 CAS 条件更新读写 |
| 协调器 | `crates/preview-coordinator/`（新 crate） | 状态机、受理、端口分配、本地执行器适配、后台任务（心跳轮询/空闲回收/启动对账）、路由解析缓存、`InProcessPreviewStore`（Compose） |
| 接入：生命周期入口 | `crates/file-server/src/handlers/build/dev.rs`（改）+ `crates/file-server-proxy/src/config.rs`（改） | handler 注入 `Option<Arc<dyn PreviewCoordination>>`（None=现状本机行为）；60000 对 7 个 dev 生命周期精确路径所有策略导向 Rust 上游 |
| 接入：预览路由 | `crates/rcoder-proxy/src/...`（改） | `/proxy/{port}` 预览解析分支 + `/internal/preview-forward/{instance}/{port}/{*path}` 宿主校验路由 |
| 部署 | `build-agent-docker/k8s/helm/nuwax-platform/templates/rcoder/deployment.yaml` 等 | POD_UID/POD_IP 注入、内部令牌、values 新键（不 bump 版本） |

TS nuwax-file-server **零改动**：60000 分流层不再把 dev 生命周期路径送往 TS；TS 继续承载其余存量路径；TS 独立部署形态（无 Rust 前置）保持现状本机行为（协调边界外，spec 已登记）。

## 2. 数据模型（preview_lifecycle 存储）

迁移集 `crates/rcoder-storage/migrations-preview-pg/0001_preview.sql`（独立迁移表 `_sqlx_preview_migrations`，`sqlx::migrate!` 编译期嵌入，复刻 userapp_lifecycle 模式）。

```sql
CREATE TABLE preview_instances (
  preview_key   TEXT PRIMARY KEY,          -- 规范化: project_id + '\x1f' + isolation 规范串 + '\x1f' + resolved_path
  project_id    TEXT NOT NULL,
  project_path  TEXT NOT NULL,             -- WorkspaceResolver 解析出的真实目录
  instance_id   TEXT NOT NULL,             -- UUIDv4，每次成功受理的 start 生成
  revision      BIGINT NOT NULL,           -- 每 preview_key 单调递增，旧写回 CAS 失败
  operation_id  TEXT NOT NULL,             -- 当前/最近操作的 UUID
  host_id       TEXT NOT NULL,             -- "{pod_uid}:{boot_id}"
  pod_name      TEXT, pod_ip TEXT,
  pid           BIGINT, port INTEGER,
  base_path     TEXT,
  state         TEXT NOT NULL CHECK (state IN ('starting','ready','stopping','stopped','failed','unknown')),
  last_heartbeat_at TIMESTAMPTZ,           -- 仅宿主心跳轮询刷新
  last_activity_at  TIMESTAMPTZ NOT NULL,  -- keep-alive/预览访问刷新（任意副本）
  detail        TEXT,                      -- 失败原因/诊断 JSON
  updated_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX preview_instances_port ON preview_instances(port) WHERE state IN ('starting','ready','stopping','unknown');
CREATE TABLE preview_operations (
  operation_id TEXT PRIMARY KEY,
  preview_key  TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('start','stop','restart')),
  state        TEXT NOT NULL CHECK (state IN ('accepted','running','succeeded','failed','uncertain')),
  host_id      TEXT NOT NULL,
  requested_port INTEGER, allocated_port INTEGER,
  created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
  result TEXT
);
CREATE INDEX preview_operations_key ON preview_operations(preview_key, created_at);
```

存储接口（`PreviewLifecycleStore` trait，shared_types 定义；PG 实现在 rcoder-storage，进程内实现在 preview-coordinator）：

- `accept_start(input) -> AcceptStartOutcome`：事务内校验旧状态可受理（无行/Stopped/Failed；Unknown 需恢复证据已由协调器落 detail）→ 行 starting + revision+1 + operation accepted。活跃行存在 → 返回 `ExistingActive(row)`（已 ready → 幂等信息；starting/unknown → 冲突上下文）。
- `publish_running(key, operation_id, revision, pid, port, base_path)`：CAS `WHERE operation_id AND revision AND state='starting'` → ready。
- `accept_stop` / `mark_stopped` / `mark_failed` / `mark_unknown` / `resolve_unknown_stopped`：CAS 于 (instance_id | operation_id, revision)；受影响行数=0 → `Conflict(current)`。
- `refresh_heartbeat(key, instance_id)`（仅 ready/unknown 行）、`refresh_activity(by port|key, instance_id)`。
- 查询：`get` / `find_active_by_port` / `active_ports` / `list_by_host` / `list_active`。
- `reconcile_host_reboot(pod_uid, boot_id)`：同 POD_UID 不同 boot_id 的活跃行 → stopped（容器重启销毁 PID 命名空间=直接证据）。

错误语义：存储层错误原样 `Unavailable`，协调器转 503/fail-fast；绝不映射为"不存在"。

## 3. 身份与端口分配

- **preview_key 规范化**（shared_types 纯函数）：`project_id + '\x1f' + isolation_norm(tenant_id,space_id,isolation_type) + '\x1f' + resolved_path`。isolation 字段与 `WorkspaceResolver::resolve_project` 的 ProjectContext 同源；不臆造 agentId（URL `{projectId}-{agentId}` 的组合由调用方 projectId 字符串天然承载）。
- **host_id**：`{POD_UID}:{boot_id}`。POD_UID/POD_IP Downward API 注入（Helm 补）；boot_id = rcoder 进程启动 `Uuid::new_v4()`（LazyLock）。Compose/本地：POD_UID 缺省 `HOSTNAME`/本机标识。
- **端口全局分配**：受理时排除活跃实例占用（`active_ports()`）+ 保留区（4000..=55000 跳过 8000..=9000），取最小可用；执行器 spawn 前 OS 级 bind 预检，strictPort 失败 op 内有界重试（≤3，重新分配）；三次失败 op=failed 并释放。同 preview_key 重启不承诺同端口（响应信封回传新 port）。

## 4. 状态机与操作流程

状态：`starting → ready → stopping → stopped`；`starting/ready/stopping → failed`；`ready/starting → unknown`；`unknown → stopped`（恢复证据落定）。

- **start**：
  1. WorkspaceResolver 解析 → preview_key。存储不可用 → 503。
  2. 条件受理：无行/终态 → 受理（host=本 Pod）；Unknown → 恢复证据（`HostEvidence` trait：K8s 查 Pod UID 存在性——kube 注入；Compose/单机：非本 host_id 即前代进程，视为终止）成立则先 CAS→stopped 再受理，否则 degraded/冲突；ready → 幂等返回当前 pid/port（取 Rust 现行幂等语义）；starting/stopping → 冲突信封。
  3. 受理落库（starting + revision+1 + operation）→ 分配端口 → 本地执行（§5）→ publish_running（CAS）→ start 信封。失败 → failed（含 detail）。执行超预算（600s 对齐 JS 10min）→ op=uncertain、行保持 starting；后续受理按 unknown 恢复路径处理，不自动重放。
- **stop**：受理=活跃行 + 操作身份匹配。按**登记 pid 进程组**杀（登记 miss/不匹配 → not_host，不裸杀）。行 stopping→stopped。迟到 stop（port/pid 与当前实例不匹配）→ 拒绝（success:false+reason），不杀新实例。
- **keep-alive（兼容层）**：
  1. 任意副本：按 port 精确定位活跃行（全局端口唯一保证无歧义）+ project_id 校验 → CAS 刷 last_activity_at。
  2. 存活判定：ready 且心跳新鲜（< TTL 90s）→ alive 信封（不回环探测）。心跳陈旧 → 回环 verify 宿主：alive→刷心跳回 alive；确认死 → 统一受理在**原宿主可达时原宿主重建/不可达时本 Pod 重建**（证据同 start）→ `action:"start"` 信封；宿主不可达且无证据 → **HTTP 200 + success:false + reason（host_unavailable/instance_unknown/port_mismatch）**（Java 从未见过非 200，最保守），绝不无条件成功、绝不在非宿主副本重建。
- **后台任务**（每 Pod，仅本 host_id 实例）：
  - 心跳轮询（30s）：本地探活 → CAS 刷 last_heartbeat_at；进程死 → failed(detail)；本地登记丢失（进程重启）→ unknown。
  - 空闲回收：`idle_recycle_secs`（**默认 0=关闭**，机制完整；测试环境显式开启验证）。心跳不刷 activity。
  - 启动对账（协调器 init）：`reconcile_host_reboot`。

## 5. 执行器（进程内 Rust DevServerManager）

执行器 = `file-server::DevServerManager`（已具备完整链：pnpm install + dev-inject + npmrc + 就绪轮询 poll_alive + stderr 错误分类 + 日志管道）。适配改动：

- 票据入口 `start_with_ticket(preview_key, project_path, port, base_path)`：外部 port（跳过本地 PortPool 决策）、登记键 = preview_key；spawn 参数/探活/日志与现行一致。
- `stop_by_registration(preview_key, expect: {instance_id,pid,port})`：仅登记匹配时按记录 pid 进程组杀；miss → not_host。替换"按 project_id 扫 ps"。
- `verify(preview_key, expect) -> {identity_match, alive, pid, port}`：登记匹配 + `is_project_alive`。
- **批次 2 含等价性审计**：对照 TS 行为清单（rollup musl/gnu 变体检测、node_modules symlink 修复、模板缓存恢复、FAST_RESTART）逐项核对 K8s 共享存储场景实际需要项，差距在 Rust 侧补齐（k8s 模板缓存恢复是 no-op，JS 报告确认）。

## 6. 预览路由（rcoder-proxy 8088）

- `/proxy/{port}/{*path}` 请求阶段（`port_proxy.rs`）默认 localhost 前插预览解析（注入 `Option<Arc<dyn PreviewRouteResolver>>`；None=现状）：
  1. 缓存查 port（正 10s/负 5s，正缓存含 instance_id+host_ip+到期；存储错误不写负缓存，降级 legacy 回环）。
  2. 命中且 host≠本机：URI 重写 `/internal/preview-forward/{instance_id}/{port}/{原剩余路径+query}` + 内部令牌头，上游 `HttpPeer(pod_ip:8088)`（连接/读超时沿用，idle 3600 保 HMR）。
  3. 命中且 host==本机：本机校验后 localhost（校验逻辑与转发路由共用）。
  4. 未命中：原回环行为不变。
- 新路由 `/internal/preview-forward/{instance_id}/{port}/{*path}`（各副本 8088）：令牌校验（错/缺=404）→ 协调器校验 instance 存在、host_id==本机、port==instance.port、本地执行器登记匹配 → 重写剩余路径、Host=127.0.0.1、上游 localhost:{port}。身份不匹配 → 410（调用方至多一次重解析）；登记 miss 但 PG 称本机宿主 → 503（对账/心跳随后收敛），不转发。
- 重试语义：仅上游**连接失败**（未发送请求）或 410，且请求无 body/未升级 WS → 至多刷新一次重解析重试；POST/流式/已升级一律不重放。
- 保留项：query、尾斜杠、base、Upgrade/Connection 透传（现 `rewrite_uri` + hop-by-hop 处理沿用）；HMR WS 用真实 Vite 版本端到端验证（Compose E2E）。

## 7. 兼容层（Java 信封，Rust 协调 handler 输出）

| 接口 | 原信封（TS，取证） | 协调后 |
|---|---|---|
| start-dev | `{success:true,message:"Development server started",projectId,pid,port}` | 成功保持；已运行（ready+心跳新鲜）幂等返回当前 pid/port（Rust 现行语义）；starting 中→冲突信封（对齐 PROJECT_STARTING 语义）；失败→现有错误结构+detail |
| keep-alive | alive:`{success:true,message:"Development server is alive",projectId,pid,port}` / 重建:`{...,action:"start"}` | 两态保持；degraded = **HTTP 200 + success:false + reason**（决策记录） |
| stop-dev | `{success:true,message:...,projectId}` | 保持；非宿主经协调定位宿主执行后同形回包；身份不匹配→success:false+reason |
| restart-dev | `{success:true,message:"Development server restart successfully",projectId,pid,port}` | 保持 |
| list-dev / get-dev-log / port-pool-status | 数组/文本/JSON | list 聚合存储视角（保留向后兼容字段+新增 host 归属列）；log 从宿主本地读（协调器查归属）；pool-status 输出协调器端口视图 |

HTTP 方法/路径/参数完全不变（GET + query）。Rust 侧 utoipa 补文档；内部端点同样 utoipa。

## 8. 配置（config.yml 新段）

```yaml
preview_coordinator:
  enabled: true                 # false=完全现状（不注入 trait，60000 不改路）
  internal_token_env: RCODER_PREVIEW_INTERNAL_TOKEN   # 令牌从 env 读，不落配置文件；空且 enabled→禁用并告警
  heartbeat_interval_secs: 30
  heartbeat_ttl_secs: 90
  idle_recycle_secs: 0          # 默认关闭
  route_cache_positive_secs: 10
  route_cache_negative_secs: 5
  start_budget_secs: 600
```

存储后端：K8s（feature kubernetes/rcoder-pg）复用主 PG 连接（`config/userapp_storage.rs:111-129` 装配范式）；Compose=进程内实现（无配置面）。enabled 且存储不可用 → 启动 fail-fast。

## 9. Helm（build-agent-docker，不 bump 版本、保留在途修改）

- deployment.yaml：补 `POD_UID`（fieldRef metadata.uid）、`POD_IP`（status.podIP）；`RCODER_PREVIEW_INTERNAL_TOKEN` env（values 新键 `rcoder.previewCoordinator.internalToken`，空=协调器禁用并告警——不引入弱令牌）。
- NetworkPolicy 已放行同 ns 8086/8088（复核确认），无需改。
- 无 npm/tarball 镜像通道（TS 零改动，不涉及 JS 包发布）。

## 10. 测试设计（先能失败，后实现）

- 单元：preview_key 规范化；端口分配（排除/重试）；状态机 CAS（旧 revision/operation/instance 写回失败）；缓存（TTL、错误不负缓存）；启动对账；信封序列化快照。
- 存储契约：InProcess 与 PG（本机 docker PG 可用时）双跑同套断言。
- 集成（Compose，真实 Vite + 模板项目）：start→`/proxy/{port}/page/` 200、HMR WS 握手+改源码热更、stop/restart/keep-alive 信封、空闲回收（显式开启）、boot_id 对账。
- 多副本（remote-k8s 专属环境，replicas≥2，`kubectl exec` 逐 Pod 直连，不依赖 ClientIP）：两宿主端口全局唯一；每 Pod 直连 8088 命中正确宿主；非宿主 keep-alive/stop/restart 仅宿主执行；旧心跳/迟到 stop 不覆盖新实例；缓存指向错误实例→410→重解析；PG 停止→生命周期 503、预览降级 legacy；Pod 重建→unknown→证据恢复。
- Helm 渲染：deploy.sh dry-run 合并顺序渲染 test values，断言 POD_UID/POD_IP/令牌/replicas/PG（不输出 Secret）。

## 11. 发现层选型论证（决策记录）

K8s 下"哪个 Pod 托管 project/port"的权威记录三个候选：**PG 注册表（选定）**——已是 K8s test/prod 硬依赖（userapp 七表同库，不新增故障域）、真事务 CAS 支撑并发受理串行化、仓库有 userapp_lifecycle 可复制范式；K8s 原生（Pod annotation + Lease resourceVersion 当 CAS）——Pod 消亡身份自动蒸发优雅，但要自造冲突重试/心跳/清理语义（= 自造租约协议，交接风险条款点名禁止）、Compose 无对应物；gossip/一致性哈希——扩缩容丢仲裁、最终一致窗口路由 miss。headless 域名只解决寻址不解决发现（pod 名与 pod IP 同生命周期），暂不需要；pod_ip + 实例身份校验已拦 IP 复用，将来换寻址层不影响设计。
