# kube-runtime 接入实施方案

状态：建议实施，未开始编码。行为边界见 [spec.md](spec.md)，证据见 [research.md](research.md)。

## 1. 实施顺序与模块边界

先合并或冻结当前并行生命周期修复，重新确认相关函数与回归基线。每批独立提交；不顺手清理无关 runtime 逻辑。

| 批次 | 改动面 | 交付 |
|---|---|---|
| A | `docker_manager/src/runtime/k8s_pod.rs`、新私有观察模块及相关测试 | 通用 Pod 状态分类与有界 watch |
| B | `k8s_builder_control.rs`、私有观察模块 | STS/Pod 事件驱动观察，保留身份控制 |
| C | runtime 初始化、关键生命周期诊断点、三个 RBAC 来源 | 有界 Event publisher |
| D，可选 | `k8s_app_deletion.rs` | 条件删除不变，仅替换确认轮询 |

新增 `k8s_observation.rs`（建议名）仅放 Kubernetes 流消费、截止时间和错误分类；不要形成可执行任意写操作的通用协调器。Pod/STS 业务判定留在相应模块。跨 crate 如确需新增业务结果契约，放 shared_types；内部观察状态不外溢。

依赖可整理为 root kube features `["runtime", "ws"]`，不升级版本。rcoder 未使用的直接 kube/k8s-openapi 依赖可另一个提交清理；不得为了精简依赖破坏 feature 传递。默认、K8s 目标分别检查。

## 2. 观察器设计

纯判定模型建议为 `Pending | Complete(T) | Rejected(E)`，由同步函数处理完整观察对象。不持 DashMap/RwLock guard 跨网络等待。

每个观察接收既有 namespace/API、固定名称和调用方要求的身份凭据、绝对 deadline 及取消信号。返回身份明确的结果或结构化错误。不重新推导 app/service 定位键。

对于仅需一次可判定状态、首错即返回的场景可以用 await_condition；本方案需要断连恢复的主路径采用 watch_object 流，显式消费其 Result。不要先用 await_condition 再通过无限外层重启掩盖错误。

逻辑顺序：检查预算 → 建立流 → select(取消、deadline、next) → 分类错误或对象 → 判定 → 必要权威复核 → 返回。整个 future 受同一 deadline 控制。实际请求开始时间、重连和完成均记录原因，不记录完整 Pod/env。

### 错误与重连表

| 观察结果 | 策略 |
|---|---|
| 403/认证失败/资源类型不支持 | 及时失败，保留 API 分类，不退化成超时 |
| 名称不存在 | 对创建/Ready 等待继续；对删除确认按捕获身份契约处理 |
| 410 RV 过期 | 保留流继续消费，由 watcher 重 list；重新验证身份 |
| 429、明确暂态服务错误、传输断连 | 有界退避后继续消费；外层 deadline 不变；暴露最后原因 |
| 解码错误、缺必要 UID、身份不符 | 失败；不按暂态无限重试 |
| 正常服务端 watch EOF | 允许 watcher 按其协议续接；不当作业务成功 |
| 意外整体流结束 | 明确观察失败，不能成功或永久悬挂 |

退避初始建议 200ms、上限 2s、带抖动；这些是实施默认值建议，不是库默认值。同一错误只采用一层退避，避免叠加 default_backoff 和自定义 sleep。检查服务端 Retry-After 是否可从当前错误接口获取；无法获取时不声称已遵循它。保留最近错误供超时诊断，禁止字符串识别。

默认使用 ListWatch。API watch timeoutSeconds 是连接轮换，不是业务总预算。等待 Ready 不使用 generation predicate；首版不做自定义去重。

取消直接 drop 流，不为每个请求 detach 一个永久后台 watcher。HTTP 等待者只取消自己的持久化结果观察，不能将此 token 传入共享创建 worker 来改变既有语义。

## 3. Builder 双资源观察

每个已受理操作持有 STS 与 Pod 两个流，先取得足够初始状态再判定；任一流出现变化运行当前语义校验。可以持有最近对象作为观察提示，但不能用另一条流的旧快照授权写入或删除。

- STS UID/归属/生命周期必须匹配保存目标。
- wake 保留 replicas=1；stop 保留 replicas=0；restart 保留旧 Pod 与新 Pod 区别。
- Pod 必须匹配 STS ownerReference，不接受同名别族/替代 STS 下的 Pod。
- 进入完成候选后直接 GET 相关对象并运行相同判定。若只是正常未完成变化，继续观察；身份替换或 replicas 冲突立即失败。
- 最后 GET 不提供跨对象事务，既有租约、CAS 和状态提交顺序保持不变。
- 两个流都在同一个 future 的取消/deadline 边界内；一个失败不得遗留另一个任务。

