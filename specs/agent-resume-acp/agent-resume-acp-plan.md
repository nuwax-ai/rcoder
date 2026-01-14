# Agent Resume via ACP 实现计划

## 方案概述

采用 **方式一：通过 NewSessionRequest 的 meta 参数** 实现 resume 功能。

核心思路：统一使用 `NewSessionRequest` + `_meta.claudeCode.options.resume` 传递 session_id，移除冗余的 `load_session` 尝试逻辑。

**重要**：Resume 是"尝试性"的，如果 session 不存在导致 Agent 启动失败，会自动降级为不传 resume 参数，创建新会话。

---

## Resume 失败降级方案（核心机制）

### 问题场景

当用户传入的 `session_id` 对应的会话不存在时（已过期、被清理、或从未存在），Agent 会启动失败：

```
No conversation found for session id: abc-123
```

### 降级策略

**优先尝试 resume，失败则降级为新会话**：

```
第一次尝试：带 resume 参数
    ↓
    ├─ 成功 → 恢复上下文，继续对话
    │
    └─ 失败（任何原因）
           ↓
       第二次尝试：不带 resume 参数
           ↓
           └─ 成功 → 创建新会话
```

**关键点**：不需要判断具体错误原因，只要 `has_resume && 启动失败`，就直接降级重试。

### 当前实现位置（需改动）

**文件**: `session_manager.rs:217-261`

**当前代码**（需简化）:
```rust
// 检查是否因为 resume 导致的失败
if has_resume
    && (error_msg.contains("No conversation found")
        || error_msg.contains("session")
        || error_msg.contains("exited with code 1"))  // ← 删除这些判断
{
    // 降级...
}
```

**改动后代码**:
```rust
// 只要带 resume 且启动失败，就降级重试
if has_resume {
    tracing::warn!(
        "⚠️ Agent 启动失败（带 resume），降级为不使用 resume 重试: error={}",
        error_msg
    );

    // 创建新的 config，不包含 resume_session_id
    let retry_config = AgentStartConfig {
        system_prompt: start_config.system_prompt,
        mcp_servers: start_config.mcp_servers,
        extra_meta: start_config.extra_meta,
        service_type: start_config.service_type,
        resume_session_id: None,  // ← 关键：去掉 resume
    };

    tracing::info!("🔄 重试启动 Agent（不使用 resume）");

    // 第二次尝试：不带 resume
    launcher.launch(..., retry_config, ...).await?
} else {
    // 不带 resume 的失败，直接返回错误
    return Err(e);
}
```

### 降级流程图

```
┌─────────────────────────────────────────────────────────────────┐
│  ChatRequest { session_id: "abc-123" }                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  acp_worker.rs                                                  │
│  └─ AgentStartConfig { resume_session_id: Some("abc-123") }     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  session_manager.rs                                             │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ 第一次尝试                                               │    │
│  │ launcher.launch(start_config)                           │    │
│  │ → meta: { claudeCode.options.resume: "abc-123" }        │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                  │
│              ┌───────────────┴───────────────┐                  │
│              ▼                               ▼                  │
│     ┌────────────────┐              ┌────────────────────┐      │
│     │ ✅ 成功        │              │ ❌ 失败            │      │
│     │ session 存在   │              │ "No conversation   │      │
│     │ 恢复上下文     │              │  found"            │      │
│     └────────────────┘              └────────────────────┘      │
│                                              │                  │
│                                              ▼                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │ 第二次尝试（降级）                                       │    │
│  │ retry_config.resume_session_id = None                   │    │
│  │ launcher.launch(retry_config)                           │    │
│  │ → meta: { }  ← 不包含 resume                            │    │
│  └─────────────────────────────────────────────────────────┘    │
│                              │                                  │
│                              ▼                                  │
│                     ┌────────────────┐                          │
│                     │ ✅ 成功        │                          │
│                     │ 创建新会话     │                          │
│                     │ 返回新 session │                          │
│                     └────────────────┘                          │
└─────────────────────────────────────────────────────────────────┘
```

### 本次改动对降级机制的影响

**需要简化**。移除错误关键字判断，改为：只要 `has_resume && 启动失败` 就降级。

| 层级 | 改动 | 降级机制 |
|------|------|----------|
| session_manager.rs | 简化降级判断逻辑 | **需改动** |
| claude_code_launcher.rs | 简化会话创建逻辑 | 不涉及 |

---

## 当前代码状态分析

