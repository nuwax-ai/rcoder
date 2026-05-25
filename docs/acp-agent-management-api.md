# ACP Agent Management API 设计文档

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

所有接口挂载在 `/agent-management/` 前缀下，作为独立的 API 路由组。
**所有接口统一使用 POST 方法**，请求参数通过 JSON Body 传递。

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

**`POST /agent-management/agents/list`**

列出容器内所有已安装的 ACP Agent。

#### 请求体 (JSON)

```json
{
  "user_id": "user_123",
  "pod_id": null,
  "tenant_id": null,
  "space_id": null,
  "isolation_type": null
}
```

#### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID（定位容器） |
| pod_id | string | 否 | Pod ID（共享容器模式） |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |

#### 响应

```json
{
  "code": "0000",
  "message": "Success",
  "data": {
    "system_info": {
      "os": "linux",
      "arch": "amd64",
      "platform": "linux/amd64"
    },
    "agents": [
      {
        "agent_id": "claude-code-acp-ts",
        "install_type": "builtin",
        "install_dir": null,
        "command": "claude-code-acp-ts",
        "args": [],
        "status": "available",
        "version": "1.0.38",
        "version_check_supported": true,
        "binary_path": "/usr/local/bin/claude-code-acp-ts",
        "installed_at": null,
        "metadata": {
          "description": "Claude Code ACP Agent (builtin)"
        }
      },
      {
        "agent_id": "codex-acp",
        "install_type": "binary",
        "install_dir": "/home/user/acp-agent",
        "command": "codex-acp",
        "args": [],
        "status": "available",
        "version": "1.2.0",
        "version_check_supported": true,
        "binary_path": "/home/user/acp-agent/bin/codex-acp",
        "installed_at": "2025-05-25T10:30:00Z",
        "metadata": {}
      }
    ],
    "total": 2,
    "install_dir": "/home/user/acp-agent"
  },
  "tid": "abc123def456",
  "success": true
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
| install_type | `"builtin"` \| `"binary"` \| `"npm"` \| null | 安装类型（未安装时为 null） |
| install_dir | string? | 安装根目录（后端内部路径，builtin 为 null） |
| command | string? | 启动命令 |
| args | string[] | 默认启动参数 |
| status | `"available"` \| `"broken"` \| `"not_installed"` \| `"unknown"` | 状态 |
| version | string? | 版本号（无法检测时为 null） |
| version_check_supported | boolean | 是否支持版本检测 |
| binary_path | string? | 可执行文件路径（未安装时为 null） |
| installed_at | string? | 安装时间 (ISO 8601)，builtin 和未安装时为 null |
| metadata | object | 扩展元数据 |

> **builtin Agent 发现机制**: builtin Agent（如 `claude-code-acp-ts`）不在注册表 `registry.json` 中。list 接口会额外扫描编译时嵌入的默认配置（`default_agents.json` / `computer_agent_default.json`），将其中 `enabled: true` 的 Agent 合并到列表结果中，`install_type` 标记为 `builtin`，`install_dir` 为 null。

---

### 3.2 检查指定 Agent 状态

**`POST /agent-management/agents/check`**

检查指定 ACP Agent 是否已安装、版本号、是否可正常执行。

#### 请求体 (JSON)

```json
{
  "user_id": "user_123",
  "agent_id": "codex-acp",
  "pod_id": null,
  "tenant_id": null,
  "space_id": null,
  "isolation_type": null
}
```

#### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID |
| agent_id | string | 是 | Agent 标识符 |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |

#### 响应 - Agent 已安装

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "system_info": {
    "os": "linux",
    "arch": "amd64",
    "platform": "linux/amd64"
  },
  "agent_id": "codex-acp",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "installed": true,
  "version": "1.2.0",
  "version_check_supported": true,
  "version_check_output": "codex-acp v1.2.0",
  "command": "codex-acp",
  "binary_path": "/home/user/acp-agent/bin/codex-acp",
  "which_output": "/home/user/acp-agent/bin/codex-acp",
  "static_checks": {
    "file_exists": true,
    "executable": true,
    "in_path": true
  },
  "installed_at": "2025-05-25T10:30:00Z",
  "metadata": {}
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
  "agent_id": "kimi-cli",
  "install_type": null,
  "install_dir": null,
  "status": "not_installed",
  "installed": false,
  "version": null,
  "version_check_supported": false,
  "version_check_output": null,
  "command": null,
  "binary_path": null,
  "which_output": null,
  "static_checks": {
    "file_exists": false,
    "executable": false,
    "in_path": false
  },
  "installed_at": null,
  "metadata": {}
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
  "agent_id": "kilo-code",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "broken",
  "installed": true,
  "version": null,
  "version_check_supported": true,
  "version_check_output": null,
  "version_check_error": "exit code 1: Permission denied",
  "command": "kilo-code",
  "binary_path": "/home/user/acp-agent/bin/kilo-code",
  "which_output": "/home/user/acp-agent/bin/kilo-code",
  "static_checks": {
    "file_exists": true,
    "executable": true,
    "in_path": true
  },
  "installed_at": "2025-05-24T15:00:00Z",
  "metadata": {}
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
  "agent_id": "codex-acp",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "installed": true,
  "version": null,
  "version_check_supported": false,
  "version_check_output": null,
  "version_check_error": null,
  "command": "codex-acp",
  "binary_path": "/home/user/acp-agent/bin/codex-acp",
  "which_output": "/home/user/acp-agent/bin/codex-acp",
  "static_checks": {
    "file_exists": true,
    "executable": true,
    "in_path": true
  },
  "installed_at": "2025-05-25T10:30:00Z",
  "metadata": {}
}
```

