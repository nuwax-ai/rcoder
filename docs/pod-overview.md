# `/computer/pod/overview` 设计方案（命名空间级 Pod 概览）

> 状态：方案评审中，未开发。
> 已确认的范围：**只覆盖 rcoder 所在 namespace**；接口名 **`/computer/pod/overview`**；
> 历史 OOM **只给「最近一次 + 近 1h 事件」**，不做长期落库。
> 硬约束：**不得改变任何既有接口的行为**。

---

## 0. 结论速览

1. **可做**，且不需要引入任何新依赖：k8s 侧的 `kube 4.2` / `k8s-openapi 0.28`、docker 侧的
   `bollard 0.21.1` 都已在依赖里；Quantity 解析、事件过滤、单容器 OOM/重启诊断也都有现成实现。
2. **唯一没有成熟 typed 库的是 `metrics.k8s.io`** —— 官方支持的做法是用 `kube` 的
   `Api<DynamicObject>` + `ApiResource::from_gvk`（不是我们手写 HTTP，见 §4）。
3. **不改既有逻辑的做法**：新增独立 trait 方法（默认 `Unsupported`），把现有两处诊断代码
   **抽成纯函数**、原调用方改为委托（行为零变化），绝不改动 `/computer/pod/resources`、
   `/pod/list`、`diagnose_agent_pod` 的对外行为。
4. **三个已知缺口**（必须在文档与 UI 上如实标注，不能假装有数据）：
   - k8s 上**非 agent 容器没有进程数**（metrics API 无 pids）；
   - **历史 OOM 拿不到**，只有最近一次终止原因 + 近 1h 事件；
   - docker 侧**没有磁盘配额**，且 `RestartCount` **不含手工 restart**（实测，见 §3.4）。

---

## 1. 目标与非目标

### 目标

给运维/同事一个"一眼看全"的入口，回答四个问题：

1. 这个命名空间里有哪些 Pod，属于谁（user / project / app）、跑在哪个节点；
2. 每个 Pod **申请了多少**（CPU / 内存 / 磁盘）、**实际用了多少**（能取到就取）；
3. 有没有异常：**是否被 OOM 杀过、重启了几次、现在是不是 CrashLoop/未就绪**；
4. 最近 1 小时内**发生过什么**（OOMKilling / BackOff / FailedScheduling / Unhealthy…）。

### 非目标

- ❌ 不做长期指标存储/趋势图（要历史请落库，独立立项）；
- ❌ 不做日志聚合（`/pod/logs` 已有）；
- ❌ 不提供写操作（重启/停止仍走既有接口）；
- ❌ 不跨 namespace（当前 SA 也只有本 ns 权限）；
- ❌ 不替代 `/computer/pod/resources`（单容器实时明细仍是那个接口）。

---

## 2. 范围

| 项 | 决定 |
|---|---|
| namespace | 固定 rcoder 所在 namespace（`RCODER_K8S_NAMESPACE` / 启动配置），**不提供跨 ns 参数** |
| 对象 | 该 namespace 下**全部 Pod**（不只 rcoder 管理的），含我们自己的组件（backend/mysql/redis…） |
| docker compose | 语义等价为"本 docker daemon 上的全部容器"，可用 `label` 过滤到 rcoder 网络 |
| 数据时效 | 一次快照 + 服务端缓存 10s（见 §6.5） |

> 说明：k8s 下"全部 Pod"包含 456 个 PVC 关联的 app 容器，19 上实测 84 Pod；docker 侧
> 容器数通常 < 30。规模都不构成压力（见 §8）。

---

## 3. 数据来源矩阵（实测，不是推测）

### 3.1 逐字段来源

| 字段 | k8s | docker compose | 说明 |
|---|---|---|---|
| Pod/容器名、节点、IP、Phase、Ready | Pod 对象 | `list_containers` + `inspect` | 已有 |
| 业务身份（service_type/user/project/app/pod_id） | Pod labels（`RuntimeContainerInfo` 已解析） | 容器 label/内存缓存 | 已有，**不从名字反解** |
| CPU request/limit | `spec.containers[].resources` | `HostConfig.NanoCpus` / `CpuQuota`+`CpuPeriod` | Quantity 解析已有（§4.4） |
| 内存 request/limit | 同上 | `HostConfig.Memory`（0 = 不限制 → `null`） | 注意 0 不等于 0 字节 |
| 磁盘（配额） | PVC `status.capacity` + `ephemeral-storage` request/limit + `emptyDir.sizeLimit` | **无配额概念**，只有 `Mounts`（Source→Destination） | docker 可选项 `SizeRw`（`size=true`）代价高，默认关 |
| 实际 CPU/内存 | ① agent 容器：agent_runner gRPC（实时）② 其他 Pod：`metrics.k8s.io` | `docker stats`（`one_shot`） | 覆盖**全部** Pod，不只我们的 |
| 进程数 | 仅 agent 容器（gRPC） | `stats.pids_stats.current` | k8s metrics API 无 pids（§3.3） |
| 重启次数 | `containerStatuses[].restartCount` | `State.RestartCount`（**只含策略重启**，§3.4） | |
| 最近一次 OOM | `lastState.terminated.reason == "OOMKilled"` | `State.OOMKilled` / exit 137 | 已有实现可复用 |
| CrashLoop/未就绪原因 | `state.waiting.reason` | 无对应概念（可用 `State.Status`） | |
| 事件 | Events API（一次 list + fieldSelector） | `docker events --since 1h`（需 timebox，§3.5） | |

