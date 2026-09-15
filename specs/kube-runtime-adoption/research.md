# kube-runtime 局部接入调研报告

日期：2026-09-14。状态：调研完成，实施建议待执行；不是功能验收报告。

## 结论

技术上可行，推荐将 kube-runtime 用作 Kubernetes 观察基础设施，保留现有 SQL 生命周期协调器、操作租约、条件写入和 Docker 实现。先完成 Pod 等待试点，再迁移 builder 控制观察，最后增加 Events。删除完成观察单独评审；暂不采用全局 reflector、Controller 或自定义 finalizer。

收益来自减少重复查询、统一有界等待、明确观察错误和改善现场诊断，不来自替代并发控制。watch 收到 Ready、资源消失或者连接恢复，都不能授权重放未知写入或释放资源租约。

相关产物：[行为规范](spec.md)、[实施方案](plan.md)、[任务与证据](tasks.md)。本方案不覆盖或修改既有 `userapp-lifecycle-convergence` 的业务契约。

## 证据范围

本次补充走读基线 HEAD 为 `45c41223cf54695eff3b53c245ba8e13f3f5c424`。工作区存在其他 agent 的生命周期、runtime 和 E2E 改动，结论针对读取时的工作区；实施前必须重新核对 diff，不能把 HEAD 当作完整被审源码。

证据等级分开记录：

| 等级 | 已有证据 | 不能证明 |
|---|---|---|
| 源码 | 项目实现与 Cargo registry 中锁定的 kube-runtime 4.2.0 源码 | 新实现编译或业务行为正确 |
| 依赖 | 前一轮 `cargo tree -p docker_manager --features kubernetes -e features -i kube-runtime --offline` 退出 0 | features 精简后的编译结果 |
| 集群只读 | 前一轮通过配置指定 SSH/context/namespace 检查资源、权限及原始 watch | rcoder Pod 内 kube-runtime 连通性、恢复能力和性能 |
| 未执行 | 新代码、Rust 编译测试、负载基线、断连故障注入、部署 E2E | 不得标为已通过 |

前一轮同日集群观察：两个节点 Ready，服务端 v1.35.4+k3s1；专属测试 namespace 中 rcoder 2/2、PostgreSQL Ready，两个 builder STS 均为 0 副本，6 个 PVC。以实际 rcoder ServiceAccount impersonation 发起按名称过滤的 Pod watch，收到 ADDED 与 BOOKMARK，正常退出。Events 新 API group 的 create/patch 授权检查均返回 no。

这些是先前只读采样结果，未在本次文档写作中重新采样，不代表当前负载。没有统计全群 PV 数量，也没有读取其他业务 namespace 内容。真实主机和认证信息只保留在忽略的配置中，不写入提交文档。

远端 kubectl 当时为 1.30.14，命令提示与服务端版本偏差超出支持范围。正式验证前应准备匹配客户端并通过 doctor；不自动替换主机全局工具。

## 前提勘误

| 原判断 | 复核结论 |
|---|---|
| features 只有 ws 起作用 | 错。runtime 不在默认 features 中；等价保留 runtime 的精简是 `["runtime", "ws"]`。仅保留 ws 会移除显式 runtime 能力，不是等价整理 |
| rcoder 的 kube 直接依赖未使用 | `crates/rcoder/src` 中未发现 kube/k8s_openapi 直接引用；可单独清理，但需编译验证 feature 传递和各目标 |
| Ready 每 500ms 轮询 | 当前 `k8s_pod.rs:35` 为 1 秒 |
| kube client 没有超时 | RCoder 未定制，但 kube-client 默认有 connect/write timeout，read timeout 默认 None；操作总截止时间仍需业务层提供 |
| 现有 RBAC 对所有候选都够用 | Pod/STS 等 list-watch 基本满足；Recorder 使用 events.k8s.io，core Events 权限不能替代 |
| Events 零风险 | 增加 API 写入、权限、队列和错误路径，不能直接影响部署结果 |
| delete helper 等价 | 不等价。内置 is_deleted 将 UID 替换视为成功，项目删除观察要求 Conflict |
| finalizer 只用于 CRD | 泛型不限 CRD；但不代替 pvc-protection，也不解决未知写入 |
| Controller 必须迁移 SQL 到 CRD | 不必须，但需另建 SQL 意图触发与恢复桥，当前没有充分收益 |
| subvolumePath 对 PVC 不变所以名称缓存永久安全 | 同名 PVC 可重建，名称不等于 UID；外部重建可绕过本地失效 |
| wait::delete 是 4.2 新增 | 未核实新增版本，不作为采用理由 |

