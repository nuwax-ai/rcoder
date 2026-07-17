# 应用管理服务设计文档 v2（重设计）

> 关系说明：本文档是 [`application-management-service-design.md`](./application-management-service-design.md)（v1）的重设计版本。
> v1 仍保留作为历史背景。当前仍处开发阶段、尚未对外提供接口，因此 v2 直接重新定义契约，不留兼容包袱。
>
> 阅读顺序建议：先读 §3「核心架构决策」——尤其 §3.1「要不要上 CRD」是整篇文档的地基，决定后续所有实现方式。

---

## 1. 目标与边界

### 1.1 目标（"启动不同语言的镜像，在里面运行服务"）

RCoder 对外提供一套 REST API，让调用方（Java 业务服务）管理**用户业务应用**的生命周期：
把一个多语言（Java / Python / TypeScript / Go / Rust / 前端）容器镜像，连同它的启动命令、环境变量、端口、健康检查、资源限制，部署成可被访问的长期运行服务。

### 1.2 关键定位（决定一切设计的两条约束）

| 定位 | 含义 | 设计后果 |
|---|---|---|
| **RCoder 是无状态的应用 Pod 引擎** | RCoder 进程不持久化任何应用元数据；重启后必须能从集群重新发现正在运行的应用 | 读路径必须"现场从集群重建"；不能依赖进程内缓存或本地 DB |
| **应用元数据归 Java 服务持久化** | "这个 app 叫什么名、镜像是什么、谁创建的、什么时候创建的"由 Java 服务的 DB 持有；RCoder 只管"把它跑起来、查它的运行时状态" | RCoder 的 API 响应分两类：**desired（来自请求）** 与 **observed（来自集群）**，二者职责分明 |

### 1.3 非目标（明确不做）

- ❌ **不做源码构建**（不内置 buildpack / Dockerfile build）。镜像由调用方事先准备好，RCoder 只消费 `image`。
- ❌ **不做应用元数据 DB**。name / created_at / 业务标签等归 Java。
- ❌ **不做 GitOps / 声明式对账循环**（至少 v2 不做；见 §14 演进路线）。
- ❌ **不做用户直接面向的前端**。RCoder 的调用方是 Java 服务。

---

## 2. 对标调研结论（v2 设计的事实依据）

调研了 4 个开源项目（3 个本地源码 + Epinio）。下表只保留**影响 v2 设计的结论**，详细调研见各仓库。

| 项目 | 形态 | 对 v2 的贡献 |
|---|---|---|
| **Epinio**（Go，最成熟的 K8s PaaS REST） | REST API + CR + Helm | ⭐ **REST 契约、desired/observed 模型、`fetch()` 对账范式、错误模型（MultiError）** 直接移植 |
| **operator-rs**（Stackable，kube-rs 之上的 operator helper） | Rust 库 | ⭐ **Server-Side Apply 幂等 apply、label-based orphan GC、`compute_conditions`** 可直接抄 |
| **nya**（Rust，K3s PaaS） | CLI + SSH + Helm | 实现层细节参考（URL 命名约定、Helm 模板化）；**形态错配**（CLI 非 REST，无对账），核心三问帮不上 |
| **kubert**（Rust，Linkerd 控制面框架） | 控制器框架 | ❌ **Skip**：控制器专用、HTTP 栈与 axum 冲突、活跃度低。最多 cherry-pick 其 prometheus 子 crate |

### 2.1 从 Epinio 学到的三条核心范式

1. **desired / observed 二分**（`pkg/api/core/v1/models/app.go`）：
   - `Configuration`（用户想要的：instances / routes / environment / services）
   - `Workload *AppDeployment`（集群里实际跑的：readyReplicas / replicas / routes）
   - 每次 GET 现场合并二者 → status
   - **RCoder 几乎已经是这个模型**（`AppInfo` desired vs `AppRuntimeInfo` observed），v2 把它正式化。

2. **`fetch()` 式对账**（`internal/application/application.go::fetch`）：每个 GET 都不读缓存，从集群（K8s CR / Secret / Deployment / Pod / Ingress）现场重建 App struct。
   - **这正是 RCoder"无状态引擎"的实现范式**。差别：Epinio 的 desired 存集群（CR + area-Secret），RCoder 的 desired 存 Java 服务。

3. **错误模型**（`pkg/api/core/v1/errors/errors.go`）：
   - 响应体 `{"errors":[{"status":int,"title":str,"details":str}]}`，**支持一次请求返回多个错误**
   - `NewNotFoundError(kind,name)` / `NewConflictError(kind,name)` 等 helper
   - **RCoder 在此基础上加一个 `code` 字段**（业务错误码字符串）

### 2.2 从 operator-rs 学到的三条可复用机制

1. **Server-Side Apply（SSA）封装**（`crates/stackable-operator/src/client/mod.rs::apply_patch`）：
   - `Patch::Apply` + `field_manager = "{global}/{scope}"` + `force = true`
   - 让"create-or-update"幂等，且每个调用点拥有独立 field manager，互不误伤
   - **这是 v2 真正实现 `update_app` 的关键**（v1 因无法 in-place 更新而 Fail-Fast）

2. **label-based orphan GC**（`cluster_resources.rs::delete_orphaned_resources`）：
   - 列出带本应用标签的所有某类型对象，删掉 UID 不在"本次期望集合"里的
   - **不依赖 OwnerReference，因此不需要父 CRD** —— 后端无关，Docker 也能用同一套思路

3. **conditions 合并逻辑**（`status/condition/mod.rs::compute_conditions`）：
   - 多个 builder 的同 type condition 合并：同 status 拼 message，变 status 才更新 `last_transition_time`
   - **RCoder 用于在 read 路径派生 `conditions[]`**（不写回集群，只返回响应）

### 2.3 明确不采纳的（避免照搬）

- **不学 Epinio 用 Helm 部署**：Helm 是 K8s-only，无法复用到 Docker 后端。RCoder 用 `Backend` trait 直接 create Deployment/Service/Ingress（K8s）与 container/network（Docker）。
- **不学 Epinio 用 in-memory map 存异步部署状态**（`deployments.go::asyncDeployJobs`）：这是 Epinio 的真实弱点（重启全丢）。RCoder 的"异步"由 Deployment 自身状态承载，见 §7.3。
- **不学 nya 的无删除路径**：v2 必须有完整删除路径。（nya 的 `latest` tag 教训——无法回滚、缓存污染——客观存在，但 v2 当前阶段**暂不强制 image 标签**，原因见 §13.2。）

---

## 3. 核心架构决策

### 3.1 【最重要】要不要引入 `App` CRD？

> 这是整个设计的地基，必须先拍板。两条路线的后果差异极大。

