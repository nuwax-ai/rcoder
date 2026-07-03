# ACP Agent Management API 设计文档

> **P0-4 架构更新(2026-06)**:rcoder 是唯一对外暴露 HTTP 的服务,agent-runner 容器不再直接对外提供 HTTP 端点(虽然本地直起模式仍保留其 HTTP 服务)。
> 本文档 §6、§B.1、§B.2 已按转发模型更新——`/agent-mgmt/*` 路径在 **rcoder** 上,通过 gRPC 转发到对应项目的 agent_runner 容器内的 `AgentMgmtService`。

## 1. 背景与目标

### 1.1 现状分析

当前 `/computer/chat` 接口通过 `agent_config.agent_server` 参数支持指定 ACP Agent：

```json
{
  "agent_config": {
    "agent_server": {
      "agent_id": "claude-code-acp-ts",
      "command": "claude-code-acp-ts",
      "args": [],
      "env": { "ANTHROPIC_API_KEY": "sk-xxx" },
      "model_env_bindings": [...]
    }
  }
}
```

**问题**：
- 默认只有 `claude-code-acp-ts` 一个 ACP Agent（编译时嵌入 JSON）
- 没有运行时安装/管理 Agent 的能力
- 用户无法动态添加新的 ACP Agent（如 kimi-cli、kilo-code、codex-acp 等）
- 没有检查 Agent 是否已安装、版本状态的机制

### 1.2 设计目标

提供一套 HTTP API，支持：
1. **查询** - 列出所有已安装的 ACP Agent 及其状态
2. **检测** - 检查指定 ACP Agent 是否可用、版本号
3. **安装（二进制）** - 上传可执行文件或压缩包，自动放置到标准路径并加入 PATH
4. **安装（包管理器）** - 通过 npm 等包管理器安装
5. **安装（URL）** - 从指定 URL 下载安装
6. **卸载** - 移除已安装的 Agent
7. **使用** - 通过 `/computer/chat` 的 `agent_server` 参数指定使用任意已安装 Agent

### 1.3 设计原则

| 原则 | 说明 |
|------|------|
| **约定大于配置** | 统一安装路径、PATH 管理，用户无需关心细节 |
| **Fail Fast** | 安装前检测环境，安装后立即验证，失败立即报错 |
| **幂等性** | 重复安装同一 Agent 不会出错，会覆盖更新 |
| **容器隔离** | Agent 安装在用户容器内，不同容器互不影响 |

---

## 2. 核心概念

### 2.1 Agent 安装目录结构

**安装根目录**: `/home/user/acp-agent/`（容器内路径，后端约定，调用方无需关心）

```
/home/user/acp-agent/                     # 安装根目录（容器内路径，后端约定）
├── bin/                                  # 可执行文件目录（加入系统 PATH）
│   ├── codex-acp                        # 上传的二进制文件
│   ├── kimi-cli                          # npm 安装的命令（符号链接或 wrapper）
│   └── kilo-code                         # 上传的二进制文件
├── lib/                                  # 压缩包解压的附属文件（动态库、配置等）
│   └── my-agent/                         # 按 agent_id 隔离
│       ├── libhelper.so
│       └── config.json
├── registry.json                         # Agent 注册表（元数据）
└── npm-global/                           # npm 全局安装目录
    └── lib/node_modules/
        ├── kimi-cli/
        └── codex-acp/
```

**宿主机路径映射**（通过 Docker 挂载）：

`/home/user/acp-agent/` 位于容器用户主目录下，该目录已通过 Docker volume 挂载到宿主机，Agent 安装后**持久化保存**，容器重启不丢失。

| 隔离类型 | 宿主机路径 | 容器路径 |
|---------|-----------|---------|
| project | `/computer-project-workspace/{user_id}/acp-agent/` | `/home/user/acp-agent/` |
| tenant/space | `/computer-project-workspace/{tenant_id}/{space_id}/acp-agent/` | `/home/user/acp-agent/` |

### 2.2 Agent 注册表 (registry.json)

```json
{
  "install_dir": "/home/user/acp-agent",
  "agents": {
    "codex-acp": {
      "agent_id": "codex-acp",
      "install_type": "binary",
      "install_dir": "/home/user/acp-agent",
      "binary_path": "/home/user/acp-agent/bin/codex-acp",
      "command": "codex-acp",
      "args": [],
      "version": "1.2.0",
      "version_check_command": ["codex-acp", "--version"],
      "installed_at": "2025-05-25T10:30:00Z",
      "updated_at": "2025-05-25T10:30:00Z",
      "metadata": {
        "source": "upload",
        "file_size": "15728640",
        "description": "Codex ACP Agent"
      }
    },
    "kimi-cli": {
      "agent_id": "kimi-cli",
      "install_type": "npm",
      "install_dir": "/home/user/acp-agent",
      "binary_path": "/home/user/acp-agent/npm-global/bin/kimi-cli",
      "command": "kimi-cli",
      "args": [],
      "version": "0.3.1",
      "version_check_command": ["kimi-cli", "--version"],
      "package_name": "@anthropic/kimi-cli",
      "package_version": "latest",
      "installed_at": "2025-05-25T11:00:00Z",
      "updated_at": "2025-05-25T11:00:00Z",
      "metadata": {}
    }
  }
}
```

> **install_dir 字段**: 注册表顶层和每个 Agent 条目都记录 `install_dir`，这是后端内部存储细节，调用方无需关心。

### 2.3 PATH 管理策略

Agent 安装后需要确保 `command` 可被系统找到：

```
安装目录: /home/user/acp-agent/（后端约定，调用方无需关心）

容器启动时预设 PATH:
  /home/user/acp-agent/bin:/home/user/acp-agent/npm-global/bin:$PATH

PATH 管理（后端自动处理）:
  1. 安装完成后，自动将 bin/ 和 npm-global/bin 加入 PATH
  2. PATH 持久化到 /etc/profile.d/acp-agents.sh（覆盖式写入）
  3. 同时将 PATH 注入到 agent_runner 启动子进程的环境变量中

安装后验证:
  which {command} → 必须返回有效路径
```

---

## 3. API 接口设计

> **P0-5 设计更新(2026-06)**:本节下面 §3.1 ~ §3.6 是**历史详细设计**(旧 API 草稿),部分字段名/路径与最新实现不同。
> **请以 §3.0 客户端调用示例和 §6 API 端点汇总为准**。
> 历史细节(请求/响应类型完整定义、错误码详细说明等)仍有参考价值,这里保留以便对照。

所有接口挂载在 `/agent-mgmt/` 前缀下，作为独立的 API 路由组。
**所有接口统一使用 POST 方法**，请求参数通过 **JSON Body**(`install` 端点用 `multipart/form-data`)传递。
- 简单 JSON 端点:`{project_id, ...}` 形式,`project_id` 必填(也兼容 `?project_id=xxx` query,JSON 优先)
- `install` 端点:`multipart/form-data`,字段 `file` (binary) + `metadata` (JSON 字符串,含 `project_id`)

### 3.0 客户端调用示例(新增)

> **容器路由说明**：所有端点支持两种路由方式：
> 1. **project_id 路由**（向后兼容）：传 `project_id` 字段
> 2. **user_id/pod_id 路由**（多租户）：传 `user_id`（或 `pod_id` + 隔离字段）
>
> 两种方式二选一，`project_id` 优先。JSON body 和 `?project_id=xxx` query 都支持（JSON 优先）。

#### 列出已安装 agents(POST + JSON body)

```bash
# 方式 1: project_id 路由
curl -X POST http://localhost:8087/agent-mgmt/agents/list \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1"}'

# 方式 2: user_id 路由（多租户）
curl -X POST http://localhost:8087/agent-mgmt/agents/list \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123"}'

# 方式 3: pod_id + 隔离字段路由
curl -X POST http://localhost:8087/agent-mgmt/agents/list \
  -H "Content-Type: application/json" \
  -d '{"pod_id": "pod_1", "tenant_id": "t1", "space_id": "s1", "isolation_type": "tenant"}'
```

响应:
```json
{
  "success": true,
  "code": "0000",
  "message": "Success",
  "data": {
    "system_info": { "os": "linux", "arch": "amd64", "platform": "linux/amd64" },
    "agents": [...],
    "total": 3,
    "install_dir": "/home/user/acp-agent"
  }
}
```

#### 查询单个 agent(POST + JSON body)

```bash
# 查询最新版本
curl -X POST http://localhost:8087/agent-mgmt/agents/get \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'

# 查询指定版本
curl -X POST http://localhost:8087/agent-mgmt/agents/get \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp", "version": "1.0.0"}'
```

#### 健康检查(POST + JSON body)

```bash
# 检查最新版本
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'

# 检查指定版本
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp", "version": "1.0.0"}'
```

#### 卸载(POST + JSON body)

```bash
# 卸载全部版本
curl -X POST http://localhost:8087/agent-mgmt/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'

# 只卸载指定版本
curl -X POST http://localhost:8087/agent-mgmt/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp", "version": "1.0.0"}'
```

#### 从 URL 安装(POST + JSON body, 多平台 + 版本检查)

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-url \
  -H "Content-Type: application/json" \
  -d '{
    "project_id": "p1",
    "agent": {
      "agent_id": "codex-acp",
      "command": "codex-acp",
      "version": "1.2.0"
    },
    "platforms": {
      "linux-x86_64": { "url": "https://cdn.example.com/codex-acp-linux-amd64.tar.gz" },
      "linux-aarch64": { "url": "https://cdn.example.com/codex-acp-linux-arm64.tar.gz" }
    }
  }'
```

#### 从 NPM 安装(POST + JSON body)

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-npm \
  -H "Content-Type: application/json" \
  -d '{
    "project_id": "p1",
    "agent": {
      "agent_id": "claude-code-acp",
      "command": "claude-code-acp"
    },
    "package": "@anthropic-ai/claude-code-acp"
  }'
```

#### 上传压缩包(POST + multipart/form-data)

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install \
  -F 'metadata={"project_id":"p1","agent_id":"codex-acp","command":"codex-acp","install_type":"BINARY","sha256":"e3b0c44298fc1c14..."};type=application/json' \
  -F 'file=@./codex-acp-linux-amd64.tar.gz;type=application/octet-stream'