## 候选分析

### A. Pod Ready 等待：推荐第一步

入口：`crates/docker_manager/src/runtime/k8s_pod.rs:178`，调用点包括 `k8s_agent_create.rs:130`。

当前逻辑包含 Ready、Succeeded 成功，Failed/CrashLoopBackOff/ImagePullBackOff 提前失败，以及不存在时等待。这是业务契约，不是通用 is_pod_running 条件。迁移时抽取纯分类器，保留原判断顺序和错误语义。

4.2.0 `await_condition` 使用 `watch_object`；后者使用 namespaced API 与 metadata.name 字段过滤，默认 ListWatch，不需要启用新服务端 StreamingList。Condition 只有 bool，可以匹配任意可判定终态，再对返回对象分类；但 await_condition 使用 try_next，首个流错误直接返回。

如果要求在原预算内恢复 410/暂态断连，应使用 watch_object 或 watcher 流并显式处理 Err 后继续 poll。底层 watcher 在后续 poll 恢复状态；不能 `.try_for_each(...)?` 后声称已自动恢复。禁止重新实现 resourceVersion 游标协议。

预期收益是减少持续等待期间 GET 与轮询发现延迟。对于已 Ready 的短请求，初始化 LIST/watch 可能没有性能收益。初版不引入额外 GET 快速路径，测量后再决定，避免未经证明地增加请求。所有结论以真实 A/B 数据验证。

### B. Builder 控制观察：价值较大，风险也更高

入口：`k8s_builder_control.rs:223-343`。普通循环顺序读取 STS 和 Pod，休眠 200ms；忽略 RTT 时理论上约 10 GET/秒/等待操作，不是实测 QPS。

STS UID、owner、生命周期、replicas 与 Pod owner/旧新 UID 都参与判定。不能简化成只看 Pod Ready，也不能因最后一次 GET 就声称获得跨资源原子快照。

方案是事件触发观察与现有保护并存：观察 STS 和 Pod，变化触发语义检查，结束前直接复核目标。先采用按操作持有的流，不引入全局广播。停止、重启、唤醒分别保留当前不同判定，不强行共享一套 Ready 条件。

### C. Events：推荐，需配套 RBAC

Recorder 使用 events.k8s.io/v1，至少 create/patch。三个配置源需一起检查：`k8s/config/rbac.yaml`、`tools/remote_k8s/manifests.py`、构建仓 `k8s/helm/nuwax-platform/templates/rcoder/clusterrole.yaml`。不能只改开发清单。

4.2.0 Recorder 内部缓存重复事件并维护 EventSeries，但重复 publish 仍可能 PATCH；聚合不是限流，也不提供完整审计。事件关联对象 namespace/UID；对象不存在时不能伪造 UID，优先保留 SQL/tracing 证据。服务端保留、配额与限流依配置而定，没有本轮验证的统一数量上限。

关键阶段事件便于 kubectl describe 定位问题。SQL 保存操作结果，tracing/OTLP 保存过程，metrics 聚合统计，Events 提供现场摘要。独立有界队列使发布失败不改变业务结果，丢弃和失败均可观测。不要发布每次轮询或把敏感 URL/凭据写入 note。

### D. 删除观察：独立后续批次

`k8s_app_deletion.rs:144-165` 当前以 404 成功、不同 UID 冲突阻止后续清理。内置 is_deleted/delete_and_finalize 不等价。若采用 watcher，只替换确认部分；保留捕获 UID/resourceVersion、Foreground DELETE、404 幂等和实际消失检查点。

PVC Terminating 强删、create 409 重试包含额外写语义，不纳入这批。已为 dead code 的 PVC Bound 等待也不为引库而改。

