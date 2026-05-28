# DevComputer - ACP Agent 开发调试接口需求文档

## 1. 背景与动机

### 1.1 现状

当前 RCoder 平台通过 `/computer/*` 系列接口提供 Computer Agent 的完整能力，包括：
- 容器管理（pod ensure/restart/stop/status）
- Agent 聊天（chat）
- 实时进度（SSE progress）
- 会话控制（cancel/stop）
- 权限审批（notify-resolved）

在现有的 `/computer/chat` 接口中，`ChatAgentConfig.agent_server` 字段**已经支持自定义 agent 命令**：

```json
{
  "user_id": "dev_user_001",
  "prompt": "Hello, test my custom agent",
  "agent_config": {
    "agent_server": {
      "command": "my-custom-agent",
      "args": ["--debug"],
      "env": {"LOG_LEVEL": "debug"},
      "agent_id": "my-custom-agent"
    }
  }
}
```

### 1.2 问题

虽然技术上可行，但存在以下问题：

1. **缺少调试专用的命名空间**：开发调试请求和生产请求混在同一个路由前缀下，无法区分流量
2. **缺少调试友好的增强能力**：现有接口的错误信息对开发者不够详细，缺少 agent 进程的 stderr 日志查看能力
3. **缺少快速迭代流程**：开发者修改 agent 代码后，需要完整的 pod restart 流程才能重新测试
4. **缺少调试专属的生命周期管理**：容器应有更短的超时清理策略，避免资源浪费

### 1.3 目标

提供一组 `/devcomputer/*` 前缀的专用调试接口，让 ACP Agent 开发者能够：

- 通过自定义 command 启动自己开发的 ACP Agent
- 使用与生产接口一致的请求/响应格式进行调试
- 通过独立的路由前缀区分调试流量和生产流量
- 获得调试增强的错误反馈（agent stderr 日志、启动诊断信息等）
- **自动检测 agent 文件变化并热重载**，无需手动 stop + 重新 chat

---

## 2. 用户角色与使用场景

### 2.1 目标用户

ACP Agent 开发者 —— 使用 ACP (Agent Client Protocol) 开发自定义 AI Agent 的工程师。

### 2.2 核心使用场景

**场景 1：启动自定义 Agent 并调试**

开发者将自己的 ACP Agent 编译产物放在约定目录 `/home/user/acp-agent/{agent-name}/` 下（如 `/home/user/acp-agent/codex-acp/codex-acp`），通过命令名即可启动：

```
开发者 → POST /devcomputer/chat
  {
    "user_id": "dev_001",
    "prompt": "分析这段代码",
    "agent_config": {
      "agent_server": {
        "command": "my-python-agent",
        "env": {"DEBUG": "true"},
        "agent_id": "my-python-agent"
      }
    }
  }
→ 返回 {project_id, session_id}

开发者 → GET /devcomputer/progress/{session_id}
→ SSE 流接收实时进度

开发者 → POST /devcomputer/chat
  {
    "user_id": "dev_001",
    "project_id": "<上次的project_id>",
    "session_id": "<上次的session_id>",
    "prompt": "再试一次"
  }
→ 复用已有会话继续调试
```

**场景 2：修改代码后自动热重载（Auto-Reload）**

开发者修改 agent 代码并重新编译后，再次发送 chat 请求，系统**自动检测文件变化并重载**，无需手动 stop：

```
开发者修改 agent 源码
→ 重新编译: cargo build --release
→ 编译产物更新: /home/user/acp-agent/my-rust-agent/my-rust-agent
→ POST /devcomputer/chat (相同 command)
→ 系统自动检测到 agent 二进制文件已更新
→ 系统自动停止旧 agent → 启动新 agent
→ 返回响应 (auto_reload.reloaded = true)
```

**场景 3：手动重启 Agent（不使用自动重载）**

```
开发者修改 agent 代码
→ POST /devcomputer/agent/stop   (停止当前 agent，容器保留)
→ 重新编译 agent
→ POST /devcomputer/chat          (用新命令重新启动 agent)
```

**场景 4：排查 Agent 启动失败**

```
开发者 → POST /devcomputer/chat
→ 返回失败，包含详细的启动诊断信息：
  - which 命令检查结果（agent 命令是否存在于 PATH）
  - agent 进程的 exit code
  - agent 进程的 stderr 输出
  - /home/user/acp-agent/{agent_name}/ 目录结构检查
```

---

## 3. ACP Agent 开发约定

### 3.1 目录结构约定

为统一自定义 ACP Agent 的管理，开发者需遵守以下极简约定：

```
/home/user/acp-agent/                        ← 所有自定义 agent 的根目录
├── codex-acp/                               ← agent 目录（以 agent 名称命名）
│   └── codex-acp                            ← 可执行文件（与目录同名）
│
├── opencode/                                ← agent 目录
│   └── opencode                             ← 可执行文件（与目录同名）
│
└── my-custom-agent/                         ← agent 目录
    └── my-custom-agent                      ← 可执行文件（与目录同名）
```

**规则**：
- 每个自定义 agent 对应一个**同名子目录**（如 `codex-acp/`）
- 子目录内放置**与目录同名的可执行文件**（如 `codex-acp/codex-acp`）
- 子目录内可存放该 agent 需要的其他附属文件（配置、数据等），不做约束