### 3.2 集群实测（2026-11-13）

| 集群 | Pods | Events | PVC | 有 metrics 的 Pod | rcoder SA 权限 |
|---|---|---|---|---|---|
| 19（nuwax-k8s-test） | 84 | ~370 | 456 | 57 | `pods` / `pods.metrics.k8s.io` / `events` / `persistentvolumeclaims` **全部 `can-i list = yes`** |
| 45（k3s 单节点） | 46 | 62 | 26 | 28 | 同上（metrics-server 已装，`kubectl top` 可用） |

> 结论：**不需要改 RBAC**。metrics-server 在 19/45 都存在，但生产/离线环境可能没有 →
> 必须降级（§5.3）。

**事件可用性与真实分布**（19，`fieldSelector` 实测）：

| 选择器 | 结果 |
|---|---|
| 无过滤 | 374 |
| `involvedObject.kind=Pod` | 312（服务端已收窄，说明该选择器可用） |
| `involvedObject.kind=Pod,type=Warning` | **52**：`Unhealthy` 41 / `FailedMount` 4 / `BackOff` 3 / `Failed` 3 / `FailedScheduling` 1 |
| `reason=OOMKilling` | **0**（当前没有 OOM 事件；`BackOff`/`Unhealthy` 才是当前的主要异常信号） |

> ⚠️ 验证坑：`kubectl get events … | wc -l` 会把 `No resources found` 也算一行，
> 从而误判"有 1 条事件"。核对数量必须解析 `-o json` 的 `items` 长度（本方案文档里的
> 数字都是这样核出来的）。

### 3.3 为什么 k8s 非 agent 容器没有进程数

- `metrics.k8s.io` 只给 `containers[].usage.{cpu,memory}`，无 pids；
- kubelet summary API（`/api/v1/nodes/<node>/proxy/stats/summary`）同样没有进程数；
- cAdvisor 的 `processes` 字段并非所有版本/端点暴露，且需要额外权限与 node 代理；
- exec 进容器 `ps` 代价与权限都不可接受。

→ **接受不对称**：`process_count` 只有 agent 容器（gRPC）与 docker 容器（stats）有值，其余 `null`。

### 3.4 docker 的 `RestartCount` 语义（本地实测）

在 23 上用一次性容器验证：

```
初始 RestartCount=0
docker restart 之后 RestartCount=0   ← 手工重启不计数
HostConfig.Memory=67108864
```

→ docker 的 `RestartCount` 只统计**重启策略**（`--restart`）触发的重启。我们自己的
`/pod/restart`、人工 `docker restart`、compose 重建都不会体现在这个字段里。
因此 compose 下的"重启过几次"必须**以 `docker events` 的 `restart`/`die` 事件为准**，
`RestartCount` 仅作参考值，并在响应里注明来源（`health.restart_source`）。

### 3.5 docker events 的一个坑

`docker events` 是**长连接流**：`since=1h` 会先回放历史，然后继续推送新事件。
一次性快照必须用 **`take_until(timeout)` 或 `until` 参数**收口（建议 300ms 或 `until=now`），
否则请求会永远挂着。bollard 的签名：

```rust
docker.events(Some(EventsOptions { since: Some("1h".into()), until: Some(...), .. }))
      -> impl Stream<Item = Result<EventMessage, Error>>
```

---

## 4. 库选型（重点：能复用就不手写）

### 4.1 已在依赖里、直接复用

| 能力 | 库 / 模块 | 版本 | 用途 |
|---|---|---|---|
| k8s 客户端 | `kube` | 4.2.0 | 列 Pod / Event / PVC、动态 API |
| k8s 类型 | `k8s-openapi` | 0.28.0 | `Pod`/`Event`/`PersistentVolumeClaim`/`ContainerStatus` |
| docker 客户端 | `bollard` | 0.21.1 | `list_containers` / `inspect_container` / `stats` / `events` |
| Quantity 解析 | `shared_types::runtime_config::quantity` | 仓库内 | `parse_memory_quantity` / `parse_cpu_quantity`（已带测试） |
| 单容器 OOM/重启诊断 | `container-runtime-api::AgentPodDiagnostic` | 仓库内 | k8s 与 docker **两侧都已实现** |
| 事件过滤模式 | `docker_manager::k8s_app_observation::app_events` | 仓库内 | 一次 list + involvedObject 归属过滤（`resource_version=0` 走 watch cache） |