第一版不共享全局 watcher。未来仅在连接量证据充分时设计按 namespace/type 的共享订阅，并单独评审初始化、慢消费者和引用计数。

## 4. Event publisher

每个 KubernetesRuntime 共享一个受管理 publisher；保存 JoinHandle/取消机制，不能在 clone 时重复创建。使用有界 mpsc，建议初值 256、单消费者；容量须由压力测试确认，不扩大到无界。

业务线程提交已脱敏结构化诊断。队列满/关闭立即增加丢弃指标并记录限速 warning，不阻塞生命周期。单次 publish 建议 3 秒上限，无无界重试；关停建议最多排空 5 秒，剩余数量记录。上述值是拟定配置，不是既有行为。

单消费者串行调用共享 Recorder，有利于避免同进程重复事件计数竞争。EventSeries 为辅助聚合，不能承诺多副本全局严格计数。操作 ID 可写入受限 note 以关联日志，但不能成为 metrics label。

仅接入关键阶段，成功事件在真实结果确认后发布，失败事件区分明确拒绝与 RecoveryRequired。hot 部署业务完成仍由 app-cli 操作身份判据确认，不由 Pod Ready 或 Event 替代。

RBAC 配套：

1. rcoder `k8s/config/rbac.yaml`。
2. rcoder `tools/remote_k8s/manifests.py` 及生成清单测试。
3. build-agent-docker `k8s/helm/nuwax-platform/templates/rcoder/clusterrole.yaml`。

新增 events.k8s.io/events create、patch；诊断工具若新增该 API group 的 list/watch，另行明确授权，不因 Recorder 扩大全资源权限。测试同时检查 role 与 serviceAccount binding。未批准前不发布 chart/镜像。

## 5. 删除观察隔离

保留现有 DELETE 参数、保存的 UID/RV、404 幂等与 Foreground。自定义观察：None 完成、同 UID 继续、不同 UID 冲突。冲突之后禁止下一目标、PVC 清理、元数据删除和成功检查点。不要调用默认 is_deleted 或 delete_and_finalize。

## 6. 测试与度量

采用真实 HTTP 流式协议服务器作基础设施故障注入，不模拟 AI。支持 LIST 初始快照/RV、按名称查询、持续 watch JSON、事件分片、BOOKMARK、410、403、EOF、受控取消。使用 barrier/通知制造窗口，所有测试带总超时，不依赖随机 sleep。

不必重写所有已有假 apiserver；受影响的固定 step 测试改为断言 method/path/query/body 和语义顺序，不能通过放宽 UID/RV 断言消除失败。

指标建议：观察数量/结果/时长、watch 重连/错误、事件队列丢弃/发送错误。标签仅含有限 resource_kind、operation_kind、outcome；app_id/UID/operation_id 放 trace，不放指标标签。请求数量可由测试 HTTP 服务器及限定客户端指标采集，不要求启用全群审计。

A/B 测试固定制品、并发度和机器配置，报告样本数与原始记录；构建耗时单列。长等待 API 请求数应下降，错误率不得恶化；性能阈值在采集基线后、测候选前固定，不能事后挑选有利指标。

## 7. 构建与真实验收

只修改文档阶段不运行 Cargo；实施阶段先聚焦，再合并门禁，禁止同 target 并发 Cargo：

```sh
cargo fmt --all -- --check
cargo check -p docker_manager --features kubernetes
cargo test -p docker_manager
cargo test -p docker_manager --features kubernetes
cargo clippy -p docker_manager -p rcoder --features kubernetes --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace
python3 -m unittest discover -s tools/remote_k8s/tests
```

实施前核对测试名称与工具前置；新增具体聚焦测试命令登记 tasks，不以当前文档中的建议命令冒充执行记录。涉及 feature 清理时追加 rcoder 默认/K8s 编译。

真实测试复用 `.env.local`，先 doctor，再按 remote-k8s 使用说明同步：

```sh
make remote-k8s-doctor
make remote-k8s-sync-start
make remote-k8s-sync-status
make remote-k8s-verify SUITE=userapp
make remote-k8s-test SUITE=chat
```

同一环境串行执行，冻结源码和镜像。chat 需要真实 LLM；缺配置是阻塞，不用模拟回答。通用 Agent Pod 等待必须覆盖 chat；仅 smoke 通过不算验收。

本地默认 feature 测试之外，若依赖/runtime 初始化影响 Compose，按现有入口构建并跑 test-e2e-compose。跨仓 chart 执行实际 values 的 helm template 和 RBAC 断言。平台 tag、npm、推镜像到发布仓不属于本方案；remote-k8s 专属验证镜像遵循现有工作流。

失败保留操作记录与测试证据，仅清理登记资源，Agent PVC 不删除。回退以独立提交和上一已知镜像进行，不新增不受测试的双实现开关，不把回退当作清锁授权。