> 当 `version_check_supported: false` 时，`status` 完全由 `static_checks` 三项静态检查决定。
> 三项全为 `true` → `available`；任一为 `false` → `broken`。

#### AgentDetailInfo 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| *(继承 AgentInfo 所有字段)* | | Agent 基本信息 |
| installed | boolean | 是否已安装 |
| version_check_output | string? | 版本检查命令的 stdout |
| version_check_error | string? | 版本检查命令的 stderr |
| which_output | string? | `which {command}` 的输出路径 |
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
   b. 无记录 → 检查是否为 builtin Agent（查找编译时嵌入的 default_agents.json）
      - 是 builtin → 使用默认配置中的 command、binary_path，跳转步骤 3
      - 不是 builtin → 返回 status = "not_installed"
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

**`POST /agent-management/agents/install/binary`**

上传可执行文件或压缩包（`.tar.gz` / `.zip`）作为 ACP Agent。后端自动检测文件类型，压缩包会自动解压。

#### 请求格式

**Content-Type**: `multipart/form-data`

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID |
| agent_id | string | 是 | Agent 标识符（如 "codex-acp"） |
| file | binary | 是 | 可执行文件或压缩包（.tar.gz / .zip） |
| command | string | 否 | 启动命令名（默认 = agent_id）。压缩包时用于定位入口可执行文件 |
| args | string | 否 | 默认启动参数 (JSON array) |
| version_check_command | string | 否 | 版本检查命令 (JSON array)，不传则不检查版本 |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |
| metadata | string | 否 | 扩展元数据 (JSON object) |

#### 支持的文件类型

| 类型 | 扩展名 | 处理方式 |
|------|--------|---------|
| 可执行文件 | 无特定扩展名 | 直接放置到 `bin/{command}` |
| tar.gz 压缩包 | `.tar.gz`, `.tgz` | 自动解压，根据 `command` 字段定位入口可执行文件 |
| zip 压缩包 | `.zip` | 自动解压，根据 `command` 字段定位入口可执行文件 |

> **文件类型检测**: 后端通过文件扩展名 + magic bytes 双重检测，调用方无需额外传参。

#### 请求示例 - 上传单个可执行文件

```bash
curl -X POST http://localhost:8087/agent-management/agents/install/binary \
  -F "user_id=user_123" \
  -F "agent_id=codex-acp" \
  -F "file=@./codex-acp-linux-amd64" \
  -F "command=codex-acp" \
  -F 'version_check_command=["codex-acp","--version"]'
```

#### 请求示例 - 上传 tar.gz 压缩包

```bash
curl -X POST http://localhost:8087/agent-management/agents/install/binary \
  -F "user_id=user_123" \
  -F "agent_id=my-agent" \
  -F "file=@./my-agent-linux-amd64.tar.gz" \
  -F "command=my-agent"
# 后端自动检测为 tar.gz，解压后在目录中找到 my-agent 可执行文件
```

#### 响应 - 单文件

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "codex-acp",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "version": "1.2.0",
  "binary_path": "/home/user/acp-agent/bin/codex-acp",
  "command": "codex-acp",
  "file_type": "executable",
  "file_size": 15728640,
  "file_count": 1,
  "installed_at": "2025-05-25T10:30:00Z"
}
```

#### 响应 - 压缩包

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "my-agent",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "version": null,
  "binary_path": "/home/user/acp-agent/bin/my-agent",
  "command": "my-agent",
  "file_type": "tar.gz",
  "file_size": 8388608,
  "file_count": 3,
  "extracted_files": ["my-agent", "libhelper.so", "config.json"],
  "installed_at": "2025-05-25T10:30:00Z"
}
```

#### 安装响应字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| file_type | `"executable"` \| `"tar.gz"` \| `"zip"` | 检测到的文件类型 |
| file_size | number | 上传文件总大小（字节） |
| file_count | number | 安装的文件数量（单文件为 1，压缩包为解压后的文件数） |
| extracted_files | string[]? | 压缩包解压后的文件列表（仅压缩包时返回） |

#### 处理流程

