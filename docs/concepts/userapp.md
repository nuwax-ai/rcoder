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
  3. **彻底删除**（`delete/app`）：删除 dev+prod 计算资源与两侧数据，保留 Deleted 生命周期和历史回执；删除的数据不可恢复。
- 生命周期操作（受理、排队、执行到终态）有完整的操作身份传递；发布/删除类长任务经 SSE 任务事件暴露进度。

### 管理服务重启与缺失容器恢复

RCoder 启动和正常停机均保留已有 Agent、UserApp 与 Builder 容器，不按名称前缀批量删除。启动后从实际资源恢复映射，或在后续使用时按现有发现流程复用；`cleanup_config.enabled` 继续控制后台闲置回收，显式停止、删除和存储清理保持各自语义。

Docker Compose 下，真实 prod 应用访问流量可以重建仍为 Active 的原应用所缺失的运行容器。恢复使用当前已应用的私有运行配置及原四个数据挂载，不创建新的应用身份，不重新提交制品下载、发布 SQL 或数据库改密。运行配置保存在 RCoder 私有持久数据目录的 `.runtime-config/prod/`，包含敏感环境变量，目录0700、文件0600；该目录不挂给应用容器。恢复使用原镜像的不可变内容身份；已被清理的镜像不会静默替换成 `latest`。

已有运行容器会在管理服务启动时补齐恢复配置；某个应用补齐失败只记录该应用的诊断，不阻断其它应用或管理服务启动。旧版本容器及配置均已丢失时，无法推导任意业务环境变量，应通过原发布操作恢复其配置，不能用空默认值冒充无损重建。显式重新部署可覆盖损坏的旧配置。

通过资源发现恢复的应用会保留原 PVC 身份见证。同一生命周期的受控清理已确认销毁对应 PVC 后，校验只读投影会退休该卷的旧 UID，后续显式部署可沿正常流程创建新卷；原见证、清理回执和诊断仍保留。尚未确认的删除、其它应用或不同 UID 的资源继续使用原保护规则，旧流量不会恢复已经销毁的数据。

readiness、状态和资源查询只观察，不启动或重建容器，也不刷新闲置时间。文件、日志和工具访问保留原有停止容器唤醒语义，不因此获得缺失容器重建权限。所有会触发恢复的应用流量必须先通过原有鉴权；Deleted 应用不会被访问复活。

容器级 Stop 的停止意图由显式 Start、Deploy 或 Restart 沿现有流程更新；旧 Stop 回执保留为历史，不再阻挡新启动后的流量恢复。后台流量不会自行反转尚有效的容器停止意图或正在执行的 Stop。

并发业务访问合并为一次协调操作。请求超时或断开仅结束等待；已经开始受理的原重建任务继续收束，不因重试创建第二套业务。内部运行容器启动与业务就绪分开观察：恢复期间可能返回503及 `Retry-After`，该值是重试建议，不能代替业务 readiness 或实际 HTTP 验证。

### 删除后重新创建

已彻底删除的应用不会因普通工作区 ensure 自动复活。需要继续使用同一 `app_id` 时，调用 `POST /api/v1/userapp/{app_id}/recreate`，提交原 `expected_lifecycle_id` 和本次稳定的 `request_id`。确认 dev/prod 计算资源均已缺失后，接口返回新的 Active 生命周期，`lifecycle_id` 改变、`lifecycle_epoch` 加一；后续操作使用新生命周期。

缺失确认使用 Deleted tombstone 的原生命周期做只读物理查询，不要求它先变为 Active，也不会创建或接管 builder。零副本 StatefulSet 仍算保留资源，单次地址查询为空不能证明资源已删除；查询失败或身份变化如实返回错误。相同请求重试返回原重建结果，原删除历史保留，旧生命周期的迟到请求不会作用于新一代。重建身份不恢复旧数据，也不自动启动计算。

## 构建与发布链

- **app-cli**：应用构建 CLI，在构建容器内编排构建过程（npm 分发 `@nuwax-ai/app-cli`）。
- **app-runtime 镜像**：统一的 UserApp 运行时（Node / Python / Java / Go 等多语言），构建与运行共用一套镜像基线。
- **file-server**：提供 UserApp 的文件/构建服务域（上传、解压、静态托管等；npm 分发 `@nuwax-ai/file-server`）。
- **workspace manifest**：两级 manifest 描述 workspace 与项目的构建/启动配置（`workspace-manifest` crate）。
- 构建产物版本化管理，prod 部署按版本进行，支持回滚到历史版本。

## 访问方式

- **路径代理**：应用流量使用 Pingora 数据面 `/api/v1/userapp/proxy/app/{stage}/{user_id}/{app_id}/{*path}`，`stage` 为 `dev` 或 `prod`，prod 流量会触发唤醒。两个环境使用同一完整前缀，切换环境只改变 stage 段。
- **子域名**：由外部网关/前端层实现（host → app 解析后转发到 rcoder 的代理路径），rcoder 后端本身是路径代理。
- dev 环境支持开发模式（Vite 等热更新预览），构建期间的日志与事件可实时获取。

