# UserApp 应用开发与发布设计文档

> 关系说明：本文档定义 UserApp 的**开发、打包、发布**流程， complement [`application-management-service-v2-design.md`](./application-management-service-v2-design.md)（后者定义 UserApp 的**运行时管理**：create/delete/start/stop/upload 等）。
>
> 阅读顺序：先读 §2「核心概念」——尤其 §2.1「workspace = app_id」是整篇地基。

---

## 1. 目标与边界

### 1.1 目标

让用户在 RCoder 平台上**开发、打包、发布**一个 UserApp 应用。一个 UserApp 是一个 **workspace 多项目**（如前端 + 后端），用户在 workspace 下开发多个子项目，平台负责打包各子项目产物、发布部署。

### 1.2 关键定位

| 定位 | 含义 | 设计后果 |
|---|---|---|
| **UserApp = workspace 多项目** | 一个 `app_id` 对应一个 workspace，含多个子项目（前端/后端/...） | 打包遍历子项目，多产物；部署单容器多服务 |
| **app_id = file-server project_id** | UserApp workspace 复用 file-server 的 project workspace 概念 | `resolve_project(app_id)` 直接复用，零新 resolver |
| **打包/发布分离** | file-server 只管 build + 取产物；Java 串 upload + 部署 | 借鉴 packageAgent（旧项目）：rcoder 不碰存储/url |
| **单容器多服务** | 一个 app-runtime-base 容器，workspace `start.sh`（supervisor）跑多服务 | 资源隔离粒度 = app；前端+后端+PG 同容器 |

### 1.3 非目标（初期不做）

- ❌ **不做多语言 build（初期）**：初期子项目全前端/Node（跑通逻辑）；python/java/rust 后续扩展。
- ❌ **不做多容器部署**：初期单容器多服务（start.sh supervisor）；每子项目一容器后续。
- ❌ **不做 rcoder 编排 publish**：方案 A（Java 串），rcoder 只提供原子接口（build + static + upload_from_url + create_app）；删除 app_publish crate。

---

## 2. 核心概念

### 2.1【地基】workspace = app_id

UserApp 的 `app_id` 对应 file-server 的 **project workspace**（`resolve_project(project_id=app_id)`）。workspace 根下有多个子项目目录。

```
{workspace_root}/{app_id}/                  ← UserApp workspace 根（file-server resolve_project）
├── workspace.manifest.toml                 ← workspace 级配置
├── start.sh                                ← workspace 级启动（supervisor 多服务）
├── userapp-frontend/                       ← 子项目（前端）
│   ├── project.manifest.toml
│   └── src/...
├── userapp-backend/                        ← 子项目（后端）
│   ├── project.manifest.toml
│   └── src/...
```

**关键**：app_id 复用 file-server 的 project_id，**不新建 resolve_userapp**。file-server 现有的 `resolve_project`、`serve_from_root`、`build_generic` 等可直接复用。

### 2.2 两级 manifest

| 层级 | 文件 | 职责 |
|---|---|---|
| **workspace 级** | `workspace.manifest.toml`（根） | 子项目列表（name + path）+ 部署配置（[deploy]：image/command/ports/env/resources） |
| **project 级** | `project.manifest.toml`（各子项目） | 项目类型（type=node/java/...）+ build 配置（cmd + artifact） |

### 2.3 借鉴 packageAgent（旧项目 Java 流程）

旧项目 `AgentWorkspaceApplicationServiceImpl#packageAgent` 的流程是本次设计的参照：

```
Java → 触发 file-server build（buildAgentPackage）→ 返 artifacts 列表
Java → 逐个调 file-server static 下载产物字节
Java → 自己 upload（fileManagementService）→ 得 fileUrl
Java → 用 fileUrl 部署
```

**核心：upload（产物→url）是 Java 做，rcoder/file-server 不碰存储。** UserApp 发布沿用此模式。

---

## 3. 整体架构（开发 → 打包 → 发布）