### 4.2 `metrics.k8s.io`：没有成熟 typed crate，用官方支持路径

调研结论：**没有维护良好的 crate 提供 `PodMetrics` 类型**。生态里三个真实做法：

| 做法 | 评价 |
|---|---|
| 手写 HTTP 打 `/apis/metrics.k8s.io/v1beta1/...` | ❌ 要自己处理鉴权、重试、超时、URL 拼接 —— 这正是要避免的 |
| **`Api<DynamicObject>` + `ApiResource::from_gvk`** | ✅ **推荐**：走 kube 的完整客户端栈（TLS/鉴权/超时/错误类型），只把返回值当动态对象解析我们要的两个字段 |
| 自定义 `impl Resource for PodMetrics` | ⚠️ 可行（`kubectl-view-allocations` 就这么干），但要自己维护 GVK/URL 段，属于"变相手写" |

推荐写法（`kube` 官方文档示例同款）：

```rust
let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
let ar = ApiResource::from_gvk(&gvk);
let api: Api<DynamicObject> = Api::namespaced_with(client, &namespace, &ar);
let list = api.list(&ListParams::default()).await?;   // 一次拿到 ns 内全部 Pod 用量
```

佐证（公开实现，均非本项目）：

- kube-rs 官方 examples 演示了用 `Api` + GVK 方式访问任意资源：
  <https://github.com/kube-rs/kube/blob/main/examples/README.md>
- `kubectl-view-allocations`（Rust）对 metrics API 的处理：
  <https://gitcode.com/gh_mirrors/ku/kubectl-view-allocations/blob/master/src/metrics.rs>
- Kubeli（Tauri+Rust）用 `Api<DynamicObject>` + `ApiResource` 取 PodMetrics：
  <https://deepwiki.com/atilladeniz/Kubeli/3.7-metrics-collection>

`PodMetrics` 里我们只用两个字段 + 两个元信息：

```json
{ "metadata": {"name": "pod-x"}, "timestamp": "...", "window": "15s",
  "containers": [ {"name": "agent", "usage": {"cpu": "12m", "memory": "1186Mi"}} ] }
```

> `window` 必须透出到响应里（`usage.window`），因为它是采样窗口而非瞬时值 ——
> 与 agent gRPC 的实时值语义不同，不能让前端混着看。

### 4.3 docker：bollard 三个 API 足够

| 需求 | bollard |
|---|---|
| 列表 | `list_containers(Some(ListContainersOptions{ all: true, .. }))` |
| 规格 + OOM + 重启 | `inspect_container(name, None)` → `HostConfig` / `State` / `RestartCount` |
| 实际用量 + 进程数 | `stats(name, Some(StatsOptions{ stream: false, one_shot: true, .. }))` → 取第一条 |
| 事件 | `events(Some(EventsOptions{ since: "1h", .. }))` + timebox（§3.5） |

`stats(one_shot=true)` 是**单次采样**（不走两帧差值），CPU 百分比按 docker 官方公式由
`cpu_stats`/`precpu_stats` 计算；`pids_stats.current` 直接给进程数。

### 4.4 Quantity 解析：已有实现，不要重写

```rust
shared_types::runtime_config::quantity::parse_memory_quantity("6144Mi") -> Option<u64>
shared_types::runtime_config::quantity::parse_cpu_quantity("500m")      -> Option<f64>
```

已覆盖 `Ki/Mi/Gi`、小数、`m` 等写法，并有单测（`k8s_builder_control/configuration.rs` 里
还做过 `6144Mi == 6Gi`、`0.5 == 500m` 的等价性断言）。

### 4.5 结论

**不新增任何依赖。** 需要新写的只有"编排/归一化"这一层（约 600~900 行 + 测试）。

---

## 5. 接口契约

### 5.1 请求