```

> `install` 端点的 `metadata` 字段是 JSON 字符串,所有元数据(包含 `project_id`)都集中在这里。
> `install_type` 取值: `BINARY`(默认) / `URL` / `NPM`（大小写不敏感）。
> URL 和 NPM 类型请使用专用端点 `/install-from-url` 和 `/install-from-npm`。

### 统一响应格式

所有接口响应均使用统一的 `HttpResult<T>` 包装结构：

```json
{
  "code": "0000",
  "message": "Success",
  "data": { ... },
  "tid": "abc123def456",
  "success": true
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| code | string | 业务状态码，`"0000"` 表示成功，其他值为错误码 |
| message | string | 状态描述（支持国际化） |
| data | object \| null | 业务数据（失败时为 null） |
| tid | string \| null | 请求追踪 ID（OpenTelemetry trace_id） |
| success | boolean | 是否成功（由 `code == "0000"` 派生，无需调用方计算） |

错误响应示例：

```json
{
  "code": "ERR_CONTAINER_NOT_FOUND",
  "message": "Target container not found",
  "data": null,
  "tid": "abc123def456",
  "success": false
}
```

### 3.1 列出已安装的 Agent

**`POST /agent-mgmt/agents/list`**

列出容器内所有已安装的 ACP Agent。

#### 请求体 (JSON)

```json
{
  "project_id": "demo-project-001"
}
```

#### 字段说明

请求体通过 `#[serde(flatten)]` 嵌入 `RoutingParams`，所有字段均为**可选**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（与 user_id/pod_id 二选一） |
| user_id | string | 条件必填 | 用户 ID（ComputerAgentRunner 模式，定位容器） |
| pod_id | string | 否 | Pod ID（有值时覆盖 user_id 作为容器标识） |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型：tenant / space / project（pod_id 有值时必填） |

> **路由规则**：`project_id` 优先；无 `project_id` 时按 `pod_id`（或 `user_id`）+ 隔离字段定位容器。详见 §3.0 容器路由说明。

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agents": [
    {
      "agent_id": "codex-acp",
      "install_type": "url",
      "status": "available",
      "version": "1.2.0",
      "binary_path": "/home/user/acp-agent/bin/codex-acp",
      "installed_at": 1716637800
    }
  ],
  "total": 1,
  "install_dir": "/home/user/acp-agent"
}
```

#### SystemInfo 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| os | string | 操作系统（如 `linux`, `darwin`, `windows`） |
| arch | string | CPU 架构（如 `amd64`, `arm64`） |
| platform | string | 平台标识（`{os}/{arch}`，如 `linux/amd64`） |

> **用途**: 调用方根据 `system_info` 决定应该上传哪个平台的二进制文件。例如 codex-acp 在 GitHub 上提供 `codex-acp-linux-amd64`、`codex-acp-linux-arm64` 等不同架构的版本。

#### AgentInfo 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| agent_id | string | Agent 标识符 |
| install_type | `"builtin"` \| `"binary"` \| `"npm"` \| `"url"` \| `"unknown"` | 安装类型 |
| status | `"available"` \| `"broken"` \| `"not_installed"` \| `"unknown"` | 状态 |
| version | string? | 版本号（无法检测时为 null） |
| binary_path | string? | 可执行文件路径（未安装时为 null） |
| installed_at | number? | 安装时间 (Unix timestamp 秒)，未安装时为 null |

---

### 3.2 检查指定 Agent 状态

**`POST /agent-mgmt/agents/check`**

检查指定 ACP Agent 是否已安装、版本号、是否可正常执行。

#### 请求体 (JSON)

```json
{
  "project_id": "demo-project-001",
  "agent_id": "codex-acp"
}
```

#### 字段说明

请求体通过 `#[serde(flatten)]` 嵌入 `RoutingParams`，路由字段均为**可选**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（与 user_id/pod_id 二选一） |
| agent_id | string | 是 | Agent 标识符 |
| version | string? | 否 | 可选版本号，不传则检查最新版本；传值则只检查指定版本 |
| user_id | string | 条件必填 | 用户 ID（ComputerAgentRunner 模式） |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型（pod_id 有值时必填） |

#### 响应 - Agent 已安装

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agent": {
    "agent_id": "codex-acp",
    "install_type": "binary",
    "installed": true,
    "status": "available",
    "version": "1.2.0",
    "version_check_supported": true,
    "static_checks": {
      "file_exists": true,
      "executable": true,
      "in_path": true
    }
  }
}
```

#### 响应 - Agent 未安装

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agent": {
    "agent_id": "kimi-cli",
    "install_type": "unknown",
    "installed": false,
    "status": "not_installed",
    "version": null,
    "version_check_supported": false,
    "static_checks": {
      "file_exists": false,
      "executable": false,
      "in_path": false
    }
  }
}
```

> **未安装时 system_info 仍有值**: 调用方在首次安装前调用 check 接口，根据 `system_info` 确定应上传哪个平台的二进制文件。

#### 响应 - Agent 已安装但损坏

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agent": {
    "agent_id": "kilo-code",
    "install_type": "binary",
    "installed": true,
    "status": "broken",
    "version": null,
    "version_check_supported": true,
    "static_checks": {
      "file_exists": true,
      "executable": true,
      "in_path": true
    }
  }
}
```

#### 响应 - 无版本检查命令的 Agent（如 codex-acp）

以 Zed 维护的 `codex-acp` 为例，该 agent 不支持 `--version` 参数，安装时不传 `version_check_command`。

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agent": {
    "agent_id": "codex-acp",
    "install_type": "binary",
    "installed": true,
    "status": "available",
    "version": null,
    "version_check_supported": false,
    "static_checks": {
      "file_exists": true,
      "executable": true,
      "in_path": true
    }
  }
}
```

> 当 `version_check_supported: false` 时，`status` 完全由 `static_checks` 三项静态检查决定。
> 三项全为 `true` → `available`；任一为 `false` → `broken`。

#### CheckAgentResponse 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| system_info | SystemInfo | 容器系统信息（操作系统、CPU 架构） |
| agent | AgentDetailInfo | Agent 详细信息 |

#### AgentDetailInfo 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| agent_id | string | Agent 标识符 |
| install_type | `"builtin"` \| `"binary"` \| `"npm"` \| `"url"` \| `"unknown"` | 安装类型 |
| installed | boolean | 是否已安装 |
| status | `"available"` \| `"broken"` \| `"not_installed"` \| `"unknown"` | 状态 |
| version | string? | 版本号 |
| version_check_supported | boolean | 是否支持版本检测 |
| static_checks | StaticCheckResult | 静态检查结果（不执行 agent 进程） |

#### StaticCheckResult 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| file_exists | boolean | binary_path 文件是否存在 |
| executable | boolean | 文件是否有可执行权限 |
| in_path | boolean | `which {command}` 是否找到 |

#### 检测逻辑

```
1. 检查注册表 registry.json 是否有记录
   a. 有记录 → 继续步骤 2
   b. 无记录 → 返回 status = "not_installed"
2. 检查 binary_path 文件是否存在（文件系统 stat 调用，不执行）
3. 执行 `which {command}` 确认 PATH 可达
4. 检查文件是否有可执行权限（Unix: 检查 executable bit）
5. 如果有 version_check_command（用户安装时指定了版本检查命令）:
   a. 执行版本检查命令（超时 5 秒）
   b. 退出码 0 → status = "available", version = 解析 stdout 输出
   c. 退出码非 0 → status = "broken", version_check_error = stderr 内容
6. 如果没有 version_check_command（用户未指定，如 codex-acp 无 --version 参数）:
   a. 不执行 agent 二进制本身（避免 ACP stdio 服务挂起或产生副作用）
   b. 仅通过步骤 2-4 的静态检查判断可用性
   c. 静态检查全通过 → status = "available", version = null
   d. 任一检查失败 → status = "broken", 附带具体失败原因
```

> **为什么没有 version_check_command 时不执行 agent?**
>
> 许多 ACP agent（如 Zed 维护的 `codex-acp`）不支持 `--version` 或 `--help` 参数。
> 裸执行 agent 二进制可能导致：
> - **stdio 模式挂起**: ACP agent 通常通过 stdin/stdout 通信，裸执行后挂起等待输入
> - **副作用**: 可能创建状态文件、启动后台进程、写入日志
> - **误判为 broken**: 不认识 `--help` 参数 → 退出码非 0 → 误报为损坏
>
> 因此，没有 `version_check_command` 时，检测仅依赖**文件系统静态检查**
> （文件存在 + PATH 可达 + 可执行权限），不尝试执行 agent 进程。
> Agent 的真正可用性由 `/computer/chat` 调用时验证。

---

### 3.3 上传二进制 Agent

**`POST /agent-mgmt/agents/install`**

上传压缩包（`.tar.gz` / `.zip`）作为 ACP Agent。通过 `multipart/form-data` 传递文件和元数据。

#### 请求格式

**Content-Type**: `multipart/form-data`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| file | binary | 是 | 压缩包（.tar.gz / .zip，最大 1GB） |
| metadata | string | 是 | JSON 字符串，包含安装元数据（见下表） |

**metadata JSON 字段**（对应 `InstallMetadataBody`）:

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（RoutingParams 字段，与 user_id/pod_id 二选一） |
| user_id | string | 条件必填 | 用户 ID（RoutingParams 字段，ComputerAgentRunner 模式） |
| pod_id | string | 否 | Pod ID（RoutingParams 字段） |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型（pod_id 有值时必填） |
| agent | object | 是 | Agent 身份信息（见下表 `AgentIdentity`） |
| install_type | string | 否 | `"BINARY"`（默认）/ `"URL"` / `"NPM"`，大小写不敏感 |
| source_url | string? | 否 | 下载 URL（URL 安装时必填） |
| npm_package | string? | 否 | npm 包名（NPM 安装时必填，如 `@anthropic-ai/claude-code-acp`） |
| sha256 | string | 否 | SHA-256 校验和（hex，可选，提供时安装后校验） |

**agent 子对象字段**（`AgentIdentity`）:

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| agent_id | string | 是 | Agent 标识符（如 "codex-acp"） |
| command | string | 是 | 入口可执行文件名（如 "codex-acp"） |
| args | string[] | 否 | 启动参数（默认空） |
| version | string? | 否 | 版本号（可选，**当前实现忽略此字段**，见下方说明） |

> **install_type 说明**：BINARY 模式必须提供 `file` 字段；URL/NPM 模式请使用专用端点 `/install-from-url` 和 `/install-from-npm`。
>
> **version 字段说明**：当前 `/install` 端点**不支持多版本并存**。虽然 metadata 中可以传 `agent.version`，但底层实现（`binary_installer::install_from_bytes` 和 `install_from_prepared_stream`）会忽略该字段，始终采用**单版本替换模式**——每次安装会覆盖同一 `agent_id` 的已有文件和注册表条目。如需多版本并存，请使用 `/install-from-url` 端点。

#### 请求示例 - 上传 tar.gz 压缩包

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install \
  -F 'metadata={"project_id":"p1","agent":{"agent_id":"codex-acp","command":"codex-acp"},"install_type":"BINARY"};type=application/json' \
  -F 'file=@./codex-acp-linux-amd64.tar.gz;type=application/octet-stream'
```

#### 请求示例 - 使用 user_id 路由

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install \
  -F 'metadata={"user_id":"user_123","agent":{"agent_id":"my-agent","command":"my-agent"},"install_type":"BINARY"};type=application/json' \
  -F 'file=@./my-agent-v1.0-linux-amd64.tar.gz;type=application/octet-stream'
```