#### 路线 A：不引入 CRD，RCoder 保持"命令式无状态引擎"（**v2 推荐**）

RCoder 直接在 REST handler 里，通过 kube-rs / bollard 命令式地 CRUD **原生**资源（Deployment / Service / ConfigMap / Secret / HTTPRoute / NodePort / PVC），全部打标签 `app.kubernetes.io/instance={app_id}, managed-by=rcoder-app-manager`。

- **desired state**：在 Java 服务 DB 里。RCoder 每次 CRUD 时由请求体携带进来。
- **observed state**：从集群现场读（`Deployment.status` / Pod 状态 / Service NodePort）。
- **GC**：label 查询 + 有序删除（operator-rs `delete_orphaned_resources` 思路）。
- **重启恢复**：label 反查所有托管资源即可重建 observed 视图（RCoder 本就不持 desired）。

#### 路线 B：引入 `App` CRD + controller（未来演进，v2 不做）

RCoder 写 `App` CR 的 spec，一个 controller（kube-rs Controller / operator-rs）异步 reconcile 出子资源，用 OwnerReference 自动 GC，status 写回 CR 的 status subresource。

**路线 B 的代价**（为什么 v2 不做）：
1. **与"无状态引擎"定位冲突**：controller 自带 reflector 缓存、leader election，是"有状态"组件。
2. **破坏 Docker 后端对称性**：CRD 在 Docker 里不存在，`Backend` trait 的两个实现会高度不对称（K8s 走 reconcile，Docker 走直接 CRUD）。
3. **与 Java 的权威性打架**：Java 已经是 desired state 的 source of truth，再在集群里放一份 App CR spec，等于两个 desired source，需要双向同步。
4. **复杂度**：CRD schema + status subresource + controller runtime + reconcile 逻辑 + 升级迁移，是 v1 代码量的数倍。
5. **operator-rs 的 owner-ref GC / status subresource 这两个最大红利确实依赖 CRD**，但 label-based GC + conditions-on-read 能覆盖 RCoder 当前 80% 的需求。

**何时切到路线 B**（明确触发条件，写进演进路线 §14）：
- 需要多副本 + 滚动升级 + 回滚（Revision 模型）；
- 需要"期望状态长期对账"（Java 下发期望，RCoder 持续修正漂移）；
- 需要 status 在集群里长期持久化（供其它 K8s 工具消费）。

**结论**：**v2 走路线 A**。同时 API 契约设计成"CRD 是内部实现细节"——将来切路线 B 时，REST 契约不变，只是 handler 内部从"直接 CRUD 子资源"换成"写 App CR spec + 等 reconcile"。

---

### 3.2 其余决策（D1–D10，建立在路线 A 之上）

| 编号 | 决策 | 依据 |
|---|---|---|
| **D1** | desired / observed 严格二分，响应类型分两套 | Epinio `App` 模型；RCoder `AppInfo`(desired) / `AppRuntimeInfo`(observed) 已是此结构 |
| **D2** | **HTTP 方法仅用 GET + POST**（部署环境网关限制，禁用 DELETE/PUT/PATCH）；写操作把动词放进 path：`POST /apps/{id}/delete`、`POST /apps/{id}/update`、`POST /apps/{id}/files/delete`。读用 GET，写用 POST；**复杂过滤的查询也走 POST + body**（`POST /apps/query`，规避 URL 编码/长度限制、便于扩展筛选条件） | 部署环境只放行 GET/POST；动词进路径是该约束下的标准做法 |
| **D3** | 更新用 **SSA 幂等 apply**（operator-rs `apply_patch` 模式），真正实现 `POST /apps/{id}/update` | operator-rs；解决 v1 update Fail-Fast |
| **D4** | 删除用 **label-based 有序 GC + orphan 扫描**，不依赖 OwnerReference | operator-rs `delete_orphaned_resources`；后端无关 |
| **D5** | status = 单个 `AppStatus` 枚举（headline，Java 友好）+ 可选 `conditions[]`（read 时派生，诊断用） | operator-rs conditions + Epinio 单串，二者结合 |
| **D6** | 错误模型：**沿用现有 `HttpResult`**（`{success,code,message,data,tid}`，不改 shared_types）；`retryable` 作为错误码的固有属性（`is_retryable_code`），不在响应体重复 | HttpResult 跨全项目共用，不动；Epinio MultiError 的"分类"思路体现在错误码枚举上 |
| **D7** | "异步部署"由 Deployment 自身状态承载：create 创建资源后立即返回 `status=Starting`，不阻塞等 ready；调用方轮询 GET | 不学 Epinio 的 in-memory job map（重启丢失） |
| **D8** | 日志流走 **WebSocket**（`GET /apps/{id}/logs/stream` WS upgrade） | 双向：Java 可发控制帧（如停止 follow）；需给 axum 加 `ws` feature |
| **D9** | 双后端用 `ContainerRuntime` trait 抽象；同一份应用语义映射到两套资源 | 已有 trait；nya 的教训：Helm 是 K8s-only 不能跨后端 |
| **D10** | **HTTP 暴露默认统一走 RCoder 内置 Pingora 代理**（两后端一致：Docker→container_ip，K8s→ClusterIP Service FQDN）；**gateway（HTTPRoute）作为可选暴露路径**，由 `http_expose` 配置切换（默认 `pingora`）；TCP 初期不对外，只给 internal ClusterIP | 消除 K8s(gateway)/Docker(Pingora) 分裂；RCoder 作唯一 HTTP 入口便于统一治理；详见 §8 |

---

## 4. REST API 契约（v2）

> 前缀：`/api/v1`。所有响应统一包 `HttpResult<T>`（`{success,data,code,message,tid}`）。

### 4.1 方法约束与 v1 → v2 变更

**HTTP 方法约束**：仅允许 `GET`（读）与 `POST`（写），禁用 `DELETE`/`PUT`/`PATCH`。写操作把动词放进 path（`/delete`、`/update`、`/files/delete`）。**因此端点路径与 v1 基本一致**（v1 本就是这套），v2 的实质改动在语义而非路径。

v1 → v2 真实变更（均与 HTTP 方法无关）：