```
GET /computer/pod/overview
```

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `include` | string(csv) | `spec,health,events,usage` | 分段开关，可只要 `health` 做轻量巡检 |
| `events_limit` | u32 | 10 | 每 Pod 返回最近 N 条事件（上限 50） |
| `service_type` | string | — | 按 `RuntimeContainerInfo.service_type` 过滤 |
| `user_id` / `app_id` / `project_id` | string | — | 按结构化身份过滤 |
| `label_selector` | string | — | 透传（k8s）；docker 下映射为 `label=` 过滤 |
| `only_problem` | bool | false | **过滤器**：只返回"有异常"的对象，异常 = 下列任一成立：① `ready=false`；② `restart_count > 0`；③ `oom_killed_last=true`；④ 最近 1h 内有 `type=Warning` 事件。默认关闭（返回全部），仅用于面板"只看问题"开关 |
| `include_sidecars` | bool | false | 是否在 `usage.containers[]` 里带上内部 sidecar（默认隐藏 `log-shipper` / `log-collector`） |
| `limit` | u32 | 500 | 安全上限，防一次吐爆 |

> 不做 `namespace` 参数（§2）。

### 5.2 响应

```jsonc
{
  "success": true,
  "code": "0000",
  "data": {
    "source": "kubernetes",            // kubernetes | docker
    "namespace": "nuwax-k8s-test",     // docker 下为 null
    "generated_at": "2026-11-13T17:20:00Z",
    "cached": true,
    "cache_age_seconds": 3,
    "summary": {
      "pods": 84, "running": 71, "not_ready": 13,
      "restarted": 9, "oom_killed_last": 3, "with_warning_events": 2
    },
    "degraded": [                       // 分区失败说明；为空表示全部成功
      // {"section":"usage","reason":"metrics_unavailable","detail":"metrics.k8s.io: 404"}
    ],
    "pods": [{
      "name": "rcoder-computer-agent-runner-6-0",
      "workload": { "kind": "StatefulSet", "name": "rcoder-computer-agent-runner-6" },
      "identity": {
        "service_type": "computer-agent-runner",
        "user_id": "6", "project_id": "6", "app_id": null, "pod_id": null
      },
      "phase": "Running",
      "ready": true,
      "node": "soddy",
      "ip": "10.42.1.206",
      "created_at": "2026-11-13T15:40:00Z",
      "age_seconds": 5400,
      "spec": {
        "cpu_request_cores": 0.5,
        "cpu_limit_cores": 2.0,
        "mem_request_mb": 1024,
        "mem_limit_mb": 6144,
        "ephemeral_storage_limit_mb": null,
        "volumes": [
          { "type": "pvc", "name": "rcoder-computer-workspace",
            "mount_path": "/home/user", "capacity_mb": 20480, "used_mb": null }
        ]
      },
      "health": {
        "restart_count": 0,
        "restart_source": "container_status",   // container_status | docker_events
        "last_terminate_reason": null,
        "last_exit_code": null,
        "oom_killed_last": false,
        "waiting_reason": null,
        "detail": null
      },
      "usage": {
        "source": "agent_grpc",     // agent_grpc | metrics_server | docker_stats
        "sampled_at": "2026-11-13T17:19:58Z",
        "window": null,             // metrics_server 时为 "15s"，gRPC 为 null（实时）
        "cpu_percent": 1.7,         // Pod 合计（含被隐藏的 sidecar）
        "cpu_cores": 0.03,
        "mem_used_mb": 1186,
        "mem_limit_mb": 6144,
        "process_count": 84,        // 拿不到就是 null
        "containers": [             // 按容器拆分；默认隐藏 sidecar（include_sidecars=false）
          { "name": "agent", "cpu_cores": 0.03, "mem_used_mb": 1186, "hidden": false },
          { "name": "log-collector", "cpu_cores": 0.0, "mem_used_mb": 7, "hidden": true }
        ],
        "hidden_containers": 1      // 被隐藏的 sidecar 数量，UI 可提示"另有 1 个内部容器"
      },
      "events": [
        { "type": "Warning", "reason": "OOMKilling", "count": 2,
          "message": "Container agent was OOMKilled", "object": "rcoder-…-6-0",
          "last_seen": "2026-11-13T09:12:03Z" }
      ]
    }]
  },
  "tid": "…"
}
```

### 5.3 降级与错误

| 情况 | 行为 |
|---|---|
| Pod 列表失败 | **整体 503**（没有列表这个接口没有意义），带 `error_detail{stage:"pod.overview.list"}` |
| metrics 不可用 | `usage` 整块 `null`（**不返回 0**），`degraded[]` 追加 `{section:"usage",reason:"metrics_unavailable"}` |
| agent gRPC 超时/失败 | 该 Pod `usage.source="metrics_server"`（有 metrics 时）或 `usage=null`；不影响其他 Pod |
| events 列取失败 | `events: []` + `degraded[]` 标注（**空数组与"取不到"必须能区分**：靠 `degraded`） |
| PVC 列取失败 | `volume.capacity_mb=null`，不报错 |
| docker events 超时 | 用已收到的部分；`health.restart_source="docker_events"` 且 `degraded[]` 标注 `partial` |

### 5.4 语义红线（与既有接口保持同一套原则）

