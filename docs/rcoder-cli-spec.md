# rcoder-cli 需求文档 — ACP Agent 命令行调试工具

## 1. 背景与动机

### 1.1 问题

ACP Agent 开发者在开发调试过程中，需要一个**轻量、快速**的方式来验证自定义 ACP Agent 的协议实现和业务逻辑。当前可选方案：

| 方案 | 循环耗时 | 依赖条件 | 适用场景 |
|------|---------|---------|---------|
| 手动构造 JSON-RPC | 分钟级 | 无 | 不可行（ACP 协议复杂） |
| `/devcomputer/*` API | 10-30 秒 | rcoder 服务 + Docker | 集成测试（平台级） |
| **rcoder-cli（本文）** | **2-3 秒** | **无** | **协议级调试（本地直连）** |

### 1.2 定位

rcoder-cli 是一个**独立的命令行工具**，作为 ACP Client 直接启动用户自定义的 ACP Agent 子进程，通过 stdin/stdout 进行 ACP 协议通信。

```
                    ┌─────────────────────────────────┐
                    │         rcoder-cli              │
                    │                                 │
                    │  ┌───────────────────────────┐  │
                    │  │  ACP Client               │  │
                    │  │  (复用 agent_abstraction)  │  │
                    │  └────────────┬──────────────┘  │
                    └───────────────┼─────────────────┘
                                    │ stdin/stdout (ACP 协议)
                                    ▼
                    ┌─────────────────────────────────┐
                    │  用户自定义 ACP Agent 子进程      │
                    │  (如: python my-agent.py)        │
                    └─────────────────────────────────┘
```

### 1.3 与 `/devcomputer` 的关系

两者互补，覆盖不同测试层级：

```
┌─────────────────────────────────────────────────────────────┐
│                     开发者测试金字塔                          │
│                                                              │
│            ┌─────────────────┐                               │
│            │  /devcomputer   │  集成测试：agent + 平台 + 容器  │
│            │  (平台级)        │                               │
│            ├─────────────────┤                               │
│            │  rcoder-cli     │  协议测试：agent + ACP 协议     │
│            │  (协议级)        │                               │
│            ├─────────────────┤                               │
│            │  单元测试        │  代码测试：函数 + 模块          │
│            │  (代码级)        │                               │
│            └─────────────────┘                               │
└─────────────────────────────────────────────────────────────┘
```

---

## 2. 使用场景

### 2.1 场景 1：单次 Prompt 测试

快速发送一条 prompt，查看 agent 响应后退出。

```bash
$ rcoder-cli chat \
    --command "python" \
    --args "./my-acp-agent/main.py" \
    --prompt "分析当前目录的代码结构"

[ACP] 正在启动 agent: python ./my-acp-agent/main.py
[ACP] 初始化连接... 成功
[ACP] 创建会话... session_id: sess_abc123

Agent: 当前目录包含以下代码结构：
  ├── src/
  │   ├── main.py
  │   └── utils.py
  └── tests/
      └── test_main.py
```

### 2.2 场景 2：交互式多轮对话

进入交互模式，连续发送多条 prompt，agent 保持上下文。

```bash
$ rcoder-cli chat \
    --command "node" \
    --args "./build/agent.js" \
    --env "DEBUG=true" \
    --interactive

[ACP] 正在启动 agent: node ./build/agent.js
[ACP] 初始化连接... 成功
[ACP] 创建会话... session_id: sess_def456
[ACP] 进入交互模式 (Ctrl+C 退出, Ctrl+D 退出)

> 帮我写一个排序函数

Agent: 好的，这里是一个快速排序实现：
  def quicksort(arr):
      ...

> 加上类型注解

Agent: 好的，更新后的版本：
  def quicksort(arr: list[int]) -> list[int]:
      ...

> Ctrl+C
[ACP] 正在停止 agent...
[ACP] 已退出
```

### 2.3 场景 3：带 MCP Server 的调试

测试 agent 与 MCP Server 的集成。

