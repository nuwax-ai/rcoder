# 应用管理服务 · 接口手册

> 面向 **Java 业务服务**（RCoder 的调用方）的实操手册。
> 目标：让你在 10 分钟内理解这套接口怎么用、怎么和 RCoder 配合、哪里有坑。

RCoder 对外提供一套 REST API，让你把一个多语言容器镜像（Java / Python / TypeScript / Go / Rust / 前端）连同启动命令、环境变量、端口、健康检查、资源限制，部署成可被访问的长期运行服务。

---

## 5 分钟速览

1. **RCoder 是无状态的应用 Pod 引擎**——它不持久化任何应用元数据；进程重启后能从集群重新发现正在跑的应用，你无需做任何恢复。
2. **desired 归 Java，observed 归 RCoder**——应用的业务字段（name/image/env 等）存在你的 DB；RCoder 只在 create/update 时按你传的执行，读时实时查集群返回运行状态。`GET /apps/{id}` 只返回 observed，**不含业务字段**；合并视图你自己拼。
3. **HTTP 只用 GET + POST**（部署网关限制）。写操作把动词放进路径：`/delete`、`/update`、`/files/delete`、`/storage/delete`。
4. **创建是异步的**——`POST /apps` 建完资源立即返回 `Starting`，不等 Ready；你轮询 `GET /apps/{id}` 观察 `Starting → Running`。
5. **删除默认保留数据**——`delete` 只删计算面，保留 code/data/logs；要连数据清传 `purge:true`。数据不可再生，这是刻意的安全设计。
6. **判断错误看 `code` 字段**，别只看 HTTP 状态码。当前只有 `ERR_BACKEND_ERROR`(500) 可重试，其余终态。详见 [03-错误处理与重试](./03-错误处理与重试.md)。

---

## 阅读导航

| 章节 | 适合 | 内容 |
|---|---|---|
| [01-定位与核心概念](./01-定位与核心概念.md) | **先读** | 无状态引擎定位、desired/observed 二分、双后端、Java/RCoder 职责分工、URL 拼接规则 |
| [02-接口手册](./02-接口手册.md) | 查接口时读 | 全部 21 个 REST 接口逐个详解（方法/路径/请求体/响应/字段/curl 示例/要点） |
| [03-错误处理与重试](./03-错误处理与重试.md) | 写客户端时读 | 错误码→HTTP 映射、retryable 分类、Java 重试决策流程、常见错误速查 |
| [04-设计考虑](./04-设计考虑.md) | 想了解 why 时读 | 每个关键决策的动机（异步/全量替换/保留数据/状态机/对账/路径契约/WS/CephFS） |
| [05-典型场景](./05-典型场景.md) | 上手时读 | 8 个端到端剧本（部署/更新/启停/删除找回/清孤儿/对账/日志/部署物），含可复制 curl |
| [06-快速开始-发布与访问](./06-快速开始-发布与访问.md) | **给同事看** | 应用发布（build→upload→create→轮询→update）+ 服务访问（access→Pingora→访问），含流程图 + FAQ |
| [07-前端项目部署](./07-前端项目部署.md) | 部署前端时读 | React/Vue + Vite 模板部署实测（vite `--host`、install、dev/preview 模式）+ Pingora 访问 + HMR，含可复制 curl + FAQ |
| [08-带数据库的应用部署](./08-带数据库的应用部署.md) | 需要数据库时读 | 单容器自带 PostgreSQL+pgweb+ttyd（app-runtime 镜像）+ Pingora 访问 pgweb/ttyd + PG 持久化，含实测 curl + FAQ |
| [09-实测问题记录](./09-实测问题记录.md) | 排查/上线前读 | 真实集群实测确认的缺陷：① update 漏 ports 丢 Pingora 注册 ② storage/query 查不到孤儿存储（含根因/复现/规避/修复建议） |

> **建议顺序**：01 → 02（快速浏览）→ 05（跑一遍场景）→ 遇到错误查 03 → 想懂 why 查 04。

---

## ⚠️ 已知限制（实现现状 vs 设计文档）

> 这些是当前代码与 v2 设计文档的差异，**务必知悉**，避免按设计文档实现而踩空。

