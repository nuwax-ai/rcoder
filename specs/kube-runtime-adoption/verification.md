# kube-runtime 接入验证记录

状态：批次 A、B、C 已实施并提交；D（删除观察）为可选后续未实施。

## 基线

- 日期：2026-09-16
- HEAD：`e5ca73bd`（分支 `codex/userapp-remove-user-id-binding`）
- kube-runtime 版本：4.2.0（Cargo.lock 锁定，未升级；workspace `kube = 4.2` 且已启用 `runtime`/`kube-runtime` feature——无依赖整理需求）
- 实施前置：Task「UserApp 移除用户绑定」（0b4691b7）已并入；`docker_manager` kubernetes feature 全量编译通过。

## 批次 A：通用 Pod 等待（已交付）

### 改动

| 文件 | 内容 |
|---|---|
| `docker_manager/src/runtime/k8s_observation.rs`（新增） | 私有观察模块：`Verdict<T>` 纯判定模型、`classify_pod_readiness` 分类器、`await_pod_verdict` 单对象有界 watch、`ObservationError` 结构化错误（Deadline/Cancelled/Fatal{code}/StreamEnded） |
| `docker_manager/src/runtime/k8s_observation_tests.rs`（新增） | 流式 HTTP 契约测试（受控 apiserver：真实 kube wire LIST 快照 + WATCH chunk 流） |
| `docker_manager/src/runtime/k8s_pod.rs` | `wait_for_pod_ready` 1s 轮询 → 单 watch 观察；`detect_container_failure`/`POD_POLL_INTERVAL_SECS` 随迁删除 |

### 语义保持（KR01–KR06 对照）

- KR01：分类器逐档单测锁死（Ready/Succeeded 成功；Failed/CrashLoopBackOff/ImagePullBackOff 拒绝；Running 未 Ready/Pending/ContainerCreating/无 status 继续）。未用 `is_pod_running` 替换业务条件。
- KR03：总 deadline 由调用方传入，循环内 `checked_duration_since` 不重置；watch 连接轮换 `timeout(290)` 与业务预算分离。
- KR04：取消 = drop select 分支随流释放；`cancellation_releases_observation` 契约测试断言取消后 5s 内收束。
- KR05：401/403 → `Fatal{code}` 及时返回（LIST 与 WATCH 两通道都有契约测试）；410/断连/正常 EOF 交由 kube-runtime 4.2 watcher 内建恢复，不自行实现 RV 协议。
- KR06：观察模块零写能力（无 create/delete/patch API 面）；`Verdict` 仅由分类器产出。

### 命令与退出码

| 命令 | 结果 |
|---|---|
| `cargo check -p docker_manager --features kubernetes` | 0 错误 |
| `cargo clippy -p docker_manager --features kubernetes --all-targets` | 0 警告 |
| `cargo nextest run -p docker_manager --features kubernetes observation` | 9/9 通过 |
| `cargo nextest run -p docker_manager --features kubernetes --no-fail-fast` | 186/187（1 失败 = `update_rejects_replacement_lifecycle`，**基线 c5782efe 同败**，与本改动无关，已用临时 worktree 验证基线复现） |
| `cargo nextest run -p docker_manager`（默认 feature） | 通过（Docker 行为不变） |

### 未运行项

- 真实 K8s（remote-k8s userapp/chat 套件）：本轮未执行——排在全任务管道的 Compose 验证与 131 环境部署之后统一执行（见任务队列）。
- 性能基线 vs watch 候选的请求数对比：未采集（需真实集群长等待场景；不预设收益数字——spec 验收边界）。

## 批次 B：Builder STS/Pod 双资源观察（已交付，commit `f5db84a6`）

### 改动

| 文件 | 内容 |
|---|---|
| `k8s_observation.rs` | 新增 `BuilderWatchEvent{Sts,Pod,PodAbsent}`、`await_builder_verdict` 双流观察：STS/Pod 各自 field_selector 锁名的 watcher 在同一 deadline/cancel/退避边界内消费（B07 同源纪律）；分类闭包 Err → Fatal 快速失败 |
| `k8s_builder_control.rs` | `apply_builder_compute_mode` 观察段轮询 → 双流观察：身份/replicas 冲突快速失败；完成候选最后 GET 复核（stop=Pod 404+STS replicas 0；ready=Pod UID 同一性+STS 身份/replicas）；同 UID Pod 重现回到观察不放大预算；`observation_error` 结构化映射（Deadline 不授权租约释放）。契约测试服务器重写为语义并发服务器（method/path 分类、PATCH 状态感知单对象 GET、DELETE preconditions 请求体断言）；修 FuturesUnordered 空流 select 自旋 |
| `k8s_app_create.rs` | 修复既有失败用例断言漂移（基线 `baf8d83d` stash 复现同败）：捕获阶段身份拒绝已结构化为 `CreationAborted{failed_at: Capture, definitive_rejection}`，按已批准行为更新断言并保留"身份核验先于一切资源副作用"保护 |

### KR 对照