```
① 开发：用户在 agent-runner 容器内，开发 UserApp workspace（多子项目）
② 打包：Java 调 file-server POST /api/userapp/build {app_id}
   → file-server resolve_project(app_id) → 读 workspace.manifest → 遍历子项目
   → 各 build_generic（project.manifest 的 cmd/artifact）→ 多子产物
   → 组装成一个整体包 workspace-package.zip（预定义结构：各子项目产物 + start.sh + scripts）
   → 返 1 个 artifact {path: "workspace-package.zip"}
③ 取整体包：Java 调 file-server GET /api/userapp/static/{app_id}/workspace-package.zip
   → 下载整体包（1 次）
④ 上传：Java upload 整体包（Java 存储服务）→ 得 1 个 url
⑤ 部署：Java 调 app_manager upload_from_url（url 解压到 UserApp code/，含前端+后端+start.sh）
   + create_app（workspace.manifest [deploy]）
   → app-runtime-base + start.sh（supervisor 跑前端 + 后端 + PG）
```

> **整体包设计**：build 后多子产物组装成一个 `workspace-package.zip`（预定义结构），Java 一次取/upload/部署（比逐个 artifacts 简单：N 次 → 1 次）。

---

## 4. 模板结构（userapp-workspace-template）

### 4.1 目录结构

```
userapp-workspace-template/                ← workspace 根（= app_id workspace）
├── workspace.manifest.toml                ← §4.2
├── start.sh                               ← §4.4（workspace 级启动，多服务）
├── userapp-frontend/                      ← 前端子项目（Next.js）
│   ├── project.manifest.toml              ← §4.3
│   ├── src/ + package.json + scripts/build-standalone.mjs（从 userapp-next-template 迁入）
├── userapp-backend/                       ← 后端子项目（Node Express，初期也 Node）
│   ├── project.manifest.toml
│   ├── src/ + package.json
```

### 4.2 workspace.manifest.toml（根）

```toml
[workspace]
name = "my-userapp"

# 子项目列表（打包时遍历）
[[projects]]
name = "frontend"
path = "userapp-frontend"           # workspace 相对路径
[[projects]]
name = "backend"
path = "userapp-backend"

# 部署配置（Java create_app 用，对应 CreateAppRequest）
[deploy]
image = "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/app-runtime:latest"   # 统一多语言镜像（rust:1.97/Debian + Node/Python/Java/Go/Rust）
command = ["bash", "/app/code/start.sh"]   # bash：start.sh 用 wait -n（dash 不支持）
ports = [
  { name = "frontend", port = 3000, expose_type = "Http" },
  { name = "backend",  port = 8080, expose_type = "Http" },
  { name = "pgweb",    port = 8081, expose_type = "Http" },
]
env = { NODE_ENV = "production", POSTGRES_USER = "app", POSTGRES_PASSWORD = "app", POSTGRES_DB = "app" }
resources = { cpu = "1", memory = "1Gi" }
```

### 4.3 project.manifest.toml（子项目）

```toml
[project]
name = "frontend"
type = "node"                       # node（初期）/ java / python / rust（后续）

[build]
cmd = "npm run build:standalone"    # 容器内跑的 native build 命令
artifact = "userapp-frontend.zip"   # 产物相对路径（cwd = 子项目目录）
```

### 4.4 start.sh（workspace 级启动，单容器多服务）

```sh
#!/bin/sh
# UserApp workspace 启动：supervisor 管前端 + 后端 + PG（app-runtime-base 内置）
# 挂载在 /app/code/（workspace 根），子项目在 /app/code/{project}/
set -e
cd /app/code

# wait-for-pg（复用 helpers）
. /app/code/scripts/lib/helpers.sh  # 如有
wait_for_pg 30 2 || exit 1

# 各子项目 migrate（如有）
# node userapp-backend/migrate.js  # 后端 DB 迁移

# 启动多服务（supervisor 或后台 + 前台）
# 前端（standalone）
(cd userapp-frontend && HOSTNAME=0.0.0.0 PORT=3000 node server.js) &
# 后端
(cd userapp-backend && HOSTNAME=0.0.0.0 PORT=8080 node server.js) &
wait
```