不关心：
- 源文件用什么语言编写（Rust、Go、Python、Node、Java...）
- 源文件的项目结构（Cargo.toml、package.json、go.mod...）
- 编译过程（在宿主机上编译，把产物放进来即可）

只关心：
- 文件可执行（`chmod +x`）
- 能通过命令名启动 ACP Agent 进程

### 3.2 可执行文件类型示例

| 类型 | 示例 | 说明 |
|------|------|------|
| 编译型二进制 | `codex-acp/codex-acp`（Rust 编译产物） | 直接放入，无需运行时依赖 |
| 静态链接二进制 | `opencode/opencode`（Go 编译产物） | 直接放入，无动态库依赖 |
| Shell 启动脚本 | `my-agent/my-agent`（`#!/bin/bash` + `exec node ...`） | 需要容器内已安装对应运行时 |
| 带运行时依赖的二进制 | `my-agent/my-agent`（动态链接 Python/C 库） | 需要容器内已安装对应运行时 |

### 3.3 PATH 配置

`/home/user/acp-agent` 目录会被加入到系统 `PATH` 环境变量中。这使得：

- 可以通过 `which codex-acp` 验证命令是否存在
- 可以通过命令名直接启动 agent，无需写全路径
- `agent_config.agent_server.command` 只需填命令名

**实现位置**（两处，缺一不可）：

| 文件 | 修改 | 作用 |
|------|------|------|
| `Dockerfile` | `ENV PATH="/home/user/acp-agent:$PATH"` + `RUN mkdir -p /home/user/acp-agent` | 容器默认 PATH，所有子进程可见 |
| `start-up.sh` | `ENV_EXPORTS` 中的 PATH 增加 `/home/user/acp-agent` | agent_runner 进程启动时的 PATH |

```bash
# 容器内的 PATH 配置
export PATH="/home/user/acp-agent:$PATH"

# 验证命令存在
$ which codex-acp
/home/user/acp-agent/codex-acp/codex-acp

$ which opencode
/home/user/acp-agent/opencode/opencode

# 直接按命令名启动
$ codex-acp
$ opencode
```

### 3.4 路径映射关系

在开发调试场景中，路径映射链如下：

```
宿主机 (Host)
  computer-project-workspace/{user_id}/acp-agent/
    ├── codex-acp/                   ← 开发者在宿主机上编译/放置的可执行文件
    │   └── codex-acp
    └── opencode/
        └── opencode
    │
    │  docker-compose bind mount
    ▼
RCoder 容器
  /app/computer-project-workspace/{user_id}/acp-agent/
    │
    │  auto-inject bind mount (host → /home/user)
    ▼
Agent Runner 容器
  /home/user/acp-agent/              ← 已加入 PATH
    ├── codex-acp/
    │   └── codex-acp                ← /home/user/acp-agent/codex-acp/codex-acp
    └── opencode/
        └── opencode                 ← /home/user/acp-agent/opencode/opencode
```

> **注意**：`/home/user/acp-agent` 实际上是 workspace bind mount 的子目录。开发者在宿主机上编译好可执行文件后放入 workspace，通过 bind mount 自动同步到容器内。

### 3.5 命令解析规则

当 `agent_config.agent_server.command` 为命令名（非绝对路径）时，系统按以下顺序解析：

```
1. 检查 /home/user/acp-agent/{command}/{command} 是否存在且可执行
   → 存在: 使用该路径
   → 不存在: 进入下一步

2. 使用 which {command} 在 PATH 中查找
   → 找到: 使用该路径
   → 未找到: 返回错误，附带诊断信息
```

---

## 4. 接口设计

### 4.1 路由总览

| 路由 | 方法 | 对应生产接口 | 说明 |
|------|------|-------------|------|
| `/devcomputer/chat` | POST | `/computer/chat` | 发送聊天请求，支持自定义 agent command |
| `/devcomputer/progress/{session_id}` | GET | `/computer/progress/{session_id}` | SSE 实时进度流 |
| `/devcomputer/agent/stop` | POST | `/computer/agent/stop` | 停止 Agent（保留容器） |
| `/devcomputer/agent/status` | POST | `/computer/agent/status` | 查询 Agent 状态 |
| `/devcomputer/agent/session/cancel` | POST | `/computer/agent/session/cancel` | 取消正在执行的任务 |
| `/devcomputer/notify-resolved` | POST | `/computer/notify-resolved` | 权限审批回调 |

**设计原则**：`/devcomputer/*` 聚焦 **agent 级别**的调试操作（启动/停止/重载），不提供 pod 级别管理接口。原因：

1. **共享容器**：`/devcomputer/chat` 和 `/computer/chat` 基于 `user_id` 共享同一个容器（如 `computer-agent-runner-dev_001`）
2. **自动创建**：`/devcomputer/chat` 内部已调用 `get_or_create_container_for_user()`，容器不存在时自动创建
3. **自愈能力**：极端场景下容器被销毁，下次 chat 请求会自动重新拉起
4. **职责分离**：pod 管理复用 `/computer/pod/*` 接口，避免重复实现

**不提供的接口及原因**：