1. **`null` ≠ `0`**：取不到就 `null`，绝不填 0（0 会被读成"空载/没重启"）。
2. **`usage.source` / `usage.window` 必带**：k8s 上 agent 实时值与 metrics 采样值不可混比。
3. **`health.oom_killed_last`** 是"最近一次终止是否 OOM"，**不是历史次数**；历史只能从
   `events`（近 1h）数，字段名与文档都不许暗示它是审计数据。
4. **`process_count`** 只在有真实来源时出现（§3.3）。
5. 事件 `count` 是 k8s 聚合计数，`last_seen` 取 `lastTimestamp || eventTime`。

### 5.5 状态码

| 码 | 场景 |
|---|---|
| 200 | 正常/部分降级（降级信息在 `degraded[]`，不改变状态码） |
| 401 | 既有 API key 中间件 |
| 503 | Pod 列表本身失败 |

---

## 6. 实现设计

### 6.1 分层与"不影响既有接口"的具体手段

```
http-server  handler/pod_handler/overview.rs      ← 新增，只依赖下面两个接口
   │
   ├── runtime.observe_pods(filter)  → Vec<PodObservation>     ← 新增 trait 方法
   │      k8s 实现 / docker 实现（各自独立模块）
   │
   └── agent 容器 usage 合并（复用既有 gRPC 调用，失败即跳过）
```

**四条"不做"的保证**：

1. **不改** `ContainerRuntime` 既有方法的签名与语义；新方法给**默认实现**
   `Err(ConfigurationError("pod overview is unsupported"))`，老实现（含 mock/fake）行为不变。
2. 现有 `pod_diagnostic(pod)`（k8s，写死 `"agent"` 容器）与 docker 内联诊断代码
   **只做"抽成纯函数 + 原调用方委托"** 的重构，输出结构完全一致（现有单测即为回归网）。
3. `/computer/pod/resources`、`/pod/list`、`/pod/count`、`diagnose_agent_pod*` 一行逻辑都不动。
4. 新 handler 不注册到任何既有路由前缀之外，Swagger 单独分 tag `overview`。

### 6.2 k8s 采集流程（一次请求）

```
1) list Pods (namespace, label_selector?)            ← 1 次 API
2) list PodMetrics (metrics.k8s.io)                  ← 1 次 API（失败即降级）
3) list PVC (namespace) → 建 name→capacity 索引       ← 1 次 API（失败即降级）
4) list Events (fieldSelector: involvedObject.kind=Pod) ← 1 次 API（失败即降级）
5) 对 agent 容器（identity.service_type ∈ computer/web/builder）并发取 gRPC 实时值
   并发上限 8、单次超时 2s、失败静默跳过
6) 合并 → 排序 → 过滤 → 截断 limit
```

> 4 次 list + N 次 gRPC（19 上 N≈20）。events 用 fieldSelector 在服务端先收窄
> （`involvedObject.kind=Pod` 是 k8s 支持的选择器），避免把 369 条全拉下来做客户端过滤。

**spec 提取**：Pod `spec.containers[]` 可能多容器（我们有 `rcoder`+`log-shipper`、
`agent`+`log-collector`）。`spec` 顶层的 cpu/mem 取**主容器**（按 `agent`/`app`/`rcoder`
顺序匹配，匹配不到取第一个），同时保留 `spec.containers[]` 逐容器的 request/limit。

**sidecar 策略（已定）**：`usage.containers[]` 默认**隐藏** `log-shipper` / `log-collector`
这类内部容器（`include_sidecars=false`），只暴露用户关心的主容器；被隐藏的数量用
`hidden_containers` 透出，UI 可显示"另有 N 个内部容器"。隐藏名单放在
`shared_types` 常量里（可配置），不散落在 handler 里。

**磁盘**：
- `volumes[].persistentVolumeClaim.claimName` → PVC `status.capacity.storage`（配额）；
- `resources.limits["ephemeral-storage"]` → 临时盘配额；
- `emptyDir.sizeLimit` → 有则填；
- **不提供 PVC 已用量**（需要额外采集链路，明确列为非目标）。

### 6.3 docker 采集流程

```
1) list containers (all=true, label filter?)          ← 1 次
2) 并发 inspect（限 8）→ spec / state / restart_count
3) 并发 stats(one_shot)（限 8，超时 2s）→ usage / pids
4) events(since=1h) + timebox(300ms) → 按容器归组
5) 合并 → 排序 → 过滤
```

**关键细节**：
- `HostConfig.Memory == 0` → `mem_limit_mb = null`（不限制，不是 0MB）；
- `NanoCpus == 0` 时用 `CpuQuota/CpuPeriod` 反推，两者都没有 → `null`；
- `state.OOMKilled` 只在"当前状态"为 OOM 时 true；**历史 OOM 从 events 的 `oom` 事件数**；
- `RestartCount` 仅参考（§3.4），重启次数以 events 里 `restart`+`die` 数为准，
  并在 `health.restart_source` 标注（**已定：不额外改造 RestartCount 语义，也不为它加探针**）；