```bash
$ rcoder-cli chat \
    --command "./my-agent" \
    --prompt "搜索最近的issue" \
    --mcp-server "github:npx -y @modelcontextprotocol/server-github" \
    --mcp-env "github:GITHUB_TOKEN=ghp_xxx"
```

### 2.4 场景 4：会话恢复

测试 agent 的会话恢复能力。

```bash
$ rcoder-cli chat \
    --command "./my-agent" \
    --prompt "继续之前的任务" \
    --resume-session "sess_def456"
```

### 2.5 场景 5：诊断 Agent 启动问题

Agent 启动失败时，输出详细诊断信息。

```bash
$ rcoder-cli chat \
    --command "/nonexistent/agent" \
    --prompt "hello"

[ACP] 正在启动 agent: /nonexistent/agent
[ACP] 错误: 无法启动 agent 进程
  原因: No such file or directory (os error 2)
  命令: /nonexistent/agent
  工作目录: /Users/dev/workspace
  诊断:
    - 命令是否存在: 否 (which 未找到)
    - 命令是否可执行: 否

$ rcoder-cli chat \
    --command "python" \
    --args "./broken-agent.py" \
    --prompt "hello"

[ACP] 正在启动 agent: python ./broken-agent.py
[ACP] 初始化连接... 超时 (50s)
[ACP] agent 进程已退出, exit_code: 1
[ACP] agent stderr 输出 (最近 20 行):
  Traceback (most recent call last):
    File "./broken-agent.py", line 3, in <module>
      import nonexistent_module
  ModuleNotFoundError: No module named 'nonexistent_module'
```

---

## 3. 接口设计

### 3.1 命令结构

```
rcoder-cli <SUBCOMMAND> [OPTIONS]
```

### 3.2 子命令

| 子命令 | 说明 |
|--------|------|
| `chat` | 启动 agent 并发送 prompt（核心命令） |
| `version` | 打印版本信息 |

### 3.3 `chat` 子命令参数

```bash
rcoder-cli chat [OPTIONS] --command <CMD> [-- <AGENT_ARGS>...]
```

| 参数 | 短参数 | 类型 | 必填 | 默认值 | 说明 |
|------|--------|------|------|--------|------|
| `--command` | `-c` | String | 是 | - | Agent 启动命令 |
| `--args` | `-a` | Vec<String> | 否 | `[]` | Agent 命令参数 |
| `--prompt` | `-p` | String | 否 | - | 发送的 prompt（不提供则进入交互模式） |
| `--interactive` | `-i` | Flag | 否 | `false` | 强制交互模式（即使提供了 --prompt） |
| `--env` | `-e` | Vec<String> | 否 | `[]` | 环境变量，格式 `KEY=VALUE` |
| `--working-dir` | `-w` | Path | 否 | 当前目录 | Agent 工作目录 |
| `--project-id` | | String | 否 | 自动生成 UUID | 项目 ID |
| `--session-id` | | String | 否 | - | 恢复已有会话 |
| `--agent-id` | | String | 否 | `custom-agent` | Agent 标识符 |
| `--agent-mode` | | String | 否 | `yolo` | Agent 模式: `yolo` / `ask` |
| `--system-prompt` | `-s` | String | 否 | - | 自定义系统提示词 |
| `--mcp-server` | | Vec<String> | 否 | `[]` | MCP Server，格式 `name:command` |
| `--mcp-env` | | Vec<String> | 否 | `[]` | MCP Server 环境变量，格式 `name:KEY=VALUE` |
| `--api-key` | | String | 否 | - | Model Provider API Key |
| `--model` | | String | 否 | - | Model Provider 模型名称 |
| `--base-url` | | String | 否 | - | Model Provider Base URL |
| `--verbose` | `-v` | Flag | 否 | `false` | 显示详细的 ACP 协议消息 |
| `--timeout` | `-t` | u64 | 否 | `300` | Prompt 超时时间（秒） |
| `--no-color` | | Flag | 否 | `false` | 禁用彩色输出 |

### 3.4 输出格式

**正常运行时**（非 verbose）：

```
[ACP] 状态信息（启动、连接、会话创建等）

Agent: <agent 的文本输出>
Agent: <agent 的工具调用信息>
Agent: <agent 的响应>
```

