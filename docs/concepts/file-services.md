# 文件服务（file-server）

file-server 是 RCoder 的文件/工作区/Git/构建/技能服务：一个独立的 Rust crate（`crates/file-server`），既能嵌入 rcoder 主服务同源提供，也能作为独立服务运行并经 npm 分发（`@nuwax-ai/file-server`）。

## 能力域

file-server 的接口按域组织（路由组装在 `crates/file-server/src/routes/mod.rs`）：

| 域 | 能力 |
|---|---|
| `/api/project` | 项目与文件工作区：内容读写、创建/删除/复制、上传（单文件/批量/附件/整项目 zip）、代码全量或指定更新、**版本备份/回滚/导出/按版本读**、技能推送 |
| `/api/git` | Git 域：读（分支/标签/日志/文件内容/状态）+ 写（init/add/commit/discard/diff/reset/checkout/revert）+ 引用管理 |
| `/api/build` | 构建与开发服务：dev server 生命周期（start/stop/restart/list/keep-alive）、构建执行、构建错误解析、日志获取 |
| `/api/computer` | Computer Agent 工作区：文件浏览/检索/执行命令/包管理/归档下载/workspace 创建与模板初始化/静态托管 |
| `/api/page/static` | 预览页静态文件服务 |

UserApp 专属域拆分在独立 crate `file-server-userapp`（`/api/v1/userapp/*` 子树）：UserApp 构建任务（含 SSE 事件流与取消）、文件族、dev server 管理、日志查询/流、构建制品下载——详见[UserApp 应用管理](userapp.md)。

各域接口的完整定义以运行时 OpenAPI 文档为准（`/api/docs`，file-server 双面之一）。

## 三种消费形态

同一套 file-server 代码有三种存在方式：

1. **rcoder 主服务内嵌**：路由与 rcoder 主端口同源合并挂载（含 workspace 解析器注入），rcoder 的文件相关接口全部由它承载。
2. **独立服务 / npm 分发**：`@nuwax-ai/file-server` npm 包（包装器 + 平台二进制），可脱离 rcoder 单独部署，例如作为 Electron 应用的本地文件后端。
3. **file-server-proxy 分流**：`:60000` 入口按策略把请求分流到 Rust file-server（默认 `all_rust` 策略）或预留的 TS 旧实现上游——存量调用方无需改端口即可平滑切换到 Rust 实现。

agent 容器内同样内嵌 file-server（开关控制），容器侧的文件操作与本进程无差别。

## workspace manifest（两级模型）

UserApp 的 workspace 用两级 manifest 描述构建与启动配置：

- `workspace.manifest.toml`：workspace 级（多项目布局、公共配置）
- `project.manifest.toml`：项目级（单个项目的构建/启动命令等）

**边界**：manifest 只描述"这个项目如何构建与运行"，不包含镜像、资源限额、Secret、端口分配——那些属于平台策略，由 UserApp 管理面（`update` / `start` 等接口）管理。类型定义见 `workspace-manifest` crate。

## 开发服务器（dev server）

`/api/build` 域提供 dev server 生命周期管理：对 Vite/Next 等前端项目启动带热更新的开发服务器、查询端口与框架信息、保活与停止。开发模式下访问开发服务器的流量经代理直达容器内 dev server，热更新（WebSocket）透传。

## 相关文档

- [UserApp 应用管理](userapp.md)：file-server-userapp 域与构建发布链
- [架构总览](../architecture/overview.md)：file-server 家族的 crate 划分
- [宿主机形态](../deployment/host.md)：单机形态下的文件根目录约定