---

## 5. file-server UserApp 接口（独立，复用 resolve_project）

### 5.1 `POST /api/userapp/build`（workspace 级打包，返**整体包**）

**请求**：
```json
{ "appId": "app-xxx" }
```

**流程**（file-server `handlers/userapp/`）：
1. `resolve_project(app_id)` → workspace 根（复用 file-server 现有 resolver）
2. 读 `workspace.manifest.toml` → 子项目列表
3. 遍历子项目：
   - 读 `{workspace}/{project_path}/project.manifest.toml` → `{type, cmd, artifact}`
   - 调 `build_generic(build_manager, app_id, cmd, cwd={workspace}/{path}, artifact, log_dir, timeout)` → 各子产物
4. **组装成一个整体包 `workspace-package.zip`**（预定义结构）：
   ```
   workspace-package.zip
   ├── userapp-frontend/          ← 前端 standalone（子产物解压/组装）
   ├── userapp-backend/           ← 后端 build 产物
   ├── start.sh                   ← workspace 级启动（从 workspace 根拷）
   ├── scripts/lib/helpers.sh     ← wait_for_pg 等
   ```
5. 返 **1 个整体产物**

**响应**：
```json
{
  "success": true,
  "artifact": { "path": "workspace-package.zip", "fileName": "workspace-package.zip" }
}
```

### 5.2 `GET /api/userapp/static/{app_id}/{*rest}`（取整体包）

- `resolve_project(app_id)` → workspace → `serve_from_root`（复用 `static_files.rs:145`，COMPUTER_CORS Range 支持大产物）
- Java 一次取 `workspace-package.zip`（整体包）

### 5.3 与现有 file-server 的关系

| 现有接口 | UserApp 新接口 | 关系 |
|---|---|---|
| `POST /api/computer/build-agent-package`（agent 包，多 artifacts） | `POST /api/userapp/build`（workspace 整体包） | 独立，UserApp 组装 workspace-package.zip |
| `GET /api/page/static/{project_id}/{*rest}`（网页静态） | `GET /api/userapp/static/{app_id}/{*rest}` | 独立路由前缀，复用 serve_from_root |
| `resolve_project` | 同 | **直接复用**（app_id=project_id） |
| `build_generic` service | 同 | **直接复用**（各子项目调） |

**UserApp 接口独立（/api/userapp/...），不动旧网页/computer 逻辑。**

---

## 6. 部署（单容器多服务）

### 6.1 部署流程（Java 串，整体包简化）

1. Java 取整体包（§5.2）→ upload（Java 存储）→ **1 个** url
2. Java 调 `POST /api/v1/apps/{app_id}/upload-from-url`（app_manager，url 下载解压到 code/，含前端+后端+start.sh）
3. Java 调 `POST /api/v1/apps`（create_app，workspace.manifest [deploy] → CreateAppRequest）

### 6.2 容器内运行

- 镜像：`app-runtime`（统一多语言：rust:1.97/Debian 底 + Node/Python/Java/Go/Rust 全有；内置 PG/pgweb/ttyd + supervisor）
- command：`["bash", "/app/code/start.sh"]`（workspace 级，supervisor `[program:app]` 跑多服务，见 §6.3）
- 端口：前端 3000 + 后端 8080 + pgweb 8081（workspace.manifest [deploy].ports）
- code/ 挂载：workspace 根（含子项目 + start.sh）

### 6.3 start.sh 多服务（supervisor [program:app] 模型）

**运行时模型**（已核实 `docker/app-runtime-base/start-app.sh`）：
- ENTRYPOINT `start-app.sh`（bash）把 UserApp `command`（如 `["bash","/app/code/start.sh"]`）包成 **supervisor `[program:app]`**（`autorestart=true`），`supervisord` 作 PID 1。
- supervisor 另管 `[program:postgresql/pgweb/ttyd]`（PG 首启 initdb 异步，避免 liveness 杀）。
- 所以 **workspace start.sh = 单个 supervisor program**：前台阻塞，退出 → supervisor 整组重启。

