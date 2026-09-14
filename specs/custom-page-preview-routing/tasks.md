# Custom Page 多副本预览路由 — 实施任务

批次划分与完成标准（2026-09-14 计划评审后修订：执行器纯 Rust、TS 仓零改动、K8s=PG/Compose=进程内）。验收证据在本文件逐批登记（命令+退出码+报告路径）；历史证据不作数。

## 批次 0：文档同步

- [x] T0.1 spec/plan/tasks 按批准计划修订（纯 Rust 执行器、TS 零改动、存储分形态、发现层选型论证、keep-alive 降级信封决策）。

## 批次 1：契约/存储（无接线，行为零变化）

- [x] T1.1 `crates/shared_types/src/preview.rs`：契约全集（preview_key 规范化、状态机枚举、记录、AcceptStartInput/Outcome、ActivityFlushEntry、PreviewStoreError、PreviewLifecycleStore trait、四信封 camelCase + ToSchema、degraded_reason 词汇）。信封快照测试当场抓出 camelCase 缺失（已修）。设计强化：端口分配移入 accept_start 存储事务内（与受理原子），比原计划"协调器先分配再传入"更强。
- [x] T1.2 `crates/rcoder-storage/src/preview_lifecycle/{mod,domain,postgres,pg_tests}.rs` + `migrations-preview-pg/0001_preview.sql`：PG 实现全接口，全 CAS（FOR UPDATE + 条件更新，rows_affected=0 即冲突并回读现行行）。SQL 全字面量（SqlSafeStr 约束）。
- [x] T1.3 `crates/preview-coordinator`（新 crate，已入 workspace）：InProcessPreviewStore + contract_suite（11 组断言：受理前置/CAS 旧写回不覆盖新实例/端口全局唯一/unknown 证据门禁/GREATEST activity 刷盘/心跳/启动对账/查询视图）。
- [x] T1.4 验证关卡（2026-09-14）：`cargo fmt --all` clean；`cargo clippy -p shared_types -p rcoder-storage -p preview-coordinator --all-targets --features pg -- -D warnings` exit 0；`cargo nextest run -p shared_types -p rcoder-storage -p preview-coordinator --features pg` **327/327 全绿 0 skipped**——其中 `pg_preview_store_satisfies_contract` 在本机真实 PG（RCODER_PG_TEST_DSN）上运行通过，两后端同一断言集双跑通过。
  - 设计修订（用户问询后）：keep-alive activity 改为「内存累积 + 30s 批量刷盘 GREATEST」——activity 不参与正确性判定（存活用心跳），稳态零 PG 写/读；回收判定阈值 = TTL + 2×刷盘间隔（安全余量消除边界竞态）。

## 批次 2：协调核心与全入口

- [ ] T2.1 preview-coordinator 服务对象：start/stop/restart/keep_alive 业务流（plan §4 状态机，含 unknown 证据门禁 `HostEvidence` trait + 单机实现、uncertain 不自动重放）、端口全局分配（排除+重试≤3）、路由解析缓存（正/负 TTL、错误不负缓存）、后台任务（心跳轮询 30s/启动对账/空闲回收默认关）、内部令牌校验。
- [ ] T2.2 Rust 执行器适配：DevServerManager 票据入口（start_with_ticket 外部 port+preview_key 键 / stop_by_registration 按登记 pid 组 / verify / 日志按归属），agent-runner 与独立形态未注入时保持现状。
- [ ] T2.3 **等价性审计**：对照 TS 行为清单（rollup musl/gnu 检测、node_modules symlink 修复、模板缓存恢复（k8s no-op）、FAST_RESTART、.npmrc）逐项核对 Rust start 链在 K8s 共享存储场景需要的项，差距在 Rust 侧补齐并记录结论。
- [ ] T2.4 8086 内部端点 `/api/v1/preview-internal/*`（utoipa）：跨 Pod 执行代理（受理时非宿主的 stop/verify 派发）+ 预览转发校验查询；令牌中间件。
- [ ] T2.5 file-server 接入：`handlers/build/dev.rs` 七 handler 注入 `Option<Arc<dyn PreviewCoordination>>`；信封兼容层逐字段对齐 + 快照测试锁定四信封。
- [ ] T2.6 `file-server-proxy/src/config.rs`：dev 生命周期 7 路径所有策略 → Rust 上游；路由判定单测矩阵（三策略 × 路径集）；其余路径分流不变回归。
- [ ] T2.7 rcoder 装配：config 段 `preview_coordinator`（plan §8）、启动 fail-fast（对齐 `config/userapp_storage.rs` 装配范式）、POD_UID/POD_IP/boot_id 身份构造、kube HostEvidence（feature 门控）、后台任务 graceful shutdown。
- [ ] T2.8 验证关卡：fmt/clippy/nextest（preview-coordinator + file-server + file-server-proxy + shared_types + rcoder）全绿；Compose 本地 all_rust 冒烟（start/keep-alive/stop/restart 信封等价、已在运行幂等 start）。