| 接口 | 不提供的理由 |
|------|-------------|
| `/devcomputer/pod/ensure` | chat 请求自动确保容器存在，无需显式调用 |
| `/devcomputer/pod/restart` | 调试维度是 agent 不是容器，agent 重启用 stop + chat；容器重启用 `/computer/pod/restart` |
| `/devcomputer/pod/status` | 查询的是同一个容器，直接用 `/computer/pod/status` |

### 4.2 请求/响应格式

所有 `/devcomputer/*` 接口的**请求参数和响应结构与对应的 `/computer/*` 接口完全一致**，无需定义新的数据结构。

#### 4.2.1 `/devcomputer/chat`

**请求**：`ComputerChatRequest`（与 `/computer/chat` 完全相同）

```rust
// 同 shared_types::ComputerChatRequest
{
  "user_id": "string (必填)",
  "project_id": "string (可选，自动生成)",
  "prompt": "string (必填)",
  "session_id": "string (可选)",
  "attachments": [],
  "model_provider": {},
  "agent_config": {
    "agent_server": {
      "command": "string (自定义 agent 启动命令，如 'my-rust-agent')",
      "args": ["string"],
      "env": {"key": "value"},
      "agent_id": "string",
      "agent_mode": "yolo | ask",
      "model_env_bindings": []
    },
    "context_servers": {},
    "resource_limits": {}
  },
  "pod_id": "string (可选)",
  "tenant_id": "string (可选)",
  "space_id": "string (可选)",
  "isolation_type": "string (可选)"
}
```

**响应**：`HttpResult<ChatResponse>`（与 `/computer/chat` 完全相同）

```rust
{
  "success": true,
  "data": {
    "project_id": "string",
    "session_id": "string"
  },
  "code": "string",
  "message": "string",
  "tid": "string"
}
```

#### 4.2.2 `/devcomputer/progress/{session_id}`

**请求**：路径参数 `session_id`

**响应**：SSE 事件流（与 `/computer/progress/{session_id}` 完全相同）

```
event: session_start
data: {"type": "session_start", "session_id": "...", ...}

event: assistant_message
data: {"type": "assistant_message", "content": "...", ...}

event: tool_use
data: {"type": "tool_use", "tool_name": "...", ...}
```

#### 4.2.3 其他接口

`agent/stop`、`agent/status`、`agent/session/cancel`、`notify-resolved`、`pod/ensure`、`pod/restart`、`pod/status` 的请求和响应格式与对应的 `/computer/*` 接口完全一致，不再赘述。

### 4.3 调试增强能力（差异化特性）

以下能力是 `/devcomputer` 相比 `/computer` 的增强，用于提升开发调试体验：

#### 4.3.1 详细的错误诊断信息

当 agent 启动失败或执行出错时，响应中包含更详细的诊断信息：

```rust
{
  "success": false,
  "code": "AGENT_START_FAILED",
  "message": "Agent 进程启动失败",
  "data": {
    "diagnostics": {
      "exit_code": 127,
      "stderr_tail": "bash: my-custom-agent: command not found\n",
      "command": "my-custom-agent",
      "which_result": null,
      "expected_path": "/home/user/acp-agent/my-custom-agent/my-custom-agent",
      "agent_dir_exists": false,
      "args": [],
      "working_dir": "/home/user/proj_123"
    }
  }
}
```

> **注意**：此增强依赖 agent_runner 侧 gRPC 响应的扩展。在 Phase 1 中可以先利用现有的错误信息，后续 Phase 再增强诊断能力。

#### 4.3.2 容器共享说明

`/devcomputer/*` 和 `/computer/*` **共享同一个容器**，容器由 `user_id` 标识，与路由前缀无关：

| 路由 | 容器名称 | 说明 |
|------|---------|------|
| `/computer/chat` | `computer-agent-runner-{user_id}` | 生产接口 |
| `/devcomputer/chat` | `computer-agent-runner-{user_id}` | 调试接口，同一个容器 |

**示例**：`user_id=dev_001` 的用户，无论调用 `/computer/chat` 还是 `/devcomputer/chat`，都使用同一个容器 `computer-agent-runner-dev_001`。

**设计理由**：
- 调试的是 **agent 进程**（在容器内启动的子进程），不是容器本身
- 开发者在 `/computer/chat` 创建的容器环境中开发调试自定义 agent
- `/devcomputer/*` 只是路由命名空间隔离，用于区分调试流量和生产流量

---

## 5. 自动热重载（Auto-Reload）机制

### 5.1 设计目标

开发者修改 agent 代码并重新编译后，再次发送 `/devcomputer/chat` 请求时，系统自动检测 agent 文件是否变化。如果文件变化，自动停止旧 agent 并重新启动，确保使用最新的 agent 版本。

**核心价值**：将开发者迭代循环从"修改 → 手动 stop → 重新 chat"简化为"修改 → 重新 chat"。

### 5.2 检测位置：agent_runner 内部

自动重载的检测逻辑在 **agent_runner 内部**（`handle_chat_core()` 中）执行：