- events 流必须 timebox，否则请求挂死（§3.5）。

### 6.4 归一化与排序

- 统一单位：CPU 用"核"（`f64`）+ 百分比；内存/磁盘统一 MB（`u64`，1MB=1048576B，与既有面板一致）。
- 排序（稳定）：`phase` 严重度（Failed > Pending > Unknown > Running/Succeeded）
  → `oom_killed_last` → `restart_count` → `mem_used_mb` 降序 → `name` 升序。
- 过滤在排序之后、截断之前。

### 6.5 缓存 / 单飞 / 并发

| 项 | 决定 | 理由 |
|---|---|---|
| 缓存 TTL | **10s** | metrics-server 本身 15~60s 粒度；面板 2s 轮询会打爆 API 与 gRPC |
| 缓存键 | `(include, filters, events_limit, runtime)` | 过滤条件不同不能复用 |
| 单飞 | 同键并发请求只打一次 | 避免面板多标签页放大 |
| 缓存容量 | 最多 16 个键，超出清空 | 与 `/resources` 的 `_stats_cache` 同款策略 |
| 超时 | 整体 8s；子项各自 2s | 慢来源不拖垮整个响应 |

---

## 7. 复用 vs 新写

| 能力 | 现状 | 本次动作 |
|---|---|---|
| Pod 列表（含身份） | ✅ `list_containers_inner`（仅 `RUNTIME_MANAGED_LABEL`） | **新写** `list_pods_unfiltered`（不改原方法） |
| Pod 规格 | ❌ 只写不读 | 新写（Quantity 解析复用） |
| PVC 容量 | ❌ | 新写（1 次 list + 索引） |
| k8s 事件 | ✅ `app_events` 模式 | 新写通用版（同一套写法） |
| OOM/重启诊断 | ✅ `pod_diagnostic(pod)` / docker 内联 | **抽纯函数 + 委托**（零行为变化） |
| metrics | ❌ | 新写（`Api<DynamicObject>`） |
| docker stats | ❌ | 新写（bollard） |
| agent 实时 usage | ✅ gRPC `GetContainerStatus` | 复用（并发 + 超时） |
| 响应类型 | ❌ | 新写（`shared_types` 新模块） |

---

## 8. 性能与容量（基于 19/45 实测规模）

| 场景 | 调用量 | 估算 |
|---|---|---|
| k8s 19（84 Pod/57 metrics/456 PVC/369 events） | 4 次 list + ~20 次 gRPC | 冷启动 ~300–500ms，命中缓存 <5ms |
| k8s 45（46 Pod/28 metrics/26 PVC/62 events） | 4 次 list + ~10 次 gRPC | ~200–300ms |
| docker（<30 容器） | 1 list + 30 inspect（并发8）+ 30 stats（并发8） | ~400–800ms（stats 偏慢） |

响应体：84 Pod × ~1KB ≈ **<100KB**（`events_limit=10`）。`limit=500` 兜底。

> 若 stats/inspect 在 docker 侧成为瓶颈，P3 可改"只对 `only_problem=true` 或前 N 个取 stats"，
> 但 P1 先按全量做，用缓存兜住。

---

## 9. 安全与权限

- 接口挂既有 API key 中间件（与 `/pod/list` 同级），不新增匿名入口；
- k8s 只读本 namespace，SA 权限已具备（§3.2 实测），**不需要扩权**；
- 响应会暴露 Pod 名/IP/镜像标签 —— 与现有 `/pod/list` 的信息面一致，不新增敏感字段；
- 不做 `kubectl exec`/节点级接口调用。

---

## 10. 测试策略

| 层次 | 内容 |
|---|---|
| 纯函数单测 | Quantity→MB/核；Pod spec 提取（多容器、缺失 limit、0 值）；`HostConfig.Memory==0 → null`；排序/过滤；`again` 缓存键 |
| 诊断抽取回归 | 现有 `pod_diagnostic` 单测必须原样通过（证明"抽函数"没改行为） |
| 降级单测 | metrics 失败 → `usage=null`+`degraded`；events 失败 → `events=[]`+`degraded`；PVC 失败 → `capacity=null` |
| 真机验证（45） | 造 OOM：`--memory` 小容器 + 分配超限 → 校验 `oom_killed_last=true`、restart 计数、events 里 `OOMKilling` |
| 真机验证（19） | 只读跑一遍，与 `kubectl top`、`kubectl get pod -o json` 对照抽查 3 个 Pod |
| 回归 | 既有 `/pod/resources`、`/pod/list`、`/pod/status` 响应快照不变（可用现有测试 + 手工 diff） |