### ✅ 已正确实现的部分

| 文件 | 功能 | 状态 |
|------|------|------|
| `agent.rs:95-137` | `AgentStartConfig.build_meta()` 构建 `claudeCode.options.resume` | ✅ 完成 |
| `acp_worker.rs:158-180` | 判断是否需要 resume 并设置 `resume_session_id` | ✅ 完成 |
| `session_manager.rs:217-261` | Resume 失败降级重试机制（框架已完成，判断逻辑需简化） | ⚠️ 需简化 |

### ❌ 需要改动的部分

| 文件 | 问题 | 改动 |
|------|------|------|
| `claude_code_launcher.rs:438-480` | 冗余的 `load_session` 尝试逻辑 | 简化为统一 `new_session` |
| `claude_code_launcher.rs:291-299` | `session_id` 参数冗余 | 移除此参数 |
| `session_manager.rs:207-214, 247-254` | 调用 launch 时传递 `session_id_hint` | 移除此参数 |
| `session_manager.rs:224-227` | 降级判断过于复杂（检测错误关键字） | 简化为 `if has_resume` |

---

## 改动任务清单

### Task 0: 简化 session_manager.rs 降级判断逻辑（新增）

**文件**: `crates/agent_abstraction/src/session/session_manager.rs`

**改动位置**: 约 223-228 行

**当前代码**:
```rust
// 检查是否因为 resume 导致的失败
if has_resume
    && (error_msg.contains("No conversation found")
        || error_msg.contains("session")
        || error_msg.contains("exited with code 1"))
{
```

**改动后代码**:
```rust
// 只要带 resume 且启动失败，就降级重试（不判断具体错误原因）
if has_resume {
```

**同时更新日志信息**:
```rust
tracing::warn!(
    "⚠️ Agent 启动失败（带 resume），降级为不使用 resume 重试: error={}",
    error_msg
);
```

---

### Task 1: 简化 claude_code_launcher.rs 会话创建逻辑

**文件**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

**改动位置**: 约 438-480 行

**当前代码**:
```rust
// 创建会话
let session_id = match session_id_for_closure {
    Some(sid) => {
        debug!("尝试加载 ACP 会话[load_session]");
        let given_session_id = SessionId::new(sid);
        match client_conn
            .load_session(LoadSessionRequest::new(
                given_session_id.clone(),
                project_path_for_closure.clone(),
            ))
            .await
        {
            Ok(resp) => {
                debug!("ACP 会话加载成功[load_session],{:?}", resp);
                given_session_id
            }
            Err(e) => {
                warn!(
                    "load_session 失败或未实现，回退创建新会话[new_session]: {:?}",
                    e
                );
                // 注意：即使 load_session 失败，仍然会创建 new_session
                // resume_session_id 会通过 meta.claudeCode.options.resume 传递
                let new_session_request =
                    NewSessionRequest::new(project_path_for_closure.clone())
                        .mcp_servers(mcp_servers.clone())
                        .meta(system_prompt_meta.clone());
                let resp = client_conn.new_session(new_session_request).await?;
                debug!("ACP 会话创建成功[new_session],{:?}", resp);
                resp.session_id
            }
        }
    }
    None => {
        debug!("创建 ACP 会话[new_session]");
        let new_session_request =
            NewSessionRequest::new(project_path_for_closure.clone())
                .mcp_servers(mcp_servers)
                .meta(system_prompt_meta);
        let resp = client_conn.new_session(new_session_request).await?;
        debug!("ACP 会话创建成功[new_session],{:?}", resp);
        resp.session_id
    }
};
```

**改动后代码**:
```rust
// 创建会话（统一使用 new_session，resume 通过 meta 传递）
// 如果 start_config.resume_session_id 有值，build_meta() 会自动构建
// _meta.claudeCode.options.resume 结构
debug!("创建 ACP 会话[new_session]");
let new_session_request = NewSessionRequest::new(project_path_for_closure.clone())
    .mcp_servers(mcp_servers)
    .meta(system_prompt_meta);

let resp = client_conn
    .new_session(new_session_request)
    .await
    .context("ACP 会话创建失败")?;

debug!(
    "ACP 会话创建成功[new_session], session_id={}",
    resp.session_id.0
);
let session_id = resp.session_id;
```

**同时需要移除的变量**:
```rust
// 约 317 行，移除这行
let session_id_for_closure = session_id.clone();
```

---

