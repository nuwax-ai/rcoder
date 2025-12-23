# Agent Resume via ACP 设计方案

## 1. 背景

### 1.1 需求描述
通过 ACP 协议与 Agent 对话时，如果 Agent 停止后，希望能通过 `resume` 参数继续之前的上下文对话记录，与 Agent 继续对话。

### 1.2 相关资源
- **claude-code-acp**: Zed 公司对 Claude Code 的 ACP 协议封装（参考：`tmp/claude-code-acp`）
- **rust-sdk**: ACP 协议的 Rust SDK（参考：`tmp/rust-sdk`）
- **当前实现**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

## 2. 技术分析

### 2.1 ACP 协议中的会话恢复方式

ACP 协议定义了多种会话管理方法：

| 方法 | 功能 | 状态 |
|------|------|------|
| `session/new` (NewSessionRequest) | 创建新会话 | 稳定 |
| `session/load` (LoadSessionRequest) | 加载已存在的会话并重放历史 | 稳定，但 claude-code-acp 未实现 |
| `session/resume` (ResumeSessionRequest) | 恢复会话（不重放历史）| unstable，需要 `unstable_session_resume` feature |
| `session/fork` (ForkSessionRequest) | 分叉会话 | unstable |

### 2.2 claude-code-acp 的 Resume 实现机制

通过分析 `tmp/claude-code-acp/src/acp-agent.ts`，发现 claude-code-acp 支持三种 resume 方式：

#### 方式一：通过 NewSessionRequest 的 meta 参数
```typescript
// acp-agent.ts:205-216
async newSession(params: NewSessionRequest): Promise<NewSessionResponse> {
  return await this.createSession(params, {
    resume: (params._meta as NewSessionMeta | undefined)?.claudeCode?.options?.resume,
  });
}
```

**传参结构**：
```json
{
  "cwd": "/path/to/project",
  "_meta": {
    "claudeCode": {
      "options": {
        "resume": "previous-session-id"
      }
    }
  }
}
```

#### 方式二：通过 unstable_resumeSession 方法
```typescript
// acp-agent.ts:233-246
async unstable_resumeSession(params: ResumeSessionRequest): Promise<ResumeSessionResponse> {
  return await this.createSession(
    { cwd: params.cwd, mcpServers: params.mcpServers ?? [], _meta: params._meta },
    { resume: params.sessionId }
  );
}
```

#### 方式三：通过 load_session（claude-code-acp 不支持）
```typescript
// 返回 Error::method_not_found()
```

### 2.3 createSession 内部 Resume 处理逻辑

```typescript
// acp-agent.ts:593-709
private async createSession(
  params: NewSessionRequest,
  creationOpts: { resume?: string; forkSession?: boolean } = {},
): Promise<NewSessionResponse> {
  // 1. 确定 sessionId
  let sessionId;
  if (creationOpts.forkSession) {
    sessionId = randomUUID();
  } else if (creationOpts.resume) {
    sessionId = creationOpts.resume;  // 👈 使用传入的 session_id
  } else {
    sessionId = randomUUID();
  }

  // 2. 构建 extraArgs
  const extraArgs = { ...userProvidedOptions?.extraArgs };
  if (creationOpts?.resume === undefined || creationOpts?.forkSession) {
    extraArgs["session-id"] = sessionId;  // 👈 新会话才设置 session-id
  }

  // 3. 构建 Options 传递给 SDK
  const options: Options = {
    // ... 其他配置
    extraArgs,
    ...creationOpts,  // 👈 包含 { resume: sessionId }
  };

  // 4. 调用 SDK 的 query 函数
  const q = query({ prompt: input, options });
}
```

**关键发现**：
1. `resume` 参数最终通过 `Options` 传递给 `@anthropic-ai/claude-agent-sdk` 的 `query()` 函数
2. SDK 会根据 `Options.resume` 自动恢复之前的会话上下文
3. 当 resume 有值时，不设置新的 `session-id` extraArg