| 项 | 设计文档 | 当前实现现状 | 影响 / 应对 |
|---|---|---|---|
| **镜像拉取/资源不足错误码** | `ERR_IMAGE_PULL_FAILED`(502)、`ERR_RESOURCE_EXHAUSTED`(503) | **未落地**，两者都归 `ERR_BACKEND_ERROR`(500) | 收不到 502/503；区分镜像问题要看 `GET /apps/{id}` 的 `conditions[].reason`（见 [03 §3.2](./03-错误处理与重试.md)） |
| **logs follow** | `follow=true` 流式 | **未实现**（runtime 返回快照） | 实时流用 `GET /apps/{id}/logs/stream`（WebSocket）；`since` 参数也暂未透传 |
| **stats CPU/内存** | 返回资源使用 | **返回默认空值** | K8s 需装 metrics-server 才有真实数据，当前仅 `restart_count` 可靠 |
| **Exec 健康检查** | 支持 | **不支持**（运行时缺 command 字段） | 传 `Exec` 返回 `400 ERR_VALIDATION`，请用 `Http`/`Tcp` |
| **query 的 name/created_at 过滤** | 支持 | **忽略**（无状态无此字段） | 这两个维度过滤请 Java 用本地 DB 二次过滤 |
| **storage/query 的 tenant_id/space_id** | 支持 | **忽略**（无 app→租户映射） | 用 `app_ids` / `orphan_only` 收窄 |
| **Docker 模式 TCP host_port** | 返回实际端口 | create 后立即查可能**留空** | K8s 模式无此问题；开发环境定位的已知限制 |
| **HTTP 访问地址（Pingora，两后端）** | 读路径稳定返回 path | rcoder 重启后**读路径的 `external.http` 变 `null`**（HTTP 端口靠内存 `pingora_ports` 补全，重启即丢；**create 响应不受影响**） | Java 别把读路径的 `external.http` 当持久值，以 create 响应为准或自行缓存 |

---

## 接口总览（24 个）

```
生命周期   POST   /api/v1/apps                       创建（异步）
           POST   /api/v1/apps/query                 查询列表（body 过滤/分页）
           GET    /api/v1/apps/runtime               对账：列全部托管应用
           GET    /api/v1/apps/{app_id}              运行时详情（observed）
           POST   /api/v1/apps/{app_id}/update       更新（全量替换）
           POST   /api/v1/apps/{app_id}/delete       删除（默认保留数据）

操作       POST   /api/v1/apps/{app_id}/start        启动（scale=1）
           POST   /api/v1/apps/{app_id}/stop         停止（scale=0）
           POST   /api/v1/apps/{app_id}/restart      重启（rollout）

查询       GET    /api/v1/apps/{app_id}/logs         日志快照
           GET    /api/v1/apps/{app_id}/logs/file    文件日志
           GET    /api/v1/apps/{app_id}/logs/stream  日志 WebSocket 流
           GET    /api/v1/apps/{app_id}/health       健康状态
           GET    /api/v1/apps/{app_id}/stats        资源使用（best-effort）
           GET    /api/v1/apps/{app_id}/events       应用事件

文件管理   POST   /api/v1/apps/{app_id}/upload       上传文件（multipart）
           GET    /api/v1/apps/{app_id}/files        列出文件
           POST   /api/v1/apps/{app_id}/files/delete 删除文件

持久存储   GET    /api/v1/apps/{app_id}/storage          查询存储状态
           POST   /api/v1/apps/{app_id}/storage/clear    清空内容（留 PVC，仅已 delete 的 app）
           POST   /api/v1/apps/{app_id}/storage/destroy  销毁 PVC（高危·不可逆·释放配额）
           POST   /api/v1/apps/storage/query             分页查询存储（强制分页）

数据库     POST   /api/v1/apps/{app_id}/db/reset-password   重置 PG 密码（仅 app-runtime 镜像）
           POST   /api/v1/apps/{app_id}/db/create-database  新建 PG 库
```

所有响应统一包 `HttpResult<T> = { success, data, code, message, tid }`。逐个字段含义见 [02-接口手册](./02-接口手册.md)。

---

## 相关资源

- **v2 设计文档**（完整决策依据、对标调研）：[../application-management-service-v2-design.md](../application-management-service-v2-design.md)
- **核心代码**：[`crates/rcoder/src/app_manager/`](../../crates/rcoder/src/app_manager/)
  - `models.rs` — 数据模型（请求/响应/错误）
  - `routes.rs` — 路由定义
  - `handlers.rs` — HTTP 处理器（含 utoipa OpenAPI 注解）
  - `service.rs` — 核心服务实现（双后端统一）
  - `config.rs` — 配置（`AppAccessMode`、workspace 等）
- **错误码定义**：`crates/shared_types_i18n/src/error_codes.rs`（常量）、`crates/shared_types/src/model/app_error.rs`（HTTP 映射）
- **运行时抽象**：`crates/container-runtime-api/src/runtime_trait.rs`（`ContainerRuntime` trait，K8s / Docker 双实现）
- **OpenAPI 文档**：代码用 utoipa 注解，运行时通过 Swagger UI 暴露——`/api/docs/openapi.json`（机器可读，可直接生成 Java 客户端），`/api/docs`（交互式 Swagger UI）。

---

## 一句话总结

> RCoder 是无状态的应用 Pod 引擎：**你持有 desired，RCoder 现场观测 observed；你发命令、它执行；你查状态、它读集群。** 把握这条主线，其余细节都在各章节里。