| 项 | v1 | v2 | 理由 |
|---|---|---|---|
| 列表 | `POST /apps/query`（body 过滤） | `POST /apps/query`（body 过滤，**保持 v1**） | 用 body 承载过滤条件，规避 URL 编码/长度限制、便于未来扩展复杂筛选；RCoder 仅按可观测字段过滤（status/app_ids/tenant_id/space_id），name/created_at 过滤归 Java |
| 日志 | `GET /apps/{id}/logs?follow=true` | `GET /apps/{id}/logs`（快照）+ `GET /apps/{id}/logs/stream`（**WebSocket**） | 拆快照与流式（D8） |
| 创建语义 | 同步建 + 等 ready | 创建资源即返回 `Starting`，不等 ready | 异步化（D7） |
| update 语义 | Fail-Fast 拒绝 | `POST /apps/{id}/update` 走 SSA apply，真正可改 | D3 |
| delete 语义 | 普通删除 | `POST /apps/{id}/delete` 走有序 + orphan 扫描 | D4 |
| 其它写路径 | — | `POST /apps/{id}/{start,stop,restart,upload}`、`POST /apps/{id}/files/delete` 保持 v1 | 已满足 GET+POST 约束，无需改 |

### 4.2 完整端点清单

#### 生命周期

| 方法 | 路径 | 说明 | 成功码 |
|---|---|---|---|
| `POST` | `/apps` | 创建应用（创建子资源后立即返回 `Starting`） | 201 |
| `POST` | `/apps/query` | 列出应用运行时（body: `QueryAppsRequest`：filters / pagination / sort） | 200 |
| `GET` | `/apps/{id}` | 获取运行时详情（仅 observed；见 §6.1） | 200 |
| `POST` | `/apps/{id}/update` | 部分更新（SSA apply；image/env/ports/health_check/resources 均可） | 200 |
| `POST` | `/apps/{id}/delete` | 删除（有序 GC + orphan 扫描） | 200 |

> 对账专用端点（无状态引擎的关键，rcoder/Java 重启后用）：

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/apps/runtime` | 列出集群中**所有** `managed-by=rcoder-app-manager` 的应用运行时状态 |

#### 操作

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/apps/{id}/start` | scale replicas = 期望副本数（默认 1） |
| `POST` | `/apps/{id}/stop` | scale replicas = 0 |
| `POST` | `/apps/{id}/restart` | rollout restart（触发 Recreation） |

#### 查询

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/apps/{id}/logs` | 日志快照（query: `tail,timestamps,since,until`） |
| `GET` | `/apps/{id}/logs/stream` | 日志 **WebSocket** 流（query: `tail,follow,since`；WS upgrade） |
| `GET` | `/apps/{id}/health` | 健康状态（由运行时派生） |
| `GET` | `/apps/{id}/stats` | 资源使用（best-effort；K8s 需 metrics-server，Docker 原生） |
| `GET` | `/apps/{id}/events` | K8s Events / Docker events |

#### 文件管理（路径契约：app 根相对，见 §10）

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/apps/{id}/upload`（multipart，`file` + `target` + 可选 `flatten`） | 上传文件/压缩包到 `target`（app 根相对）。**自动判断（魔数）**：zip/tar.gz 解压到 `target` 目录；其它按单文件存 `target`（如 `code/app.jar`）。`flatten=true` 剥单层 wrapper 目录 |
| `GET` | `/apps/{id}/files`（query: `path`） | 列出 `path` 下的文件（默认 app 根） |
| `POST` | `/apps/{id}/files/delete`（body: `{ "path": "code/app.jar" }`） | 删除文件（app 根相对路径） |

#### 持久存储管理（v2 新增，见 §5.4）

> 删应用默认保留数据（§5.3），用这组接口显式查询/清空残留存储。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/apps/{id}/storage` | 查询存储状态：`{app_id, exists, path, modified_at, is_orphan}` |
| `POST` | `/apps/{id}/storage/delete` | 清空该 app 持久存储（**仅当 app 已 delete 时允许**，否则 409） |
| `POST` | `/apps/storage/query` | 分页查询持久存储（body: **page/page_size 必填** + filters；**强制分页、无全量模式**） |

---

## 5. 关键交互细节

### 5.1 创建（`POST /apps`）—— 异步语义（D7）

```
Java --POST /apps {desired}--> RCoder
RCoder:
  1. 生成 app_id（或接受 Java 传入）
  2. 创建 ConfigMap/Secret/PVC（如有）
  3. SSA-apply Deployment（replicas=期望, labels, envFrom, probes, ports）
  4. 创建 ClusterIP Service（{app_id}-svc，始终建）
     + 注册 Pingora backend（/proxy/{port} → {app_id}-svc，默认模式）
     [仅 gateway 模式建 HTTPRoute；TCP 初期不建 NodePort，见 §8]
  5. 立即返回 201 + AppInfo{status: Starting}
     —— 不等待 Pod Ready
Java 轮询 GET /apps/{id}，观察 status: Starting → Running
```

**为什么不等 ready**：多语言镜像 pull + 启动可能数十秒到分钟级。同步阻塞会占满连接池、触发网关超时。Deployment 自身就是"异步状态机"，Java 轮询即可，无需额外任务表（对比 Epinio 的 in-memory `asyncDeployJobs` 缺陷）。

### 5.2 更新（`POST /apps/{id}/update`）—— SSA 幂等（D3）

v1 这里 Fail-Fast。v2 用 SSA 真正实现：

| 更新字段 | K8s 动作 | Docker 动作 |
|---|---|---|
| `image` | patch Deployment.spec.template.spec.containers[].image → 触发滚动 | 重建容器（镜像变了必须重建） |
| `command` | patch Deployment...containers[].command | 重建容器 |
| `env` | apply ConfigMap（SSA） | 重建容器（env 是容器创建期参数） |
| `secrets` | apply Secret（SSA） | 重建容器 |
| `resources` | patch Deployment...resources | stop+start（资源限制变了） |
| `health_check` | patch Deployment...probes | 重建容器 |
| `ports`（新增/改 expose_type） | apply ClusterIP Service（SSA）+ 重注册 Pingora backend；gateway 模式额外 apply HTTPRoute（+ orphan 扫描旧端口） | 重建容器 + 重注册 pingora 路由 |

**幂等性**：同一 `POST /apps/{id}/update` 重复发，结果一致（SSA 保证）。返回更新后的 `AppRuntimeInfo`。

### 5.3 删除（`POST /apps/{id}/delete`）—— 有序 GC，**默认保留持久存储**（D4）

```
有序删除（K8s，只删"计算面"）：
  1. HTTPRoute / NodePort Service      （先摘流量）
  2. Service (ClusterIP)
  3. Deployment                         （停 Pod）
  4. ConfigMap / Secret
  ——到此为止。PVC / 工作空间目录默认【保留】，不删——
orphan 扫描（兜底，仅扫 K8s 计算资源）：
  5. list 所有带 instance={app_id}, managed-by=rcoder-app-manager 的【计算资源】
     任何残留 → 删除（防止前面步骤部分失败留孤儿）
     注：持久存储（共享 PVC 上的 apps/{app_id} subPath 目录）不是 K8s 资源，
         不在 label 扫描范围内，故不会被误删——这正是"默认保留"的实现保障。
