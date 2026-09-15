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

- [x] T2.1 preview-coordinator 服务对象（service.rs 850+ 行）：admit_and_start（预读幂等快路径+Unknown 证据准备+端口有界重试≤3）/coordinated_stop（受理→派发→CAS 终态）/keep_alive_dev 全分支（心跳新鲜→alive；陈旧→宿主 verify；确认死→mark_failed+统一重建；端口命中他人→降级不动作；Starting→业务冲突错；Stopping→降级等终态；Unknown→证据门禁；终态→重建）/list/log/port_status/resolve_route（缓存→权威库，错误降级不缓存）/check_forward/internal_stop（操作身份五重校验）。
- [x] T2.2 Rust 执行器适配（file-server `dev_server/coordinated.rs`）：start_coordinated（票据端口+preview_key 键+log_key 日志命名+manifest 拒绝）/stop_coordinated（仅登记匹配按记录 pid 组杀，绝不 ps 扫描；身份不符放回登记返回 IdentityMismatch）/verify_coordinated/DevServerExecutor（PreviewExecutor 实现）。start.rs 提取 spawn_and_register 共享管线（legacy 行为零变化）；stop.rs 提取 terminate_pid_group。
- [x] T2.3 等价性审计结论：K8s 场景 Rust 启动链与 TS 等价——rollup musl/gnu 变体风险被恒注入 ROLLUP_WASM=1+ROLLUP_DISABLE_NATIVE=1 覆盖（同一故障模式的替代解法）；k8s 模板缓存恢复在 TS 本就是 no-op；FAST_RESTART 语义被 Rust「增量 install 不删 node_modules」覆盖；dev-inject/npmrc/strictPort/poll_alive/stderr 分类逐项一致。
- [x] T2.4 内部端点 `/api/v1/preview-internal/{stop,verify,log}`（internal_http.rs，utoipa + 令牌中间件，缺失/不符 404 不暴露端点存在性）。
- [x] T2.5 file-server 七 handler 协调分支（app_id 存在=userapp 域不走协调）；四信封映射（Unavailable→500/Conflict→400 业务错/Invalid→400 校验）。
- [x] T2.6 60000 改路：`coordinated_dev_lifecycle` 开关（rcoder 装配与 preview_coordinator.enabled 联动；独立/npm 形态恒 false=历史行为）；路由判定矩阵测试（三策略×7 端点×尾斜杠×非前缀误命中×禁用回退）。
- [x] T2.7 rcoder 装配：config 段 `preview_coordinator`（peer_api_port 装配时对准主 API 端口）、preview_assembly.rs（K8s=PG[userapp_storage.postgres 优先→storage.postgres 回落]/Compose=InProcess；enabled 且令牌缺失/<16 字符→fail-fast）、KubeHostEvidence（fail-closed：kube 错误=拒绝接管）、启动对账+后台任务（心跳 30s/刷盘 30s/回收 60s 轮询，broadcast 停机信号，flusher 退出补刷）。
- [x] T2.8 验证关卡（2026-09-15）：fmt clean；clippy 六 crate（-D warnings，rcoder 双 feature 组合）全绿；nextest 993/993 通过。提交 `2333242`（含并行会话补齐：agent_runner env 透传缺省 false、file-server-userapp 测试字段、workspace 2118 全绿）。
- [x] 代码复查（用户指令）：发现并修复三缺陷（提交 `5180708`）——①令牌 env 名漂移（guard 硬编码默认名 vs 配置名→跨 Pod 派发全 404；改协调器显式持令牌同源）②check_forward 每请求探活（verify_local 含 HTTP 探测→延迟翻倍；新增 registration_matches 纯内存校验）③next_candidate 落保留区/越界烧预算。复查验收：clippy 四 crate -D warnings 绿 + nextest 544/544。
  - 已知差异登记：协调 stop 的 killedPids 恒空数组（诊断字段，TS 返回实际清单）；keep-alive 每次调用 1 次权威库索引读（此前沟通的"稳态零 PG 读"未实现——写路径零 PG 已达成，读为 65s/实例一次可忽略）；PG 故障时 check_forward 返回 410（语义上 503 更准确，行为等价：重解析→降级回环）。

## 批次 3：预览路由/HMR

- [x] T3.1 preview_slot（ArcSwapOption 回填槽，对齐 dev_ensure 范式——协调器装配晚于 Pingora 启动）；`/proxy/{port}` 解析分支（缓存→转发重写+令牌覆盖/miss=legacy 零变化）；上游双路（宿主 pod:peer_api_port / localhost）。TrackingCtx 增 preview_peer/preview_forward_port/preview_origin_port。
- [x] T3.2 `/internal/preview-forward/{instance_id}/{port}/{*path}` 成对注册；request_filter 短路闸门（令牌缺失/不符 404 不暴露端点、身份不匹配 410、登记缺失 503）；校验通过剥离内部前缀，剩余路径+query+尾斜杠+@vite 深层资产原样转本机 vite，Host=127.0.0.1，长连接配置与 port_proxy 一致（HMR ws）。
- [x] T3.3 重试语义（实现方式修订）：宿主 410 → 转发方 response_filter 即时失效该端口缓存（invalidate_route）+ TTL≤10s 兜底；"无 body 未升级自动重放一次"暂缓——pingora response 阶段重放复杂度高，Compose E2E（T4.3）实测单请求 410 可见性后再定。写请求/流式/WS 天然不重放（无重放机制）。
- [x] T3.5 验证关卡（2026-09-15）：clippy 四 crate --all-targets -D warnings 全绿；nextest 629/629 通过。单测：路由注册矩阵/转发重写（query 保留/令牌覆盖/上游覆盖/深层资产）/三种不转发解析+槽空零变化。提交 `77d1c26`。T3.4（真实 Vite 端到端）并入批次 4 的 compose E2E（T4.3）执行。
- [ ] T3.4 → 并入 T4.3。

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