```
1. 验证参数（user_id, agent_id, file 必填）
2. 验证文件大小（上限 500MB）
3. 定位目标容器（根据 user_id / pod_id / isolation_type）
4. 确保安装目录存在：
   mkdir -p /home/user/acp-agent/bin
5. 自动检测文件类型（扩展名 + magic bytes）:
   - .tar.gz / .tgz → 压缩包
   - .zip → 压缩包
   - 其他 → 单文件
6a. 单文件处理:
    - 将文件写入临时位置：/tmp/acp-install-{uuid}/{command}
    - 移动到目标位置：mv → /home/user/acp-agent/bin/{command}
6b. 压缩包处理:
    - 解压到临时目录：/tmp/acp-install-{uuid}/
    - 在解压目录中查找 command 对应的可执行文件
    - 将入口可执行文件移动到 bin/{command}
    - 其余文件（动态库、配置等）移动到 lib/{agent_id}/
7. 添加执行权限：chmod +x
8. 更新 PATH 持久化脚本
9. 验证安装：which {command} → 确认 PATH 可达
10. 更新注册表 registry.json
11. 清理临时文件
12. 返回安装结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| 文件过大 | ERR_FILE_TOO_LARGE | 超过 500MB 限制 |
| 无执行权限且 chmod 失败 | ERR_PERMISSION_DENIED | 文件系统权限问题 |
| which 验证失败 | ERR_PATH_NOT_FOUND | PATH 配置异常 |
| 容器不存在 | ERR_CONTAINER_NOT_FOUND | 需要先创建容器 |
| 压缩包解压失败 | ERR_ARCHIVE_EXTRACT_FAILED | 文件损坏或非标准格式 |
| 压缩包中找不到入口文件 | ERR_ENTRY_NOT_FOUND | `command` 指定的可执行文件在解压目录中不存在 |

---

### 3.4 通过包管理器安装 Agent

**`POST /agent-management/agents/install/package`**

通过 npm 等包管理器安装 ACP Agent。

#### 请求体 (JSON)

```json
{
  "user_id": "user_123",
  "agent_id": "kimi-cli",
  "package_manager": "npm",
  "package_name": "@anthropic/kimi-cli",
  "package_version": "latest",
  "command": "kimi-cli",
  "args": [],
  "version_check_command": ["kimi-cli", "--version"],
  "registry_url": null,
  "pod_id": null,
  "tenant_id": null,
  "space_id": null,
  "isolation_type": null,
  "metadata": {
    "description": "Kimi CLI ACP Agent"
  }
}
```

#### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID |
| agent_id | string | 是 | Agent 标识符 |
| package_manager | string | 是 | 包管理器：`npm` / `bun` / `pnpm` |
| package_name | string | 是 | 包名（如 `@anthropic/kimi-cli`） |
| package_version | string | 否 | 版本号（默认 `latest`） |
| command | string | 否 | 启动命令名（默认 = agent_id） |
| args | string[] | 否 | 默认启动参数 |
| version_check_command | string[] | 否 | 版本检查命令 |
| registry_url | string | 否 | 自定义 npm registry |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |
| metadata | object | 否 | 扩展元数据 |

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "kimi-cli",
  "install_type": "npm",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "version": "0.3.1",
  "binary_path": "/home/user/acp-agent/npm-global/bin/kimi-cli",
  "command": "kimi-cli",
  "package_name": "@anthropic/kimi-cli",
  "package_version": "latest",
  "install_output": "added 128 packages in 12s",
  "installed_at": "2025-05-25T11:00:00Z"
}
```

#### 处理流程

```
1. 验证参数
2. 定位目标容器
3. 确保 npm 全局安装目录存在且加入 PATH：
   mkdir -p /home/user/acp-agent/npm-global
   export PATH="/home/user/acp-agent/npm-global/bin:$PATH"
   npm config set prefix /home/user/acp-agent/npm-global
4. 执行安装命令（在容器内执行，超时 5 分钟）：
   npm install -g {package_name}@{package_version}
   或带自定义 registry:
   npm install -g {package_name}@{package_version} --registry={registry_url}
5. 更新 PATH 持久化脚本
6. 验证安装：
   which {command}
7. 更新注册表 registry.json
8. 返回安装结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| 包管理器不存在 | ERR_PACKAGE_MANAGER_NOT_FOUND | 容器内未安装 npm/bun/pnpm |
| npm install 失败 | ERR_PACKAGE_INSTALL_FAILED | 网络或包问题 |
| 安装后命令不可用 | ERR_COMMAND_NOT_FOUND | 包未正确导出 bin |
| 超时 | ERR_INSTALL_TIMEOUT | 安装超过 5 分钟 |

---

### 3.5 通过 URL 安装 Agent

**`POST /agent-management/agents/install/url`**

从指定 URL 下载可执行文件或压缩包作为 ACP Agent。后端（容器内）直接下载文件，自动检测文件类型，压缩包会自动解压。

#### 请求体 (JSON)

```json
{
  "user_id": "user_123",
  "agent_id": "codex-acp",
  "url": "https://github.com/zed-industries/codex-acp/releases/download/v1.2.0/codex-acp-linux-arm64",
  "headers": null,
  "checksum": null,
  "command": "codex-acp",
  "args": [],
  "version_check_command": null,
  "pod_id": null,
  "tenant_id": null,
  "space_id": null,
  "isolation_type": null,
  "metadata": {}
}
```

#### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID |
| agent_id | string | 是 | Agent 标识符（如 "codex-acp"） |
| url | string | 是 | 下载 URL（支持 `http://` / `https://`） |
| headers | object | 否 | 自定义 HTTP 请求头（用于认证，如 `{"Authorization": "Bearer xxx"}`） |
| checksum | string | 否 | 文件校验和（格式：`sha256:hex` 或 `sha512:hex`），下载后验证文件完整性 |
| command | string | 否 | 启动命令名（默认 = agent_id） |
| args | string[] | 否 | 默认启动参数 |
| version_check_command | string[] | 否 | 版本检查命令，不传则不检查版本 |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |
| metadata | object | 否 | 扩展元数据 |

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "codex-acp",
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "status": "available",
  "version": null,
  "binary_path": "/home/user/acp-agent/bin/codex-acp",
  "command": "codex-acp",
  "file_type": "executable",
  "file_size": 15728640,
  "file_count": 1,
  "source_url": "https://github.com/zed-industries/codex-acp/releases/download/v1.2.0/codex-acp-linux-arm64",
  "installed_at": "2025-05-25T10:30:00Z"
}
```

#### 与 install/binary 响应的区别

| 字段 | install/binary | install/url |
|------|---------------|-------------|
| source_url | 无 | 有（下载源 URL） |
| file_type | 有 | 有 |
| file_count | 有 | 有 |
| extracted_files | 压缩包时有 | 压缩包时有 |

> 其余字段与 `install/binary` 一致，支持同样的文件类型（可执行文件 / .tar.gz / .zip）和自动解压逻辑。

#### 处理流程

```
1. 验证参数（user_id, agent_id, url 必填）
2. 验证 URL 格式（必须是 http:// 或 https://）
3. 定位目标容器（根据 user_id / pod_id / isolation_type）
4. 确保安装目录存在：
   mkdir -p /home/user/acp-agent/bin