### Task 2: 移除 launch 方法的 session_id 参数

**文件**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

**改动位置**: 约 279-299 行（方法签名和文档）

**当前代码**:
```rust
/// 启动 Claude Code ACP Agent 服务
///
/// # 参数
/// - `project_id`: 项目 ID
/// - `project_path`: 项目工作目录
/// - `session_id`: 可选的会话 ID（用于恢复会话）
/// - `model_provider`: 模型提供商配置
/// - `start_config`: Agent 启动配置（包含系统提示词等）
/// - `client`: ACP 客户端实现
///
/// # 返回值
/// 返回 LauncherConnectionInfoComplete，包含会话信息和生命周期守卫
pub async fn launch(
    &self,
    project_id: String,
    project_path: PathBuf,
    session_id: Option<String>,
    model_provider: Option<ModelProviderConfig>,
    start_config: AgentStartConfig,
    client: C,
) -> Result<LauncherConnectionInfoComplete> {
```

**改动后代码**:
```rust
/// 启动 Claude Code ACP Agent 服务
///
/// # 参数
/// - `project_id`: 项目 ID
/// - `project_path`: 项目工作目录
/// - `model_provider`: 模型提供商配置
/// - `start_config`: Agent 启动配置（包含系统提示词、resume_session_id 等）
/// - `client`: ACP 客户端实现
///
/// # Resume 机制
/// 如果需要恢复会话，通过 `start_config.resume_session_id` 传递 session_id，
/// 会自动构建 `_meta.claudeCode.options.resume` 结构传递给 Agent。
///
/// # 返回值
/// 返回 LauncherConnectionInfoComplete，包含会话信息和生命周期守卫
pub async fn launch(
    &self,
    project_id: String,
    project_path: PathBuf,
    model_provider: Option<ModelProviderConfig>,
    start_config: AgentStartConfig,
    client: C,
) -> Result<LauncherConnectionInfoComplete> {
```

---

### Task 3: 更新 session_manager.rs 的 launch 调用

**文件**: `crates/agent_abstraction/src/session/session_manager.rs`

**改动位置 1**: 约 207-214 行（第一次调用）

**当前代码**:
```rust
let result = launcher
    .launch(
        project_id.clone(),
        project_path.clone(),
        session_id_hint.clone(),
        model_provider.clone(),
        start_config.clone(),
        client,
    )
    .await;
```

**改动后代码**:
```rust
let result = launcher
    .launch(
        project_id.clone(),
        project_path.clone(),
        model_provider.clone(),
        start_config.clone(),
        client,
    )
    .await;
```

**改动位置 2**: 约 247-254 行（降级重试调用）

**当前代码**:
```rust
launcher
    .launch(
        project_id.clone(),
        project_path,
        session_id_hint,
        model_provider.clone(),
        retry_config,
        C::default(),
    )
    .await?
```

**改动后代码**:
```rust
launcher
    .launch(
        project_id.clone(),
        project_path,
        model_provider.clone(),
        retry_config,
        C::default(),
    )
    .await?
```

---

### Task 4: 清理 session_manager.rs 的方法签名（可选）

**文件**: `crates/agent_abstraction/src/session/session_manager.rs`

**改动位置**: `create_new_session` 方法签名（约 185-196 行）

**当前代码**:
```rust
pub async fn create_new_session(
    &self,
    project_id: String,
    project_path: PathBuf,
    session_id_hint: Option<String>,
    model_provider: Option<ModelProviderConfig>,
    start_config: AgentStartConfig,
    client: C,
) -> Result<Arc<SessionInfo>> {
```

**评估**:
- `session_id_hint` 参数当前未被使用（只是传递给 launcher）
- 可以移除，但需要检查 `get_or_create_session` 等上游调用方
- **建议**: 暂时保留此参数，避免过大改动范围

---

### Task 5: 移除未使用的 import

**文件**: `crates/agent_abstraction/src/compat/claude_code_launcher.rs`

**改动位置**: 约 11-14 行

**当前代码**:
```rust
use agent_client_protocol::{
    Agent, Client, ClientSideConnection, Implementation, InitializeRequest, LoadSessionRequest,
    McpServer, McpServerStdio, NewSessionRequest, PromptRequest, SessionId,
};
```

**改动后代码**:
```rust
use agent_client_protocol::{
    Agent, Client, ClientSideConnection, Implementation, InitializeRequest,
    McpServer, McpServerStdio, NewSessionRequest, PromptRequest, SessionId,
};
```