**Verbose 模式**（`-v`）：

```
[ACP] 状态信息
[ACP→Agent] InitializeRequest { protocol_version: "2025-03-26", ... }
[ACP←Agent] InitializeResponse { agent_info: { name: "my-agent", ... }, ... }
[ACP→Agent] NewSessionRequest { ... }
[ACP←Agent] NewSessionResponse { session_id: "sess_abc123", ... }
[ACP→Agent] PromptRequest { content_blocks: [Text("hello")], ... }
[ACP←Agent] SessionNotification { type: "text_delta", content: "Hi!", ... }
[ACP←Agent] PromptResponse { stop_reason: "end_turn", ... }

Agent: Hi!
```

### 3.5 退出码

| 退出码 | 含义 |
|--------|------|
| `0` | 正常退出（agent 处理完成或用户主动退出） |
| `1` | 通用错误（参数错误、网络错误等） |
| `2` | Agent 进程启动失败 |
| `3` | ACP 协议通信错误（初始化失败、超时等） |
| `4` | Agent 进程异常退出（非零退出码） |
| `130` | 用户中断（Ctrl+C） |

---

## 4. 技术方案概述

### 4.1 项目结构

```
crates/
├── rcoder-cli/                    # 新增 crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                # 入口：clap 参数解析 + tokio::main
│       ├── cli.rs                 # CLI 参数定义 (clap derive)
│       ├── commands/
│       │   ├── mod.rs
│       │   └── chat.rs            # chat 子命令实现
│       ├── notifier/
│       │   ├── mod.rs
│       │   └── terminal.rs        # TerminalSessionNotifier (实现 SessionNotifier trait)
│       ├── registry/
│       │   ├── mod.rs
│       │   └── simple.rs          # SimpleSessionRegistry (单会话, 实现 SessionRegistry trait)
│       └── output/
│           ├── mod.rs
│           └── formatter.rs       # 终端输出格式化（颜色、缩进）
```

### 4.2 依赖关系

```
rcoder-cli
    ├── agent_abstraction     # 核心：ACP 连接、进程管理、会话管理
    ├── agent_config          # Agent 配置加载
    ├── shared_types          # 共享类型
    ├── clap (derive)         # CLI 参数解析
    ├── tokio (full)          # 异步运行时
    ├── crossterm             # 终端控制（颜色、光标）
    ├── rustyline             # 交互式输入（readline 风格，可选）
    └── tracing-subscriber    # 日志输出
```

### 4.3 核心复用策略

rcoder-cli 直接复用 `agent_abstraction` 的公开 API，**不需要修改任何 `pub(crate)` 项的可见性**：

```
rcoder-cli
    │
    │  使用公开 API
    ▼
agent_abstraction
    ├── SacpClaudeCodeLauncher::launch()          ← 启动 agent 子进程
    ├── AcpSessionManager::get_or_create_session() ← 会话管理
    ├── AcpAgentWorker::process_request()          ← 处理 prompt
    ├── SessionNotifier trait                      ← CLI 实现终端版本
    ├── SessionRegistry trait                      ← CLI 实现简化版本
    ├── PermissionRequestHandler trait              ← CLI 使用 YoloHandler 或交互版本
    ├── DirectModelRuntimeEnvResolver              ← CLI 使用直连模式
    └── AgentLifecycleGuard                        ← 进程生命周期管理
```

**CLI 需要新实现的 trait 实现**：

| Trait | agent_runner 实现 | rcoder-cli 实现 | 差异 |
|-------|------------------|-----------------|------|
| `SessionNotifier` | `SseSessionNotifier` (推送 SSE) | `TerminalSessionNotifier` (终端打印) | 输出目标不同 |
| `SessionRegistry` | `AgentSessionRegistry` (DashMap 多会话) | `SimpleSessionRegistry` (单会话) | 复杂度不同 |
| `PermissionRequestHandler` | `PermissionManager` (规则引擎+SSE) | `YoloPermissionRequestHandler` (已有) 或 `InteractivePermissionHandler` (终端交互) | CLI 场景更简单 |

