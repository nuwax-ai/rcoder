# 应用管理服务 · 接口手册

> 面向 **Java 业务服务 / 前端同事**（RCoder 的调用方）的实操手册。
> 目标：让你在 10 分钟内理解这套接口怎么用、怎么和 RCoder 配合、哪里有坑。

RCoder 对外提供一套 REST API，让你把一个多语言容器镜像（Java / Python / TypeScript / Go / Rust / 前端）连同启动命令、环境变量、端口、健康检查、资源限制，部署成可访问的长期运行服务；并提供建构建发布体系（源码 → 制品 → 灰度激活 → 版本回滚）。

---

## 5 分钟速览

1. **RCoder 是应用 Pod 引擎**——desired（业务字段）归你，observed（运行状态）归 RCoder；create/update 时按你传的执行，读时实时查集群返回。
2. **HTTP 只用 GET + POST**（部署网关限制）。写操作把动词放进路径：`/delete`、`/update`、`/files/delete`、`/storage/delete`。
3. **创建是异步的**——`POST /apps` 建完资源立即返回 `Starting`，不等 Ready；你轮询 `GET /apps/{id}` 观察 `Starting → Running`。
4. **删除默认保留数据**——`delete` 只删计算面，保留 code/data/logs；要连数据清传 `purge:true`。
5. **发布有三阶段**——build（构建）→ prepare（预备入库）→ activate（切流+ensure 容器+等就绪，单接口收敛到 active/failed，失败**保留现场**）。失败后恢复用 rollback。见 [10-发布与版本管理](./10-发布与版本管理.md)。
6. **判断错误看 `code` 字段**，别只看 HTTP 状态码。详见 [03-错误处理与重试](./03-错误处理与重试.md)。

---

## 阅读导航