---

## 改动验证清单

### 编译验证
- [ ] `cargo build -p agent_abstraction` 编译通过
- [ ] `cargo build --workspace` 全项目编译通过
- [ ] `cargo clippy` 无新增警告

### 单元测试
- [ ] `AgentStartConfig.build_meta()` 测试用例通过
- [ ] 现有测试用例通过

### 集成测试
- [ ] 新建会话：正常创建
- [ ] Resume 会话：传入有效 session_id，恢复上下文
- [ ] Resume 失败降级：传入无效 session_id，自动降级为新会话

---

## 实现顺序

```
Step 1: Task 0 - 简化 session_manager.rs 降级判断逻辑
    │
    └─ 移除错误关键字判断，改为 if has_resume

Step 2: Task 1 - 简化 claude_code_launcher.rs 会话创建逻辑
    │
    ├─ 移除 load_session 尝试
    ├─ 统一使用 new_session
    └─ 移除 session_id_for_closure 变量

Step 3: Task 2 - 移除 launch 方法的 session_id 参数
    │
    └─ 更新方法签名和文档

Step 4: Task 3 - 更新 session_manager.rs 的 launch 调用
    │
    ├─ 更新第一次 launch 调用
    └─ 更新降级重试调用

Step 5: Task 5 - 清理未使用的 import
    │
    └─ 移除 LoadSessionRequest

Step 6: 验证
    │
    ├─ 编译验证
    ├─ 运行测试
    └─ 手动测试 resume 流程
```

---

## 风险评估

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| 移除 session_id 参数导致编译错误 | 低 | 低 | 搜索所有调用方并更新 |
| Resume 功能回归 | 低 | 中 | 简化降级机制更健壮 |
| 逻辑改动影响其他功能 | 低 | 中 | 充分测试 |

---

## 预估工作量

| Task | 预估代码行数 | 复杂度 |
|------|-------------|--------|
| Task 0 | -4, +2 | 低 |
| Task 1 | -30, +15 | 低 |
| Task 2 | -2, +5 | 低 |
| Task 3 | -2, +0 | 低 |
| Task 5 | -1, +0 | 低 |
| **总计** | **约 -39 行，+22 行** | **低** |

---

## 附录：完整数据流（改动后）

```
┌─────────────────────────────────────────────────────────────────┐
│  HTTP/gRPC Request                                              │
│  ChatRequest { session_id: "abc-123", prompt: "..." }           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  acp_worker.rs                                                  │
│  ├─ 检查内存中是否存在会话                                        │
│  │   └─ 存在且 session_id 匹配 → with_resume_session_id()       │
│  └─ 构建 AgentStartConfig { resume_session_id: Some("abc-123") }│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  session_manager.rs                                             │
│  └─ launcher.launch(project_id, project_path,                   │
│                     model_provider, start_config, client)       │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  claude_code_launcher.rs                                        │
│  ├─ start_config.build_meta() 构建:                             │
│  │   {                                                          │
│  │     "claudeCode": {                                          │
│  │       "options": { "resume": "abc-123" }                     │
│  │     }                                                        │
│  │   }                                                          │
│  └─ NewSessionRequest::new(cwd).meta(meta)  ← 统一入口          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ ACP session/new
┌─────────────────────────────────────────────────────────────────┐
│  claude-code-acp                                                │
│  ├─ newSession() 解析 _meta.claudeCode.options.resume           │
│  └─ query({ options: { resume: "abc-123" } })                   │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
     ┌────────────────┐              ┌────────────────────────┐
     │ ✅ 会话存在    │              │ ❌ 会话不存在          │
     │ → 恢复上下文   │              │ → 抛出错误             │
     │ → 返回成功     │              │ "No conversation found"│
     └────────────────┘              └────────────────────────┘
                                              │
                                              ▼
                    ┌─────────────────────────────────────────────────┐
                    │  session_manager.rs 降级处理                     │
                    │  ├─ 检测错误关键字                               │
                    │  ├─ 创建 retry_config { resume_session_id: None }│
                    │  └─ 第二次 launcher.launch(retry_config)         │
                    └─────────────────────────────────────────────────┘
                                              │
                                              ▼
                    ┌─────────────────────────────────────────────────┐
                    │  ✅ 成功创建新会话                               │
                    │  └─ 返回新的 session_id                         │
                    └─────────────────────────────────────────────────┘
```