```

**默认保留持久存储的理由**：应用可重建，数据不可再生。误删应用若连带抹掉 `data/`（数据库、用户上传、文件日志）是灾难性的。因此 `POST /apps/{id}/delete` 只删"计算面"（Deployment/Service/路由/配置），**保留"数据面"**（code/data/logs 目录）。

**可选一键全删**：`POST /apps/{id}/delete` 接受 body `{"purge": true}` → 在上述流程末尾追加存储清理（等价于先 delete 再调 §5.4 的 `storage/delete`）。默认 `purge=false`。

**orphan 扫描是关键**：即使前面有序删除中途失败，最后一步 label 扫描保证不留**计算资源**孤儿。这是 operator-rs `delete_orphaned_resources` 的核心价值。

Docker 后端：删容器 + 删网络 + 摘 pingora 路由 / host port；**保留** `app-workspace/{app_id}/` 目录（同理，`purge=true` 才删）。

### 5.4 持久存储管理（v2 新增）

删应用后数据默认保留，需要专门接口让 Java 显式管理这些"残留数据"，否则会无限堆积。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/apps/{id}/storage` | 查询持久存储状态：`{app_id, exists, path, modified_at, is_orphan}` |
| `POST` | `/apps/{id}/storage/delete` | 清空该 app 的持久存储（code/data/logs 全删） |
| `POST` | `/apps/storage/query` | 分页查询持久存储（body: `QueryStorageRequest`，**page/page_size 必填** + filters；**强制分页、无全量模式**） |

**安全约束**：`POST /apps/{id}/storage/delete` **仅当该 app 的计算资源不存在（即已 delete）时才允许**；若 app 仍存在（Running/Stopped 等），返回 `409 INVALID_STATE`，避免清空在用数据导致损坏。强制工作流：`delete app →（数据保留）→ 确认无误后再 storage/delete`。

**实现**：
- **K8s**：删除共享 PVC（`project-workspace-pvc`）上的 `apps/{app_id}` subPath 目录（**不删 PVC 本身**——PVC 是多 app 共享的）。
- **Docker**：删除 `app-workspace/{app_id}/` 目录。

**重建复用**：若 Java 用同一 `app_id` 重新 `POST /apps`，由于存储目录还在，新应用会自动挂回旧数据（适合"误删应用、找回数据"场景）。若需要干净起点，先 `storage/delete` 再 create。

**存储查询（强制分页，无全量模式）**：扫存储后端（遍历 PVC/工作空间目录树）代价高，`page`/`page_size` 为**必填**，`page_size` 上限 100，**不存在"返回全部"的模式**。建议业务用 `filters`（`app_ids`/`tenant_id`/`space_id`）收窄范围，避免大范围扫描。

```json
POST /apps/storage/query
{
  "page": 1,                       // 必填，从 1 开始
  "page_size": 20,                 // 必填，≤ 100
  "filters": {                     // 可选，用于收窄扫描范围
    "orphan_only": true,           //   true=只返回"有数据、无对应运行应用"的孤儿
    "app_ids": ["app-123"],        //   按 app_id 精确过滤（最省扫描）
    "tenant_id": "t1",
    "space_id": "s1"
  }
}
→ PaginatedResponse<StorageInfo>
  StorageInfo = { app_id, exists, path, modified_at, is_orphan }   // 全部 O(1) 单次 stat，不遍历
```

**存储大小策略（绝不能用 `du`）**：app 存储落在 Rook/Ceph 裸盘上的共享 CephFS PVC（per-app subPath）。RCoder 虽把它挂载为普通路径，但在 CephFS 上**每次 `stat`/`du` 都是一次到 MDS 的网络往返**，遍历目录树代价极高且会压垮 MDS——**RCoder 绝不用 `du`/递归 stat 去算 size**。

因此 `StorageInfo` **不含 `size_bytes`**。per-app 存储用量是"存储观测性"关切，不属于应用生命周期，交给 Ceph 原生体系：`ceph-mgr` + Prometheus `ceph_exporter` + Grafana 观测，配合 CephFS **每目录配额** `ceph.quota.max_bytes`（`setfattr -n ceph.quota.max_bytes -v <bytes> <dir>`）做限额。这样 RCoder 既不算 size 也不背存储计费的锅。

> **备选**（仅当将来业务确需在 RCoder 接口里直接拿 per-app 大小时再考虑）：CephFS 的 MDS 内置维护每目录递归大小，可读虚拟 xattr `ceph.dir.rbytes`（一次 `getfattr`，O(1)、不遍历），仅在 `GET /apps/{id}/storage` 实现，且需确认挂载已开启 xattr 支持 + RCoder 引入 `getfattr`/libcephfs 能力；**列表接口无论如何都不算 size**。当前 v2 不做。

`is_orphan` 字段始终返回（无论是否过滤），让业务一眼识别孤儿。这条查询路径与 `GET /apps/runtime`（列运行中应用）互补，合起来覆盖"应用 vs 数据"两侧对账。

---

## 6. 数据模型

### 6.1 desired（请求侧）—— 由 Java 持有，请求时携带

```rust
// 创建（v1 已有，v2 增 ephemeral_storage + 多租户字段，不动结构）
pub struct CreateAppRequest {
    pub name: String,
    pub image: String,                       // 镜像引用；当前阶段允许 latest（运维未具备版本化发布，见 §13.2）
    pub command: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub secrets: Option<HashMap<String, String>>,
    pub resources: Option<ResourceLimits>,
    pub ports: Option<Vec<PortConfig>>,      // name/port/expose_type(Http|Tcp)
    pub health_check: Option<HealthCheckConfig>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
}

// 更新（v1 是部分字段，v2 补齐 ports / health_check，让 update 能改全部可变字段）
pub struct UpdateAppRequest {
    pub name: Option<String>,                // K8s 后端 name 不可改（Deployment 名固定）→ 报错或忽略
    pub image: Option<String>,
    pub command: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub secrets: Option<HashMap<String, String>>,
    pub resources: Option<ResourceLimits>,
    pub ports: Option<Vec<PortConfig>>,      // v2 新增（整段替换语义，由 SSA + orphan 扫描保证）
    pub health_check: Option<HealthCheckConfig>, // v2 新增
}
```

### 6.2 observed（响应侧）—— 集群现场读