5. 在容器内发起下载（流式写入临时文件，超时 10 分钟）：
   curl -fSL -o /tmp/acp-install-{uuid}/{filename} "{url}"
   或带认证头:
   curl -fSL -H "Authorization: Bearer xxx" -o /tmp/... "{url}"
6. 校验文件大小（上限 500MB）
7. 如果有 checksum 参数，验证文件完整性：
   sha256sum /tmp/acp-install-{uuid}/{filename} == checksum
8. 自动检测文件类型（扩展名 + magic bytes）:
   - 优先从 URL 路径推断扩展名
   - 如果 URL 无扩展名（如 GitHub API），从 Content-Type 或 magic bytes 判断
9a. 单文件处理:
    - 移动到 bin/{command}，chmod +x
9b. 压缩包处理:
    - 解压到临时目录，查找 command 入口文件
    - 入口文件 → bin/{command}
    - 其余文件 → lib/{agent_id}/
10. 更新 PATH 持久化脚本
11. 验证安装：which {command}
12. 更新注册表 registry.json（记录 source_url）
13. 清理临时文件
14. 返回安装结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| URL 格式无效 | ERR_INVALID_URL | 非 http/https 协议或格式错误 |
| 下载失败（HTTP 4xx/5xx） | ERR_DOWNLOAD_FAILED | 远程文件不存在或服务器错误 |
| 下载超时 | ERR_DOWNLOAD_TIMEOUT | 下载超过 10 分钟 |
| 文件过大 | ERR_FILE_TOO_LARGE | 下载文件超过 500MB 限制 |
| 校验和不匹配 | ERR_CHECKSUM_MISMATCH | 下载文件与指定 checksum 不一致 |
| 压缩包解压失败 | ERR_ARCHIVE_EXTRACT_FAILED | 文件损坏或非标准格式 |
| 压缩包中找不到入口文件 | ERR_ENTRY_NOT_FOUND | `command` 指定的可执行文件不存在 |
| 容器不存在 | ERR_CONTAINER_NOT_FOUND | 需要先创建容器 |

---

### 3.6 卸载 Agent

**`POST /agent-management/agents/uninstall`**

卸载已安装的 ACP Agent。

#### 请求体 (JSON)

```json
{
  "user_id": "user_123",
  "agent_id": "codex-acp",
  "pod_id": null,
  "tenant_id": null,
  "space_id": null,
  "isolation_type": null
}
```

#### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| user_id | string | 是 | 用户 ID |
| agent_id | string | 是 | Agent 标识符 |
| pod_id | string | 否 | Pod ID |
| tenant_id | string | 否 | 租户 ID |
| space_id | string | 否 | 空间 ID |
| isolation_type | string | 否 | 隔离类型 |

#### 响应

> 以下仅展示 `data` 字段内容，省略外层 HttpResult 包装。

```json
{
  "agent_id": "codex-acp",
  "uninstalled": true,
  "install_type": "binary",
  "install_dir": "/home/user/acp-agent",
  "removed_path": "/home/user/acp-agent/bin/codex-acp"
}
```

#### 处理逻辑

```
1. 读取 registry.json，检查是否有记录
2. 根据 install_type 执行卸载：
   - binary: rm /home/user/acp-agent/bin/{command}
   - npm: npm uninstall -g {package_name}
   - builtin: 拒绝卸载（返回错误）
3. 从注册表移除记录
4. 更新 PATH 持久化脚本（如果已无其他 Agent，从 PATH 中移除）
5. 返回卸载结果
```

#### 错误场景

| 错误 | code | 说明 |
|------|------|------|
| Agent 不在注册表中 | ERR_AGENT_NOT_FOUND | 指定 agent_id 未安装 |
| 尝试卸载内置 Agent | ERR_BUILTIN_AGENT | builtin 类型不允许卸载 |
| 文件删除失败 | ERR_UNINSTALL_FAILED | 文件系统权限或 IO 错误 |
| npm uninstall 失败 | ERR_PACKAGE_UNINSTALL_FAILED | npm 命令执行失败 |

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
   c. 如果没找到 → 查找默认配置 default_agents.json
   d. 都没有 → 报错 ERR_AGENT_NOT_FOUND
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
curl -X POST http://localhost:8087/agent-management/agents/check \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "codex-acp"}'
# → {
#     "system_info": { "os": "linux", "arch": "arm64", "platform": "linux/arm64" },
#     "installed": false,
#     "status": "not_installed"
#   }