#### 响应 - 压缩包

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "my-agent",
  "status": "available",
  "binary_path": "my-agent",
  "file_type": "tar.gz",
  "file_count": 3,
  "file_size": 8388608,
  "version": null,
  "source_url": null
}
```

#### 安装响应字段说明（InstallAgentResponse，所有安装端点通用）

| 字段 | 类型 | 说明 |
|------|------|------|
| agent_id | string | Agent 标识符 |
| status | `"available"` \| `"broken"` \| `"not_installed"` \| `"unknown"` | 安装后状态 |
| binary_path | string | 可执行文件路径 |
| file_type | string | 检测到的文件类型（`"executable"` / `"tar.gz"` / `"zip"` / `"npm"`） |
| file_size | number | 文件大小（字节） |
| file_count | number? | 安装的文件数量（压缩包为解压后的文件数，单文件为 null） |
| version | string? | 版本号（可选） |
| source_url | string? | 下载源 URL（URL 安装时有值） |
| action | string? | 本次操作类型（`"installed"` / `"updated"` / `"skipped"`，URL 安装时有值） |
| installed | boolean | 本次是否实际执行了下载安装（`action != "skipped"` 时为 true） |
| previous_version | string? | 更新前的版本号（首次安装为 null，跳过时等于 version） |
| platform | string? | 实际匹配的平台 key（如 `"linux-x86_64"`，URL 安装时有值） |

#### 处理流程

```
1. 验证参数（agent_id, command, metadata 必填）
2. 验证文件大小（上限 1GB）
3. 定位目标容器（根据 project_id / user_id / pod_id + 隔离字段）
4. SHA-256 校验（如果 metadata 中提供了 sha256）
5. 自动检测文件类型（扩展名 + magic bytes）:
   - .tar.gz / .tgz → 压缩包 ✓
   - .zip → 压缩包 ✓
   - 其他 → 拒绝（返回 ERR_AGENT_MGMT_UNSUPPORTED_TYPE，当前仅支持压缩包）
6. 确定安装目录：
   - version 有值 → {install_dir}/{agent_id}/{version}/（当前实现 version 被忽略，始终走下方逻辑）
   - version 无值 → {install_dir}/{agent_id}/（单版本替换模式）
7. 如果目标目录已存在，删除整个目录（remove_dir_all）
8. 创建目标目录，将压缩包写入 staging 文件
9. 解压压缩包（tar.gz 或 zip）到目标目录
10. 剥掉单个顶层目录包装（如 deepagents-dev-templates-0.2.9/）
11. 检测包类型：
    a. 目录型包（存在 agent-package.json / package.json）:
       - 从 metadata 读取入口脚本和 args
       - binary_path = 目录路径
    b. 二进制型包（无 metadata 文件）:
       - 在解压目录中查找 command 对应的可执行文件
       - binary_path = 入口可执行文件路径
12. 构建 AgentManifest，调用 registry.upsert() 覆盖式更新注册表
13. 清理 staging 文件
14. 返回安装结果
```

> **与 `/install-from-url` 的区别**：
> - `/install` 端点：单版本替换模式，每次安装覆盖同一 agent_id 的已有版本
> - `/install-from-url` 端点：多版本并存模式，不同版本安装到独立目录，支持幂等跳过

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| 文件过大 | ERR_AGENT_MGMT_BINARY_TOO_LARGE | 超过 1GB 限制 |
| 无执行权限 | ERR_AGENT_MGMT_PERMISSION_DENIED | 文件系统权限问题 |
| 校验和不匹配 | ERR_AGENT_MGMT_CHECKSUM_MISMATCH | SHA-256 校验失败 |
| 压缩炸弹 | ERR_AGENT_MGMT_ARCHIVE_BOMB | 解压累计超限 |
| 路径遍历 | ERR_AGENT_MGMT_PATH_TRAVERSAL | 压缩包含 `..` 等逃逸路径 |
| 容器不存在 | ERR_CONTAINER_NOT_FOUND | 需要先创建容器 |
| agent 已存在 | ERR_AGENT_MGMT_ALREADY_INSTALLED | 重复安装 |
| 磁盘满 | ERR_AGENT_MGMT_DISK_FULL | 容器磁盘空间不足 |

---

### 3.4 通过包管理器安装 Agent

**`POST /agent-mgmt/agents/install-from-npm`**

通过 npm 全局安装 ACP Agent。

#### 请求体 (JSON)

```json
{
  "project_id": "p1",
  "agent": {
    "agent_id": "claude-code-acp",
    "command": "claude-code-acp"
  },
  "package": "@anthropic-ai/claude-code-acp"
}
```

#### 字段说明

请求体通过 `#[serde(flatten)]` 嵌入 `RoutingParams`，路由字段均为**可选**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（与 user_id/pod_id 二选一） |
| agent | object | 是 | Agent 身份信息（见下表 `AgentIdentity`） |
| package | string | 是 | npm 包名（如 `@anthropic-ai/claude-code-acp`） |
| user_id | string | 条件必填 | 用户 ID（ComputerAgentRunner 模式） |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型（pod_id 有值时必填） |

**agent 子对象字段说明**（`AgentIdentity`）:

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| agent_id | string | 是 | Agent 标识符 |
| command | string | 是 | 入口可执行文件名（通常是包名去掉 scope） |
| args | string[] | 否 | 启动参数（默认空） |

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。
> 响应类型为 `InstallAgentResponse`，与 `install` 和 `install-from-url` 端点通用。

```json
{
  "agent_id": "claude-code-acp",
  "status": "available",
  "binary_path": "claude-code-acp",
  "file_type": "npm",
  "file_size": 0,
  "version": "1.0.38",
  "source_url": null
}
```

#### 处理流程

```
1. 验证参数（agent_id, command, package 必填）
2. 定位目标容器（根据 project_id / user_id / pod_id + 隔离字段）
3. 确保 npm 全局安装目录存在且加入 PATH：
   mkdir -p /home/user/acp-agent/npm-global
   export PATH="/home/user/acp-agent/npm-global/bin:$PATH"
   npm config set prefix /home/user/acp-agent/npm-global
4. 执行安装命令（在容器内执行，超时 5 分钟）：
   npm install -g {package}
5. 更新 PATH 持久化脚本
6. 验证安装：
   which {command}
7. 更新注册表 registry.json
8. 返回安装结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| npm install 失败 | ERR_AGENT_MGMT_INSTALL_FAILED | 网络或包问题 |
| 安装超时 | ERR_AGENT_MGMT_COMMAND_TIMEOUT | 安装超过超时限制 |
| agent 已存在 | ERR_AGENT_MGMT_ALREADY_INSTALLED | 重复安装 |
| 容器不存在 | ERR_CONTAINER_NOT_FOUND | 需要先创建容器 |

---

### 3.5 通过 URL 安装 Agent（多平台 + 多版本并存）

**`POST /agent-mgmt/agents/install-from-url`**

从指定 URL 下载压缩包安装 ACP Agent。支持**多平台 URL** + **版本号** + **多版本并存**。

> **执行位置**：此端点在 **rcoder 侧（宿主机）** 直接执行，不通过 gRPC 转发到 agent_runner 容器。
> rcoder 调用 `agent_install_strategy::do_install_from_url`，基于 `AgentDownloadManager` 实现"下载到缓存 → 解压到版本目录 → 更新注册表"的流程。
> 多版本并存的核心：每个版本安装到独立的 `{install_dir}/{agent_id}/{version}/` 目录，不同版本互不干扰。
>
> **设计参考**: Tauri 更新 manifest 的 `platforms` 字段设计（`{os}-{arch}` 命名，每个平台条目含 `url` / `sha256` / `size`）。

#### 请求体

```json
{
  "project_id": "p1",
  "agent": {
    "agent_id": "codex-acp",
    "command": "codex-acp",
    "args": ["--serve"],
    "version": "1.2.0"
  },
  "platforms": {
    "linux-x86_64": {
      "url": "https://cdn.example.com/agents/codex-acp/1.2.0/codex-acp-linux-amd64.tar.gz",
      "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "size": 15728640
    },
    "linux-aarch64": {
      "url": "https://cdn.example.com/agents/codex-acp/1.2.0/codex-acp-linux-arm64.tar.gz",
      "sha256": "a1b2c3d4e5f67890abcdef1234567890abcdef1234567890abcdef1234567890",
      "size": 14680064
    }
  }
}
```

#### 字段说明

请求体通过 `#[serde(flatten)]` 嵌入 `RoutingParams`，路由字段均为**可选**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（与 user_id/pod_id 二选一） |
| agent | object | 是 | Agent 身份信息（见下表 `AgentIdentity`） |
| platforms | object | 是 | 平台 → 下载信息映射（不能为空） |
| force | boolean | 否 | 强制重新安装（取消正在进行的安装，重新开始），默认 false |
| user_id | string | 条件必填 | 用户 ID（ComputerAgentRunner 模式，定位容器） |
| pod_id | string | 否 | Pod ID（有值时覆盖 user_id 作为容器标识） |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型：tenant / space / project（pod_id 有值时必填） |

**agent 子对象字段说明**:

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| agent_id | string | 是 | Agent 标识符（如 "codex-acp"） |
| command | string | 是 | 入口可执行文件名（如 "codex-acp"） |
| args | string[] | 否 | 启动参数（默认空） |
| version | string | 否 | 期望安装的语义化版本号（用于比较是否需要更新） |

#### PlatformEntry 字段说明（platforms 的值）

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 该平台的下载 URL（`http://` / `https://`） |
| sha256 | string | 否 | 该平台文件的 SHA-256 校验和（hex） |
| size | number | 否 | 文件大小（字节，用于磁盘空间预检查） |

#### 平台 key 命名规范

格式: `{os}-{arch}`

| OS | Arch | 完整 key 示例 |
|----|------|--------------|
| linux | x86_64 | `linux-x86_64` |
| linux | aarch64 | `linux-aarch64` |
| darwin | aarch64 | `darwin-aarch64` |
| darwin | x86_64 | `darwin-x86_64` |

> **映射说明**: agent-runner 容器内 `SystemInfo` 返回 `os="linux", arch="amd64"`。
> 查找 platforms 时自动归一化: `amd64 → x86_64`，`arm64 → aarch64`。
> 例如容器报告 `arch="amd64"`，查找 key `linux-x86_64`。

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。
> 响应类型为 `InstallAgentResponse`，与 `install` 和 `install-from-npm` 端点通用。

**响应 - 新版本安装（多版本并存）**:

```json
{
  "agent_id": "codex-acp",
  "status": "available",
  "binary_path": "codex-acp",
  "file_type": "binary",
  "file_size": 15728640,
  "version": "1.2.0",
  "source_url": "https://cdn.example.com/agents/codex-acp/1.2.0/codex-acp-linux-amd64.tar.gz",
  "action": "installed",
  "installed": true,
  "previous_version": null,
  "platform": "linux-x86_64"
}
```