**start.sh 设计**（`#!/bin/bash` + `wait -n`）：
1. `wait_for_pg`（PG 由 supervisor 托管，已就绪或秒级就绪）
2. frontend `migrate.js`（Drizzle，幂等）
3. 后台起前端（`node userapp-frontend/server.js`）+ 后端（`node userapp-backend/server.js`）
4. `wait -n` 阻塞 → 任一服务退出 → 脚本退出 → **supervisor 整组重启**（含 PG wait + migrate，幂等安全）

**用 bash 而非 sh**：`wait -n` 是 bash 扩展（dash 不支持）；command 必须是 `["bash",...]`，shebang `#!/bin/bash`。app-runtime-base 是 Ubuntu 24.04，bash 必有。

**粗粒度重启（初期）**：一个服务崩 → 整组（前端+后端）重启。后续可拆多 `[program]` 独立重启（需 start-app.sh 支持多 program，或 workspace 自写 supervisor conf，见 §10）。

---

## 7. 关键决策

| 决策 | 选择 | 理由 |
|---|---|---|
| workspace 概念 | app_id = project_id（复用 resolve_project） | 对齐 file-server，零新 resolver |
| manifest 分层 | 两级（workspace + project） | 清晰；workspace 编排，project build |
| 打包/发布分离 | file-server build + static；Java upload + deploy | 借鉴 packageAgent；rcoder 不碰存储 |
| 部署模式 | 单容器多服务（start.sh） | 简单，资源隔离=app；后续多容器 |
| 初期子项目 | 全前端/Node | 跑通逻辑；后续 python/java/rust |
| app_publish crate | 删除 | 方案 A Java 串，编排不需要 |
| file-server 旧逻辑 | 零改动 | UserApp 独立 /api/userapp/ 接口 |

---

## 8. 复用点（不造轮子）

| 能力 | 复用 | 说明 |
|---|---|---|
| workspace 解析 | `file-server::WorkspaceResolver::resolve_project` | app_id=project_id |
| build 执行 | `file-server::service::build_generic::build_generic` | 各子项目调（cmd → artifact） |
| 静态文件下载 | `file-server::handlers::static_files::serve_from_root` | + COMPUTER_CORS（Range） |
| build 并发 | `file-server::service::build_manager::BuildManager` | 全局信号量 + 项目互斥 |
| url 下载部署 | `app_manager::upload_from_url` | 阶段1（无 SSRF，局域网友好） |
| 创建应用 | `app_manager::create_app` | 现有（workspace.manifest [deploy] → CreateAppRequest） |

---

## 9. 实施路线

| 步骤 | 内容 | 依赖 |
|---|---|---|
| **1. 模板** | 新建 `userapp-workspace-template`（workspace 根 + 前端 + 后端 Node 子项目 + 两级 manifest + start.sh） | 无 |
| **2. file-server 接口** | `handlers/userapp/`（build workspace 遍历 + static）+ `manifest.rs`（两级 toml）+ routes 注册 | 步骤 1（模板验证） |
| **3. 删 app_publish** | 删 `crates/app_publish/` + 回退 rcoder 主接入（Cargo + router.rs） | 无（清理） |
| **4. 部署适配** | workspace start.sh（多服务）+ create_app（[deploy]） | 步骤 1 + 2 |

---

## 10. 待定/后续

- **多语言 build**：python/java/rust 子项目（project.manifest type + build_generic 工具链；agent-runner 镜像补 Go/Gradle）
- **多容器部署**：每子项目一容器（编排多容器 + 网络 + 共享存储）
- **rcoder 编排 publish**（可选）：若 Java 不想串，rcoder 加 publish 编排（app_publish 复活或 file-server 内）
- **workspace manifest schema 校验**：toml 严格校验 + 版本（schema = 1）