```rust
// 运行时信息（v1 已有，v2 补 conditions）
pub struct AppRuntimeInfo {
    pub app_id: String,
    pub status: AppStatus,                   // headline 枚举（见 §6.3）
    pub phase: String,                       // 原始 phase：Running/Pending/CrashLoopBackOff/...
    pub message: Option<String>,             // 失败原因
    pub replicas: i32,
    pub ready_replicas: i32,
    pub restart_count: u32,
    pub pod_ip: Option<String>,
    pub node: Option<String>,
    pub started_at: Option<String>,
    pub ports: Vec<AppPortStatus>,           // 端口运行时状态；Pingora 模式 external_port 通常 None（HTTP 经 Pingora，TCP 不对外）
    pub access: AccessInfo,
    pub conditions: Vec<Condition>,          // 【v2 新增】诊断用，read 时派生
}

// 【v2 新增】borrow 自 operator-rs ClusterCondition（精简版）
pub struct Condition {
    pub r#type: String,                      // Ready / Available / Progressing / Error
    pub status: String,                      // True / False / Unknown
    pub reason: Option<String>,              // 简短机器码：ImagePullBackOff / CrashLoopBackOff
    pub message: Option<String>,             // 人读描述
    pub last_transition_time: Option<String>,
}
```

### 6.3 `AppStatus` 枚举（headline，Java 友好）

保留 v1 枚举不变（Java 已依赖语义）：

```rust
pub enum AppStatus { Created, Starting, Running, Stopping, Stopped, Error, Deleting }
```

派生规则（read 时，对应 Epinio `fetch()` 的派生逻辑）：

| 集群观测 | AppStatus | conditions 示例 |
|---|---|---|
| Deployment 不存在 | (404 → 应用不存在) | — |
| replicas=0 | `Stopped` | Ready=False(reason:ScaledDown) |
| replicas>0 且 ready=0 且无 BAD waiting | `Starting` | Progressing=True |
| replicas>0 且 ready≥1 | `Running` | Ready=True, Available=True |
| 容器 CrashLoopBackOff / ImagePullBackOff / terminated≠0 | `Error` | Error=True(reason:CrashLoopBackOff), message=exit code |
| 正在删除（labels 还在但 Deployment terminating） | `Deleting` | — |

**关键**：`status` 给 Java 做状态机判断；`conditions[]` 给人/前端做诊断。二者同源派生，不矛盾。

### 6.4 创建时的 `AppInfo`（desired + observed 合并快照）

仅 `POST /apps` 返回 `AppInfo`（含 desired 全字段 + access + 初始 status=Starting）。后续读路径统一返回 `AppRuntimeInfo`（observed only）——因为 RCoder 不持 desired，读时拿不到 name/image 等业务字段，那些归 Java。

> **设计取舍**：v1 文档里 `GET /apps/{id}` 返回完整 desired + observed（image/command/env 全有），这在 RCoder 无状态前提下**做不到**（RCoder 读时不知道 env/image 历史值，只能从 ConfigMap/Deployment spec 反推，但反推不等于业务 desired）。v2 诚实划分：`AppRuntimeInfo` 只给 observed；如果 Java 需要合并视图，由 Java 自己用持有的 desired + 调 RCoder 拿 observed 合并。

---

## 7. 双后端实现（D9）

### 7.1 `ContainerRuntime` trait（已有，v2 明确语义）

trait 定义位于 `crates/container-runtime-api/src/runtime_trait.rs`。v2 要求每个后端实现以下能力（**命令式**，对应路线 A）：

```rust
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    // 生命周期
    async fn create_deployment(&self, app_id: &str, spec: &AppSpec) -> Result<DeploymentStatus>;
    async fn patch_deployment(&self, app_id: &str, spec: &AppSpecPatch) -> Result<()>;
    async fn delete_deployment(&self, app_id: &str) -> Result<()>;
    async fn scale(&self, app_id: &str, replicas: i32) -> Result<()>;
    async fn restart(&self, app_id: &str) -> Result<()>;

    // 观测
    async fn get_deployment_status(&self, app_id: &str) -> Result<Option<DeploymentStatus>>;
    async fn list_deployments(&self, filter: &ListFilter) -> Result<Vec<DeploymentStatus>>; // 对账用
    async fn get_app_logs(&self, app_id: &str, params: &LogParams) -> Result<Vec<ContainerLogEntry>>;

    // GC 兜底
    async fn delete_app_resources(&self, app_id: &str) -> Result<()>; // 有序 + orphan 扫描
}
```

### 7.2 K8s 后端资源映射

| 应用语义 | K8s 资源 | 标签 |
|---|---|---|
| 副本 + 镜像 + command + probes + resources | `Deployment` | `instance={app_id}, managed-by=rcoder-app-manager, app.kubernetes.io/name=user-app` |
| env（非敏感） | `ConfigMap` `{app_id}-config` | 同上 |
| secrets（敏感） | `Secret` `{app_id}-secret` | 同上 |
| 内部服务发现 + Pingora 后端 | `Service` (ClusterIP) `{app_id}-svc`（**两种暴露模式都建**：Pingora 转发目标 / 集群内互调） | 同上 |
| HTTP 端口对外（默认 Pingora） | **不建额外资源**——RCoder 内置 Pingora 转发到 `{app_id}-svc:{port}`，access 返回 path `/proxy/{port}`（Java 拼 RCoder 入口） | — |
| HTTP 端口对外（可选 gateway） | `HTTPRoute` `{app_id}-route`（path `/apps/{app_id}`，挂 `nuwax-gateway`），**仅 `http_expose=gateway` 时建** | 同上 |
| TCP 端口对外 | **初期不对外**（不建 NodePort）；仅 ClusterIP 供集群内访问 | — |
| 持久存储 | `PVC`（复用 workspace PVC 的 `apps/{app_id}` subPath，见 §10） | — |

**所有 K8s 写操作走 SSA**（operator-rs `apply_patch` 模式）：`field_manager="rcoder-app-manager/{resource-kind}"`，`force=true`。这让 create-or-update 自然合一，update 实现零成本。

### 7.3 Docker 后端资源映射

| 应用语义 | Docker 资源 |
|---|---|
| 副本（Docker 无副本概念） | 单容器，replicas>0 = 存在，=0 = stop 不删 |
| 镜像 + command + env + resources | 容器 create 参数 |
| 内部服务发现 | 容器名（同 Docker network 内 DNS） |
| HTTP 端口对外 | pingora 代理路由 `/proxy/{port}/{path}`（既有机制） |
| TCP 端口对外 | **初期不对外**（不做 host port 映射）；仅 Docker 网络内可达 |
| 持久存储 | bind-mount `app-workspace/{app_id}/` |

Docker 后端的 update：image/command/env/resources 变化 → 重建容器（Docker 不支持 in-place），但对外**保持容器名不变**，pingora 路由与 host port 尽量复用。

### 7.4 后端选择