# 3. 根据 system_info 下载对应架构的二进制并上传
#    平台是 linux/arm64，选择 codex-acp-linux-arm64
curl -X POST http://localhost:8087/agent-management/agents/install/binary \
  -F "user_id=user_123" \
  -F "agent_id=codex-acp" \
  -F "file=@./codex-acp-linux-arm64" \
  -F "command=codex-acp"
# → { "status": "available", "version": null, "file_type": "executable", "file_count": 1 }

# 4. 再次确认状态（静态检查通过，version = null）
curl -X POST http://localhost:8087/agent-management/agents/check \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "codex-acp"}'
# → {
#     "system_info": { "os": "linux", "arch": "arm64", "platform": "linux/arm64" },
#     "installed": true, "status": "available", "version": null,
#     "version_check_supported": false,
#     "static_checks": { "file_exists": true, "executable": true, "in_path": true }
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
# 1. 通过 npm 安装 kimi-cli
curl -X POST http://localhost:8087/agent-management/agents/install/package \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent_id": "kimi-cli",
    "package_manager": "npm",
    "package_name": "@anthropic/kimi-cli",
    "package_version": "latest",
    "command": "kimi-cli",
    "version_check_command": ["kimi-cli", "--version"]
  }'
# → { "status": "available", "version": "0.3.1" }

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
curl -X POST http://localhost:8087/agent-management/agents/check \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "my-agent"}'
# → { "system_info": { "os": "linux", "arch": "amd64", ... }, "installed": false }

# 2. 上传 tar.gz 压缩包（后端自动检测并解压）
curl -X POST http://localhost:8087/agent-management/agents/install/binary \
  -F "user_id=user_123" \
  -F "agent_id=my-agent" \
  -F "file=@./my-agent-v1.0-linux-amd64.tar.gz" \
  -F "command=my-agent"
# → {
#     "status": "available",
#     "file_type": "tar.gz",
#     "file_count": 3,
#     "extracted_files": ["my-agent", "libhelper.so", "config.json"],
#     "binary_path": "/home/user/acp-agent/bin/my-agent"
#   }

# 3. 验证安装
curl -X POST http://localhost:8087/agent-management/agents/check \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "my-agent"}'
# → { "installed": true, "status": "available", "static_checks": { ... } }
```

### 5.4 场景四：从 GitHub Releases URL 安装 Agent

Agent 发布在 GitHub Releases 或 OSS 上时，直接给 URL 即可安装。

```bash
# 1. 检查系统信息，确定应该下载哪个架构
curl -X POST http://localhost:8087/agent-management/agents/check \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "codex-acp"}'
# → { "system_info": { "os": "linux", "arch": "arm64", ... }, "installed": false }

# 2. 通过 URL 安装（容器直接下载）
curl -X POST http://localhost:8087/agent-management/agents/install/url \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent_id": "codex-acp",
    "url": "https://github.com/zed-industries/codex-acp/releases/download/v1.2.0/codex-acp-linux-arm64",
    "command": "codex-acp"
  }'
# → {
#     "status": "available",
#     "file_type": "executable",
#     "file_size": 15728640,
#     "source_url": "https://github.com/.../codex-acp-linux-arm64"
#   }

# 3. 从需要认证的私有仓库安装
curl -X POST http://localhost:8087/agent-management/agents/install/url \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user_123",
    "agent_id": "my-private-agent",
    "url": "https://oss.example.com/agents/my-agent-v1.0.tar.gz",
    "headers": { "Authorization": "Bearer eyJhbGciOi..." },
    "checksum": "sha256:a1b2c3d4e5f6...",
    "command": "my-agent"
  }'
# → {
#     "status": "available",
#     "file_type": "tar.gz",
#     "file_count": 3,
#     "source_url": "https://oss.example.com/agents/my-agent-v1.0.tar.gz"
#   }
```

### 5.5 场景五：查看所有已安装 Agent

```bash
# 列出所有已安装 Agent
curl -X POST http://localhost:8087/agent-management/agents/list \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123"}'
# → {
#     "system_info": { "os": "linux", "arch": "amd64", "platform": "linux/amd64" },
#     "agents": [
#       { "agent_id": "claude-code-acp-ts", "install_type": "builtin", "status": "available", ... },
#       { "agent_id": "codex-acp", "install_type": "binary", "status": "available", ... },
#       { "agent_id": "kimi-cli", "install_type": "npm", "status": "available", ... }
#     ],
#     "total": 3,
#     "install_dir": "/home/user/acp-agent"
#   }
```

### 5.6 场景六：卸载 Agent

```bash
# 卸载 codex-acp
curl -X POST http://localhost:8087/agent-management/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "codex-acp"}'
# → { "uninstalled": true, "install_type": "binary" }

# 尝试卸载内置 Agent（被拒绝）
curl -X POST http://localhost:8087/agent-management/agents/uninstall \
  -H "Content-Type: application/json" \
  -d '{"user_id": "user_123", "agent_id": "claude-code-acp-ts"}'
# → { "code": "ERR_BUILTIN_AGENT", "message": "Cannot uninstall builtin agent",
#     "data": null, "success": false }
```

---

## 6. API 端点汇总

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/agent-management/agents/list` | 列出所有已安装 Agent |
| POST | `/agent-management/agents/check` | 检查指定 Agent 状态 |
| POST | `/agent-management/agents/install/binary` | 上传二进制 Agent |
| POST | `/agent-management/agents/install/package` | 包管理器安装 Agent |
| POST | `/agent-management/agents/install/url` | 通过 URL 下载安装 Agent |
| POST | `/agent-management/agents/uninstall` | 卸载 Agent |
| POST | `/computer/chat` | 聊天（通过 `agent_server` 使用已安装 Agent） |