### 2.4 当前项目的实现分析

#### 当前流程 (`claude_code_launcher.rs:438-480`)
```rust
let session_id = match session_id_for_closure {
    Some(sid) => {
        // 尝试 load_session
        match client_conn.load_session(LoadSessionRequest::new(...)).await {
            Ok(resp) => given_session_id,
            Err(e) => {
                // 失败时回退到 new_session
                // resume_session_id 通过 meta.claudeCode.options.resume 传递
                let new_session_request = NewSessionRequest::new(...)
                    .mcp_servers(mcp_servers)
                    .meta(system_prompt_meta);  // 👈 包含 resume
                client_conn.new_session(new_session_request).await?
            }
        }
    }
    None => {
        // 创建新会话
        client_conn.new_session(NewSessionRequest::new(...)).await?
    }
};
```

#### 问题分析
1. **冗余逻辑**：先尝试 `load_session`（必定失败），然后回退到 `new_session`
2. **混淆概念**：`session_id` 参数和 `resume_session_id` 参数的关系不清晰
3. **缺少 ACP 原生 resume**：未使用 `ResumeSessionRequest`

## 3. 设计方案

### 3.1 方案选择

推荐使用 **方式一：通过 NewSessionRequest 的 meta 参数** 实现 resume：

**理由**：
1. **稳定性**：不依赖 unstable feature
2. **兼容性**：适用于所有支持 ACP 的 Agent
3. **当前实现已支持**：`AgentStartConfig.build_meta()` 已经正确构建了 `claudeCode.options.resume` 结构

### 3.2 核心改动

#### 3.2.1 简化 claude_code_launcher.rs 的会话创建逻辑

**改动位置**：`crates/agent_abstraction/src/compat/claude_code_launcher.rs:438-480`

**改动前**：
```rust
let session_id = match session_id_for_closure {
    Some(sid) => {
        // 先尝试 load_session，失败后再 new_session
        // ...
    }
    None => {
        // new_session
    }
};
```

**改动后**：
```rust
// 统一使用 new_session，resume 通过 meta 传递
let new_session_request = NewSessionRequest::new(project_path_for_closure.clone())
    .mcp_servers(mcp_servers)
    .meta(system_prompt_meta);  // 已包含 claudeCode.options.resume

let session_id = client_conn
    .new_session(new_session_request)
    .await?
    .session_id;
```

#### 3.2.2 参数语义调整

| 参数 | 原语义 | 新语义 |
|------|--------|--------|
| `session_id: Option<String>` (launch 方法) | 传入则尝试 load_session | **移除**，不再使用 |
| `AgentStartConfig.resume_session_id` | 通过 meta 传递 | **保持**，作为唯一的 resume 参数来源 |

#### 3.2.3 数据流示意图

```
┌──────────────────────────────────────────────────────────────────────────┐
│                           RCoder / Agent Runner                           │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  ChatPrompt                                                               │
│  ├─ session_id: "abc-123"  (用于标识当前对话，可能是新的或历史的)          │
│  └─ ...                                                                   │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  acp_worker.rs                                                            │
│  ├─ 检查会话是否需要 resume                                                │
│  │   └─ 如果 session_id 匹配已存在的会话 → 设置 resume_session_id          │
│  └─ 构建 AgentStartConfig                                                 │
│      └─ resume_session_id: Some("abc-123")                                │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  session_manager.rs                                                       │
│  └─ 调用 ClaudeCodeLauncher::launch()                                     │
│      └─ start_config: AgentStartConfig { resume_session_id, ... }         │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  claude_code_launcher.rs                                                  │
│  ├─ start_config.build_meta() 构建:                                       │
│  │   {                                                                    │
│  │     "systemPrompt": { "append": "..." },                               │
│  │     "claudeCode": {                                                    │
│  │       "options": {                                                     │
│  │         "resume": "abc-123"  ← resume_session_id                       │
│  │       }                                                                │
│  │     }                                                                  │
│  │   }                                                                    │
│  └─ NewSessionRequest::new(...).meta(meta)                                │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼ ACP 协议 (session/new)
┌──────────────────────────────────────────────────────────────────────────┐
│  claude-code-acp (Agent)                                                  │
│  ├─ newSession() 解析 _meta.claudeCode.options.resume                     │
│  ├─ createSession(params, { resume: "abc-123" })                          │
│  └─ query({ options: { resume: "abc-123", ... } })                        │
└──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  @anthropic-ai/claude-agent-sdk                                           │
│  └─ 根据 Options.resume 恢复之前的会话上下文                               │
└──────────────────────────────────────────────────────────────────────────┘
```

