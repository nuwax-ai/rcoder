# UserApp 自动回收与流量唤醒 设计文档（Spec + Plan）

> 状态：Draft（待评审）
> 范围：UserApp（`ServiceType::UserApp`）的闲置 scale-to-0 回收 + 流量唤醒（wake-on-traffic）
> 不涉及：agent-runner 容器（已有回收链路）、UserAppBuilder、PVC 删除、健康端点

---

## 0. 决策摘要（已与产品确认）

| 决策点 | 选定方案 |
|---|---|
| 唤醒客户端体验 | **Hold-and-wait 带上限**：请求挂起等 Ready，上限 `wake_timeout`(默认 60s)，超时转 503+`Retry-After` |
| 闲置信号源 | **HTTP 访问为主**（pingora `request_filter` 记录 per-app 访问）。Gateway 模式流量绕过 rcoder，为已知限制 |
| 落地范围 | **一次性全做**：回收（扫描器+scale0）+ 流量唤醒，同批交付 |
| 配置粒度 | 全局默认 + per-app API 字段（持久化为 `rcoder.io/*` 注解）覆盖 |
| 默认策略 | 全局 `enabled` 默认 **true**；per-app 默认 **可回收**（absent / `recycle_enabled=true` = 免费用户）。付费 app 设 `recycle_enabled=false` 永不回收 |
| 持久化 | v1 in-memory（rcoder 单实例），重启重置时钟；注解持久化作 v2 |

---

## 1. Spec（做什么）

### 1.1 背景与动机

UserApp 全量业务接口已验证通过。长期运行的 UserApp 占用 K8s 计算资源（Pod/内存/IP），但很多 app（demo、低频内部工具、预览站）几天没人访问。需要：

1. 闲置超过阈值 → 自动 **scale replicas → 0**（回收算力，**不删任何资源**：Deployment/Service/NodePort/PVC/pingora 路由全保留）。
2. 有真实流量到达 → 自动 **scale → 1** 拉起，等 Ready 后代理。
3. 全局开关 + 可配阈值（默认 5 天），per-app 可覆盖。

**现状缺口**（勘探结论）：
- `cleanup_task`（[cleaner.rs:206](../../crates/rcoder/src/cleanup_task/cleaner.rs)）**故意跳过 UserApp**——UserApp 不进 `state.projects` 表，扫描器看不到。
- **无任何 per-app HTTP 访问记录**：pingora 只有 per-port 聚合计数器（[types.rs:45](../../crates/rcoder-proxy/src/service/types.rs)），多 app 共用端口无法归属；`/proxy/stats` 仅端口维度。
- 现有 `last_activity`（[entry.rs:31](../../crates/shared_types/src/container/entry.rs)）只服务 agent-runner。
- 停止分成两种业务语义：`recycle_app` 用于空闲回收，允许后续流量唤醒；`stop_app` 用于用户手动停止，必须持续保持停止，不能被访问意外拉起。二者都只 scale-to-zero，不删除 PVC。

### 1.2 功能需求

- **R1 自动回收**：闲置超过阈值的 Running UserApp → scale replicas 0。仅回收 `managed-by=rcoder-app-manager` 的 Deployment；跳过 `protection_seconds` 内新建的 app。
- **R2 配置开关**：全局 `enabled`(默认 true，可由部署侧关闭) + `idle_timeout_seconds`(默认 432000=5天) + `scan_interval_seconds`(默认 3600) + `wake_timeout_seconds`(默认 60) + `protection_seconds`(默认 300)。
- **R3 流量唤醒**：仅由空闲回收产生的 stopped UserApp 收到 `/proxy/userapp/prod/{user_id}/{id}/...` 请求时，自动 scale 1 → 轮询 Ready（上限 `wake_timeout`）→ Ready 后正常代理；超时返回 503+`Retry-After: 15`。用户手动停止和发布切换期间禁止流量唤醒。
- **R4 per-app 回收策略（付费/免费分层）**：默认所有 UserApp **可回收**（= 免费用户语义）。允许对**单独 app** 设为不回收——通过 `CreateAppRequest`/`UpdateAppRequest` 的 `recycle_enabled: Option<bool>` 字段（rcoder 持久化为 Deployment 注解 `rcoder.io/recycle-enabled`），`false` = **永不回收**（付费 / 需常驻的 app）。另支持 `idle_timeout_seconds: Option<u64>` 字段（注解 `rcoder.io/idle-timeout-seconds`）覆盖全局阈值。**rcoder 不感知"付费/免费"业务概念**，只看布尔；tier→bool 映射由 Java 调用方决定（付费 → `recycle_enabled=false`）。

