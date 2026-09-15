# kube-runtime 接入验证记录

状态：批次 A 已实施并提交；B/C/D 本轮未实施（独立批次推进，见末尾说明）。

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

## 批次 B/C/D：本轮未实施

- B（Builder STS/Pod 双资源观察）：A 的语义与测试已通过、满足推进前置，但本轮任务队列还包含 Compose/K8s 全链部署验证管道；B 涉及 `k8s_builder_control` wake/restart/stop 三路径的双流改造与身份复核重构，为避免与部署验证抢跑混线，作为独立批次留待下一轮（每批独立交付，不与 A 混成大改动）。
- C（Events publisher）：spec 定义为独立可选交付，不阻塞 A/B；未实施。
- D（删除观察）：spec 明确可选后续，未实施。
- 保留价值：A 的分类器/观察模块/契约测试可直接被 B 复用（`await_pod_verdict` 的分类回调注入设计即为此预留）。