### 3.3 详细代码改动

#### 文件 1: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

```rust
// 改动 launch 方法签名，移除 session_id 参数
pub async fn launch(
    &self,
    project_id: String,
    project_path: PathBuf,
    // session_id: Option<String>,  // 👈 移除此参数
    model_provider: Option<ModelProviderConfig>,
    start_config: AgentStartConfig,
    client: C,
) -> Result<LauncherConnectionInfoComplete> {
```

```rust
// 简化会话创建逻辑（约 438-480 行）
// 改动前：
// let session_id = match session_id_for_closure { ... }

// 改动后：
debug!("创建 ACP 会话[new_session]");
let new_session_request = NewSessionRequest::new(project_path_for_closure.clone())
    .mcp_servers(mcp_servers)
    .meta(system_prompt_meta);  // 已包含 resume 信息

let session_id = client_conn
    .new_session(new_session_request)
    .await
    .context("ACP 会话创建失败")?
    .session_id;

debug!("ACP 会话创建成功[new_session], session_id={}", session_id.0);
```

#### 文件 2: `crates/agent_abstraction/src/session/session_manager.rs`

```rust
// 更新 launch 调用，移除 session_id 参数
let result = launcher
    .launch(
        project_id.clone(),
        project_path.clone(),
        // None,  // 👈 移除 session_id 参数
        model_provider.clone(),
        start_config.clone(),
        acp_client,
    )
    .await;
```

### 3.4 Resume 失败降级机制

#### 3.4.1 问题场景

当传入的 `session_id` 对应的会话不存在时（例如：会话历史已过期、被清理、或从未存在），Agent 会启动失败并抛出错误。

**典型错误信息**：
```
No conversation found for session id: abc-123
```
或
```
exited with code 1
```

#### 3.4.2 当前降级实现

降级逻辑已在 `session_manager.rs:217-261` 实现：

```rust
// session_manager.rs
let connection_info = match result {
    Ok(info) => info,
    Err(e) => {
        let error_msg = format!("{:?}", e);

        // 检查是否因为 resume 导致的失败
        if has_resume
            && (error_msg.contains("No conversation found")
                || error_msg.contains("session")
                || error_msg.contains("exited with code 1"))
        {
            tracing::warn!(
                "⚠️ Agent 启动失败（可能因 --resume），重试不使用 --resume: error={}",
                error_msg
            );

            // 创建新的 config，不包含 resume_session_id
            let retry_config = AgentStartConfig {
                system_prompt: start_config.system_prompt,
                mcp_servers: start_config.mcp_servers,
                extra_meta: start_config.extra_meta,
                service_type: start_config.service_type,
                resume_session_id: None, // ✅ 去掉 resume
            };

            tracing::info!("🔄 重试启动 Agent（不使用 --resume）");

            // 重试启动
            launcher.launch(..., retry_config, ...).await?
        } else {
            // 其他错误，直接返回
            return Err(e);
        }
    }
};
```

#### 3.4.3 降级流程图