### 1.3 非目标

- **不删 PVC**（硬约束，回收 = scale0，不是 delete；销毁存储仍走独立的 `POST /apps/{id}/storage/destroy`）。
- **不做 Gateway 模式唤醒**（HTTPRoute → Envoy 绕过 rcoder，需 Envoy 级钩子，超出 v1）。
- **不做 CPU/metrics 闲置判断**（v1 仅 HTTP 访问信号；CPU 兜底确认留 v2）。
- **rcoder 不感知业务 tier**（付费/免费/套餐）：只暴露 per-app `recycle_enabled` 布尔与 `idle_timeout_seconds`；tier→字段映射归 Java 调用方，rcoder 不存用户/计费元数据。
- **不持久化 `last_accessed` 到 DB/注解**（v1 in-memory，重启给所有 Running app 重置时钟；注解持久化留 v2）。
- **不动**：K8s runtime 创建逻辑、PVC、路 B（UserAppBuilder）、已交付的健康端点。

### 1.4 行为矩阵

| 场景 | 回收扫描器 | 流量唤醒 | 客户端结果 |
|---|---|---|---|
| Running + 频繁访问 | 不回收（last_accessed 新） | 不触发 | 正常代理 200 |
| Running + 闲置 > 阈值 | 回收 → scale0，记 stopped_apps | — | — |
| 空闲回收后 + 来请求 | —（已 stopped） | scale1 → Ready → 代理 | hold-and-wait 后 200（≤60s） |
| 空闲回收后 + 60s 未 Ready | — | 超时 | 503 + Retry-After（后台继续起） |
| 空闲回收后 + 并发多请求 | — | 合流成一次 wake | 共享同一次唤醒结果 |
| 用户手动停止 + 来请求 | — | 禁止唤醒 | 503，保持 replicas=0 |
| 发布切换期间 + 来请求 | — | 禁止唤醒 | 503，避免旧/新版本并发启动 |
| 默认（无注解 = 免费用户） | 按阈值回收 | 唤醒 | 正常 |
| 注解 recycle-enabled=false（付费） | 跳过，永不回收 | — | 常驻 Running |
| Gateway 模式 | 不适用（无访问信号） | 不适用 | 已知限制 |

---

## 2. Plan（如何实现）

### 2.1 架构总览

```
                  ┌──────────────────────── rcoder pod (single instance) ────────────────────────┐
                  │                                                                       │
  客户端 ──HTTP──► │  Pingora (NodePort 30435)                                             │
                  │   request_filter  ──► ① touch(app_id)  [更新 last_accessed, 节流]       │
                  │                   ──► ③ if stopped_apps 含 app_id:                       │
                  │                          wake_or_join(app_id) [hold-and-wait ≤60s]        │
                  │                          Ready → Ok(false) 继续代理                       │
                  │                          Timeout → 503+Retry-After                        │
                  │   upstream_peer ──► app_backends[(app_id,port)] → Service FQDN → Pod      │
                  │                                                                       │
                  │  ② UserAppRecycleScanner (interval 3600s)                               │
                  │     list_deployments → 比 last_accessed > 阈值 → recycle_app(scale0)     │
                  │     → 标记为可流量唤醒                                                  │
                  │                                                                       │
                  │  AppActivityRegistry (in-memory DashMap):                               │
                  │     last_accessed / stopped / wake_blocked / waking                      │
                  └───────────────────────────────────────────────────────────────────────────┘
```

三个组件：① 访问追踪（pingora hot path）、② 回收扫描器（bg task）、③ 流量唤醒（pingora request_filter）。共享一个 `AppActivityRegistry`。

### 2.2 核心数据结构

新增于 `app_manager`（实现跨 crate trait，见 2.6）：

```rust
/// UserApp 活动状态注册表（in-memory，rcoder 单实例）
pub struct AppActivityRegistry {
    /// app_id → 最近一次真实 HTTP 访问时间（节流更新，5s 粒度）
    last_accessed: DashMap<String, Instant>,
    /// app_id → 已停止（scale0）记录；空闲回收和手动停止都维护
    stopped: DashSet<String>,
    /// app_id → 禁止由流量唤醒；手动停止和发布切换时设置
    wake_blocked: DashSet<String>,
    /// app_id → 进行中的唤醒句柄（并发合流用）；leader 负责 scale+wait，follower join
    waking: DashMap<String, Arc<WakeHandle>>,
}
```