**响应 - 跳过安装（精确版本已存在）**:

```json
{
  "agent_id": "codex-acp",
  "status": "available",
  "binary_path": "codex-acp",
  "file_type": "binary",
  "file_size": 0,
  "version": "1.2.0",
  "source_url": null,
  "action": "skipped",
  "installed": false,
  "previous_version": "1.2.0",
  "platform": null
}
```

**响应 - v 前缀归一化（"v1.2.0" 和 "1.2.0" 视为同一版本）**:

```json
// 已安装 "1.2.0"，再次请求 "v1.2.0" → skipped
{
  "agent_id": "codex-acp",
  "action": "skipped",
  "version": "v1.2.0",
  "previous_version": "v1.2.0",
  "installed": false
}
```

**响应 - 新版本并存安装（旧版本保留）**:

```json
// 已有 v1.1.0，请求 v1.2.0 → 新安装（两个版本并存）
{
  "agent_id": "codex-acp",
  "status": "available",
  "binary_path": "codex-acp",
  "file_type": "binary",
  "file_size": 15728640,
  "version": "1.2.0",
  "source_url": "https://cdn.example.com/agents/codex-acp/1.2.0/codex-acp-linux-amd64.tar.gz",
  "action": "installed",
  "installed": true,
  "previous_version": null,
  "platform": "linux-x86_64"
}
```

#### 响应字段说明（InstallAgentResponse）

| 字段 | 类型 | 说明 |
|------|------|------|
| agent_id | string | Agent 标识符 |
| status | `"available"` \| `"broken"` \| `"not_installed"` \| `"unknown"` | 安装后状态 |
| binary_path | string | 可执行文件路径 |
| file_type | string | 检测到的文件类型（`"executable"` / `"tar.gz"` / `"zip"` / `"npm"` / `"binary"`） |
| file_size | number | 文件大小（字节，跳过时为 0） |
| file_count | number? | 安装的文件数量（压缩包为解压后的文件数，跳过时为 null） |
| version | string? | 当前版本号 |
| source_url | string? | 实际下载的 URL（跳过时为 null） |
| action | string? | 本次操作类型（`"installed"` / `"updated"` / `"skipped"`） |
| installed | boolean | 本次是否实际执行了下载安装（`action != "skipped"` 时为 true） |
| previous_version | string? | 更新前的版本号（首次安装为 null，跳过时等于 version） |
| platform | string? | 实际匹配的平台 key（如 `"linux-x86_64"`，跳过时为 null） |

#### 处理流程（rcoder 侧，多版本并存模式）

```
1. 验证参数
   - agent_id, command 必填
   - version 必填，必须是合法 semver（如 "1.0.0"、"v2.1.3"）
   - platforms 不能为空
   - 每个 platform entry 的 url 必须是 http:// 或 https://
2. 根据 ServiceType 解析安装目录（Strategy Pattern）:
   - ComputerAgentRunner → /app/computer-project-workspace/{user_id}/acp-agent
   - RCoder → /app/project_workspace/{project_id}/acp-agent
3. 平台匹配:
   a. 获取当前系统 os/arch
   b. 归一化: amd64 → x86_64, arm64 → aarch64
   c. 构造 key: "{os}-{arch}" (如 "linux-x86_64")
   d. 在 platforms 中查找 → 没找到返回 ERR_AGENT_MGMT_PLATFORM_NOT_FOUND
4. 缓存检查（幂等核心）:
   a. 检查 {cache_dir}/{agent_id}/{normalized_version}/ 是否存在
   b. 已缓存 → 跳过下载，直接从缓存复制（零延迟）
   c. 未缓存 → 执行下载
5. 下载到缓存目录:
   a. 并发锁: 同一 agent_id:version 只有一个下载任务
   b. 双重检查: 锁内再次检查缓存（防止并发重复下载）
   c. 下载到临时文件 → rename 到 {cache_dir}/{agent_id}/{version}/
6. 复制到安装目录（多版本并存的关键）:
   a. 目标路径: {install_dir}/{agent_id}/{version}/（每个版本独立目录）
   b. 如果目标已存在 → 删除（确保干净复制）
   c. 检测文件类型（magic bytes + 扩展名）:
      - tar.gz / zip → 解压到目标目录 + 规范化目录结构（去除单层 wrapper）
      - 其他 → 直接复制
7. 更新注册表 registry.json:
   a. 记录 agent_id, version, command, args, install_dir
   b. 版本归一化: "v1.0.0" 和 "1.0.0" 视为同一版本
8. 返回安装结果（action = "installed" 或 "skipped"）
```

> **与 `/computer/chat` 自动安装的关系**：`/computer/chat` 请求中如果 `agent_config.agent_server` 携带了 `version` 和 `platforms` 字段，
> chat handler 会在启动 agent 前自动调用同一个 `do_install_from_url` 函数，流程完全一致。
> 业务方可以在 chat 请求中嵌入安装信息，无需单独调用 `/install-from-url` 端点。

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| platforms 中无当前系统 URL | ERR_AGENT_MGMT_PLATFORM_NOT_FOUND | 如容器是 linux-aarch64,但 platforms 只有 linux-x86_64 |
| version 格式不合法 | ERR_AGENT_MGMT_INVALID_VERSION | 非语义化版本号（新模式时校验） |
| URL 格式无效 | ERR_AGENT_MGMT_INVALID_MANIFEST | 非 http/https 协议 |
| 下载失败 | ERR_AGENT_MGMT_INSTALL_FAILED | 远程文件不存在或服务器错误 |
| 下载超时 | ERR_AGENT_MGMT_COMMAND_TIMEOUT | 下载超时（10 分钟） |
| 校验和不匹配 | ERR_AGENT_MGMT_CHECKSUM_MISMATCH | 下载文件与 sha256 不一致 |
| 压缩炸弹 | ERR_AGENT_MGMT_ARCHIVE_BOMB | 解压累计超限 |
| 路径遍历 | ERR_AGENT_MGMT_PATH_TRAVERSAL | 压缩包含 `..` 等逃逸路径 |
| 容器不存在 | ERR_CONTAINER_NOT_FOUND | 需要先创建容器 |
| 磁盘满 | ERR_AGENT_MGMT_DISK_FULL | 预检查磁盘空间不足 |

---

### 3.6 卸载 Agent

**`POST /agent-mgmt/agents/uninstall`**

卸载已安装的 ACP Agent。

#### 请求体 (JSON)

```json
{
  "project_id": "p1",
  "agent_id": "codex-acp"
}
```

#### 字段说明

请求体通过 `#[serde(flatten)]` 嵌入 `RoutingParams`，路由字段均为**可选**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| project_id | string | 条件必填 | 项目 ID（与 user_id/pod_id 二选一） |
| agent_id | string | 是 | Agent 标识符 |
| version | string? | 否 | 可选版本号，不传则卸载全部版本；传值则只卸载指定版本（向后兼容） |
| user_id | string | 条件必填 | 用户 ID（ComputerAgentRunner 模式） |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 条件必填 | 租户 ID（pod_id 有值时必填） |
| space_id | string | 条件必填 | 空间 ID（pod_id 有值时必填） |
| isolation_type | string | 条件必填 | 隔离类型（pod_id 有值时必填） |

#### 响应 - 卸载全部版本

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "codex-acp",
  "uninstalled": true,
  "install_type": "binary",
  "removed_versions": ["1.2.0", "1.1.0"]
}
```

#### 响应 - 卸载指定版本

```json
{
  "agent_id": "codex-acp",
  "uninstalled": true,
  "install_type": "url",
  "removed_versions": ["1.0.0"]
}
```

#### 响应字段说明（UninstallAgentResponse）

| 字段 | 类型 | 说明 |
|------|------|------|
| agent_id | string | Agent 标识符 |
| uninstalled | boolean | 是否成功卸载 |
| install_type | `"builtin"` \| `"binary"` \| `"npm"` \| `"url"` \| `"unknown"` | 安装类型 |
| removed_versions | string[] | 被卸载的版本列表 |

#### 处理逻辑

```
1. 读取 registry.json，检查是否有记录
   - version 有值 → 查找指定版本
   - version 无值 → 查找最新版本（向后兼容）
2. builtin 类型拒绝卸载（返回 ERR_AGENT_MGMT_BUILTIN_PROTECTED）
3. 安全检查：binary_path 必须在安装目录下（防止 manifest 被篡改后删除系统文件）
4. 删除操作：
   - version 有值 → 只删除 {install_dir}/{agent_id}/{version}/ 目录 + 注册表移除该版本
   - version 无值 → 删除整个 {install_dir}/{agent_id}/ 目录 + 注册表移除所有版本
5. 如果删除指定版本后 agent 无剩余版本，自动清理空的父目录
6. 返回卸载结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| Agent 不在注册表中 | ERR_AGENT_MGMT_NOT_FOUND | 指定 agent_id 未安装 |
| 尝试卸载内置 Agent | ERR_AGENT_MGMT_BUILTIN_PROTECTED | builtin 类型不允许卸载 |
| 文件删除失败 | ERR_AGENT_MGMT_UNINSTALL_FAILED | 文件系统权限或 IO 错误 |
| manifest 被篡改 | ERR_AGENT_MGMT_INVALID_MANIFEST | binary_path 超出安装目录范围 |

---

## 4. 与 `/computer/chat` 的集成

### 4.1 使用已安装的 Agent

安装完成后，通过 `/computer/chat` 的 `agent_server` 参数指定使用：

```json
{
  "user_id": "user_123",
  "prompt": "帮我写一个 React 组件",
  "agent_config": {
    "agent_server": {
      "agent_id": "codex-acp",
      "command": "codex-acp",
      "args": [],
      "env": {
        "CODEX_MODEL": "gpt-4o"
      },
      "model_env_bindings": [
        { "env_key": "CODEX_API_KEY", "source": "api_key" },
        { "env_key": "CODEX_BASE_URL", "source": "base_url" },
        { "env_key": "CODEX_MODEL", "source": "default_model" }
      ]
    }
  }
}
```

### 4.2 使用 npm 安装的 Agent

```json
{
  "user_id": "user_123",
  "prompt": "帮我分析这段代码",
  "agent_config": {
    "agent_server": {
      "agent_id": "kimi-cli",
      "command": "kimi-cli",
      "args": ["--acp"],
      "env": {},
      "model_env_bindings": [
        { "env_key": "KIMI_API_KEY", "source": "api_key" },
        { "env_key": "KIMI_BASE_URL", "source": "base_url" },
        { "env_key": "KIMI_MODEL", "source": "default_model" }
      ]
    }
  }
}
```

### 4.3 简化用法：只传 agent_id

如果只需要指定 agent_id，其他参数从注册表自动填充：

```json
{
  "user_id": "user_123",
  "prompt": "hello",
  "agent_config": {
    "agent_server": {
      "agent_id": "codex-acp"
    }
  }
}
```