- KR02/KR07：stop=STS uid/rv 前置 patch + Pod DELETED 完成 + 复核 404/replicas 0；restart=Pod uid/rv 前置 delete + 新 Pod Ready 完成（旧 Pod Ready 不完成新 restart——LIST 空集 + watch ADDED 驱动）；wake=replicas 0→1 patch + 新 Pod Ready + replicas 保持 1 复核；STS 替换（replacement-sts uid）在写前拒绝且 recorder 断言零写。
- KR09 前置：写拒绝（PATCH/DELETE 403）传播为 RequestRejected，recorder 断言 preconditions。

### 命令与退出码（2026-09-16）

| 命令 | 结果 |
|---|---|
| `cargo nextest run -p docker_manager --features kubernetes actual_bound actual_stop` | 2/2 通过 |
| `cargo nextest run -p docker_manager --features kubernetes --no-fail-fast` | 196/196 通过（含上表） |
| `cargo nextest run -p docker_manager --no-fail-fast`（默认 feature） | 84/84 通过 |
| `cargo clippy -p docker_manager --features kubernetes --all-targets` | 0 警告 |
| `cargo fmt --all -- --check` | clean |

## 批次 C：Event publisher + RBAC（已交付）

### 改动

| 文件 | 内容 |
|---|---|
| `k8s_event_publisher.rs`（新增） | 受管共享 publisher：有界 mpsc(256) + 单消费者串行 kube `Recorder`；`publish` 非阻塞 try_send（满/关闭计丢弃+30s 限速 warning）；单次发布 3s 上限无重试；关停=最后一个句柄 Drop → biased 取消优先 → 总预算 5s 有界排空（外层超时包住在途发布）→ 剩余数量计数；note 1kB UTF-8 边界截断、reason/action 固定词表 `&'static str`（操作 ID 仅入 note）；`start` 返回计数器句柄（运维观测面） |
| `kubernetes_runtime.rs` | `event_publisher` 字段（Arc 共享，clone 不重建；Default=inactive 供测试构造） |
| `k8s_builder_control.rs` | `apply_builder_compute_mode` 包一层结果上报瓶颈：成功事件在真实结果确认后发布（ComputeStopped/ComputeStarted），失败区分明确拒绝（ControlRejected）与不可确认（ControlUncertain，对齐 RecoveryRequired）；fire-and-forget 不改变控制结果 |
| `k8s/config/rbac.yaml`、`tools/remote_k8s/manifests.py`、build-agent-docker `clusterrole.yaml`（跨仓） | events.k8s.io/events create+patch 最小授权（三来源同步；core events 只读保留给诊断查询） |
| `tools/remote_k8s/tests/test_workflow.py` | render 测试断言 Role 含 events.k8s.io create/patch 规则 + RoleBinding→ServiceAccount 绑定链 |

### KR 对照

- KR09：`event_queue_full_drops_without_blocking_lifecycle`（满队精确丢弃计数 + publish 永不阻塞）；`event_publish_rejections_are_counted_and_business_unaffected`（403 全计数、零重试）；`event_shutdown_drain_is_bounded_and_records_remaining`（总预算有界 + 剩余记录）；`event_success_publishes_via_events_api`（真实 events.k8s.io create 契约）。
- KR10：API group=events.k8s.io（Recorder 契约）、note 截断、固定词表；RBAC 三来源 create/patch 与测试断言（`diagnostic_event_note_is_sanitized_to_1kb`、manifests render 测试）。

### 修复过程中发现并修复的实现缺陷

- 排空预算最初只约束 recv 不包住在途发布（逐条 3s 可突破总预算）→ 外层 `timeout_at` 包住整个排空段。
- 主循环 `select!` 无偏随机导致关停可在取消前持续消费（排空预算失效）→ `biased` + 取消分支优先。
- 测试语义服务器 accept 循环 `FuturesUnordered` 空流 `next()` 永远 Ready(None) → select 自旋（93% CPU）→ 改纯 accept + detach。

### 命令与退出码（2026-09-16）

| 命令 | 结果 |
|---|---|
| `cargo nextest run -p docker_manager --features kubernetes --no-fail-fast` | 202/202 通过 |
| `cargo nextest run -p docker_manager --no-fail-fast`（默认 feature） | 84/84 通过 |
| `cargo clippy -p docker_manager --features kubernetes --all-targets` | 0 警告 |
| `cargo fmt --all -- --check` | clean |
| `python3 -m unittest tests.test_workflow`（tools/remote_k8s） | 9/9 OK |

## 未运行项（A/B/C 共同）

- 真实 K8s（remote-k8s userapp/chat/gateway 套件）：排在全任务管道的 Compose 验证与 131 环境部署之后统一执行。
- 性能基线 vs watch 候选的请求数对比：未采集（需真实集群长等待场景；不预设收益数字）。
- build-agent-docker chart 变更：clusterrole.yaml 已改，随该仓镜像构建管道一并生效，不单独发布 chart。

## 批次 D：可选后续未实施

- 删除观察（`k8s_app_deletion`/`k8s_builder_deletion` 迁移到观察模块）：spec 明确可选，不阻塞 A/B/C。