### 2.3 组件 ①：访问追踪（AppAccessTracker）

**目的**：记录每个 app 的最近访问时间，作为闲置判断的唯一信号源。

- pingora `request_filter`（[mod.rs:113](../../crates/rcoder-proxy/src/service/mod.rs)）解析路径，对 `/proxy/userapp/prod/{user_id}/{app_id}/...` 路由调 `tracker.touch(app_id)`。
- **节流**：`touch` 内部仅当 `now - last > 5s` 才写 DashMap，避免高 QPS 下的锁竞争（entry API）。
- 非	app 路由（VNC/project/api 等）不 touch。
- DI：照 `container_lookup` 模式（[mod.rs:72/102](../../crates/rcoder-proxy/src/service/mod.rs)），`PingoraProxyService` + `PortProxy` 各加一个 `access_tracker: Option<Arc<dyn AppAccessTracker>>` 字段，经 `PingoraProxyService::new` 注入。

### 2.4 组件 ②：回收扫描器（UserAppRecycleScanner）

仿 `AgentCleaner::run`（[cleaner.rs:280](../../crates/rcoder/src/cleanup_task/cleaner.rs)）的 interval 循环，挂在 `start_all_background_tasks`（[background_tasks.rs:23](../../crates/rcoder/src/background_tasks.rs)），返回第 N 个 `JoinHandle`。

每个 tick：
1. `enabled=false` 直接 return。
2. `runtime.list_deployments()` 枚举 `managed-by=rcoder-app-manager` 的 UserApp（复用 `rebuild_pingora_backends` 的枚举方式，[app_pingora.rs:90](../../crates/app_manager/src/app_pingora.rs)）。
3. 对每个 **Running**（`spec.replicas > 0`）app：
   - 读注解 `rcoder.io/recycle-enabled`：**absent 或 `"true"` → 可回收（免费用户默认）**；`"false"` → 跳过（付费 / 常驻 app，永不回收）；
   - 阈值 = 注解 `rcoder.io/idle-timeout-seconds` ?? 全局 `idle_timeout_seconds`；
   - `now - last_accessed[app_id] > 阈值` 且 龄期 > `protection_seconds` → 回收。
4. 回收动作 = 调 `AppService::recycle_app(app_id)`（scale0）→ `registry.mark_recycled(app_id)`；该状态明确允许流量唤醒。
5. 每 tick 错误隔离（单个 app 失败不影响其他），warn 记录。

**关键**：扫描器只回收 Running 闲置 app；已 stopped 的不再处理。`last_accessed` 在 wake 时已 touch，避免"唤醒中被误回收"竞态。

### 2.5 组件 ③：流量唤醒（wake-on-traffic）

注入 `request_filter`，在 ① touch 之后：

```
request_filter(session):
    解析 app_id（matchit router，RouteType::AppPortProxy）
    if app_id is None: return Ok(false)             // 非 app 路由
    access_tracker.touch(app_id)                     // ①
    if !wake_control.is_stopped(app_id):
        return Ok(false)                             // 正在运行，正常代理
    // ② stopped 且允许 wake → 唤醒（hold-and-wait）
    match wake_control.ensure_running(app_id).await {
        Ready | AlreadyRunning => { registry.mark_running(app_id); return Ok(false) }
        Timeout => { respond_503(Retry-After: 15); return Ok(true) }   // 已直接响应
        Failed(e) => { respond_503; return Ok(true) }
    }
```

**`ensure_running(app_id)` 状态机（并发合流）**：

```
- waking 含 app_id → join 现有 handle，返回其结果（多请求共享一次 scale-up）
- 否则 → 成为 leader：
    insert waking[app_id] = handle
    scale_deployment(app_id, 1)
    poll Deployment .status.readyReplicas >= 1（轮询 1s/次，上限 wake_timeout=60s）
    remove waking[app_id]
    返回 Ready / Timeout
```

合流机制建议 `DashMap<String, Arc<WakeHandle>>`，`WakeHandle` 内含 `Notify` + `Mutex<Option<WakeOutcome>>`：leader 完成后 notify 所有 follower 读取结果。leader 意外 panic/超时由 `DashMap::entry` 的 RAII 清理兜底（remove on drop）。