**解析逻辑**：

```
1. 如果 agent_server 提供了 command → 直接使用
2. 如果 agent_server 只提供了 agent_id，没有 command:
   a. 查找注册表 registry.json
   b. 如果找到 → 使用注册表中的 command、args
   c. 没找到 → 报错 ERR_AGENT_MGMT_NOT_FOUND
```

---

## 5. 完整使用流程示例

### 5.1 场景一：检查环境 → 上传 codex-acp 并使用

先通过 check 接口获取容器平台信息，再上传对应架构的二进制文件。

```bash
# 1. 确保容器存在
curl -X POST http://localhost:8087/computer/pod/ensure \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123"}'

# 2. 检查 codex-acp 是否已安装（同时获取系统信息）
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'
# → {
#     "system_info": { "os": "linux", "arch": "arm64", "platform": "linux/arm64" },
#     "agent": { "installed": false, "status": "not_installed", ... }
#   }

# 3. 根据 system_info 下载对应架构的压缩包并上传
#    平台是 linux/arm64，选择 codex-acp-linux-arm64.tar.gz
curl -X POST http://localhost:8087/agent-mgmt/agents/install \
  -F 'metadata={"project_id":"p1","agent_id":"codex-acp","command":"codex-acp","install_type":"BINARY"};type=application/json' \
  -F 'file=@./codex-acp-linux-arm64.tar.gz;type=application/octet-stream'
# → { "status": "available", "version": null, "file_type": "tar.gz", "file_count": 1 }

# 4. 再次确认状态（静态检查通过，version = null）
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'
# → {
#     "system_info": { "os": "linux", "arch": "arm64", "platform": "linux/arm64" },
#     "agent": {
#       "installed": true, "status": "available", "version": null,
#       "version_check_supported": false,
#       "static_checks": { "file_exists": true, "executable": true, "in_path": true }
#     }
#   }

# 5. 通过 /computer/chat 使用 codex-acp
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "prompt": "帮我写一个 Python 脚本",
    "agent_config": {
      "agent_server": {
        "agent_id": "codex-acp",
        "command": "codex-acp",
        "env": {},
        "model_env_bindings": [
          {"env_key": "CODEX_API_KEY", "source": "api_key"},
          {"env_key": "CODEX_BASE_URL", "source": "base_url"},
          {"env_key": "CODEX_MODEL", "source": "default_model"}
        ]
      }
    }
  }'
```

### 5.2 场景二：通过 npm 安装 kimi-cli 并使用

```bash
# 1. 通过 npm 安装 claude-code-acp
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-npm \
  -H "Content-Type: application/json" \
  -d '{
    "project_id": "p1",
    "agent": {
      "agent_id": "claude-code-acp",
      "command": "claude-code-acp"
    },
    "package": "@anthropic-ai/claude-code-acp"
  }'
# → { "status": "available", "version": "1.0.38" }

# 2. 使用 kimi-cli
curl -X POST http://localhost:8087/computer/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "prompt": "帮我分析这段代码的性能问题",
    "agent_config": {
      "agent_server": {
        "agent_id": "kimi-cli",
        "command": "kimi-cli",
        "args": ["--acp"],
        "model_env_bindings": [
          {"env_key": "KIMI_API_KEY", "source": "api_key"},
          {"env_key": "KIMI_BASE_URL", "source": "base_url"},
          {"env_key": "KIMI_MODEL", "source": "default_model"}
        ]
      }
    }
  }'
```

### 5.3 场景三：上传 tar.gz 压缩包

某些 Agent 以压缩包形式分发（包含主程序 + 动态库），直接上传即可。

```bash
# 1. 检查系统信息
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "my-agent"}'
# → { "system_info": { "os": "linux", "arch": "amd64", ... }, "agent": { "installed": false, ... } }

# 2. 上传 tar.gz 压缩包（后端自动检测并解压）
curl -X POST http://localhost:8087/agent-mgmt/agents/install \
  -F 'metadata={"project_id":"p1","agent_id":"my-agent","command":"my-agent","install_type":"BINARY"};type=application/json' \
  -F 'file=@./my-agent-v1.0-linux-amd64.tar.gz;type=application/octet-stream'
# → {
#     "status": "available",
#     "file_type": "tar.gz",
#     "file_count": 3,
#     "binary_path": "/home/user/acp-agent/bin/my-agent"
#   }

# 3. 验证安装
curl -X POST http://localhost:8087/agent-mgmt/agents/check \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "my-agent"}'
# → { "agent": { "installed": true, "status": "available", "static_checks": { ... } } }
```

### 5.4 场景四：通过 URL 安装 Agent（多平台 + 多版本并存）

Agent 发布在 GitHub Releases 或 OSS 上时，业务方每次调用 `install-from-url`，传入版本号和多平台 URL。
agent-runner 自动判断：精确版本已存在则跳过下载（幂等），不存在则安装（多版本并存）。
版本号归一化处理：`v1.0.0` 和 `1.0.0` 视为同一版本。

#### 5.4.1 首次安装（agent 未安装）

```bash
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-url \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent": {
      "agent_id": "codex-acp",
      "command": "codex-acp",
      "version": "1.2.0"
    },
    "platforms": {
      "linux-x86_64": {
        "url": "https://cdn.example.com/codex-acp/1.2.0/codex-acp-linux-amd64.tar.gz"
      },
      "linux-aarch64": {
        "url": "https://cdn.example.com/codex-acp/1.2.0/codex-acp-linux-arm64.tar.gz"
      }
    }
  }'
# → action: "installed", installed: true, version: "1.2.0", previous_version: null
#   平台自动匹配容器系统架构（如 linux-aarch64），下载对应 URL
```

#### 5.4.2 重复调用（版本相同，幂等跳过）

```bash
# 再次调用，version 与已安装版本相同
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-url \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent": {
      "agent_id": "codex-acp",
      "command": "codex-acp",
      "version": "1.2.0"
    },
    "platforms": { ... }
  }'
# → action: "skipped", installed: false, version: "1.2.0"
#   不下载，直接返回现有 agent 信息（零延迟）
```

#### 5.4.3 新版本并存安装

```bash
# 已有 v1.2.0，再安装 v1.3.0（两个版本并存，旧版本不删除）
curl -X POST http://localhost:8087/agent-mgmt/agents/install-from-url \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent": {
      "agent_id": "codex-acp",
      "command": "codex-acp",
      "version": "1.3.0"
    },
    "platforms": {
      "linux-x86_64": {
        "url": "https://cdn.example.com/codex-acp/1.3.0/codex-acp-linux-amd64.tar.gz",
        "sha256": "abc123..."
      },
      "linux-aarch64": {
        "url": "https://cdn.example.com/codex-acp/1.3.0/codex-acp-linux-arm64.tar.gz",
        "sha256": "def456..."
      }
    }
  }'
# → action: "installed", installed: true, version: "1.3.0", previous_version: null
#   新版本安装到独立目录，v1.2.0 保留不变
```

#### 5.4.4 Java 业务方典型调用模式

```java
// 业务方每次使用 agent 前调用，无需关心是否已安装
String version = configCenter.getLatestVersion("codex-acp");
InstallResponse resp = httpClient.post("/agent-mgmt/agents/install-from-url", Map.of(
    "user_id", userId,
    "agent", Map.of(
        "agent_id", "codex-acp",
        "command", "codex-acp",
        "version", version
    ),
    "platforms", Map.of(
        "linux-x86_64", Map.of("url", "https://cdn.example.com/codex-acp/" + version + "/linux-amd64.tar.gz"),
        "linux-aarch64", Map.of("url", "https://cdn.example.com/codex-acp/" + version + "/linux-arm64.tar.gz")
    )
));

// 无需 check → install 两步操作，一个接口搞定
if (resp.action.equals("skipped")) {
    // 该精确版本已安装，直接使用
} else {
    // installed，新版本已就绪（旧版本仍保留）
}
```

### 5.5 场景五：查看所有已安装 Agent

```bash
# 列出所有已安装 Agent（仅用户安装的，不含内置）
curl -X POST http://localhost:8087/agent-mgmt/agents/list \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1"}'
# → {
#     "system_info": { "os": "linux", "arch": "amd64", "platform": "linux/amd64" },
#     "agents": [
#       { "agent_id": "codex-acp", "install_type": "url", "status": "available", ... },
#       { "agent_id": "kimi-cli", "install_type": "npm", "status": "available", ... }
#     ],
#     "total": 2,
#     "install_dir": "/home/user/acp-agent"
#   }
```

### 5.6 场景六：卸载 Agent

```bash
# 卸载 codex-acp（全部版本）
curl -X POST http://localhost:8087/agent-mgmt/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp"}'
# → { "uninstalled": true, "agent_id": "codex-acp", "install_type": "binary" }

# 只卸载指定版本（保留其他版本）
curl -X POST http://localhost:8087/agent-mgmt/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "codex-acp", "version": "1.0.0"}'
# → { "uninstalled": true, "agent_id": "codex-acp", "install_type": "url" }

# 尝试卸载内置 Agent（被拒绝）
curl -X POST http://localhost:8087/agent-mgmt/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"project_id": "p1", "agent_id": "claude-code-acp-ts"}'
# → { "code": "ERR_AGENT_MGMT_BUILTIN_PROTECTED", "message": "Cannot uninstall builtin agent",
#     "data": null, "success": false }
```

---

## 6. API 端点汇总

> **P0-5 设计更新(2026-06)**:全部 7 个端点改用 **POST + body JSON** 接收请求,完全替换旧的 GET/DELETE 路由。
> - 简单 JSON 端点使用 `I18nJsonOrQuery` 提取器(优先 body JSON,兼容 `?project_id=xxx` query 调试)
> - `install` 端点改用 `multipart/form-data`(字段 `file` + 字段 `metadata` JSON 字符串)
> - 路径从 `/{id}` 改为 `/list` / `/get` / `/check` / `/uninstall` 等动词路径,语义更清晰
> - **容器路由**：所有端点支持 `project_id`(向后兼容) 或 `user_id`/`pod_id` + 隔离字段(多租户)两种路由方式
> - **gRPC 转发**：rcoder 通过独立的 `AgentMgmtService`（非 `AgentService`）gRPC 转发到 agent_runner 容器