```
rcoder → gRPC Chat → agent_runner
                         │
                         ▼
                   handle_chat_core()
                         │
                         ├── [1] AGENT_REGISTRY 查找现有 agent
                         │
                         ├── [2] Auto-Reload 检测 ← 在此处执行
                         │   ├── 解析 command → 定位 agent 可执行文件
                         │   ├── 获取文件 mtime+size → 对比存储的快照
                         │   └── 如果变化 → graceful_stop 旧 agent
                         │                   → 从 AGENT_REGISTRY 移除
                         │
                         ├── [3] 正常流程（get_or_create_session）
                         │   └── 旧 agent 已移除，自动创建新 session
                         │
                         └── [4] 发送 prompt → 返回响应
```

**选择 agent_runner 内部的原因**：

| 维度 | agent_runner 内部 | rcoder 侧（备选方案） |
|------|------------------|-------------------|
| **路径复杂度** | 直接访问 `/home/user/acp-agent/` | 需要 bind mount 路径映射链 |
| **RPC 次数** | 1 次（gRPC Chat 内部处理） | 2 次（先 StopAgent，再 Chat） |
| **架构内聚性** | agent 生命周期完全内聚在 agent_runner | 跨容器编排，职责分散 |
| **现有模式复用** | 复用 `is_model_config_changed()` 模式 | 需要新增跨容器协调逻辑 |
| **对 rcoder 的侵入** | 零侵入 | 需要在 handler 中插入检测逻辑 |

### 5.3 与现有架构的融合点

agent_runner 已有两个高度相关的机制，auto-reload 是对它们的自然扩展：

#### 已有机制 1：Model Config 变化检测

在 `AcpSessionManager::get_or_create_session()` 中，已有 model config 变化检测：

```rust
// session_manager.rs 现有逻辑
let model_changed = existing.is_model_config_changed(&new_model_provider);
if model_changed {
    // 模型变了 → 销毁旧 session，创建新 session
    // 旧 agent 子进程会被 AgentLifecycleGuard 清理
}
```

Auto-reload 复用同样的模式，增加 **agent 可执行文件变化检测**：

```rust
// 扩展后的逻辑
let model_changed = existing.is_model_config_changed(&new_model_provider);
let agent_binary_changed = existing.is_agent_binary_changed(&new_command);

if model_changed || agent_binary_changed {
    // 模型或 agent 二进制变了 → 销毁旧 session，创建新 session
}
```

#### 已有机制 2：Busy Agent 自动取消

在 `handle_chat_core()` 中，已有 agent 忙时自动取消当前任务：

```rust
// chat_handler.rs 现有逻辑
if agent_info.status == AgentStatus::Active || agent_info.status == AgentStatus::Pending {
    cancel_current_task(&cancel_tx, &session_id, &project_id).await;
    // 取消后复用同一个 agent session
}
```

Auto-reload 在此基础上增加：如果 agent 二进制文件变化，不只是取消任务，而是**停止整个 agent 进程并重建**。

### 5.4 监控目标文件解析

基于 ACP Agent 目录约定（第 3 节），agent_runner 直接在容器文件系统上解析目标文件：

```rust
/// ACP Agent 根目录（容器内路径）
const ACP_AGENT_ROOT: &str = "/home/user/acp-agent";

/// 解析 agent 可执行文件路径
fn resolve_agent_binary(command: &str, _args: &[String]) -> Option<PathBuf> {
    // 规则 1: command 名 → 检查约定目录下的同名子目录中的同名可执行文件
    //   如 command = "codex-acp"
    //   → /home/user/acp-agent/codex-acp/codex-acp
    let acp_binary = PathBuf::from(ACP_AGENT_ROOT)
        .join(command)
        .join(command);
    if acp_binary.exists() {
        return Some(acp_binary);
    }

    // 规则 2: command 是绝对路径，直接检查
    if command.starts_with('/') {
        let path = PathBuf::from(command);
        if path.exists() {
            return Some(path);
        }
    }

    // 规则 3: 使用 which 在 PATH 中查找
    which::which(command).ok()
}
```

> **说明**：auto-reload 监控的是**可执行文件本身**（如 `codex-acp/codex-acp`）。不关心源语言和编译方式，只关心最终产物是否发生了变化。
```

### 5.5 文件变化检测策略

**检测方式：mtime + size 双因子**

```rust
/// 文件快照 — 存储在 ProjectAndAgentInfo 中
#[derive(Debug, Clone)]
pub struct AgentBinarySnapshot {
    /// 可执行文件路径（容器内路径）
    pub path: PathBuf,
    /// 文件修改时间
    pub mtime: SystemTime,
    /// 文件大小（字节）
    pub size: u64,
}

/// 判断 agent 二进制文件是否变化
fn is_agent_binary_changed(
    current: &AgentBinarySnapshot,
    stored: &Option<AgentBinarySnapshot>,
) -> bool {
    match stored {
        None => false,  // 首次启动，不算变化
        Some(stored) => {
            if current.path != stored.path {
                return true;  // 命令本身变了（指向不同文件）
            }
            current.mtime != stored.mtime || current.size != stored.size
        }
    }
}
```

### 5.6 编译竞态处理（Stability Check）

**问题**：开发者执行 `cargo build` 时，二进制文件正在被写入。如果 agent_runner 在这个窗口内检测到"文件变了"，可能会启动一个不完整的二进制。

**方案：稳定性校验**

```
文件变化检测流程:
    │
    ├── 第一次检查: mtime + size → snapshot_A
    │
    ├── 等待 stability_window (默认 500ms)
    │
    ├── 第二次检查: mtime + size → snapshot_B
    │
    ├── if snapshot_A == snapshot_B:
    │       → 文件已稳定，确认变化
    │
    └── if snapshot_A != snapshot_B:
            → 文件仍在写入，继续等待
            → 最多重试 3 次 (500ms × 3 = 1.5s)
            → 超过重试次数: 放弃本次自动重载，日志警告