---

## 11. 分阶段计划

| 期 | 交付 | 验收标准 | 估时 |
|---|---|---|---|
| **P1** | k8s/docker 的 `spec` + `health` + `events`；无 `usage` | 45 上能看出"哪个容器被 OOM 杀过/重启几次"；对 `kubectl` 输出抽查一致 | 1.5 天 |
| **P2** | `usage` 双源（metrics + gRPC / docker stats） | 45 上 agent 容器 `source=agent_grpc`、其他 Pod `source=metrics_server`、docker `source=docker_stats`；metrics 被禁时整块 null 且 `degraded` 有标注 | 1 天 |
| **P3** | 面板「集群概览」页 + `only_problem` 快捷过滤；可选 SSE | 面板能按 OOM/重启排序、点行跳 `/resources` | 1 天 |

P1 的收益最直接：**立刻能定位"哪个容器在反复被 OOM 杀"**（19 上 backend 那两个
`Init:CrashLoopBackOff` 就是这类问题）。

---

## 12. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 生产没有 metrics-server | 没有 usage | `usage=null` + `degraded`，不是错误；文档明示 |
| k8s 非 agent 无进程数 | 字段缺失 | 明确 `null` 语义，面板显示 `—` 并带 tooltip |
| `RestartCount` 不含手工重启（实测） | 数字偏小 | docker 用 events 计数 + `restart_source` 标注 |
| events 只留 1h | 历史不可追溯 | 接口与文档都写清"近 1h"，长期需求另立落库项 |
| docker events 长连接 | 请求挂死 | timebox（§3.5）+ 单测覆盖超时 |
| 面板 2s 轮询打爆 API | API 压力 | 10s 缓存 + 单飞 + 前端节流 |
| 一次性吐 500 Pod | 响应过大 | `limit` + `include` 分段 |
| 与 `/resources` 口径不一致 | 数字对不上 | 两者都注明来源与采样窗口；`/overview` 不替代 `/resources` |
| events 里长期没有 OOM 事件（实测 19 为 0 条） | 误以为"没 OOM 就没事" | 概览同时用 `health`（重启/BackOff）与 Warning 事件（当前主要是 `Unhealthy`/`BackOff`）做交叉验证 |

---

## 13. 决策记录（已确认）

| # | 问题 | 决定 |
|---|---|---|
| 1 | docker 策略重启是否要计入 `RestartCount` | **不用计入**。`RestartCount` 原样透出（仅策略重启），重启次数以 events 为准，不额外加探针 |
| 2 | 多容器 Pod 的 usage | **拆开**：`usage.containers[]` 逐容器；默认隐藏 `log-shipper` / `log-collector` 等内部 sidecar，用 `hidden_containers` 提示数量 |
| 3 | `only_problem` 的"问题"定义 | 四个条件**任一**即算问题（未就绪 / 有重启 / 最近一次 OOM / 近 1h 有 Warning 事件），默认关闭 |
| 4 | 面板入口位置 | 待定（见下） |

## 14. 仍待确认

1. **面板入口位置**：单容器视图加"健康卡片"（轻）vs 顶部加 Tab 的命名空间概览表（重）。
2. **概览表粒度**：Pod 一行 + 展开看容器，还是容器一行（84 Pod → 约 190 容器行）？
3. 详情抽屉里事件条数默认多少（10 还是 20）。

---

## 15. 本地调试面板（`:8765`）接入分析

> 对象：`build-agent-docker/k8s/scripts/oom-dashboard`（`index.html` 684 行 + `server.py` 480 行，
> 无框架、原生 DOM、`poll()` 每 2s 一轮）。本节只做分析，不含实现。

### 15.1 面板现状（决定了怎么接）

| 事实 | 影响 |
|---|---|
| 只有一个"关注容器"维度（`OOM_WATCH_USER_ID` 合成条目） | 概览是**新增维度**，不是替换 |
| 已有目标切换（`base`/`user`/`project` + localStorage） | overview 必须复用同一 `TARGET`，否则会出现"上面看 19、下面看 45" |
| 服务端只是薄代理（`/api/rcoder-pod`、`/api/rcoder/*`）+ 2s 小缓存 | 需要新增一个带 10s 缓存的 `/api/overview` 代理 |
| 单文件 HTML（便于运维整包拷走） | 建议**继续单文件**，用注释分区；拆 `overview.js` 会破坏"单文件可拷贝"的优点 |
| `poll()` 是 2s 主循环 | 概览不能塞进这个循环（84 Pod 太重），要独立 `pollOverview()`（10s） |

### 15.2 三个接入档位