---

## 7. 错误码汇总

| 错误码 | 说明 | 涉及的接口 |
|--------|------|-----------|
| ERR_AGENT_NOT_FOUND | 指定 agent_id 未安装 | uninstall, check |
| ERR_BUILTIN_AGENT | builtin 类型不允许卸载 | uninstall |
| ERR_UNINSTALL_FAILED | 文件删除失败 | uninstall |
| ERR_PACKAGE_UNINSTALL_FAILED | npm uninstall 失败 | uninstall |
| ERR_FILE_TOO_LARGE | 上传文件超过 500MB 限制 | install/binary, install/url |
| ERR_PERMISSION_DENIED | 文件系统权限问题 | install/binary, install/url |
| ERR_PATH_NOT_FOUND | PATH 配置异常，which 验证失败 | install/binary, install/package, install/url |
| ERR_CONTAINER_NOT_FOUND | 目标容器不存在 | 所有接口 |
| ERR_PACKAGE_MANAGER_NOT_FOUND | 容器内未安装 npm/bun/pnpm | install/package |
| ERR_PACKAGE_INSTALL_FAILED | npm install 失败 | install/package |
| ERR_COMMAND_NOT_FOUND | 安装后命令不可用 | install/package, install/url |
| ERR_INSTALL_TIMEOUT | 安装超时（超过 5 分钟） | install/package |
| ERR_ARCHIVE_EXTRACT_FAILED | 压缩包解压失败（文件损坏或非标准格式） | install/binary, install/url |
| ERR_ENTRY_NOT_FOUND | 压缩包中找不到 `command` 指定的入口可执行文件 | install/binary, install/url |
| ERR_INVALID_URL | URL 格式无效（非 http/https 协议） | install/url |
| ERR_DOWNLOAD_FAILED | 下载失败（HTTP 4xx/5xx 或网络错误） | install/url |
| ERR_DOWNLOAD_TIMEOUT | 下载超时（超过 10 分钟） | install/url |
| ERR_CHECKSUM_MISMATCH | 下载文件与指定 checksum 不一致 | install/url |

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

```rust
/// 默认安装目录常量
pub const DEFAULT_ACP_AGENT_INSTALL_DIR: &str = "/home/user/acp-agent";

/// 系统平台信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SystemInfo {
    /// 操作系统（如 "linux", "darwin", "windows"）
    pub os: String,
    /// CPU 架构（如 "amd64", "arm64"）
    pub arch: String,
    /// 平台标识（"{os}/{arch}"，如 "linux/amd64"）
    pub platform: String,
}

impl SystemInfo {
    /// 从当前运行环境获取系统信息
    pub fn current() -> Self {
        let os = std::env::consts::OS.to_string();
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64".to_string(),
            "aarch64" => "arm64".to_string(),
            other => other.to_string(),
        };
        let platform = format!("{}/{}", os, arch);
        Self { os, arch, platform }
    }
}

/// 上传文件类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UploadFileType {
    /// 单个可执行文件
    Executable,
    /// tar.gz 压缩包
    TarGz,
    /// zip 压缩包
    Zip,
}

/// 安装类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallType {
    /// 内置 Agent（编译时嵌入）
    Builtin,
    /// 上传二进制文件
    Binary,
    /// 通过包管理器安装
    Npm,
}

/// Agent 状态
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallStatus {
    /// 可用
    Available,
    /// 已安装但损坏
    Broken,
    /// 未安装
    NotInstalled,
    /// 状态未知
    Unknown,
}

/// 列出已安装 Agent 请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListInstalledAgentsRequest {
    pub user_id: String,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// 检查 Agent 状态请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckAgentStatusRequest {
    pub user_id: String,
    pub agent_id: String,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// 卸载 Agent 请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UninstallAgentRequest {
    pub user_id: String,
    pub agent_id: String,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// 二进制安装请求（multipart/form-data 字段）
///
/// 由于二进制上传使用 multipart/form-data 而非 JSON，
/// 此结构体用于解析表单字段（不包含 file 字段）。
/// file 字段通过 axum 的 Multipart 提取器单独处理。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallAgentBinaryForm {
    pub user_id: String,
    pub agent_id: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    pub version_check_command: Option<Vec<String>>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    #[serde(default)]
    pub metadata: Option<HashMap<String, String>>,
}

/// Agent 信息（用于列表和详情响应）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentInfo {
    pub agent_id: String,
    pub install_type: Option<AgentInstallType>,
    pub install_dir: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub status: AgentInstallStatus,
    pub version: Option<String>,
    pub version_check_supported: bool,
    pub binary_path: Option<String>,
    pub installed_at: Option<String>,
    pub metadata: HashMap<String, String>,
}

/// Agent 详情（包含版本检查输出等调试信息）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentDetailInfo {
    /// 容器系统信息（操作系统、CPU 架构）
    pub system_info: SystemInfo,
    #[serde(flatten)]
    pub info: AgentInfo,
    pub installed: bool,
    pub version_check_output: Option<String>,
    pub version_check_error: Option<String>,
    pub which_output: Option<String>,
    /// 静态检查结果（不执行 agent 进程）
    /// 当 version_check_supported = false 时，status 完全由此字段决定
    pub static_checks: StaticCheckResult,
}

/// 静态检查结果（文件系统层面，不执行 agent 进程）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StaticCheckResult {
    /// binary_path 文件是否存在
    pub file_exists: bool,
    /// 文件是否有可执行权限
    pub executable: bool,
    /// which {command} 是否找到
    pub in_path: bool,
}

/// 包管理器安装请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallAgentPackageRequest {
    pub user_id: String,
    pub agent_id: String,
    pub package_manager: String,
    pub package_name: String,
    #[serde(default = "default_latest")]
    pub package_version: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub version_check_command: Option<Vec<String>>,
    pub registry_url: Option<String>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// URL 安装请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstallAgentFromUrlRequest {
    pub user_id: String,
    pub agent_id: String,
    /// 下载 URL（http:// 或 https://）
    pub url: String,
    /// 自定义 HTTP 请求头（用于认证等）
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// 文件校验和（格式："sha256:hex" 或 "sha512:hex"）
    pub checksum: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub version_check_command: Option<Vec<String>>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// 安装结果
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentInstallResult {
    pub agent_id: String,
    pub install_type: AgentInstallType,
    pub install_dir: String,
    pub status: AgentInstallStatus,
    pub version: Option<String>,
    pub binary_path: String,
    pub command: String,
    pub installed_at: String,
    /// 安装过程的输出（npm install 日志等）
    pub install_output: Option<String>,
    /// 检测到的上传文件类型
    pub file_type: Option<UploadFileType>,
    /// 文件大小（字节）
    pub file_size: Option<u64>,
    /// 安装的文件数量（单文件为 1，压缩包为解压后的文件数）
    pub file_count: Option<u32>,
    /// 压缩包解压后的文件列表（仅压缩包时返回）
    pub extracted_files: Option<Vec<String>>,
    /// 下载源 URL（仅 url 安装时返回）
    pub source_url: Option<String>,
    /// 包名（仅 npm 安装）
    pub package_name: Option<String>,
    /// 包版本（仅 npm 安装）
    pub package_version: Option<String>,
}

/// Agent 列表响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentListResponse {
    /// 容器系统信息（操作系统、CPU 架构）
    pub system_info: SystemInfo,
    pub agents: Vec<AgentInfo>,
    pub total: usize,
    pub install_dir: String,
}

/// 卸载结果
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentUninstallResult {
    pub agent_id: String,
    pub uninstalled: bool,
    pub install_type: AgentInstallType,
    pub install_dir: String,
    pub removed_path: Option<String>,
}

fn default_latest() -> String {
    "latest".to_string()
}
```