```
┌─────────────────────────────────────────────────────────────────┐
│  ChatRequest { session_id: "abc-123" }                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  session_manager.rs                                             │
│  └─ 第一次尝试：start_config.resume_session_id = Some("abc-123")│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  claude_code_launcher.launch()                                  │
│  └─ NewSessionRequest + meta { claudeCode.options.resume }      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ ACP 协议
┌─────────────────────────────────────────────────────────────────┐
│  claude-code-acp                                                │
│  └─ SDK query({ options: { resume: "abc-123" } })               │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              │                               │
              ▼                               ▼
     ┌────────────────┐              ┌────────────────────────┐
     │ 会话存在       │              │ 会话不存在             │
     │ → 恢复上下文   │              │ → 抛出错误             │
     │ → 返回成功     │              │ "No conversation found"│
     └────────────────┘              └────────────────────────┘
                                              │
                                              ▼
                              ┌─────────────────────────────────────────┐
                              │  session_manager.rs 降级处理            │
                              │  ├─ 检测错误关键字                       │
                              │  ├─ 创建新 config: resume_session_id=None│
                              │  └─ 第二次尝试：不带 resume              │
                              └─────────────────────────────────────────┘
                                              │
                                              ▼
                              ┌─────────────────────────────────────────┐
                              │  claude_code_launcher.launch()          │
                              │  └─ NewSessionRequest (无 resume)       │
                              └─────────────────────────────────────────┘
                                              │
                                              ▼
                              ┌─────────────────────────────────────────┐
                              │  成功创建新会话                          │
                              │  └─ 返回新的 session_id                  │
                              └─────────────────────────────────────────┘
```

#### 3.4.4 错误检测关键字

当前实现检测以下关键字来判断是否因 resume 失败：

| 关键字 | 来源 | 说明 |
|--------|------|------|
| `"No conversation found"` | SDK 错误 | 明确表示会话不存在 |
| `"session"` | 通用匹配 | 捕获其他 session 相关错误 |
| `"exited with code 1"` | 进程退出 | Agent 进程异常退出 |

**建议改进**：可以增加更精确的错误码匹配，例如：
- `"ENOENT"` - 会话文件不存在
- `"invalid session"` - 无效会话

#### 3.4.5 降级策略配置（可选扩展）

未来可以考虑增加降级策略配置：

```rust
pub struct ResumePolicy {
    /// 是否启用降级
    pub enable_fallback: bool,
    /// 最大重试次数
    pub max_retries: u32,
    /// 降级时是否保留部分配置（如 MCP 服务器）
    pub preserve_mcp_servers: bool,
}
```

### 3.5 备选方案：使用 ResumeSessionRequest (unstable)

如果需要使用 ACP 原生的 resume 方法，需要：

1. **启用 unstable feature**：
```toml
# Cargo.toml
agent-client-protocol = { version = "0.6", features = ["unstable_session_resume"] }
```

2. **添加 resume_session 调用**：
```rust
#[cfg(feature = "unstable_session_resume")]
async fn resume_session(&self, session_id: SessionId, cwd: PathBuf) -> Result<SessionId> {
    let request = ResumeSessionRequest::new(session_id.clone(), cwd);
    let response = self.client_conn.resume_session(request).await?;
    Ok(response.session_id)
}
```

**不推荐此方案**：
- 依赖 unstable API，可能随时变更
- claude-code-acp 的 `unstable_resumeSession` 内部也是通过 `createSession` 处理，效果相同

## 4. API 变更

### 4.1 内部 API 变更

| 组件 | 改动 |
|------|------|
| `ClaudeCodeLauncher::launch()` | 移除 `session_id` 参数 |
| `SessionManager` | 调用 launch 时不再传递 session_id |

### 4.2 外部 API 无变更

gRPC `ChatRequest` 和 HTTP API 保持不变，`session_id` 字段继续用于：
1. 标识会话（用于进度订阅、取消等）
2. 决定是否需要 resume（在 `acp_worker.rs` 中判断）

## 5. 测试计划