| 章节 | 适合 | 内容 |
|---|---|---|
| [01-定位与核心概念](./01-定位与核心概念.md) | **先读** | 引擎定位、desired/observed 二分、双后端、Java/RCoder 职责分工、URL 拼接规则 |
| [02-接口手册](./02-接口手册.md) | 查接口时读 | 核心生命周期/日志/文件/存储接口逐个详解（方法/路径/请求体/响应/curl） |
| [03-错误处理与重试](./03-错误处理与重试.md) | 写客户端时读 | 错误码→HTTP 映射、retryable 分类、Java 重试决策流程 |
| [04-设计考虑](./04-设计考虑.md) | 想了解 why 时读 | 每个关键决策的动机 |
| [05-典型场景](./05-典型场景.md) | 上手时读 | 8 个端到端剧本，含可复制 curl |
| [06-快速开始-发布与访问](./06-快速开始-发布与访问.md) | **给同事看** | 应用发布 + 服务访问，含流程图 + FAQ |
| [07-前端项目部署](./07-前端项目部署.md) | 部署前端时读 | React/Vue + Vite 模板部署实测 |
| [08-带数据库的应用部署](./08-带数据库的应用部署.md) | 需要数据库时读 | app-runtime 镜像内置 PG + pgweb + ttyd |
| **[09-操作与运维接口](./09-操作与运维接口.md)** | **新增** | 启停/重启/回收策略/URL上传/存储销毁/数据库管理 |
| **[10-发布与版本管理](./10-发布与版本管理.md)** | **新增** | Java 六步发布编排（build→取包→中转→start+url 部署）+ 失败保留现场 + rollback + 状态机 |
| **[11-任务与SSE事件](./11-任务与SSE事件.md)** | **新增** | 构建发布任务查询 + SSE 实时进度事件格式 + 断线重连 |
| **[12-开发模式与文件接口](./12-开发模式与文件接口.md)** | **新增** | per-app 开发容器（RWO 块卷）+ `/api/userapp` 文件接口族 + AI 开发对话 + PG 凭据对齐 + 反代分流下的 Java 对接 |
| **[13-反向代理对接指引](./13-反向代理对接指引.md)** | **新增** | 60000 反代分流规则 + X-Service-Type/X-App-Id header 约定 + TS↔userApp 路径对照表 + 开发域终端/桌面代理（/userapp/*）+ 端口表（给实现代理的同事） |

> **建议顺序**：01 → 02（快速浏览）→ 05（跑一遍场景）→ 06（发布流程）→ 遇到错误查 03 → 发布细节查 10/11。

---

## 对接指南（给要写客户端的同事）

### 1. 拿到 Swagger 文档

```
# 交互式 Swagger UI（浏览器打开，可直接试调）
http://<rcoder-host>:<port>/api/docs

# OpenAPI JSON（可导入 Apifox / Postman / 生成 Java 客户端）
http://<rcoder-host>:<port>/api/docs/openapi.json
```

> Swagger 按功能分组（tag）：**应用管理**（生命周期/操作/日志/文件/存储/数据库）、**应用发布**（start+url 部署，releases 接口已删除——见第 10 章）、**UserApp 发布**（build/publish 任务）。

### 2. 最小对接路径

```
Day 1：读 01（概念）→ 用 Swagger UI 试调 POST /apps + GET /apps/{id}
Day 2：读 05（场景）→ 跑通部署→访问链路
Day 3：读 10（发布）→ 跑通 build→prepare→activate 链路
```

### 3. 常用地址

| 环境 | 地址 |
|---|---|
| Docker Compose 本地 | `http://127.0.0.1:8090` |
| K8s devspace 本地 | `http://127.0.0.1:8290` |
| K8s 测试集群 | `http://<node-ip>:30295`（NodePort） |
| Swagger UI | `<上述任意地址>/api/docs` |

### 4. 必须理解的 3 个语义

1. **异步创建**：POST /apps 返回 Starting，你轮询 GET /apps/{id} 等到 Running。
2. **发布单接口收敛**：activate 同步"切流+等就绪"——status=active 即成功；status=failed（200 返回）即就绪失败且**现场保留**（code/快照/制品包不动，供排查），放弃新版调 rollback 一键恢复。注意等待期同步阻塞，HTTP 读超时要 ≥ readinessTimeoutSeconds。
3. **错误看 code**：HTTP 状态码只分大类，具体原因看 response body 的 `code` + `message` 字段。

---

## 接口总览（37 个）

```
生命周期   POST   /api/v1/apps                                创建（异步）
           POST   /api/v1/apps/query                          查询列表（body 过滤/分页）
           GET    /api/v1/apps/runtime                        对账：列全部托管应用
           GET    /api/v1/apps/{app_id}                       运行时详情（observed）
           POST   /api/v1/apps/{app_id}/update                更新（全量替换）
           POST   /api/v1/apps/{app_id}/delete                删除（默认保留数据）

操作       POST   /api/v1/apps/{app_id}/start                 启动（scale=1）
           POST   /api/v1/apps/{app_id}/stop                  停止（scale=0）
           POST   /api/v1/apps/{app_id}/restart               重启（rollout）
           POST   /api/v1/apps/{app_id}/recycle-policy        设置闲置回收策略（免重启）

日志       POST   /api/v1/apps/{app_id}/logs/sources/query    查询声明的日志源
           POST   /api/v1/apps/{app_id}/logs/query            多服务文件日志快照
           POST   /api/v1/apps/{app_id}/logs/stream           SSE 实时流
           GET    /api/v1/apps/{app_id}/health                健康状态
           GET    /api/v1/apps/{app_id}/stats                 资源使用（best-effort）
           GET    /api/v1/apps/{app_id}/events                应用事件

文件管理   POST   /api/v1/apps/{app_id}/upload                上传文件（multipart）
           POST   /api/v1/apps/{app_id}/upload-from-url       从 URL 下载并上传
           GET    /api/v1/apps/{app_id}/files                 列出文件
           POST   /api/v1/apps/{app_id}/files/delete          删除文件

持久存储   GET    /api/v1/apps/{app_id}/storage               查询存储状态
           POST   /api/v1/apps/{app_id}/storage/clear         清空内容（留 PVC）
           POST   /api/v1/apps/{app_id}/storage/destroy      销毁 PVC（高危·不可逆）
           POST   /api/v1/apps/storage/query                  分页查询存储

数据库     POST   /api/v1/apps/{app_id}/db/reset-password     重置 PG 密码
           POST   /api/v1/apps/{app_id}/db/create-database    新建 PG 库


构建发布   POST   /api/v1/apps/{app_id}/build                        触发源码构建（自动建 Builder）
           POST   /api/v1/apps/{app_id}/publish                      完整发布（一步）
           POST   /api/v1/apps/publish/tasks/query                   任务列表分页查询
           GET    /api/v1/apps/publish/tasks/{task_id}               任务状态快照
           GET    /api/v1/apps/publish/tasks/{task_id}/stream        SSE 实时进度
           POST   /api/v1/apps/publish/tasks/{task_id}/cancel        取消任务

代理访问   GET    /proxy/apps/{user_id}/{app_id}/{port}/{path}                 访问部署的应用
```

所有响应统一包 `HttpResult<T> = { success, data, code, message, tid }`（全部接口一致）。逐个字段含义见各章节。

---

## ⚠️ 已知限制（实现现状 vs 设计文档）

| 项 | 设计文档 | 当前实现现状 | 影响 / 应对 |
|---|---|---|---|
| **镜像拉取/资源不足错误码** | 502/503 | 归 `ERR_BACKEND_ERROR`(500) | 看 `GET /apps/{id}` 的 `conditions[].reason` |
| **logs follow** | `follow=true` 流式 | 未实现（快照） | 实时流用 `/logs/stream`（SSE） |
| **stats CPU/内存** | 返回资源使用 | 返回默认空值 | 仅 `restart_count` 可靠 |
| **Exec 健康检查** | 支持 | 不支持 | 传 `Exec` 返回 400，用 `Http`/`Tcp` |
| **query 的 name/created_at 过滤** | 支持 | K8s+PG 模式生效（rcoder 侧 `userapp_metadata` 表存业务元数据）；Docker Compose 模式忽略（Java 本地二次过滤） |
| **storage/query 的 tenant_id/space_id** | 支持 | 忽略 | 用 `app_ids` / `orphan_only` |
| **HTTP 访问地址** | 读路径稳定返回 | rcoder 重启后读路径 `external.http` 可能 `null` | 以 create 响应为准或自行缓存 |

---

## 相关资源

- **Swagger UI**（交互式）：`/api/docs`
- **OpenAPI JSON**（客户端生成）：`/api/docs/openapi.json`
- **v2 设计文档**：[../application-management-service-v2-design.md](../application-management-service-v2-design.md)
- **核心代码**：`crates/app_manager/`（应用管理）+ `crates/rcoder/src/userapp_publish/`（构建发布）

---

## 一句话总结

> RCoder 是应用 Pod 引擎：**你持有 desired，RCoder 现场观测 observed；你发命令、它执行；你查状态、它读集群。** 发布体系则是：**build 出制品 → prepare 入库 → activate 切流+等就绪定生死；失败留现场，rollback 显式恢复。**