```

### 5.7 完整处理流程

auto-reload 检测插入在 `handle_chat_core()` 中，位于 AGENT_REGISTRY 查找之后、PendingGuard 创建之前：

```
handle_chat_core(input, context)
    │
    ├── 1. AGENT_REGISTRY 查找现有 agent（复用现有逻辑）
    │      agent_info = AGENT_REGISTRY.get_agent_info(&project_id)
    │
    ├── 2. Busy Agent 自动取消（复用现有逻辑）
    │      if agent_info.status == Active/Pending:
    │          cancel_current_task(...)
    │
    ├── 3. [Auto-Reload 检测] ← 新增逻辑
    │      │
    │      ├── 3a. 前置条件判断
    │      │   if agent_info is None → 跳过（首次启动，无需检测）
    │      │   if command 未指定（使用默认 agent） → 跳过
    │      │   if auto_reload.enabled == false → 跳过
    │      │
    │      ├── 3b. 解析 agent 可执行文件路径
    │      │   binary_path = resolve_agent_binary(command, args)
    │      │   if binary_path is None → 跳过，日志警告
    │      │
    │      ├── 3c. 获取当前文件快照
    │      │   current = AgentBinarySnapshot::from_path(binary_path)
    │      │
    │      ├── 3d. 与存储的快照对比
    │      │   stored = agent_info.agent_binary_snapshot
    │      │   changed = is_agent_binary_changed(current, stored)
    │      │
    │      ├── 3e. 如果文件变化（或 auto_reload.force == true）:
    │      │   │
    │      │   ├── 稳定性检查 (stability check)
    │      │   │   sleep(stability_window_ms)
    │      │   │   verify = AgentBinarySnapshot::from_path(binary_path)
    │      │   │   if verify != current: retry (最多 3 次)
    │      │   │
    │      │   ├── 确认变化，执行热重载:
    │      │   │   tracing::info!(
    │      │   │       "Auto-reload triggered: agent={}, "
    │      │   │       "file={}, mtime changed",
    │      │   │       command, binary_path.display()
    │      │   │   );
    │      │   │
    │      │   ├── 优雅停止旧 agent 进程
    │      │   │   if let Some(stop_handle) = &agent_info.stop_handle {
    │      │   │       stop_handle.graceful_stop().await;  // SIGTERM → 3s → SIGKILL
    │      │   │   }
    │      │   │
    │      │   ├── 从 AGENT_REGISTRY 移除旧 session
    │      │   │   AGENT_REGISTRY.remove(&project_id);
    │      │   │
    │      │   └── 设置 reload 标记
    │      │       reload_info = Some(AutoReloadInfo { reloaded: true, ... })
    │      │
    │      └── 3f. 如果文件未变化:
    │          正常流程（复用已有 agent）
    │
    ├── 4. PendingGuard 创建（复用现有逻辑）
    │      let pending_guard = PendingGuard::new(&AGENT_REGISTRY, &project_id);
    │
    ├── 5. Prompt 构建 + process_request（复用现有逻辑）
    │      // 旧 agent 已从 registry 移除，
    │      // get_or_create_session() 会自动创建新 session + 启动新 agent
    │
    ├── 6. 存储文件快照（agent 启动成功后）
    │      agent_info.agent_binary_snapshot = Some(current);
    │
    └── 7. 返回响应（附带 reload_info）
```

### 5.8 数据结构变更

#### agent_runner 侧变更

```rust
// ===== shared_types/src/model/agent_model.rs =====

/// ProjectAndAgentInfo — 新增 agent_binary_snapshot 字段
pub struct ProjectAndAgentInfo {
    pub project_id: String,
    pub session_id: SessionId,
    pub prompt_tx: mpsc::Sender<PromptRequest>,
    pub cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
    pub model_provider: Option<ModelProviderConfig>,
    pub request_id: Option<String>,
    pub status: AgentStatus,
    pub last_activity: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub stop_handle: Option<Arc<dyn AgentLifecycle>>,

    // ========== 新增字段 ==========

    /// agent 可执行文件的快照，用于 auto-reload 变化检测
    pub agent_binary_snapshot: Option<AgentBinarySnapshot>,
}

/// agent 可执行文件快照
#[derive(Debug, Clone)]
pub struct AgentBinarySnapshot {
    /// 可执行文件路径（容器内路径）
    pub path: PathBuf,
    /// 文件修改时间
    pub mtime: SystemTime,
    /// 文件大小（字节）
    pub size: u64,
}
```

#### 请求扩展（AutoReloadConfig）

```rust
/// 自动重载配置（嵌入 agent_config）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoReloadConfig {
    /// 是否启用自动重载（devcomputer 默认 true，computer 默认 false）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 强制重载，不检查文件变化
    #[serde(default)]
    pub force: bool,
    /// 显式指定监控的文件路径
    /// 如果为空，则根据 command 自动推断
    pub watch_files: Option<Vec<String>>,
    /// 稳定性检查窗口（毫秒），默认 500
    #[serde(default = "default_stability_window")]
    pub stability_window_ms: u64,
}
```

> `AutoReloadConfig` 嵌入到 `ChatAgentConfig` 中，通过 gRPC 请求传递到 agent_runner。

#### 响应增强

```rust
/// gRPC ChatResponse 新增字段
pub struct GrpcChatResponse {
    // ... 现有字段 ...