启动时按 feature flag / 配置决定：
- `--features kubernetes`（生产） → `KubernetesRuntime`
- 默认 / `docker` feature（开发） → `DockerRuntime`

devspace 本地 K8s 测试也用 `KubernetesRuntime`。

---

## 8. 服务暴露策略

> **v2 修订（D10）**：HTTP 暴露**默认统一走 RCoder 内置 Pingora 代理**（K8s/Docker 两后端一致），不再默认依赖外部 gateway；**gateway（HTTPRoute）作为可选暴露路径**保留，由 `http_expose` 配置切换（默认 `pingora`）。TCP 初期不对外。

### 8.1 两种 HTTP 暴露模式

| 模式 | 触发条件 | K8s 资源 | `access.external.http` |
|---|---|---|---|
| **Pingora（默认）** | `http_expose=pingora` | ClusterIP Service（Pingora 转发到 `{app_id}-svc:{port}`） | `/proxy/{port}`（**path，Java 拼 RCoder 入口**） |
| **Gateway（可选）** | `http_expose=gateway` | ClusterIP Service + HTTPRoute（path `/apps/{app_id}`，挂 `nuwax-gateway`） | `/apps/{app_id}`（path，**Java 拼 gateway 域名**） |

Docker 后端两种模式都走 Pingora（`/proxy/{port}` → container_ip），无 gateway 概念。两种模式、两后端都建 ClusterIP Service `{app_id}-svc`：Pingora 模式它是转发目标，Gateway 模式它是 HTTPRoute 的 backendRef，集群内互调也都用它。

### 8.2 为什么默认 Pingora（统一两后端）

- **消除 K8s(gateway) / Docker(Pingora) 分裂**：此前 K8s 走外部 gateway、Docker 走 Pingora，两后端 access 形态不一致、Java 要分别处理。统一后两后端都"返 path + Java 拼 host"，完全对称。
- **RCoder 作唯一 HTTP 入口**：便于未来统一鉴权 / 限流 / 观测；access 只返 path（`/proxy/{port}`），host 由 Java 持有（它本就知道 RCoder 入口，否则没法访问）。
- **gateway 留作可选**：当需要 7 层路径路由（一个 gateway 域名承载多 app，靠 `/apps/{id}` 区分）或对接既有 Envoy/Cilium Gateway 体系时，切 `http_expose=gateway`。

代价（可接受）：RCoder 成 HTTP 流量单点（它挂则所有 app 不可达）；RCoder 自身需对外暴露 Pingora 监听端口（见 §8.3）。

### 8.3 RCoder 自身对外入口（部署侧）

Pingora 监听 `proxy_config.listen_port`（RCoder Pod 内，默认 8088）。客户端访问 RCoder 需暴露该端口：
- **K8s**：RCoder Service 用 NodePort / LoadBalancer 暴露 Pingora 端口，或前置 Ingress。
- **Docker**：Pingora 直接监听宿主机端口。

`access.external.http` **只返 path（`/proxy/{port}`），不含 host**——Java 既然通过 RCoder 访问应用，必然已持有 RCoder 入口（ip:port/域名），RCoder 不必替它拼（避免依赖 `RCODER_EXTERNAL_HOST` 配置、避免多环境域名耦合）。

### 8.4 TCP 端口（初期不对外）

TCP 服务（如容器内 PostgreSQL）**初期不支持对外**：
- K8s 不建 NodePort；Docker 不做 host port 映射。
- `access.external.tcp` 留空；`access.internal` 仍返回 ClusterIP FQDN + 端口，供**同集群内**互调（如业务 Pod 连 app 的 PG）。
- 未来如需对外，启用 NodePort 路径即可（`apply_app_nodeport` 已实现，加配置开关）。

### 8.5 AccessInfo 结构

```rust
pub struct AccessInfo {
    pub external: ExternalAccess {
        pub http: Option<String>,        // Pingora: path "/proxy/{port}"
                                         // Gateway: path "/apps/{app_id}"
        pub tcp: Vec<TcpPortMapping>,    // 初期为空（TCP 不对外）
    },
    pub internal: InternalAccess {
        pub domain: String,              // {prefix}-{app_id}-svc.{ns}.svc.cluster.local（两模式都有）
        pub short_domain: String,
        pub ports: Vec<InternalPort>,
    },
}
```

> ⚠️ **无状态下的已知限制**：Pingora backend 注册表（`pingora_ports`）是内存态，rcoder 重启后丢失。重启后**读路径**（get/query/runtime）的 `external.http` 会变 `null`；**create 响应不受影响**（用请求端口生成）。Java 应以 create 响应地址为准或自行缓存。两后端同理（除非后续把端口映射持久化）。

---

## 9. 生命周期与对账

### 9.1 状态机

```
                ┌──────────────────────────────────┐
                ▼                                   │
POST /apps → Starting ──ready≥1──→ Running ──scale0──→ Stopped
                │                     │                   │
                │                  rollout              scale1
                │                     ↓                   │
                │                  Starting              │
                │                                         │
              fail(crash/image)                          │
                ↓                                         │
              Error ───POST /update(修 image/env)─────┘
                │
          POST /delete
                ↓
             Deleting ──GC完成──→ (404)
```

### 9.2 重启恢复（无状态核心）

RCoder 进程重启后：
1. **不需要加载任何本地状态**（没有）。
2. Java 调 `GET /apps/runtime` → RCoder label 反查所有 `managed-by=rcoder-app-manager` 的 Deployment，对每个返回 `AppRuntimeInfo`。
3. Java 对照自己的 DB，发现"DB 有但集群没有"→ 重新 create；"集群有但 DB 没有"→ 调 `POST /apps/{id}/delete` 或保留（Java 策略）。

这条链路 **不需要任何额外存储**，完全靠 label + 集群 API。这是路线 A 的核心优势。

---

## 10. 文件管理契约

### 10.1 目录约定（app 根相对）

```
{app 根}/                      # K8s: PVC 的 apps/{app_id} subPath 挂到 /app；Docker: app-workspace/{app_id}/
├── code/                      # 应用代码（启动前上传，运行时只读）
├── data/                      # 应用数据（读写）
└── logs/                      # 应用日志（容器 stdout 之外的文件日志，读写）
```

### 10.2 路径契约（v1 Bug3 已确立，v2 正式化）

- `POST /apps/{id}/upload` 的 `target` 字段、`GET /apps/{id}/files?path=`、`POST /apps/{id}/files/delete`（body）的 `path` 字段：**一律 app 根相对**（如 `code/app.jar`、`data/db/`）。
- 实现层做 **path traversal 防护**：`canonicalize(target).starts_with(app_root)`，越界 → 400。
- 不带前缀时默认指向 app 根。

### 10.3 与容器视图的关系