## 批次 3：预览路由/HMR

- [ ] T3.1 rcoder-proxy：注入 `PreviewRouteResolver`（None=现状）；`/proxy/{port}` 解析分支（缓存→转发重写/本机校验/miss=legacy）+ 上游 pod_ip:8088 / localhost 双路。
- [ ] T3.2 `/internal/preview-forward/{instance_id}/{port}/{*path}` 路由：令牌校验、instance/host/port/登记四重校验、路径+query+尾斜杠保留、Host=127.0.0.1、410/503 语义。
- [ ] T3.3 重试语义：连接失败/410 且无 body 未升级 → 至多一次重解析重试；写请求/流式/WS 不重放。单测矩阵。
- [ ] T3.4 Compose 集成测试（真实 Vite 项目模板）：`/proxy/{port}/page/` 200、资产 query/尾斜杠、HMR WS 握手+改源码触发热更、stop 后 410/负缓存收敛、其余 `/proxy/{port}` 回归不变。
- [ ] T3.5 验证关卡：fmt/clippy/nextest 含 rcoder-proxy 全绿 + T3.4 场景报告。

## 批次 4：构建配置/E2E

- [ ] T4.1 build-agent-docker：deployment.yaml 补 POD_UID/POD_IP + `RCODER_PREVIEW_INTERNAL_TOKEN` env + values 新键 `rcoder.previewCoordinator`（不动在途 Chart.yaml/VERSION 修改）。
- [ ] T4.2 Helm 渲染验证：deploy.sh dry-run 合并顺序渲染 k8s-test values，断言新增 env/replicas/PG 挂载；不输出 Secret。
- [ ] T4.3 Compose E2E 新套件 `custom_page_preview`（tests-e2e/tests/，纳入 make 入口）：生命周期信封、预览 200、HMR、空闲回收（显式开启）、boot 对账。全绿退出码 0。
- [ ] T4.4 remote-k8s 多副本 E2E：专属环境（.env.local，replicas≥2，kubectl exec 逐 Pod 直连）：plan §10 多副本断言全集。记录 run_id/镜像 digest/源码指纹/断言清单于 `.remote-k8s/<env>/`。
- [ ] T4.5 交付登记：修改清单、验证命令与退出码、实际通过范围、未验证项（Java 实链路、公网 OpenResty error_page 独立验收、TS 独立部署形态边界）写入本文件。

完成标准：T4.3/T4.4 报告全绿且可复现；无共享集群/共享 release 变更；未自动 commit/push。

## 验收证据登记

（实施时逐批追加：日期、命令、退出码、报告路径、源码指纹。）

- 批次 0：2026-09-14 文档三件套修订完成（本文件+spec.md+plan.md）。
- 批次 1：
- 批次 2：
- 批次 3：
- 批次 4：