**Ready 判定**：轮询 Deployment `.status.readyReplicas >= 1`（app 无关，不依赖后端 /ready）；复用现有 k8s_query 能力。镜像 `IfNotPresent` 缓存命中 + app 进程启动 + `initialDelaySeconds` 决定冷启时长（通常 10–40s）。

### 2.6 跨 crate 接口（trait）

放 `shared_types`（与现有 `ContainerLookup` 同位，[mod.rs:72](../../crates/rcoder-proxy/src/service/mod.rs)），rcoder-proxy / app_manager / rcoder 三方共用：

```rust
#[async_trait]
pub trait AppAccessTracker: Send + Sync {
    /// 更新最近访问（内部节流）；仅 /proxy/userapp/prod/* 路由调用
    fn touch(&self, app_id: &str);
}

#[async_trait]
pub trait AppWakeControl: Send + Sync {
    /// app 是否已 stopped（scale0）
    fn is_stopped(&self, app_id: &str) -> bool;
    /// 确保 app Running；stopped→唤醒(hold-and-wait)，running→立即 Ready
    async fn ensure_running(&self, app_id: &str) -> WakeOutcome;
    /// 标记 Running（wake 成功或外部 start 后）
    fn mark_running(&self, app_id: &str);
}

pub enum WakeOutcome { Ready, AlreadyRunning, Timeout, Failed(String) }
```

`AppService` 持有 `AppActivityRegistry` 并实现两个 trait；扫描器（rcoder crate）直接调 `AppService` 方法（rcoder 依赖 app_manager，无需 trait）。

### 2.7 配置设计

**rcoder config**（[config.rs:182 `CleanupConfigSettings`](../../crates/rcoder/src/config.rs) 同款范式）：

```rust
pub struct UserAppRecycleConfig {
    #[serde(default = "default_true")] pub enabled: bool,       // default true（免费用户默认回收；部署侧可 helm/env 关闭）
    #[serde(default = "default_5d")] pub idle_timeout_seconds: u64,        // 432000
    #[serde(default = "default_1h")] pub scan_interval_seconds: u64,       // 3600
    #[serde(default = "default_60")] pub wake_timeout_seconds: u64,        // 60
    #[serde(default = "default_300")] pub protection_seconds: u64,         // 300
}
```

挂 `AppConfig`（与 `cleanup_config` 并列）。env 覆盖：`RCODER_USERAPP_RECYCLE_ENABLED` / `RCODER_USERAPP_IDLE_TIMEOUT_SECONDS` / …（逐字段 env override，照 [config.rs:618](../../crates/rcoder/src/config.rs) 模式）。

**helm**（`build-agent-docker/k8s/helm/nuwax-platform`）：
- `values.yaml`：`rcoder.userAppRecycle: { enabled, idleTimeoutSeconds, scanIntervalSeconds, wakeTimeoutSeconds, protectionSeconds }`
- `templates/rcoder/deployment.yaml`：新增 env 段（照 [deployment.yaml:138 `RCODER_PER_AGENT_PVC_ENABLED`](../build-agent-docker/k8s/helm/nuwax-platform/templates/rcoder/deployment.yaml) 模式）。

**per-app API 字段 → 注解**（rcoder 在 create/update 时把请求字段 stamp 为 Deployment 注解；扫描器只读注解，二者解耦）：
- `CreateAppRequest`/`UpdateAppRequest` 加 `recycle_enabled: Option<bool>`（None / `true` = 默认可回收）、`idle_timeout_seconds: Option<u64>`。
- 注解 `rcoder.io/recycle-enabled`：**absent / `"true"` = 可回收（免费默认）**；`"false"` = 永不回收（付费 / 常驻）。
- 注解 `rcoder.io/idle-timeout-seconds`：覆盖全局阈值（如 `"86400"`）。
- Java 调用方按 app tier 决定字段值（付费 → `recycle_enabled=false`），rcoder 不感知 tier、不存计费元数据。

### 2.8 状态一致性与重启重建