| 后端 | app 根（RCoder 视角） | app 根（容器视角） |
|---|---|---|
| K8s | `/app/project_workspace/apps/{app_id}`（复用 rcoder workspace PVC subPath） | `/app` |
| Docker | `{RCODER_WORKSPACE_ROOT}/{app_id}` | `/app`（bind-mount） |

### 10.4 upload 压缩包自动解压（v2 新增）

`POST /apps/{id}/upload` 支持单文件 + 压缩包，**魔数判断**（`download_utils::detect_file_type`，不靠文件名后缀，`app.jar.zip` 也能识别为 zip）：

| 上传内容 | 判断 | target 语义 | 行为 |
|---|---|---|---|
| **zip / tar.gz** | 魔数（`PK\x03\x04` / `\x1f\x8b`） | **解压目录**（如 `code/`） | 解压到 `target/` 下，保留压缩包内目录结构；可选 `flatten=true` 剥单层 wrapper 目录 |
| **其它**（单文件：jar/二进制/脚本） | unknown | **文件路径**（如 `code/app.jar`） | 存为 `target` |

**响应** `UploadResult` 新增 `extracted_count: Option<usize>`（压缩包解压文件数；单文件为 None / 省略）。

**安全防护**（复用 `download_utils`，不引新库）：
- **zip slip**：`sanitize_entry_path` 拒压缩包内 `..`/绝对路径/NUL + `ensure_within` canonicalize 校验每条目在 dest 内。
- **压缩炸弹**：`MAX_EXTRACTED_SIZE = 1GiB`（解压总字节超限 → `ERR_VALIDATION`）。
- **符号链接**：tar.gz 跳过（防 symlink 逃逸）。
- **target path traversal**：`validate_upload_target` 在 `create_dir_all` **前**拒 target 含 `..`/绝对路径/空（避免副作用在工作空间外创建目录）；叠加 §10.2 的 canonicalize + starts_with。
- **body limit**：upload 路由单独挂 1GiB `DefaultBodyLimit`（覆盖全局 50MB）。
- **解压**：`spawn_blocking`（同步 IO 不阻塞 tokio）；临时文件 `tempfile::NamedTempFile`（解压后自动删）。

**错误映射**：解压失败（zip slip / 超 1GiB / 无效压缩包 / IO）→ `ArchiveError` → `ERR_VALIDATION`（400），经 `map_archive_error`。

**不自动 restart**：upload/解压后不重启 app（与现有一致），Java 按需调 `POST /apps/{id}/restart`。

---

## 11. 日志流（D8，WebSocket）

- **快照**：`GET /apps/{id}/logs?tail=1000&timestamps=true&since=...` → `HttpResult<Vec<LogEntry>>`
- **流式**：`GET /apps/{id}/logs/stream?tail=1000&follow=true` → **WebSocket**（WS upgrade）
  - 服务端 → 客户端：每条日志一个 `Message::Text`（`LogEntry` 的 JSON）
  - 客户端 → 服务端：可发控制帧（如 `{"cmd":"stop"}`）或直接关连接 → 服务端 abort follow

**为什么 WS 不是 SSE**：双向——Java 可主动停止 follow / 切换 tail；与 Docker、Kubernetes 自身的日志流（SPDY/WS）一致；Epinio 也用 WS。

实现：K8s 用 kube-rs `Api::log_stream`；Docker 用 bollard `logs(follow=true)`。**容器 stdout/stderr 是唯一日志来源**（v1 已修正从文件读取的错误）。rcoder 当前无 WS 终结能力（终端 ws 是 pingora 反代到 ttyd），需给 axum 加 `ws` feature + 新建 `stream_app_logs` handler（`axum::extract::ws::WebSocketUpgrade`）。

---

## 12. 错误模型（D6）

### 12.1 响应体

沿用现有 `HttpResult`（**不改 shared_types**）：

```json
{
  "success": false,
  "data": null,
  "code": "ERR_APP_NOT_FOUND",      // 业务错误码（机器可读，ERR_ 前缀常量）
  "message": "应用不存在: app-123",  // 人读（service 层透传的具体信息）
  "tid": "abc-123"
}
```

> `retryable` 不在响应体里（HttpResult 无 details 字段）；它是错误码的固有属性，见 §12.3。

### 12.2 错误码与 HTTP 状态码

| code | HTTP | 场景 |
|---|---|---|
| `ERR_VALIDATION` | 400 | 参数缺失/非法（如 path traversal、port 重复、资源 Quantity 非法） |
| `ERR_OPERATION_NOT_SUPPORTED` | 400 | K8s 后端尝试改不可变字段（如 name） |
| `ERR_APP_NOT_FOUND` | 404 | 应用不存在（Deployment 404） |
| `ERR_FILE_NOT_FOUND` | 404 | 文件管理目标不存在 |
| `ERR_APP_ALREADY_EXISTS` | 409 | 创建时 Deployment 已存在 |
| `ERR_INVALID_STATE` | 409 | 状态不允许操作（如 Deleting 中又 start；对运行中应用 storage/delete） |
| `ERR_BACKEND_ERROR` | 500 | K8s/Docker API 调用失败（透传 source） |
| `ERR_IMAGE_PULL_FAILED` | 502 | 镜像拉取失败（ImagePullBackOff） |
| `ERR_RESOURCE_EXHAUSTED` | 503 | 集群资源不足（调度失败） |

> 常量定义在 `shared_types_i18n::error_codes`，HTTP 映射在 `shared_types::AppError::status_from_code`，retryable 在 `is_retryable_code`。

### 12.3 错误分类（D6 + operator-rs 的薄弱点补强）

operator-rs 没做好的"retryable vs terminal"分类，v2 用 `is_retryable_code(code)` 明确：

- **可重试**（Java 可指数退避重发）：`ERR_BACKEND_ERROR`(瞬时 5xx/timeout) / `ERR_RESOURCE_EXHAUSTED` / `ERR_IMAGE_PULL_FAILED`(临时 registry 故障)
- **终态**（重发无用，需修改请求）：`ERR_VALIDATION` / `ERR_OPERATION_NOT_SUPPORTED` / `ERR_APP_NOT_FOUND` / `ERR_APP_ALREADY_EXISTS` / `ERR_INVALID_STATE` / `ERR_FILE_NOT_FOUND`

`retryable` 是错误码的固有属性，**不在响应体重复**（HttpResult 不改）；Java 按 `code` 调 `is_retryable_code` 或本地查表决策。

---

## 13. 多租户与隔离

### 13.1 字段