### 5.1 单元测试
- [ ] `AgentStartConfig.build_meta()` 正确构建 resume 结构
- [ ] 当 `resume_session_id` 为 None 时，meta 不包含 claudeCode 字段

### 5.2 集成测试
- [ ] 新建会话：不传 session_id，正常创建
- [ ] Resume 会话：传入历史 session_id，能够继续对话
- [ ] Resume 失败降级：传入无效 session_id，能够降级为新会话

### 5.3 降级场景测试

#### 场景 1：会话不存在
```bash
# 传入一个不存在的 session_id
curl -X POST /chat -d '{
  "project_id": "proj1",
  "session_id": "non-existent-session-id",
  "prompt": "Hello"
}'

# 预期：
# 1. 第一次尝试失败，日志显示 "No conversation found"
# 2. 自动降级，去掉 resume 重试
# 3. 成功创建新会话，返回新的 session_id
```

#### 场景 2：会话已过期
```bash
# 使用一个曾经存在但已过期/清理的 session_id
curl -X POST /chat -d '{
  "project_id": "proj1",
  "session_id": "expired-session-id",
  "prompt": "继续之前的对话"
}'

# 预期：同场景 1，自动降级为新会话
```

#### 场景 3：会话存在且有效
```bash
# 第一次对话
curl -X POST /chat -d '{"project_id":"proj1", "prompt":"记住数字 42"}'
# 返回 session_id: "valid-session-123"

# 第二次对话（resume）
curl -X POST /chat -d '{
  "project_id": "proj1",
  "session_id": "valid-session-123",
  "prompt": "我之前说的数字是多少？"
}'

# 预期：Agent 回答 "42"（证明上下文已恢复）
```

### 5.4 手动测试场景
```bash
# 1. 首次对话
curl -X POST /chat -d '{"project_id":"proj1", "prompt":"Hello"}'
# 返回 session_id: "sess-abc-123"

# 2. 继续对话（resume）
curl -X POST /chat -d '{"project_id":"proj1", "session_id":"sess-abc-123", "prompt":"继续刚才的话题"}'
# Agent 应该能够记住之前的上下文
```

## 6. 风险评估

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| SDK 不支持 resume | Agent 无法恢复上下文 | 当前 claude-code-acp 已支持 |
| session_id 过期/无效 | resume 失败 | 已有降级逻辑，自动创建新会话 |
| 协议变更 | resume 字段位置变化 | 跟踪 claude-code-acp 更新 |

## 7. 实现步骤

1. **Phase 1**：简化 `claude_code_launcher.rs`
   - 移除 `load_session` 尝试逻辑
   - 统一使用 `new_session` + meta

2. **Phase 2**：清理接口
   - 移除 launch 方法的 `session_id` 参数
   - 更新所有调用方

3. **Phase 3**：测试验证
   - 添加单元测试
   - 执行集成测试

## 8. 附录

### 8.1 相关代码位置

| 文件 | 行号 | 描述 |
|------|------|------|
| `crates/agent_abstraction/src/compat/claude_code_launcher.rs` | 291-573 | launch 方法 |
| `crates/agent_abstraction/src/traits/agent.rs` | 95-142 | `AgentStartConfig.build_meta()` |
| `crates/agent_abstraction/src/session/acp_worker.rs` | 164-167 | resume 判断逻辑 |
| `crates/agent_abstraction/src/session/session_manager.rs` | 200-260 | launcher 调用 |
| `crates/agent_abstraction/src/session/session_manager.rs` | 217-261 | **Resume 失败降级逻辑** |
| `tmp/claude-code-acp/src/acp-agent.ts` | 205-246, 593-709 | claude-code-acp resume 实现 |

### 8.2 参考资料

- [ACP Protocol Specification](https://agentclientprotocol.com)
- [claude-code-acp GitHub](https://github.com/zed-industries/claude-code-acp)
- [Agent Client Protocol Rust SDK](https://github.com/agentclientprotocol/rust-sdk)