    /// 自动重载信息
    pub auto_reload: Option<AutoReloadInfo>,
}

pub struct AutoReloadInfo {
    /// 是否触发了热重载
    pub reloaded: bool,
    /// 触发重载的文件列表
    pub changed_files: Vec<String>,
    /// 旧 agent 是否成功停止
    pub old_agent_stopped: bool,
    /// 实际监控的目标文件列表
    pub watched_files: Vec<String>,
}
```

### 5.9 代码改动清单

| 文件 | 变更 | 复杂度 |
|------|------|--------|
| `shared_types/agent_model.rs` | `ProjectAndAgentInfo` 新增 `agent_binary_snapshot` 字段 | 低 |
| `shared_types/chat_agent_config.rs` | `ChatAgentConfig` 新增 `auto_reload` 字段 | 低 |
| `shared_types/proto/agent.proto` | `ChatResponse` 新增 `auto_reload` 字段 | 低 |
| `agent_runner/service/chat_handler.rs` | `handle_chat_core()` 中插入 auto-reload 检测逻辑 | 中 |
| `agent_runner/grpc/agent_service_impl.rs` | gRPC 响应中填充 `auto_reload` 字段 | 低 |
| `agent_runner/src/auto_reload.rs` | **新增**：文件快照、路径解析、stability check 等工具函数 | 中 |

> **rcoder 侧无需任何修改**。auto-reload 完全在 agent_runner 内部闭环。

### 5.10 边界场景处理

| 场景 | 处理方式 |
|------|---------|
| **首次请求**（无存储快照） | 正常启动 agent，存储文件快照，不触发重载 |
| **agent 未在运行** | `AGENT_REGISTRY` 中无记录，跳过检测，正常启动 |
| **命令文件不存在** | `resolve_agent_binary()` 返回 None，跳过自动重载，后续由 launcher 报错 |
| **稳定性检查超时**（文件持续写入） | 放弃本次自动重载，日志警告，复用旧 agent |
| **旧 agent stop 失败** | `graceful_stop()` 内部已有 SIGTERM → 3s → SIGKILL 机制，强制终止 |
| **command 未指定（使用默认 agent）** | 跳过自动重载（默认 agent 不需要热重载） |
| **开发者想强制重载** | `auto_reload.force: true`，跳过文件检查直接 stop + 重建 |
| **开发者想关闭自动重载** | `auto_reload.enabled: false`，跳过检测 |
| **command 本身变了**（如从 agent-A 切换到 agent-B） | `binary_path` 不同，直接判定为变化 |
| **同一 project 连续请求** | 快照存储在 `ProjectAndAgentInfo` 中，随 agent 生命周期管理 |

---

## 6. 技术方案概述

### 6.1 架构设计原则

- **核心逻辑复用**：`/devcomputer/*` 与 `/computer/*` 共享相同的业务逻辑实现，避免代码重复
- **差异通过配置注入**：通过 Auto-Reload 配置参数区分调试/生产行为
- **最小侵入性**：不修改现有 `/computer/*` 接口的行为

### 6.2 实现策略

#### 路由层

在 `router.rs` 中新增 `devcomputer_routes` 路由组：

```rust
let devcomputer_routes = Router::new()
    .route("/devcomputer/chat", post(handler::handle_devcomputer_chat))
    .route("/devcomputer/progress/{session_id}", get(handler::devcomputer_agent_progress))
    .route("/devcomputer/agent/stop", post(handler::devcomputer_agent_stop))
    .route("/devcomputer/agent/status", post(handler::devcomputer_agent_status))
    .route("/devcomputer/agent/session/cancel", post(handler::devcomputer_agent_session_cancel))
    .route("/devcomputer/notify-resolved", post(handler::devcomputer_notify_resolved))
    .with_state(state.clone());
```

#### Handler 层

每个 `devcomputer` handler 是对现有 `computer` handler 的薄包装，核心差异：

1. **Auto-Reload 配置注入**：为 devcomputer 请求默认开启 auto_reload

```
handle_devcomputer_chat()
    │
    ├── 注入 auto_reload 默认配置（enabled=true）
    │
    └── 调用 handle_computer_chat_core()  ← 复用现有核心逻辑
        └── gRPC Chat 转发 → agent_runner 内部处理 auto-reload
```

#### 容器管理层

`ComputerContainerManager` 中的方法需要支持通过参数区分调试/生产模式：

- 容器名称前缀不同
- 其余逻辑（创建、查找、销毁）完全复用

#### agent_runner 层

auto-reload 逻辑在 agent_runner 内部实现，具体变更见第 5.9 节。核心改动：

- `handle_chat_core()` 中插入文件变化检测逻辑
- `ProjectAndAgentInfo` 新增 `agent_binary_snapshot` 字段存储文件快照
- 新增 `auto_reload.rs` 模块封装路径解析、快照对比、stability check 等工具函数

rcoder 侧**无需任何修改**。auto-reload 完全在 agent_runner 内部闭环。

### 6.3 核心复用关系

```
┌─────────────────────────────────────────────────────┐
│                   Router Layer                       │
│                                                      │
│  /computer/*          /devcomputer/*                 │
│  routes               routes                         │
│     │                     │                          │
│     ▼                     ▼                          │
│  computer handlers    devcomputer handlers           │
│     │                     │                          │
│     │         ┌───────────┘                          │
│     ▼         ▼                                      │
│  ┌────────────────────────────────┐                  │
│  │   Shared Core Logic (rcoder)  │                  │
│  │   - Container management      │                  │
│  │   - gRPC forwarding           │                  │
│  │   - DuckDB state management   │                  │
│  │   - SSE stream proxying       │                  │
│  └──────────────┬─────────────────┘                  │
│                 │ gRPC Chat                          │
│                 ▼                                    │
│  ┌────────────────────────────────┐                  │
│  │   agent_runner (容器内)        │                  │
│  │                                │                  │
│  │  handle_chat_core()            │                  │
│  │   ├── AGENT_REGISTRY 查找      │                  │
│  │   ├── [Auto-Reload 检测] ← 新增（第 5 节）        │
│  │   ├── get_or_create_session    │                  │
│  │   └── process_request          │                  │
│  └────────────────────────────────┘                  │
└─────────────────────────────────────────────────────┘
```

---

## 7. 数据流

### 7.1 `/devcomputer/chat` 完整数据流

```
Client
  │
  │ POST /devcomputer/chat
  │ {user_id, prompt, agent_config: {agent_server: {command: "my-agent"}}}
  ▼
┌─────────────────────────────────────────────────┐
│  rcoder: handle_devcomputer_chat                │
│                                                  │
│  1. 校验请求参数                                  │
│  2. 自动生成 project_id（如未提供）                │
│  3. 并发保护（pod_creating guard）                 │
│  4. 创建/获取容器（与 /computer/chat 共享同一容器） │
│     - 容器名: computer-agent-runner-{user_id}     │
│  5. 确保 DuckDB 映射                              │
│  6. 创建工作空间目录                               │
│  7. gRPC GetStatus 预检                           │
│  8. gRPC Chat RPC 转发                            │
│     - 携带 agent_config（含 command + auto_reload）│
│  9. 更新 DuckDB session 映射                      │
│  10. 返回 ChatResponse (含 auto_reload 信息)      │
└──────────────────────┬──────────────────────────┘
                       │ gRPC Chat
                       ▼
┌─────────────────────────────────────────────────┐
│  agent_runner (容器内):                          │
│                                                  │
│  1. 接收 gRPC Chat 请求                           │
│  2. 解析 agent_config.agent_server               │
│  3. AGENT_REGISTRY 查找现有 agent                │
│                                                  │
│  4. [Auto-Reload 检测] (第 5 节)                  │
│     - 解析 command → 可执行文件路径               │
│       /home/user/acp-agent/{command}/{command}    │
│     - 获取文件 mtime+size → 对比存储快照           │
│     - stability check (500ms 稳定性校验)           │
│     - 如果变化:                                   │
│       → graceful_stop 旧 agent                   │
│       → AGENT_REGISTRY.remove(project_id)         │
│                                                  │
│  5. get_or_create_session()                      │
│     - 旧 agent 已移除 → 自动创建新 session         │
│     - 启动新 agent 子进程                          │
│       CommandWrap::with_new(command, ...)         │
│       stdin/stdout piped (ACP 协议通信)            │
│     - ACP 协议握手，获取 session_id               │
│     - 存储 agent_binary_snapshot                  │
│                                                  │
│  6. 转发 prompt 到 agent                          │
│  7. 返回 GrpcChatResponse (含 auto_reload 信息)   │
└──────────────────────┬──────────────────────────┘
                       │
                       ▼
                  Client 收到响应
                  {project_id, session_id, auto_reload}
```

### 7.2 `/devcomputer/progress/{session_id}` 数据流

```
Client
  │
  │ GET /devcomputer/progress/{session_id}
  ▼
┌─────────────────────────────────────────────────┐
│  rcoder: devcomputer_agent_progress             │
│                                                  │
│  1. 通过 session_id 查询 DuckDB 获取容器信息       │
│  2. 获取容器实时 IP（Docker API）                  │
│  3. gRPC SubscribeProgress streaming RPC         │
│  4. 将 ProgressEvent 转换为 SSE Event             │
│  5. 返回 SSE 流给客户端                            │
└─────────────────────────────────────────────────┘
```

---

## 8. 非目标

以下是本需求**不涉及**的范围：

1. **不修改现有 `/computer/*` 接口**的任何行为
2. **不新增 ACP 协议能力**（如新的工具类型、新的消息格式）
3. **不提供 Web UI**（仅提供 HTTP API，UI 由调用方自行实现）
4. **不提供多用户协作调试**（每个 user_id 一个独立容器）
5. **不提供实时文件监听**（使用 on-request 轮询检查，不使用 inotify/notify）

---

## 9. 约束与假设

### 9.1 约束

- 容器共享同一个 Docker/K8s 运行时环境
- 调试接口需要经过与生产接口相同的鉴权中间件（API Key）
- 容器的资源配额受全局配置限制
- 自定义 ACP Agent 必须放在 `/home/user/acp-agent/{agent_name}/` 目录下
- 可执行文件必须与 agent 目录同名（如 `my-agent/my-agent`）
- `/home/user/acp-agent` 目录已加入容器 PATH 环境变量

### 9.2 假设

- 开发者的自定义 agent 命令已正确实现 ACP 协议（stdin/stdout 通信）
- 开发者的 agent 代码和编译产物通过 workspace bind mount 同步到容器内
- 开发者了解 ACP 协议的基本交互流程
- 开发者的编译过程不会持续超过 stability check 的最大等待时间（1.5 秒）

---

## 10. 分期规划

### Phase 1：核心接口（最小可用）

实现调试的基础路由和核心操作，**完全复用现有逻辑**：

| 接口 | 优先级 | 说明 |
|------|--------|------|
| `/devcomputer/chat` | P0 | 核心聊天接口，支持自定义 agent command |
| `/devcomputer/progress/{session_id}` | P0 | SSE 进度流 |
| `/devcomputer/agent/stop` | P0 | 停止 agent |
| `/devcomputer/agent/status` | P1 | 查询状态 |

### Phase 2：完整生命周期 + ACP Agent 目录约定

| 接口 | 优先级 | 说明 |
|------|--------|------|
| ACP Agent 目录约定 | P0 | `/home/user/acp-agent/{name}/` + PATH 配置 |
| `/devcomputer/agent/session/cancel` | P1 | 取消任务 |
| `/devcomputer/notify-resolved` | P1 | 权限审批 |

### Phase 3：自动热重载 + 调试增强

| 能力 | 优先级 | 说明 |
|------|--------|------|
| **Auto-Reload 热重载** | P0 | 文件变化检测 + 自动 stop/restart agent |
| 详细错误诊断 | P1 | agent stderr、exit code、which 检查 |
| auto_reload.force | P1 | 强制重载（不检查文件变化） |
| 容器自动超时清理 | P2 | 容器 30 分钟无活动自动销毁 |
| Agent 日志查看接口 | P2 | 查看 agent 进程的 stdout/stderr |
| 调试会话历史 | P3 | 查看调试请求和响应历史 |

---

## 11. 验收标准

### Phase 1 验收标准

1. **AC-1**：`POST /devcomputer/chat` 能接收包含自定义 `agent_config.agent_server.command` 的请求，并在容器中启动指定的 agent 进程
2. **AC-2**：`GET /devcomputer/progress/{session_id}` 能通过 SSE 接收 agent 的实时进度事件
3. **AC-3**：`POST /devcomputer/agent/stop` 能停止当前运行的 agent，但保留容器
4. **AC-4**：`/devcomputer/*` 和 `/computer/*` 共享同一个容器（由 `user_id` 标识），不使用独立的容器前缀
5. **AC-5**：所有接口的请求参数和响应结构与对应的 `/computer/*` 接口完全一致
6. **AC-6**：开发者完成 agent 修改后，可以通过 stop → chat 流程快速重启调试

### Phase 3 验收标准（Auto-Reload）

7. **AC-7**：开发者修改 agent 二进制文件后重新发送 chat 请求，系统自动检测变化并重载 agent
8. **AC-8**：稳定性检查确保不会启动正在写入中的不完整二进制文件
9. **AC-9**：`auto_reload.force: true` 能强制重载，不检查文件变化
10. **AC-10**：`auto_reload.enabled: false` 能关闭自动重载
11. **AC-11**：响应中的 `auto_reload` 字段正确反映重载状态（reloaded、changed_files 等）
12. **AC-12**：命令文件不存在时，通过 `which` 检查返回有意义的诊断信息

---

## 12. 术语表

| 术语 | 说明 |
|------|------|
| ACP | Agent Client Protocol，AI Agent 的标准化通信协议 |
| SACP | Simplified ACP，基于 stdin/stdout 的 ACP 简化实现 |
| agent_runner | 运行在容器内的 Agent 运行时，负责管理 ACP Agent 的生命周期 |
| rcoder | API 网关/编排层，负责容器管理和请求路由 |
| ServiceType | 容器服务类型标识，区分 RCoder / ComputerAgentRunner |
| ChatAgentConfig | 聊天请求中的 Agent 配置，包含自定义命令、MCP 服务器等 |
| agent_server | ChatAgentConfig 中定义 agent 启动命令的子配置 |
| GrpcChannelPool | rcoder 侧的 gRPC 连接池，管理与各容器的 gRPC 连接 |
| DuckDB | rcoder 使用的嵌入式数据库，存储项目、会话、容器的映射关系 |
| Auto-Reload | 自动热重载机制，检测 agent 文件变化后自动 stop 旧 agent 并启动新 agent |
| FileSnapshot | 文件快照，记录文件的 mtime + size 用于变化检测 |
| Stability Check | 稳定性校验，等待文件写入完成后才确认变化，避免编译竞态 |
| AgentBinarySnapshot | agent 可执行文件快照（存储在 agent_runner 的 `ProjectAndAgentInfo` 中），记录文件路径、mtime、size 用于变化检测 |