### E. reflector/Store：暂缓

候选仅限状态展示和诊断列表。get_container_info 等名字不能判断安全性，必须追溯消费者是否用于创建、删除、exec、唤醒或数据路径选择。capture_deletion、pvc_resize_identity、claim_app_storage、create→wait→return 保留直接身份查询和 CAS。

Store 是最终一致的每进程视图。初始未同步、watch 中断、writer 退出时要暴露不可信状态；Store 不存在不能证明资源不存在。正常 watch 比 30 秒 TTL 通常及时，但断连 Store 可能陈旧更久，不能默认安全性提升。cleanup_all 只能把 Store 当候选索引，不能据此授权删除。

subvolume 缓存优先绑定 PVC UID/PV 身份，并在权威路径校验。没有证据支持全量集群 PV watch 的成本。当前观察到的专属 namespace 规模也不能外推生产规模。

### F. Controller/scheduler/finalizer：不纳入

SQL 操作记录仍需持久化发现、条件认领和恢复。Controller 不能消除这些工作；SQL 变化也不会自动触发 K8s root watch。scheduler 可独立使用，但现有 DelayQueue 已提供延迟；新增去重必须包含生命周期和物理资源身份，避免合并不同代次的清理。

## 决策矩阵

| 候选 | 价值 | 成本/风险 | 可逆性 | 权限/测试 | 决策 |
|---|---|---|---|---|---|
| Pod 等待 | 中高 | 中/中 | 高 | 流式契约测试 | 首批 |
| Builder 双资源观察 | 高，待量化 | 中高/高 | 高，独立提交 | 身份与竞态回归 | 第二批 |
| Events | 运维价值中高 | 中/中 | 高 | 新 API group RBAC 与发布故障 | 第三批 |
| 删除观察 | 中 | 中/高 | 高 | 替换资源及清理边界 | 独立后续 |
| Pod Store | 未证明 | 高/高 | 中 | 缓存新鲜度与任务管理 | 暂缓 |
| PV Store/Controller | 当前低 | 高/高 | 低 | 范围大 | 不采用 |

## 反方论证与停止条件

依赖已经被启用，不代表采用组件没有成本。当前主要缺陷是身份和恢复授权，替换观察库不能修复。watch 会增加长连接、错误恢复和测试复杂度。若试点无法保留身份/错误语义，或者请求量、等待耗时、维护成本没有明确改善，应停止扩展，保留纯分类器与确定性测试，允许继续轮询。

## 官方及本地参考

- 按用户要求已克隆官方仓库至 `/Users/soddy/Documents/git-workspace/kube-rs`，检出与 Cargo.lock 一致的 `4.2.0` tag，commit `526f1f2cfa996ed0e92510feabb2d2327df5e49f`（detached HEAD，工作树干净）。没有以主分支行为推断当前版本。
- 已用 Python 字节比较确认克隆源码中的 `kube-runtime/src/wait.rs`、`watcher.rs`、`events.rs` 与此前读取的 Cargo registry 4.2.0 文件完全一致。再次核对首错返回、后续 poll 恢复、UID 替换判成功和 Events API group，结论不变。未编译上游仓库。
- 锁定版本本地源码：Cargo registry `kube-runtime-4.2.0/src/wait.rs:54`、`watcher.rs:688,779,879`、`events.rs:307`。本地源码用于核对精确版本，不能仅依赖 latest 页面。
- [kube-rs 架构](https://kube.rs/architecture/)
- [await_condition](https://docs.rs/kube-runtime/4.2.0/kube_runtime/wait/fn.await_condition.html)
- [Recorder 与 RBAC](https://docs.rs/kube-runtime/4.2.0/kube_runtime/events/struct.Recorder.html)
- [官方部署权限说明](https://kube.rs/controllers/manifests/)
- [WatchStreamExt](https://docs.rs/kube-runtime/4.2.0/kube_runtime/trait.WatchStreamExt.html)

注意：generation predicate 常用于忽略 status 更新，Ready 观察不能照搬，否则可能过滤真正需要的变化。
