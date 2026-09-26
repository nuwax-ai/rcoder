# UserApp 应用管理

UserApp 是 RCoder 面向用户的应用托管面：把"AI 帮你写出来的应用"直接变成"能访问的服务"。一个 UserApp（`app_id`）就是一个用户应用的完整托管单元，覆盖从源码构建、版本化发布到运行、回收、删除的全生命周期。

## 双环境模型

每个 app 具备两个独立环境，操作面互相隔离：

| 环境 | 运行形态 | 用途 |
|------|---------|------|
| **dev**（开发环境） | UserappBuilder 容器（K8s 下为 STS，非常驻，空闲自动回收） | 应用构建、开发模式运行、文件接口 |
| **prod**（运行环境） | 独立运行容器（K8s 下为 Deployment + per-app PVC） | 对外提供服务的正式实例 |

dev 与 prod 各有独立的存储：删除 prod 运行容器默认保留数据，dev 环境空闲回收时其 PVC 也保留复用。

## 生命周期

```
创建（url 部署自动创建）
  → 构建（dev 环境，app-cli 容器内编排）
  → 发布（构建产物版本化，部署到 prod）
  → 运行（对外服务，经代理访问）
  → 停止 / 闲置回收（scale-to-zero，保留存储）
  → 流量唤醒（有请求即自动拉起）
  → 删除（三档语义，见下）
```

- **停止与回收是统一的 scale-to-zero 语义**：手动 stop 或闲置自动回收都只缩容计算资源，存储不动；下一个到达的请求会自动把应用唤醒拉起（流量即唤醒）。
- **删除分三档**，存储语义明确：
  1. 删除 prod 运行容器（`{app_stage}/delete`）：默认保留存储；`purge=true` 连数据面一起销毁；
  2. 存储族接口（`storage` 查询/清空/销毁）：按需操作数据面；
  3. **彻底删除**（`delete/app`）：dev+prod 容器、两侧 PVC 与元数据一步收敛，幂等，不可恢复。
- 生命周期操作（受理、排队、执行到终态）有完整的操作身份传递；发布/删除类长任务经 SSE 任务事件暴露进度。

## 构建与发布链

- **app-cli**：应用构建 CLI，在构建容器内编排构建过程（npm 分发 `@nuwax-ai/app-cli`）。
- **app-runtime 镜像**：统一的 UserApp 运行时（Node / Python / Java / Go 等多语言），构建与运行共用一套镜像基线。
- **file-server**：提供 UserApp 的文件/构建服务域（上传、解压、静态托管等；npm 分发 `@nuwax-ai/file-server`）。
- **workspace manifest**：两级 manifest 描述 workspace 与项目的构建/启动配置（`workspace-manifest` crate）。
- 构建产物版本化管理，prod 部署按版本进行，支持回滚到历史版本。

## 访问方式

- **路径代理**（内置，两种形态同一路由契约）：真实流量走 Pingora 数据面 `GET /proxy/app/{stage}/{user_id}/{app_id}/{*path}`（`stage` 区分 dev/prod，prod 流量会触发唤醒）；HTTP API 侧登记的文档接口为 `/api/v1/userapp/proxy/app/...` 同形态。
- **子域名**：由外部网关/前端层实现（host → app 解析后转发到 rcoder 的代理路径），rcoder 后端本身是路径代理。
- dev 环境支持开发模式（Vite 等热更新预览），构建期间的日志与事件可实时获取。

### 业务就绪查询（只读）

`GET /api/v1/userapp/{app_id}/{app_stage}/readiness` 回答"当前实例声明的服务集合 + Pingap 入口是否满足健康契约"——与容器探针（`/health`、`/ready`）分离。要点：

- 查询成功恒 200，**`data.ready` 才表示业务可用**；`status` 覆盖 `not_deployed/starting/stopping/stopped/ready/degraded/failed/unknown/unsupported`，`reason_code` 为结构化原因，`services[]` 为各服务明细、`proxy` 为入口与生效配置观察。
- **只读保证**：查询不启动/唤醒/停止任何服务、不刷新闲置计时、不阻塞 Stop/Restart；停止中的实例返回 `stopping`，旧运行时无新接口返回 `unsupported`。
- 调用方（界面/Java）建议：显式启动后轮询（间隔 ≥2s、不重叠）；`starting` 显示等待、`ready` 打开预览、`failed` 给出日志入口；`ready=false` 不是接口调用失败；离开页面停止轮询（轮询不会取消实际部署）。完整字段以运行时 OpenAPI（`/api/docs`）为准。

### 访问失败的友好提示页

浏览器直接打开应用页面、而代理失败时（应用未起、上游不可达、或可确认来源的 Pingap 自产 502/503/504），rcoder 返回自包含的 HTML 提示页：保留真实 HTTP 状态码与原 URL，提供「重新访问」按钮和诊断编号；`fetch`/JS/图片等资源请求得到结构化 JSON 而非 HTML，HEAD 无正文，写请求不重放，已开始的 SSE/WebSocket 不受影响。应用自己返回的错误正文（含自己的 502）原样透传。

页面可通过管理接口热替换（部署级一份，不按应用区分）：

```http
PUT    /api/v1/admin/userapp/error-page   # text/html; charset=utf-8，≤512 KiB
GET    /api/v1/admin/userapp/error-page   # 权威/本副本加载状态与 in_sync
DELETE /api/v1/admin/userapp/error-page   # 幂等恢复内置页
```

仅支持 `{{RCODER_TITLE}}`、`{{RCODER_MESSAGE}}`、`{{RCODER_DIAGNOSTIC_ID}}`、`{{RCODER_STATUS}}` 四个转义占位符（静态页亦可）。存储按 rcoder 自身部署形态选择：宿主机/Compose 用本地持久化文件；K8s Pod 内用固定 ConfigMap + 目录投射（多副本最终收敛，换页不重建镜像、不重启 Pod）。

## API 入口

完整接口以运行时 OpenAPI 文档为准（`/api/docs`）。代表性端点（`/api/v1/userapp/*`）：

| 端点（节选） | 说明 |
|------|------|
| `POST /{app_id}/start` | 部署/启动应用（url 部署自动创建） |
| `POST /{app_id}/stop` / `restart` | 停止（scale-to-zero，支持流量唤醒）/ 重启 |
| `POST /{app_id}/{app_stage}/delete` | 删除 prod 运行容器（默认保留存储，`purge=true` 连数据面） |
| `POST /{app_id}/delete/app` | 彻底删除：dev+prod 容器、两侧 PVC 与元数据一步收敛（幂等） |
| `GET /{app_id}/{app_stage}/storage` 等存储族 | 存储查询/清空/销毁 |
| `POST /{app_id}/{app_stage}/upload` 等文件族 | 文件上传/列表/删除 |
| `GET /api/v1/userapp/proxy/app/{stage}/{user_id}/{app_id}/{*path}` | 应用访问代理（文档接口；数据面见下） |

## 相关文档

- [开发环境 owner 恢复](../userapp-dev-owner-recovery.md)：管理进程丢失后的自动恢复、停止语义与 Python 缓存
- [子路径预览与静态资源](userapp-subpath-routing.md)：Vite dev/prod 路径配对、存量项目升级与真实进程回归
- [架构总览](../architecture/overview.md)：UserApp 域的 crate 划分（app_manager / app-cli / workspace-manifest / file-server-userapp 等）
- [gRPC 内部通信](../architecture/grpc.md)：rcoder 与构建/运行容器之间的通信基座