### A.2 注册表类型 (agent_runner 内部)

```rust
/// Agent 注册表条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistryEntry {
    pub agent_id: String,
    pub install_type: AgentInstallType,
    pub install_dir: String,
    pub binary_path: String,
    pub command: String,
    pub args: Vec<String>,
    pub version: Option<String>,
    pub version_check_command: Option<Vec<String>>,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
    pub installed_at: String,
    pub updated_at: String,
    pub metadata: HashMap<String, String>,
}

/// Agent 注册表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// 默认安装根目录
    pub install_dir: String,
    /// 已安装 Agent 映射表（key = agent_id）
    pub agents: HashMap<String, AgentRegistryEntry>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            install_dir: DEFAULT_ACP_AGENT_INSTALL_DIR.to_string(),
            agents: HashMap::new(),
        }
    }
}
```

---

## 附录 B: 路由注册

### B.1 rcoder 主服务路由

文件路径: `crates/rcoder/src/router.rs`

```rust
let agent_mgmt_routes = Router::new()
    // 列出已安装 Agent
    .route("/agent-management/agents/list", post(handler::list_installed_agents))
    // 检查指定 Agent 状态
    .route("/agent-management/agents/check", post(handler::check_agent_status))
    // 上传二进制 Agent（限制 500MB）
    .route(
        "/agent-management/agents/install/binary",
        post(handler::install_agent_binary)
            .layer(RequestBodyLimitLayer::new(500 * 1024 * 1024)),
    )
    // 通过包管理器安装 Agent
    .route(
        "/agent-management/agents/install/package",
        post(handler::install_agent_package),
    )
    // 通过 URL 安装 Agent
    .route(
        "/agent-management/agents/install/url",
        post(handler::install_agent_from_url),
    )
    // 卸载 Agent
    .route(
        "/agent-management/agents/uninstall",
        post(handler::uninstall_agent),
    );
```

### B.2 agent_runner 容器内路由

文件路径: `crates/agent_runner/src/http_server/router.rs`

```rust
let agent_mgmt_routes = Router::new()
    .route("/agent-management/agents/list", post(handler::list_installed_agents_local))
    .route("/agent-management/agents/check", post(handler::check_agent_status_local))
    .route("/agent-management/agents/install/binary", post(handler::install_agent_binary_local))
    .route("/agent-management/agents/install/package", post(handler::install_agent_package_local))
    .route("/agent-management/agents/install/url", post(handler::install_agent_from_url_local))
    .route("/agent-management/agents/uninstall", post(handler::uninstall_agent_local));
```

---

## 附录 C: gRPC Proto 定义

文件路径: `crates/shared_types/proto/agent.proto`

rcoder 主服务收到请求后，通过 gRPC 转发到容器内的 agent_runner 执行：