应用入口的主机和端口使用实际对外的数据面地址。Compose 默认主 HTTP API 为 8090，Pingora 宿主映射为 8089（容器内监听 8088）；本地应用预览应将上述完整路径拼接到数据面地址。Kubernetes/Gateway 地址从部署配置读取，不能照搬本地端口。

主服务 OpenAPI 也描述同形态的应用代理路径，但当前主 HTTP 路由并未挂载这两个应用代理 handler，不能将主 API 端口当作应用流量入口。ttyd/VNC/音频/IME/DBX 工具族的主 HTTP 入口另有 307 跳转；重定向中的 loopback 地址也不能自动代表远程浏览器可达地址。实际业务流量应使用部署对外暴露的代理/Gateway 入口。

### 业务就绪查询（只读）

`GET /api/v1/userapp/{app_id}/{app_stage}/readiness?user_id={user_id}` 回答"当前实例声明的服务集合 + Pingap 入口是否满足健康契约"——与容器探针（`/health`、`/ready`）分离。要点：

- `user_id` 为当前接口必填的调用方参数；实例仍按 `app_id` 与 `app_stage` 定位，不按用户 ID 隔离或选择实例。调用方需显式传参。
- 查询成功恒 200，**`data.ready` 才表示业务可用**；`status` 覆盖 `not_deployed/provisioning/starting/stopping/stopped/ready/degraded/failed/unknown/unsupported`，`reason_code` 为结构化原因，`services[]` 为各服务明细、`proxy` 为入口与生效配置观察。
- dev/prod 按当前实际容器或 Pod 分别观察，查询前后复核实例及控制意图；已停止返回 `stopped`，dev 计算资源申请/接管中返回 `provisioning`（只出现在 dev：平台在准备 builder，业务快照尚未产生），业务编排启动中返回 `starting`，真正缺少部署才返回 `not_deployed`。整个查询限时 8 秒，预算耗尽返回 `unknown/OBSERVE_INCOMPLETE`，可稍后重查。
- **只读保证**：查询不启动/唤醒/停止任何服务、不刷新闲置计时、不阻塞 Stop/Restart；停止中的实例返回 `stopping`，旧运行时无新接口返回 `unsupported`。
- 调用方（界面/Java）建议：显式启动后轮询（间隔 ≥2s、不重叠）；`provisioning` 与 `starting` 同属等待态，继续轮询即可（业务快照一旦出现，`provisioning` 即让位给 `not_deployed`/`ready` 等真实状态）；`ready` 打开预览、`failed` 给出日志入口；`ready=false` 不是接口调用失败；离开页面停止轮询（轮询不会取消实际部署）。完整字段以运行时 OpenAPI（`/api/docs`）为准。

### DBX 界面就绪查询（只读）

`GET /api/v1/userapp/{app_id}/{app_stage}/dbx/readiness?user_id={user_id}` 观察指定 dev/prod 实例中的 DBX HTTP 服务，独立于应用业务就绪和数据库连接健康：

- `ready`：DBX HTTP 服务已响应；不代表数据库账号或业务 SQL 已通过验证。
- `starting`：计算资源正在启动，或运行中的 DBX 尚未响应。
- `stopped`：计算资源缺失或已停止。
- `unknown`：无法确认，例如观察超时、实例换代、执行通道不可用或计算资源异常退出。`reason_code` 是稳定原因码，`message` 是可选诊断详情。

总查询预算为 3 秒，包含存储查询、运行时定位和探测。Docker Published 使用 DBX 实际发布端口；宿主 K8s 使用绑定当前物理实例的只读 exec，不把管理端口的发布映射改成 4224。

查询不启动容器、不刷新闲置计时，也不阻止用户启停；`unknown` 不应永久禁止用户直接访问。需要唤醒时沿用既有 DBX 访问入口。

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
| `POST /{app_id}/delete/app` | 彻底删除计算与数据，保留 Deleted 生命周期和历史回执（幂等） |
| `POST /{app_id}/recreate` | 已删除应用确认无计算资源后创建新生命周期，同请求重试返回原结果 |
| `GET /{app_id}/{app_stage}/storage` 等存储族 | 存储查询/清空/销毁 |
| `POST /{app_id}/{app_stage}/upload` 等文件族 | 文件上传/列表/删除 |
| `/api/v1/userapp/proxy/app/{stage}/{user_id}/{app_id}/{*path}`（数据面） | 应用流量路径；直接访问 Pingora/Gateway 对外入口，端口关系见上文“访问方式” |

## 相关文档

- [开发环境 owner 恢复](../userapp-dev-owner-recovery.md)：管理进程丢失后的自动恢复、停止语义与 Python 缓存
- [子路径预览与静态资源](userapp-subpath-routing.md)：Vite dev/prod 路径配对、存量项目升级与真实进程回归
- [架构总览](../architecture/overview.md)：UserApp 域的 crate 划分（app_manager / app-cli / workspace-manifest / file-server-userapp 等）
- [gRPC 内部通信](../architecture/grpc.md)：rcoder 与构建/运行容器之间的通信基座