停止状态的维护点：
- `recycle_app`（扫描器）→ `mark_recycled`，允许 wake-on-traffic
- `stop_app`（手动 API、发布切换）→ `mark_wake_blocked`，禁止 wake-on-traffic
- `start_app`（API）→ `mark_running`
- wake 成功 → `mark_running`
- **rcoder 重启**：`AppService::new` 调 `rebuild_stopped_apps()`；`spec.replicas==0` 且注解 `rcoder.io/wake-on-traffic=false` 的恢复为 wake-blocked，其余 scale0 应用恢复为可唤醒。注解缺失按可唤醒处理，兼容已部署的旧资源。

`last_accessed` 重启策略（v1）：Running app 的 `last_accessed` 初始化为 `now`（重启给一个完整 grace 周期，最坏多宽限一个阈值，可接受）；Stopped app 无需 last_accessed。

### 2.9 并发与竞态

| 竞态 | 处理 |
|---|---|
| 扫描器回收 vs wake 同时发生 | wake 已先 `touch`（更新 last_accessed），扫描器读到新时间 → 不回收。额外：扫描器跳过 `waking` 中的 app |
| 并发请求触发多次 scale-up | `waking` 句柄合流，仅 leader 执行 scale，follower join |
| stop 与 start API 并发 | 先持久化 `rcoder.io/wake-on-traffic` 再 scale；registry 使用 entry API 更新，失败时恢复原状态 |
| 发布切换 vs 外部流量 | 停止旧 workspace 前写入 wake-blocked，阻止代理流量在切换窗口重新拉起 |
| rcoder 重启丢失 waking 态 | 重启后 stopped_apps 重建自 K8s，waking 清空；下次请求重新发起 wake（幂等） |

### 2.10 错误处理（Fail Fast）

- `enabled=false`：扫描器与 wake 钩子完全短路（`access_tracker`/`wake_control` 注入 None 时 pingora 走原逻辑）。
- scale/wake 失败：warn + 不崩；wake 返回 `Failed` → 客户端 503，不影响其他 app。
- 扫描器单 app 异常：隔离，不中断 tick。
- 触发条件缺配置（如 wake_control 未注入但 enabled=true）：启动时 warn（Fail Fast 暴露部署侧配置不一致）。

### 2.11 改动清单（file:line 锚点）

| # | 文件 | 改动 |
|---|---|---|
| 1 | `shared_types/src/lib.rs`（新 trait 文件） | 加 `AppAccessTracker` / `AppWakeControl` / `WakeOutcome` |
| 2 | `app_manager/src/activity_registry.rs`（新） | `AppActivityRegistry`（last_accessed/stopped/waking + mark/touch/ensure_running） |
| 3 | `app_manager/src/service.rs:30` | `AppService` 加 `activity: AppActivityRegistry`；`new()`(:48) 加 `rebuild_stopped_apps()` |
| 4 | `app_manager/src/app_ops.rs` | `start_app`/`stop_app`/`recycle_app` 分别维护 running、wake-blocked、wakeable 状态 |
| 4a | `app_manager/src/models.rs` + `k8s_app_create.rs` | `CreateAppRequest`/`UpdateAppRequest` 加 `recycle_enabled`/`idle_timeout_seconds` 字段；create/update stamp 为 `rcoder.io/*` 注解（absent = 可回收） |
| 5 | `app_manager/src/service.rs` | impl `AppAccessTracker` + `AppWakeControl`（透传 activity） |
| 6 | `rcoder-proxy/src/service/mod.rs:60,79` | `PingoraProxyService` + `PortProxy` 加 `access_tracker`/`wake_control` 字段；`PingoraProxyService::new` 注入 |
| 7 | `rcoder-proxy/src/service/mod.rs:113` | `request_filter`：解析 app_id → touch → stopped 检查 → wake（hold-and-wait/503） |
| 8 | `rcoder/src/config.rs:182`(旁) | `UserAppRecycleConfig` + env override + 挂 `AppConfig` |
| 9 | `rcoder/src/cleanup_task/userapp/`（新目录） | `scanner.rs`（interval loop + 回收逻辑） |
| 10 | `rcoder/src/background_tasks.rs:23` | `start_userapp_recycle_task`，返 JoinHandle |
| 11 | `rcoder/src/router.rs` / `main.rs` | 装配：scanner 拿 AppService；pingora 注入 tracker/wake_control |
| 12 | `build-agent-docker/.../values.yaml` + `deployment.yaml` | `userAppRecycle` 配置段 + env |

预计 ~450–550 行，集中在新模块，对现有 K8s runtime / PVC / 路 B 零改动。