`CreateAppRequest` 带 `tenant_id` / `space_id`（可选）。资源命名与标签：
- app_id 全局唯一（不带租户前缀，避免命名膨胀）
- 标签 `rcoder.io/tenant={tenant_id}, rcoder.io/space={space_id}`（与现有 `rcoder.io/app-id` 等前缀统一）
- K8s namespace：可按租户分（`rcoder-apps-{tenant}`）或统一 `rcoder-apps`（按 label 隔离）。**v2 默认统一 namespace + label 隔离**（运维简单）；按租户分 namespace 留作配置项。

### 13.2 镜像与命名约束

- `image` **当前阶段允许 `latest`**（无 tag 时 K8s/Docker 默认补 `:latest`）。原因：现阶段尚未具备按版本号发布应用镜像的完整运维能力（无 CI 产出 `:semver` / `@sha256`、无版本化发布流程），所有发布的应用镜像统一用 `latest` tag，强制 tag/digest 会阻塞全部调用方。代码不做 `latest` 校验（曾短暂加入后移除，见 commit 2378e22）。
  - nya 的 `latest` 教训（无法回滚、节点镜像缓存不一致、缓存污染）是真实的运维风险，**留作未来收紧的依据**。
  - **演进触发**：当具备版本化发布能力（CI 产出 `:semver` / `@sha256`、调用方按版本发布）后，在此重新启用"禁止 latest / 必须带 tag"校验，并迁移现有 `latest` 引用。
- `app_id` / `name`：DNS-1123 label 合规（`[a-z0-9]([-a-z0-9]*[a-z0-9])?`），Epinio 也这么校验。

---

## 14. 从 v1 迁移（开发阶段，无外部兼容包袱）

| v1 代码位置 | v2 改动 |
|---|---|
| `handlers.rs` | 端点路径保持 v1（GET+POST、写操作动词进路径，§4.1）；日志拆 `logs`+`logs/stream`；`map_app_error` 扩展错误码分类（§12） |
| `service.rs::update_app` | 从 Fail-Fast 改为 SSA apply（§5.2） |
| `service.rs::delete_app` | 加 orphan 扫描；**默认保留持久存储**，支持 `purge=true` 一键全删（§5.3） |
| `handlers.rs` / `service.rs`（新增） | 新增存储管理端点：`GET /apps/{id}/storage`、`POST /apps/{id}/storage/delete`、`POST /apps/storage/query`（§5.4） |
| `models.rs::AppRuntimeInfo` | 加 `conditions` 字段（§6.2） |
| `models.rs::UpdateAppRequest` | 加 `ports` / `health_check` 字段（§6.1） |
| `models.rs`（新增） | `DeleteAppRequest { purge: Option<bool> }`、`QueryStorageRequest { page: u32, page_size: u32 /* 必填, ≤100 */, filters }` + `StorageFilters { orphan_only, app_ids, tenant_id, space_id }`、`StorageInfo { app_id, exists, path, modified_at, is_orphan }`（**不含 size_bytes**——CephFS 上不能用 du，见 §5.4） |
| `runtime_trait.rs` | 加 `patch_deployment` / `delete_app_resources` / `get_app_storage` / `delete_app_storage` / `list_orphan_storage`（§7.1） |
| K8s 后端写操作 | 全面改用 SSA `apply_patch`（替换现在的 `create` + `patch` 分支）；delete 不再删 PVC subPath 目录 |

开发阶段、未对外提供接口，故可直接改契约，无需版本共存。

---

## 15. 未来演进（路线 B 触发条件）

当且仅当出现下列需求，再评估切到 CRD + controller（§3.1 路线 B）：

1. **需要应用版本/回滚**：引入 `Revision`（Knative 模型），每次更新产生不可变版本，支持 `rollback`。
2. **需要持续对账**：Java 下发期望，RCoder 持续修正集群漂移（此时 controller 的价值显著）。
3. **需要 status 长期持久化在集群**：供 prometheus / 其它 K8s 工具消费 App CR status。
4. **需要多副本 + 复杂滚动策略**（蓝绿/金丝雀）：CRD + controller 配合 Flagger 等。

切路线 B 时，**REST 契约（§4）不变**——CRD 是 RCoder 内部实现细节，handler 内部从"直接 CRUD 子资源"换成"写 App CR spec + 等 reconcile + 读 CR status"。这是 v2 设计 API 时刻意保留的迁移余地。

---

## 16. 决策状态

**✅ 已确认**：

- **路线 A（不引入 CRD）** —— v2 保持命令式无状态引擎。
- **HTTP 方法仅 GET + POST**，写操作动词进路径（§3.2 D2）。
- **删除默认保留持久存储**（§5.3），数据安全优先；新增 `purge` 一键全删 + 独立存储管理接口（§5.4）。
- **日志流用 WebSocket**（§11，非 SSE）—— 双向，Java 可发控制帧。
- **创建异步化**（D7，建完资源立即返回 Starting，不等 ready）。
- **错误模型不改 `HttpResult`**；`retryable` 作为错误码固有属性（`is_retryable_code`），不在响应体重复（§12）。
- **HTTP 暴露默认走 RCoder Pingora 代理**（K8s/Docker 两后端统一），gateway（HTTPRoute）作为可选暴露路径（`http_expose` 切换）；TCP 初期不对外（D10 / §8）。

**仍待确认**（以下是我做了判断但希望和你确认的点。如有异议，现在调整成本最低）：

1. **`GET /apps/{id}` 只返回 observed（`AppRuntimeInfo`）**，desired 合并视图交给 Java —— 与 v1 文档不同（v1 期望 GET 返回完整 desired+observed）。
2. **K8s namespace 策略**：默认统一 `rcoder-apps` + label 隔离，不按租户分 namespace。
3. ~~**`image` 禁止 `latest`**~~ —— **已决策：当前阶段允许 `latest`**（运维未具备版本化发布能力，应用镜像统一用 latest；详见 §13.2）。未来具备版本发布能力后再收紧为强约束。

---

## 附：调研索引

- Epinio：`/Users/soddy/Documents/git-workspace/epinio`（关键文件：`pkg/api/core/v1/models/app.go`、`internal/application/application.go::fetch`、`pkg/api/core/v1/errors/errors.go`、`internal/api/v1/router.go`）
- operator-rs：`/Users/soddy/Documents/git-workspace/operator-rs`（关键文件：`crates/stackable-operator/src/client/mod.rs::apply_patch`、`.../cluster_resources.rs::delete_orphaned_resources`、`.../status/condition/mod.rs::compute_conditions`）
- nya：`/Users/soddy/Documents/git-workspace/nya`（参考：URL 命名约定、Helm 模板化、event-bus 编排；反面教材：无对账 / 无删除）
- kubert：`/Users/soddy/Documents/git-workspace/kubert`（**不采纳**；控制器专用，与 axum 冲突）