| 方法 | 路径 | 请求体类型 | gRPC 转发 |
|------|------|-----------|----------|
| POST | `/agent-mgmt/agents/list`             | `ListAgentsRequest` JSON:`{project_id?, user_id?, pod_id?, ...}` | `AgentMgmtService.ListAgents` |
| POST | `/agent-mgmt/agents/get`              | `GetAgentRequest` JSON:`{project_id?, agent_id, version?, ...}` | `AgentMgmtService.GetAgent` |
| POST | `/agent-mgmt/agents/check`            | `CheckAgentRequest` JSON:`{project_id?, agent_id, version?, ...}` | `AgentMgmtService.CheckAgent` |
| POST | `/agent-mgmt/agents/install`          | `multipart/form-data`:`file`(binary) + `metadata`(JSON 字符串) | `AgentMgmtService.InstallAgent` (client streaming) |
| POST | `/agent-mgmt/agents/install-from-url` | `InstallFromUrlRequest` JSON:`{project_id?, agent, platforms, force?, ...}` | rcoder 侧直接处理（不经过 gRPC） |
| POST | `/agent-mgmt/agents/install-from-npm` | `InstallFromPackageManagerRequest` JSON:`{project_id?, agent, package, ...}` | `AgentMgmtService.InstallAgent` (metadata only) |
| POST | `/agent-mgmt/agents/uninstall`        | `UninstallAgentRequest` JSON:`{project_id?, agent_id, version?, ...}` | `AgentMgmtService.UninstallAgent` |

> **`...`** 表示共享的 `RoutingParams` 字段：`user_id`, `pod_id`, `tenant_id`, `space_id`, `isolation_type`。
> 所有端点的请求体都通过 `#[serde(flatten)]` 嵌入 `RoutingParams`（定义在 `shared_types`），`project_id` 在 `RoutingParams` 内，优先路由，无 `project_id` 时按 `pod_id`/`user_id` 定位容器。
>
> **`install-from-url` 特殊说明**：该端点在 rcoder 端直接处理（不通过 gRPC 转发到 agent_runner），rcoder 调用 `agent_install_strategy::do_install_from_url` 在宿主机完成下载和注册表更新。支持多版本并存：每个版本安装到独立的 `{install_dir}/{agent_id}/{version}/` 目录，精确版本已存在时幂等跳过。`/computer/chat` 请求中携带 `version` + `platforms` 时也会自动调用同一个函数。

---

## 7. 错误码汇总

> **实现说明**:agent-runner 端在 gRPC `Status.message` 中以 `"{code}: {msg}"` 前缀传递业务错误码,
> rcoder 转发层(`status_to_app_error`)解析前缀还原为 `AppError`,前端拿到的错误码与直连 agent-runner HTTP 一致。
> rcoder 自身还会产生两个新增错误码(`ERR_PROJECT_NOT_FOUND` / `ERR_AGENT_RUNNER_UNAVAILABLE`),
> 用于"项目不存在"和"agent-runner 不可用"两种转发层失败模式。
> 错误码定义文件: `crates/shared_types_i18n/src/error_codes.rs`

### 7.1 agent-runner 业务错误码(21 个)

| 错误码 | 说明 | 涉及的接口 |
|--------|------|-----------|
| `ERR_AGENT_MGMT_NOT_FOUND` | 指定 agent_id 未安装 | uninstall, check, get |
| `ERR_AGENT_MGMT_ALREADY_INSTALLED` | 重复安装 | install |
| `ERR_AGENT_MGMT_INVALID_MANIFEST` | 安装元数据字段缺失或冲突 | install |
| `ERR_AGENT_MGMT_CHECKSUM_MISMATCH` | 下载/上传文件 SHA256 不匹配 | install-from-url |
| `ERR_AGENT_MGMT_ARCHIVE_BOMB` | 解压后体积超阈值(防 zip bomb) | install, install-from-url |
| `ERR_AGENT_MGMT_PATH_TRAVERSAL` | 压缩包含 `..` 等逃逸路径 | install, install-from-url |
| `ERR_AGENT_MGMT_COMMAND_TIMEOUT` | 安装命令执行超时 | install |
| `ERR_AGENT_MGMT_INSTALL_FAILED` | 安装命令返回非零 | install, install-from-npm, install-from-url |
| `ERR_AGENT_MGMT_UNINSTALL_FAILED` | 文件删除失败 | uninstall |
| `ERR_AGENT_MGMT_CHECK_FAILED` | `which` 验证或版本检查失败 | check |
| `ERR_AGENT_MGMT_BINARY_TOO_LARGE` | 二进制超过 1GB 限制 | install |
| `ERR_AGENT_MGMT_UNSUPPORTED_TYPE` | 不支持的 install_type | install |
| `ERR_AGENT_MGMT_BUILTIN_PROTECTED` | builtin 类型不允许卸载 | uninstall |
| `ERR_AGENT_MGMT_STREAM_TRUNCATED` | client streaming 断流 | install |
| `ERR_AGENT_MGMT_DISK_FULL` | 容器磁盘空间不足 | install, install-from-url |
| `ERR_AGENT_MGMT_PERMISSION_DENIED` | 文件系统权限问题 | install, uninstall |
| `ERR_AGENT_MGMT_UNKNOWN_AGENT` | manifest 中 agent_id 未知 | check |
| `ERR_AGENT_MGMT_INVALID_CHUNK` | streaming chunk 格式错误 | install |
| `ERR_AGENT_MGMT_PLATFORM_NOT_FOUND` | `platforms` 中无匹配当前系统的 URL | install-from-url |
| `ERR_AGENT_MGMT_INVALID_VERSION` | `version` 格式不合法(非语义化版本号) | install-from-url |
| `ERR_AGENT_MGMT_INSTALL_CANCELLED` | 安装被取消(force=true 时取消正在进行的安装) | install-from-url |

### 7.2 rcoder 转发层错误码(2 个,新增)

| 错误码 | 说明 | 涉及的接口 |
|--------|------|-----------|
| `ERR_PROJECT_NOT_FOUND` | URL 中 `project_id` 未注册 / 容器未创建 | 所有接口 |
| `ERR_AGENT_RUNNER_UNAVAILABLE` | gRPC 连接失败 / 容器离线 / Status 无业务码前缀 | 所有接口 |

---

## 8. 实现优先级建议

| 阶段 | 内容 | 复杂度 |
|------|------|--------|
| P0 | 类型定义 + 二进制上传 + 状态检查 | 中 |
| P1 | npm 安装 + 卸载 + 列表 | 中 |
| P2 | URL 安装 + agent_id 自动解析 + 注册表集成 | 中 |
| P3 | gRPC 流式传输优化 + 磁盘空间检查 | 低 |

---
---

# 附录

> 以下内容为实现层面的细节，主要面向 Rust 后端开发者。

## 附录 A: Rust 类型定义

### A.1 请求/响应类型 (shared_types)

文件路径: `crates/shared_types/src/agent_mgmt_types.rs`

> 以下类型定义与代码保持一致（截至 2026-06-15）。

```rust
/// 多租户容器路由参数（所有 /agent-mgmt/* 端点共享）
///
/// 所有请求体通过 `#[serde(flatten)]` 嵌入此结构体。
/// - `project_id` 有值时: 按 project_id 查找（向后兼容）
/// - `user_id` 或 `pod_id` 有值时: 按容器标识查找（多租户模式）
/// - 路由字段全部可选，校验逻辑在 handler 层
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RoutingParams {
    /// 项目 ID（与 user_id/pod_id 二选一）
    pub project_id: Option<String>,
    /// 用户 ID（ComputerAgentRunner 模式，定位容器）
    pub user_id: Option<String>,
    /// 容器复用标识（有值时覆盖 user_id 作为容器标识）
    pub pod_id: Option<String>,
    /// 租户 ID（pod_id 有值时必填，同时接受字符串和数字）
    #[serde(deserialize_with = "flexible_string")]
    pub tenant_id: Option<String>,
    /// 空间 ID（pod_id 有值时必填，同时接受字符串和数字）
    #[serde(deserialize_with = "flexible_string")]
    pub space_id: Option<String>,
    /// 隔离类型：tenant / space / project（pod_id 有值时必填）
    pub isolation_type: Option<String>,
}

/// 默认安装目录常量
pub const DEFAULT_ACP_AGENT_INSTALL_DIR: &str = "/home/user/acp-agent";

/// 二进制上传时单 chunk 大小(1 MB)
pub const UPLOAD_CHUNK_SIZE: usize = 1024 * 1024;

/// 最大允许的二进制文件大小(1 GB)
pub const MAX_BINARY_SIZE: u64 = 1024 * 1024 * 1024;

/// 解压后累计字节上限(1 GB,防 zip bomb)
pub const MAX_EXTRACTED_SIZE: u64 = 1024 * 1024 * 1024;

/// URL 下载超时(10 分钟)
pub const URL_DOWNLOAD_TIMEOUT_SECS: u64 = 600;

/// 系统平台信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SystemInfo {
    pub os: String,
    pub arch: String,
    pub platform: String,
}

/// Agent 安装类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallType {
    Builtin,
    Binary,
    Npm,
    Url,
    #[default]
    Unknown,
}

/// Agent 安装状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallStatus {
    Available,
    Broken,
    NotInstalled,
    #[default]
    Unknown,
}

/// Agent 注册表条目（列表响应）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentInfo {
    pub agent_id: String,
    pub install_type: InstallType,
    pub status: AgentInstallStatus,
    pub version: Option<String>,
    pub binary_path: Option<String>,
    pub installed_at: Option<i64>,  // Unix timestamp 秒
}

/// 列出已安装 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ListAgentsRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
}

/// 列出已安装 Agent 的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListAgentsResponse {
    pub system_info: SystemInfo,
    pub agents: Vec<AgentInfo>,
    pub total: usize,
    pub install_dir: String,
}

/// 静态检查结果
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct StaticCheckResult {
    pub file_exists: bool,
    pub executable: bool,
    pub in_path: bool,
}

/// Agent 详情（check/get 响应）
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AgentDetailInfo {
    pub agent_id: String,
    pub install_type: InstallType,
    pub installed: bool,
    pub status: AgentInstallStatus,
    pub version: Option<String>,
    pub version_check_supported: bool,
    pub static_checks: StaticCheckResult,
}

/// 检查指定 Agent 状态的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct CheckAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    pub agent_id: String,
    pub version: Option<String>,
}

/// 查询单个 Agent 详情的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct GetAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    pub agent_id: String,
    pub version: Option<String>,
}

/// 检查指定 Agent 状态的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckAgentResponse {
    pub system_info: SystemInfo,
    pub agent: AgentDetailInfo,
}

/// Agent 身份信息（所有安装端点共享的 `agent` 子对象）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct AgentIdentity {
    pub agent_id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// URL 安装 Agent 的请求（多平台 + 版本管理）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct InstallFromUrlRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    pub agent: AgentIdentity,
    pub platforms: std::collections::HashMap<String, PlatformEntry>,
    /// 强制重新安装（取消正在进行的安装，重新开始）
    #[serde(default)]
    pub force: bool,
}

/// 包管理器安装 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct InstallFromPackageManagerRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    pub agent: AgentIdentity,
    pub package: String,
}

/// 平台下载信息（platforms map 的值）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PlatformEntry {
    pub url: String,
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

/// 安装操作类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallAction {
    Installed,
    Updated,
    Skipped,
}