---

## 3. Task 拆解（执行单元）

- **T1**：`shared_types` 加三个 trait/enum（AppAccessTracker / AppWakeControl / WakeOutcome）。完成标准：编译过，clippy 零 warning。
- **T2**：`app_manager` 新建 `activity_registry.rs`（DashMap 三表 + touch 节流 + mark_stopped/running + ensure_running 状态机 + rebuild_stopped_apps）。完成标准：单元测试覆盖 touch 节流、wake 合流、超时。
- **T3**：`AppService` 持有 activity + impl 两 trait + `start_app`/`stop_app`/`recycle_app` 维护不同停止语义 + `new()` 从副本数和注解重建状态 + `CreateAppRequest`/`UpdateAppRequest` 加 `recycle_enabled`/`idle_timeout_seconds` 字段并 stamp `rcoder.io/*` 注解。完成标准：编译，现有 app_manager 测试不退化。
- **T4**：`UserAppRecycleConfig` + env override + 挂 AppConfig + 启动装配（pingora 注入 tracker/wake_control，None 时短路）。
- **T5**：pingora `request_filter` 注入字段 + 唤醒逻辑（路径解析/touch/stopped 检查/wake hold-and-wait/503）。完成标准：stopped app 来请求能唤醒并代理。
- **T6**：`cleanup_task/userapp/scanner.rs` + `background_tasks` 注册。完成标准：闲置 app 被 scale0 且记 stopped_apps。
- **T7**：helm `values.yaml` + `deployment.yaml` 配置段。
- **T8**：clippy（default + `--features kubernetes`）零 warning + `cargo test` 全绿 + fmt。
- **T9**：229 部署 E2E（见 §4）。

依赖：T1 → T2 → T3 → {T4,T5,T6} 并行 → T7 → T8 → T9。

---

## 4. 验证方案

1. **静态**：`cargo fmt --check` + `cargo clippy --default-features --features kubernetes`（零 warning）+ `cargo test`（全绿，含新增 activity_registry 单测）。
2. **229 E2E**（Pingora 模式，NodePort 30435）：
   - **回收**：`enabled=true`，`idle_timeout_seconds=120`（测试用短阈值），发布一个 app → 停止访问 > 2min → 扫描器 scale0 → `kubectl get deploy` replicas=0，PVC/Service 仍在 → `stopped_apps` 含该 id。
   - **唤醒**：空闲回收 scale0 后 `curl http://192.168.32.229:30435/proxy/userapp/prod/{user_id}/{id}/...` → hold-and-wait ≤60s → 200，Deployment replicas 回 1。
   - **手动停止隔离**：调用 stop API 后访问代理路径 → 返回 503，Deployment 持续 replicas=0；显式 start 后恢复。
   - **超时**：人为让 app 启动失败（坏 command）→ wake 60s 超时 → 客户端收 503+Retry-After。
   - **并发合流**：stopped app 并发 5 请求 → 仅 1 次 scale-up（日志/事件验证）。
   - **注解覆盖**：app 注解 `recoder.io/recycle-enabled=false` → 闲置不被回收。
   - **重启重建**：分别准备空闲回收和手动停止的 scale0 app → 重启 rcoder → 前者仍能被访问唤醒，后者仍保持停止。
   - **Gateway 模式**（如可选）：确认已知限制（不回收、不唤醒），不 crash。

---

## 5. 风险与限制

- **Gateway 模式不支持**（HTTPRoute 绕过 rcoder）。当前 229 部署为 Pingora 模式，不受影响；切 Gateway 需 v2 加 Envoy 钩子。
- **冷启延迟感知**：hold-and-wait 期间客户端像"卡住"。缓解：60s 上限 + 超时 503+Retry-After；前端可加 loading 态。web 应用首屏冷启 10–40s 可接受。
- **重启 grace 膨胀**：v1 重启给 Running app 重置闲置时钟，最坏多宽限一个阈值。注解持久化（v2）可消除。
- **out-of-band scale**：用户手动 `kubectl scale 0` 没有同步业务意图；rcoder 重启前可能仍直连失效 backend，重启后会按注解重建。运维应使用 stop/recycle API，避免绕过状态机。
- **rcoder 单实例假设**：in-memory registry 不支持多副本；当前 `replicaCount:1`，满足。多副本需上 DB/注解（v2）。