| 档 | 内容 | 依赖 | 改动量 | 价值 |
|---|---|---|---|---|
| **A（推荐先做）** | 单容器视图上加一张「健康 / 规格」卡：重启次数、最近是否 OOM、等待原因、CPU/内存限额、最近 3 条事件 | 后端 P1 即可（health+spec+events） | 页面 +1 卡（~60 行）、server +1 代理（~40 行） | 直接回答"我这台容器为什么反复重启" |
| **B（其次）** | 顶部「命名空间概览」可折叠区：Pod 表（名称/身份/Phase/Ready/规格/用量/重启/OOM/事件数），支持 `only_problem` 过滤与排序，点行回填 `TARGET` 并滚到 A | 后端 P1（无 usage 列）/ P2（有 usage 列） | 页面 ~150 行、server 复用同一代理 | 一眼看到"谁在被 OOM 杀"，是 19 上 backend `Init:CrashLoopBackOff` 这类问题的定位入口 |
| **C（后置）** | 详情抽屉（events/containers/spec 全展开）、CSV 导出、自动刷新开关 | A+B | ~150 行 | 排障效率 |

### 15.3 关键设计点

1. **必须走服务端代理**，不要浏览器直连 rcoder：统一 `TARGET`、避免 CORS、可缓存/单飞、跨机器切换无需改前端逻辑。
   代理路由建议 `GET /api/overview?base=…&only_problem=…`，缓存 10s（面板 2s 轮询会打爆 84 Pod 的采集）。
2. **目标模型复用**：`TARGET.base` 决定查哪台；`TARGET.user` 只用于"高亮当前容器"（在概览表里标一行），
   不作为 overview 的必需参数。
3. **轮询节奏**：主循环 2s 不变；概览独立 10s；`document.hidden` 时暂停；概览失败**不得影响**主循环（各自 try/catch）。
4. **旧版本兼容**：rcoder 返回 404 / `ERR_OPERATION_NOT_SUPPORTED` 时，页面显示
   「当前 rcoder 版本不支持集群概览」并隐藏该区块，**不能白屏、不能把空表当"没有 Pod"**。
5. **数值口径**：每行带 `usage.source` 徽标（`agent_grpc` 实时 / `metrics_server` ~15s / `docker_stats`）；
   `null` 一律显示 `—`（不是 0）；sidecar 默认折叠，行尾提示"另有 N 个内部容器"。
6. **表格粒度**：建议 **Pod 一行 + 点击展开容器**（84 Pod → 约 190 容器行，直接铺容器行会太长）。
7. **渲染性能**：只在数据指纹变化时重建 DOM（84 行 × 10s，`innerHTML` 全量重建也可接受，但要避免 2s 级别的重建）。

### 15.4 布局草案（在现有页面上增量加，不推翻）

```
┌ OOM 观测面板  k8s · 192.168.1.19:30295 · user 6        [切换] [复位] ┐
├ [目标栏：rcoder 地址 / user_id / project_id]  ← 已存在                  ┤
├ ▼ 命名空间概览（84 pods · 3 个有问题）        ← B 档新增（可折叠）      │
│   ⚠ backend-7f6d…  CrashLoop  重启 62  近1h: BackOff×3                   │
│   ⚠ es-0           Running    重启 1   OOM(最近一次)                     │
│     redis-0        Running    重启 0                                     │
├ 关注容器卡片（内存/CPU/进程/距限额 + 曲线）   ← 已存在                    │
├ 🩺 健康与规格（重启/最近OOM/等待原因/限额/最近事件） ← A 档新增            │
├ Agent 会话（每 project 一行 + 停止）          ← 已存在                    │
└ 容器内进程 TOP + 角色分组                     ← 已存在                    │
```

### 15.5 代价与风险

| 项 | 说明 |
|---|---|
| 页面长度 | A+B 合计约 +210 行（684 → ~900）；仍是单文件，可接受 |
| 兼容矩阵 | 面板要同时能跑在 `0.1.427`（无概览）、`0.1.438`（有 overview 无 tenant 参数）、未来版本 → 全部走"能力探测 + 降级" |
| 数据放大 | 概览一次 <100KB / 10s；对 127.0.0.1 本地工具无压力 |
| 误导风险 | 最大风险是"概览说没问题、单容器视图却在涨"——两者来源不同（前者 metrics 15s / 后者 gRPC 实时），UI 必须显示来源与采样时间 |
| 安全 | 仍是 loopback 只读工具；概览只增信息面（Pod 名/IP），与 `kubectl get pods` 等价 |

### 15.6 建议

先做 **A**（与后端 P1 同批），它把"重启/OOM/限额"这三件最有用的事直接贴到现有视图上，
改动最小、回归面最小；等 P2 的 `usage` 上线后再加 **B** 的概览表。