/// 安装响应（所有安装端点通用）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallAgentResponse {
    pub agent_id: String,
    pub status: AgentInstallStatus,
    pub binary_path: String,
    pub file_type: String,
    pub file_count: Option<usize>,
    pub file_size: u64,
    pub version: Option<String>,
    pub source_url: Option<String>,
    // === 多平台版本管理字段 ===
    pub action: Option<InstallAction>,
    pub installed: bool,
    pub previous_version: Option<String>,
    pub platform: Option<String>,
}

/// 卸载 Agent 的请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct UninstallAgentRequest {
    #[serde(flatten)]
    pub routing: RoutingParams,
    pub agent_id: String,
    pub version: Option<String>,
}

/// 卸载 Agent 的响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UninstallAgentResponse {
    pub uninstalled: bool,
    pub install_type: InstallType,
    pub agent_id: String,
    pub removed_versions: Vec<String>,
}
```

### A.2 注册表类型 (agent_runner 内部)

> 注册表文件 `registry.json` 存储在安装根目录下，格式为 `Vec<AgentManifest>`（数组，非 map）。

```rust
/// Agent 注册表条目（registry.json 中的每条记录）
///
/// 文件路径: `crates/agent_runner/src/agent_mgmt/installer/mod.rs`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent_id: String,
    pub install_type: String,       // "binary" | "npm" | "url"
    pub binary_path: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub version: Option<String>,
    pub installed_at: i64,          // Unix timestamp 秒
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,   // 如 "linux-x86_64"
}
```

> **注册表文件格式**: `registry.json` 是一个 JSON 数组，每个元素是一个 `AgentManifest`。
> rcoder 的 `list_agents` handler 优先从宿主机直接读取此文件（快速路径），容器未就绪时才回退到 gRPC。

---

## 附录 B: 路由注册

> **P0-5 设计更新(2026-06)**:全部 8 个端点改用 **POST + body JSON** 或 **multipart/form-data(install)**,
> 路径从 `/{id}` 改为动词路径(`/list` / `/get` / `/check` / `/uninstall`)。
> 旧 GET/DELETE 路由(2026-06 之前)已完全下线。
> rcoder 端不做任何 agent 安装/卸载逻辑,只做参数提取 + gRPC 转发。
> agent_runner 端(本地直起模式)与本节描述保持一致(也用 POST + body)。

### B.1 rcoder 主服务路由(对外)

文件路径: `crates/rcoder/src/router.rs`

```rust
use crate::handler;

let install_route = Router::new()
    .route(
        "/agent-mgmt/agents/install",
        post(handler::install_agent),
    )
    .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))  // 1GB
    .layer(tower_http::limit::RequestBodyLimitLayer::new(1024 * 1024 * 1024));

let agent_mgmt_routes = Router::new()
    // 查询类(POST + JSON body, 使用 I18nJsonOrQuery 提取器)
    .route("/agent-mgmt/agents/list",             post(handler::list_agents))
    .route("/agent-mgmt/agents/get",              post(handler::get_agent))
    .route("/agent-mgmt/agents/check",            post(handler::check_agent))
    // 安装(multipart, 1GB 限制)
    .merge(install_route)
    // 安装(URL 和 NPM, POST + JSON body)
    .route("/agent-mgmt/agents/install-from-url",  post(handler::install_from_url))
    .route("/agent-mgmt/agents/install-from-npm",  post(handler::install_from_npm))
    // 卸载(POST + body)
    .route("/agent-mgmt/agents/uninstall",        post(handler::uninstall_agent))
    .with_state(state.clone());
```

> `project_id` 优先从 body JSON 读取,也兼容 `?project_id=xxx` query(`I18nJsonOrQuery` 自动合并,JSON 优先)。
> 所有端点都支持 `RoutingParams` 字段(project_id, user_id, pod_id, tenant_id, space_id, isolation_type)通过 `#[serde(flatten)]` 嵌入请求体。
> 7 个端点全部使用 POST 方法，通过独立的 `AgentMgmtService` gRPC 转发到 agent_runner 容器。

### B.2 agent_runner 容器内路由(本地直起模式)

文件路径: `crates/agent_runner/src/http_server/router.rs`

> 本地直起模式(无 rcoder,直接 `cargo run -p agent_runner`)下保留 HTTP 端点,便于本地开发调试。
> 生产部署通过 K8s/Docker 时,这些路径不会被外部访问——rcoder 通过 gRPC 转发。
> **本节路径与 B.1 完全一致**(都是 POST + body),只是内部直连 installer,不经 gRPC。

```rust
let agent_mgmt_routes = Router::new()
    .route("/agent-mgmt/agents/list",             post(handler::list_agents))
    .route("/agent-mgmt/agents/get",              post(handler::get_agent))
    .route("/agent-mgmt/agents/check",            post(handler::check_agent))
    .route("/agent-mgmt/agents/install",          post(handler::install_agent))
    .route("/agent-mgmt/agents/install-from-url",  post(handler::install_from_url))
    .route("/agent-mgmt/agents/install-from-npm",  post(handler::install_from_npm))
    .route("/agent-mgmt/agents/uninstall",        post(handler::uninstall_agent));
```

### B.3 转发层数据流(新增)

```
外部 HTTP 客户端
  │  POST /agent-mgmt/agents/list  { project_id: "P" }
  ▼
rcoder HTTP handler (handler/agent_mgmt_handler.rs)
  │  1. validate_routing_params(&body.routing)
  │  2. resolve_container_target(state, project_id, routing)
  │     ├─ Path A: project_id 有值 → state.get_project(project_id)
  │     ├─ Path B: user_id/pod_id → runtime.get_container_info_by_identifier()
  │     └─ Path C: 都没有 → ERR_VALIDATION
  │  3. build_ctx(state) → AgentMgmtForwardCtx
  │  4. fwd_list_agents(&ctx, &project)
  ▼
agent_mgmt_forward::list_agents
  │  5. resolve_client(ctx, project)
  │     ├─ get_realtime_container_ip(runtime, container_name, fallback_ip)
  │     │   → 实时查询容器 IP,失败回退 fallback_ip
  │     ├─ format!("{ip}:{GRPC_DEFAULT_PORT}")
  │     └─ pool.get_mgmt_client(addr) → AgentMgmtServiceClient<Channel>
  │  6. client.list_agents(req)
  ▼
gRPC over Docker internal network
  ▼
agent_runner 容器内 AgentMgmtService
  │  list_agents → installer.list_agents() → ListAgentsResponse
  ▼
proto → shared_types::ListAgentsResponse (via conversion.rs)
  ▼
HttpResult<ListAgentsResponse> JSON
```

---

## 附录 C: gRPC Proto 定义

文件路径: `crates/shared_types_grpc/proto/agent.proto`

> **架构说明**: Agent 管理使用**独立的 `AgentMgmtService`**（与 `AgentService` 平级），不混入 `AgentService`。
> 所有安装类型（binary/url/npm）共用一个 `InstallAgent` client streaming RPC，通过 metadata 中的 `install_type` 字段区分。

rcoder 主服务收到请求后，通过 gRPC 转发到容器内的 agent_runner 执行：

```proto
// ========== Agent Service（核心对话服务）==========
service AgentService {
    rpc Chat(ChatRequest) returns (ChatResponse);
    rpc SubscribeProgress(ProgressRequest) returns (stream ProgressEvent);
    rpc CancelSession(CancelRequest) returns (CancelResponse);
    rpc ResolvePermission(ResolvePermissionRequest) returns (ResolvePermissionResponse);
    rpc GetStatus(GetStatusRequest) returns (GetStatusResponse);
    rpc StopAgent(StopAgentRequest) returns (StopAgentResponse);
    rpc GetContainerStatus(GetContainerStatusRequest) returns (GetContainerStatusResponse);
    rpc GetVncStatus(GetVncStatusRequest) returns (GetVncStatusResponse);
}

// ========== Agent Management Service（独立服务，P0-1）==========
//
// 部署在 agent_runner 容器内，提供安装/卸载/检查 agent 二进制的能力。
// 二进制上传通过 client streaming (InstallAgent) 走 1MB chunk。
service AgentMgmtService {
    // 列出已安装的 agent
    rpc ListAgents(ListAgentsRequest) returns (ListAgentsResponse);
    // 上传二进制（支持单文件/tar.gz/zip，client streaming）
    rpc InstallAgent(stream InstallAgentRequest) returns (InstallAgentResponse);
    // 卸载 agent
    rpc UninstallAgent(UninstallAgentRequest) returns (UninstallAgentResponse);
    // 检查指定 agent 状态
    rpc CheckAgent(CheckAgentRequest) returns (CheckAgentResponse);
    // 查询单个 agent 详情（快速）
    rpc GetAgent(GetAgentRequest) returns (GetAgentResponse);
}

// ========== Agent 管理消息类型 ==========

message ListAgentsRequest {
    reserved 1;  // proto3 允许空 message，reserved 防止 field number 被复用
}

message ListAgentsResponse {
    SystemInfo system_info = 1;
    repeated AgentInfo agents = 2;
    int32 total = 3;
    string install_dir = 4;
}

message SystemInfo {
    string os = 1;
    string arch = 2;
    string platform = 3;
}

message AgentInfo {
    string agent_id = 1;
    string install_type = 2;       // "builtin" | "binary" | "npm" | "url" | "unknown"
    string status = 3;             // "available" | "broken" | "not_installed" | "unknown"
    optional string version = 4;
    optional string binary_path = 5;
    optional int64 installed_at = 6;  // Unix timestamp 秒
}

message CheckAgentRequest {
    string agent_id = 1;
    optional string version = 2;
}

message CheckAgentResponse {
    SystemInfo system_info = 1;
    AgentDetailInfo agent = 2;
}

message GetAgentRequest {
    string agent_id = 1;
    optional string version = 2;
}

message GetAgentResponse {
    bool found = 1;
    AgentDetailInfo agent = 2;  // 仅当 found=true 有效
}

message AgentDetailInfo {
    string agent_id = 1;
    string install_type = 2;       // "builtin" | "binary" | "npm" | "url" | "unknown"
    bool installed = 3;
    string status = 4;             // "available" | "broken" | "not_installed" | "unknown"
    optional string version = 5;
    bool version_check_supported = 6;
    StaticCheckResult static_checks = 7;
}

message StaticCheckResult {
    bool file_exists = 1;
    bool executable = 2;
    bool in_path = 3;
}

// 上传 binary 用的 streaming request
// 首包携带 metadata，后续包只携带 data
message InstallAgentRequest {
    message Metadata {
        optional string agent_id = 1;
        optional string command = 2;
        repeated string args = 3;
        optional string sha256 = 4;
        optional InstallType install_type = 5;  // 缺省 BINARY
        optional string source_url = 6;         // URL 安装时必填
        optional string npm_package = 7;        // NPM 安装时必填
        // === 多平台版本管理 ===
        optional string version = 8;            // 期望安装的版本号（semver）
        optional string platforms = 9;          // HashMap<String, PlatformEntry> 的 JSON 序列化
        optional bool force = 10;               // 强制重新安装
    }
    Metadata metadata = 1;   // 首包携带
    bytes data = 2;          // 后续 chunk 的数据
}

message InstallAgentResponse {
    string agent_id = 1;
    string status = 2;             // "available" | "broken" | "not_installed" | "unknown"
    string binary_path = 3;
    string file_type = 4;          // "executable" | "tar.gz" | "zip" | "npm"
    optional int32 file_count = 5;
    int64 file_size = 6;
    optional string version = 7;
    optional string source_url = 8;
    // === 多平台版本管理响应字段 ===
    string action = 20;            // "installed" | "updated" | "skipped"
    bool installed = 21;           // 本次是否实际安装
    string previous_version = 22;  // 更新前版本（首次安装为空）
    string platform = 23;          // 实际匹配的平台 key（如 "linux-x86_64"）
}

message UninstallAgentRequest {
    string agent_id = 1;
    optional string version = 4;   // 可选版本号，不传则卸载全部版本
}

message UninstallAgentResponse {
    bool uninstalled = 1;
    InstallType install_type = 2;
    string agent_id = 3;
    repeated string removed_versions = 5;
}
```