### 4.4 交互模式架构

```
┌──────────────────────────────────────────────────────────┐
│                      main thread                          │
│                                                           │
│  ┌──────────┐    ┌──────────┐    ┌───────────────────┐   │
│  │ stdin    │───→│ readline │───→│ prompt_tx.send()  │   │
│  │ (用户输入)│    │ (行编辑)  │    │ (发送给 ACP 连接) │   │
│  └──────────┘    └──────────┘    └───────────────────┘   │
│                                            │              │
│                                            ▼              │
│                              ┌────────────────────────┐   │
│                              │ ACP Connection Task    │   │
│                              │ (run_sacp_connection)  │   │
│                              │                        │   │
│                              │ stdin/stdout ←→ Agent  │   │
│                              └──────────┬─────────────┘   │
│                                         │                 │
│                                         ▼                 │
│                              ┌────────────────────────┐   │
│                              │ SessionNotifier        │   │
│                              │ (终端输出)              │   │
│                              └────────────────────────┘   │
└──────────────────────────────────────────────────────────┘
```

**关键设计**：用户输入（stdin readline）和 agent 输出（SessionNotifier → stdout）需要协调，避免输出打断用户正在输入的行。使用 `crossterm` 的终端控制能力，在 agent 输出时清除当前输入行，输出完成后恢复。

### 4.5 核心流程

```
main()
    │
    ├── clap 解析参数
    ├── 初始化 tracing (日志)
    │
    └── tokio::main
         │
         ├── 构建 ChatCommandConfig (从 CLI 参数)
         │   ├── command, args, env
         │   ├── working_dir, project_id
         │   ├── mcp_servers
         │   └── model_provider
         │
         ├── 创建 DirectModelRuntimeEnvResolver
         ├── 创建 YoloPermissionRequestHandler (或 Interactive)
         ├── 创建 TerminalSessionNotifier
         ├── 创建 SimpleSessionRegistry
         │
         ├── 创建 AcpSessionManager<N, R>
         │
         ├── 构建 AgentStartConfig
         │   ├── agent_server_override: {command, args, env}
         │   ├── mcp_servers
         │   ├── system_prompt
         │   └── agent_mode
         │
         ├── 调用 session_manager.get_or_create_session()
         │   └── SacpClaudeCodeLauncher::launch()
         │       ├── 启动子进程
         │       ├── ACP 初始化
         │       └── 创建会话
         │
         ├── 如果有 --prompt: 发送单次 prompt，等待响应
         ├── 如果 --interactive 或无 --prompt: 进入交互循环
         │   └── loop { readline → send_prompt → 等待响应 → 打印 }
         │
         └── 清理: lifecycle_guard.graceful_stop()
```

---

## 5. `TerminalSessionNotifier` 输出设计

### 5.1 事件映射

`SessionNotifier` 的每个方法对应 ACP 协议的一类通知，CLI 需要将其映射为终端输出：

| SessionNotifier 方法 | ACP 事件 | 终端输出示例 |
|---------------------|---------|-------------|
| `notify_session_update` | `assistant_message` | `Agent: 这是 AI 的回复内容...` |
| `notify_session_update` | `tool_use` | `Agent [tool]: 调用 read_file("main.py")` |
| `notify_session_update` | `tool_result` | `Agent [tool_result]: 文件内容...` |
| `notify_session_update` | `thinking` | `Agent [thinking]: 让我分析一下...` (仅 verbose) |
| `notify_prompt_end` | prompt 完成 | `[ACP] Agent 响应完成` |
| `notify_prompt_error` | prompt 失败 | `[ACP] 错误: agent 返回错误...` |

### 5.2 颜色方案