```proto
// 新增 RPC 方法
service AgentService {
    // ... 现有方法 ...
    
    // Agent 管理
    rpc ListInstalledAgents(ListInstalledAgentsRequest) returns (ListInstalledAgentsResponse);
    rpc CheckAgentStatus(CheckAgentStatusRequest) returns (CheckAgentStatusResponse);
    rpc InstallAgentBinary(stream InstallAgentBinaryChunk) returns (InstallAgentResult);
    rpc InstallAgentPackage(InstallAgentPackageRequest) returns (InstallAgentResult);
    rpc InstallAgentFromUrl(InstallAgentFromUrlRequest) returns (InstallAgentResult);
    rpc UninstallAgent(UninstallAgentRequest) returns (UninstallAgentResponse);
}

// ========== Agent 管理消息类型 ==========

message ListInstalledAgentsRequest {
    // 安装目录由后端约定，无需客户端传入
}

message ListInstalledAgentsResponse {
    SystemInfoProto system_info = 1;
    repeated AgentInfoProto agents = 2;
    int32 total = 3;
    string install_dir = 4;
}

message SystemInfoProto {
    string os = 1;
    string arch = 2;
    string platform = 3;
}

message AgentInfoProto {
    string agent_id = 1;
    string install_type = 2;       // "builtin" | "binary" | "npm"
    string install_dir = 3;        // 安装目录（builtin 为空）
    string command = 4;
    repeated string args = 5;
    string status = 6;             // "available" | "broken" | "not_installed" | "unknown"
    string version = 7;            // 可为空
    bool version_check_supported = 8;
    string binary_path = 9;
    string installed_at = 10;      // ISO 8601
    map<string, string> metadata = 11;
}

message CheckAgentStatusRequest {
    string agent_id = 1;
}

message CheckAgentStatusResponse {
    SystemInfoProto system_info = 1;
    AgentInfoProto agent_info = 2;
    bool installed = 3;
    string version_check_output = 4;
    string version_check_error = 5;
    string which_output = 6;
    // 静态检查结果（不执行 agent 进程）
    bool file_exists = 7;
    bool executable = 8;
    bool in_path = 9;
}

message InstallAgentBinaryChunk {
    // 第一个 chunk 包含元数据，后续 chunk 包含文件内容
    oneof payload {
        InstallAgentBinaryMetadata metadata = 1;
        bytes file_data = 2;
    }
}

message InstallAgentBinaryMetadata {
    string agent_id = 1;
    string command = 2;
    repeated string args = 3;
    repeated string version_check_command = 4;
    map<string, string> metadata = 5;
    string filename = 6;           // 原始文件名
    int64 total_size = 7;          // 文件总大小（字节）
    string file_type = 8;          // "executable" | "tar.gz" | "zip"（由 rcoder 检测后传入）
}

message InstallAgentPackageRequest {
    string agent_id = 1;
    string package_manager = 2;    // "npm" | "bun" | "pnpm"
    string package_name = 3;
    string package_version = 4;
    string command = 5;
    repeated string args = 6;
    repeated string version_check_command = 7;
    string registry_url = 8;
    map<string, string> metadata = 9;
}

message InstallAgentFromUrlRequest {
    string agent_id = 1;
    string url = 2;                // 下载 URL
    map<string, string> headers = 3;  // 自定义 HTTP 请求头
    string checksum = 4;           // "sha256:hex" 或 "sha512:hex"
    string command = 5;
    repeated string args = 6;
    repeated string version_check_command = 7;
    map<string, string> metadata = 8;
}

message InstallAgentResult {
    string agent_id = 1;
    string install_type = 2;
    string install_dir = 3;
    string status = 4;
    string version = 5;
    string binary_path = 6;
    string command = 7;
    string installed_at = 8;
    string install_output = 9;     // npm install 日志等
    string file_type = 10;         // "executable" | "tar.gz" | "zip"
    int64 file_size = 11;
    int32 file_count = 12;         // 安装的文件数量
    repeated string extracted_files = 13;  // 压缩包解压后的文件列表
    string source_url = 14;        // 下载源 URL（仅 url 安装）
    string package_name = 15;      // 仅 npm
    string package_version = 16;   // 仅 npm
    bool success = 17;
    string error_code = 18;
    string error_message = 19;
}

message UninstallAgentRequest {
    string agent_id = 1;
}

message UninstallAgentResponse {
    string agent_id = 1;
    bool uninstalled = 2;
    string install_type = 3;
    string install_dir = 4;
    string removed_path = 5;
    bool success = 6;
    string error_code = 7;
    string error_message = 8;
}
```

**注意**: `InstallAgentBinary` 使用 client streaming，将大文件分块传输（每块 1MB），避免 gRPC 单次消息大小限制。

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
3. 如果仍找不到，返回 `ERR_ENTRY_NOT_FOUND` 错误

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
| 上传恶意文件 | 限制文件大小 (500MB)；容器隔离不影响宿主机 |
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

### E.3 对默认配置 JSON 的兼容

现有的 `default_agents.json` 和 `computer_agent_default.json` 保持不变。注册表 (`registry.json`) 是运行时的额外层，优先级高于编译时默认配置。

**配置解析优先级**（从高到低）：
1. 用户请求中的 `agent_server.command` + `args`
2. 运行时注册表 `registry.json`
3. 编译时默认配置 `default_agents.json`