**注意**: `InstallAgent` 使用 client streaming，将大文件分块传输（每块 1MB），避免 gRPC 单次消息大小限制。URL 和 NPM 安装模式下 data 通常为空（仅首包 metadata 有效）。

---

## 附录 D: 实现要点

### D.1 模块划分

```
crates/agent_runner/
├── src/
│   ├── agent_mgmt/                    # 新模块：Agent 管理
│   │   ├── mod.rs
│   │   ├── registry.rs               # 注册表读写 (registry.json)
│   │   ├── installer/
│   │   │   ├── mod.rs
│   │   │   ├── binary_installer.rs   # 二进制安装逻辑（含压缩包解压）
│   │   │   ├── url_installer.rs      # URL 下载安装逻辑
│   │   │   └── npm_installer.rs      # npm 安装逻辑
│   │   ├── checker.rs                # Agent 状态检测
│   │   └── uninstaller.rs            # 卸载逻辑
│   └── http_server/
│       └── handlers/
│           └── agent_mgmt.rs          # HTTP handler 层

crates/shared_types/
└── src/
    └── agent_mgmt_types.rs           # 新文件：请求/响应类型
```

### D.2 关键实现注意事项

1. **DashMap 使用**: 注册表读写需要用 DashMap 保护并发访问，或使用文件锁
2. **gRPC 流式传输**: 二进制上传使用 client streaming，每块 1MB
3. **容器内执行**: npm install 和 chmod 等命令在容器内执行，通过 gRPC 调用
4. **PATH 持久化**: 在容器的 `/etc/profile.d/` 或 `~/.bashrc` 中添加 PATH 配置
5. **版本检测超时**: 版本检查命令超时设为 5 秒，避免阻塞
6. **幂等安装**: 重复安装同一 Agent 时覆盖旧版本，先卸载再安装
7. **压缩包解压依赖**: `flate2` + `tar` 处理 .tar.gz，`zip` crate 处理 .zip
8. **文件类型检测**: 优先通过扩展名判断，扩展名不可靠时用 magic bytes 二次确认
9. **URL 下载实现**: 容器内使用 `reqwest` 或 `curl` 命令下载，流式写入避免内存占用

### D.2.1 压缩包解压实现要点

```toml
[dependencies]
flate2 = "1"     # gzip 解压
tar = "0.4"      # tar 归档
zip = "2"        # zip 解压
```

```rust
use std::io::Read;
use std::path::Path;

/// 检测上传文件类型
fn detect_file_type(filename: &str, data: &[u8]) -> UploadFileType {
    let lower = filename.to_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return UploadFileType::TarGz;
    }
    if lower.ends_with(".zip") {
        return UploadFileType::Zip;
    }
    // magic bytes 兜底
    if data.len() >= 4 && data[0] == 0x50 && data[1] == 0x4b {
        return UploadFileType::Zip;  // PK.. (ZIP magic)
    }
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        return UploadFileType::TarGz; // gzip magic
    }
    UploadFileType::Executable
}

/// 解压压缩包并放置文件
fn extract_and_place(
    file_type: &UploadFileType,
    data: &[u8],
    command: &str,
    agent_id: &str,
    bin_dir: &Path,     // /home/user/acp-agent/bin/
    lib_dir: &Path,     // /home/user/acp-agent/lib/{agent_id}/
) -> Result<Vec<String>> {
    let tmp_dir = tempdir()?;

    match file_type {
        UploadFileType::TarGz => {
            let gz = flate2::read::GzDecoder::new(data);
            let mut archive = tar::Archive::new(gz);
            archive.unpack(&tmp_dir)?;
        }
        UploadFileType::Zip => {
            let cursor = std::io::Cursor::new(data);
            let mut archive = zip::ZipArchive::new(cursor)?;
            archive.extract(&tmp_dir)?;
        }
        _ => unreachable!(),
    }

    // 查找 command 对应的入口可执行文件
    let entry_file = find_entry_file(&tmp_dir, command)?;

    // 移动入口到 bin/
    std::fs::rename(&entry_file, bin_dir.join(command))?;

    // 其余文件移到 lib/{agent_id}/
    let mut extracted = vec![command.to_string()];
    std::fs::create_dir_all(lib_dir)?;
    for entry in walkdir::WalkDir::new(&tmp_dir) {
        // 跳过入口文件，其余移动到 lib/
        // ...
    }

    Ok(extracted)
}
```

**压缩包内入口文件查找逻辑**：

1. 在解压目录根层查找名为 `{command}` 的文件
2. 如果根层没有，递归查找第一个匹配 `{command}` 的可执行文件
3. 如果仍找不到，返回 `ERR_AGENT_MGMT_INVALID_MANIFEST` 错误

### D.3 PATH 管理实现

安装接口在处理时需要动态管理 PATH：

```bash
# 由安装接口自动维护的脚本: /etc/profile.d/acp-agents.sh
# 每次安装/卸载时覆盖重写此文件

# 安装目录（后端约定）
export ACP_AGENTS_DIR="/home/user/acp-agent"

# 将 bin 和 npm-global/bin 加入 PATH
export PATH="${ACP_AGENTS_DIR}/bin:${ACP_AGENTS_DIR}/npm-global/bin:${PATH}"

# npm 全局安装路径配置
if command -v npm &> /dev/null; then
    npm config set prefix "${ACP_AGENTS_DIR}/npm-global" 2>/dev/null || true
fi
```

### D.4 安全考虑

| 风险 | 防护措施 |
|------|---------|
| 上传恶意文件 | 限制文件大小 (1GB)；容器隔离不影响宿主机 |
| npm 包投毒 | 仅安装到容器内；容器重建后清空 |
| PATH 注入 | command 名称验证（只允许字母、数字、连字符、下划线） |
| 磁盘占满 | 安装前检查可用磁盘空间 (至少 1GB 可用) |
| Zip 炸弹 | 解压前检查压缩比，解压大小不超过原始大小的 100 倍 |
| 路径穿越 | 解压时校验文件路径，禁止 `../` 跳出安装目录 |

### D.5 `which` crate - 跨平台可执行文件查找

在检测 Agent 是否安装、PATH 是否可达时，推荐使用 Rust 的 `which` crate 进行跨平台查找。

**Cargo.toml 依赖**:

```toml
[dependencies]
which = "7"
```

**核心 API**:

```rust
use which::{which, which_in};

// 1. which() - 在系统 PATH 中查找可执行文件
//    等同于命令行 `which codex-acp`
match which("codex-acp") {
    Ok(path) => {
        // path: PathBuf, 如 "/home/user/acp-agent/bin/codex-acp"
        println!("found: {}", path.display());
    }
    Err(which::Error::CannotFindBinaryPath) => {
        println!("not found in PATH");
    }
    Err(e) => {
        println!("error: {}", e);
    }
}

// 2. which_in() - 在指定 PATH 中查找（可自定义搜索路径）
//    适用于安装目录尚未加入系统 PATH 时的临时查找
let custom_path = "/home/user/acp-agent/bin:/home/user/acp-agent/npm-global/bin";
match which_in("codex-acp", Some(custom_path), std::env::current_dir().unwrap()) {
    Ok(path) => println!("found: {}", path.display()),
    Err(_) => println!("not found"),
}
```

**在 Agent 检测逻辑中的应用**:

```rust
use which::which;
use std::os::unix::fs::PermissionsExt;

/// 执行静态检查（不运行 agent 进程）
fn perform_static_checks(command: &str, binary_path: &str) -> StaticCheckResult {
    // 1. 检查文件是否存在
    let file_exists = std::path::Path::new(binary_path).exists();

    // 2. 检查可执行权限
    let executable = std::fs::metadata(binary_path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false);

    // 3. 使用 which crate 检查 PATH 可达性
    let in_path = which(command).is_ok();

    StaticCheckResult {
        file_exists,
        executable,
        in_path,
    }
}

/// 获取 which 输出（用于 AgentDetailInfo.which_output）
fn get_which_output(command: &str) -> Option<String> {
    which(command)
        .ok()
        .map(|p| p.display().to_string())
}
```

**`which` crate 的优势**:
- 跨平台：在 Linux/macOS/Windows 上行为一致
- 无副作用：仅检查 PATH 和文件权限，不执行任何二进制
- 快速：不涉及子进程创建，纯文件系统操作
- 与 `std::process::Command` 的 PATH 解析逻辑一致

---

## 附录 E: 与现有系统的兼容性

### E.1 对 `ChatAgentServerConfig` 的增强

现有字段不变，`command` 变为可选（可从注册表自动解析）：

```rust
pub struct ChatAgentServerConfig {
    pub agent_id: Option<String>,        // 现有
    pub command: Option<String>,         // 现有，可选（可从注册表解析）
    pub args: Option<Vec<String>>,       // 现有
    pub env: Option<HashMap<String, String>>, // 现有
    pub model_env_bindings: Vec<ModelEnvBinding>, // 现有
    pub agent_mode: Option<String>,      // 现有
    pub metadata: Option<HashMap<String, String>>, // 现有
}
```

### E.2 对 `prompt_assembler.rs` 的增强

```rust
// 新增：从注册表解析 command
pub fn get_agent_server_config(&self, default_agent_id: &str) -> AgentConfig {
    // 1. 优先使用用户提供的 agent_server 覆盖
    // 2. 如果没有 command，查找注册表
    // 3. 最后回退到默认配置
}
```

### E.3 配置解析优先级

注册表 (`registry.json`) 是运行时的唯一 agent 来源。

**配置解析优先级**（从高到低）：
1. 用户请求中的 `agent_server.command` + `args`
2. 运行时注册表 `registry.json`
3. 未找到 → 报错 `ERR_AGENT_MGMT_NOT_FOUND`