| 类型 | 颜色 | 示例 |
|------|------|------|
| `[ACP]` 状态信息 | 暗灰色 | `[ACP] 初始化连接...` |
| `Agent:` 文本输出 | 白色（默认） | `Agent: 这是回复` |
| `Agent [tool]:` 工具调用 | 黄色 | `Agent [tool]: read_file("main.py")` |
| `Agent [thinking]:` 思考过程 | 暗灰色 + 斜体 | `Agent [thinking]: ...` |
| `[ACP]` 错误信息 | 红色 | `[ACP] 错误: 连接超时` |
| `[ACP→Agent]` 协议消息 (verbose) | 蓝色 | `[ACP→Agent] PromptRequest {...}` |
| `[ACP←Agent]` 协议消息 (verbose) | 绿色 | `[ACP←Agent] SessionNotification {...}` |

---

## 6. 非目标

1. **不做 HTTP/gRPC 客户端** — rcoder-cli 是本地直连，不走 rcoder 服务
2. **不做多会话管理** — CLI 一次只运行一个 agent 会话
3. **不做容器管理** — 不涉及 Docker/K8s
4. **不做持久化** — 不存储会话历史、不持久化配置
5. **不做 GUI/TUI** — 纯命令行，不做终端 UI（如 `ratatui`）

---

## 7. 分期规划

### Phase 1：核心单次交互

最小可用版本，支持单次 prompt 测试。

| 能力 | 优先级 |
|------|--------|
| `--command` + `--args` 启动自定义 agent | P0 |
| `--prompt` 单次发送 | P0 |
| `TerminalSessionNotifier` 基础输出 | P0 |
| `--env` 环境变量传递 | P0 |
| `--working-dir` 工作目录 | P0 |
| Agent 进程生命周期管理 | P0 |
| 错误诊断输出（stderr、exit code） | P0 |
| 退出码规范 | P0 |

### Phase 2：交互模式

| 能力 | 优先级 |
|------|--------|
| `--interactive` 多轮对话 | P0 |
| readline 行编辑 | P1 |
| 输入/输出协调（避免输出打断输入） | P1 |
| Ctrl+C 优雅退出 | P0 |

### Phase 3：高级功能

| 能力 | 优先级 |
|------|--------|
| `--mcp-server` MCP Server 集成 | P1 |
| `--resume-session` 会话恢复 | P1 |
| `--verbose` ACP 协议消息追踪 | P1 |
| `--system-prompt` 自定义系统提示词 | P2 |
| `--api-key` / `--model` / `--base-url` Model Provider 配置 | P2 |

---

## 8. 验收标准

### Phase 1 验收标准

1. **AC-1**：`rcoder-cli chat --command "my-agent" --prompt "hello"` 能启动 `my-agent` 子进程，通过 ACP 协议发送 prompt，并在终端输出 agent 响应
2. **AC-2**：`--env KEY=VALUE` 能正确传递环境变量到 agent 子进程
3. **AC-3**：`--working-dir` 能设置 agent 的工作目录
4. **AC-4**：agent 启动失败时，输出 stderr 内容和诊断信息
5. **AC-5**：agent 异常退出时，输出 exit code 和 stderr 尾部内容
6. **AC-6**：退出码符合规范（0/1/2/3/4/130）

### Phase 2 验收标准

7. **AC-7**：无 `--prompt` 时自动进入交互模式，支持多轮对话
8. **AC-8**：交互模式下 agent 输出不会打断用户正在输入的行
9. **AC-9**：Ctrl+C 能优雅停止 agent 进程并退出 CLI

---

## 9. 术语表

| 术语 | 说明 |
|------|------|
| ACP Client | ACP 协议的客户端，负责启动 agent 子进程并与其通信。rcoder-cli 和 agent_runner 都是 ACP Client |
| ACP Agent | 实现 ACP 协议的子进程，通过 stdin/stdout 与 Client 通信 |
| SessionNotifier | `agent_abstraction` 中定义的 trait，用于接收 agent 的实时通知 |
| SessionRegistry | `agent_abstraction` 中定义的 trait，用于管理 session 到 project 的映射 |
| ByteStreams | `agent-client-protocol` 提供的传输层，包装 AsyncWrite + AsyncRead |
| SACP | Simplified ACP，基于 stdin/stdout 的 ACP 简化实现 |
| PromptRequest | ACP 协议中发送 prompt 的请求类型 |
| SessionNotification | ACP 协议中 agent 推送实时更新的通知类型 |
