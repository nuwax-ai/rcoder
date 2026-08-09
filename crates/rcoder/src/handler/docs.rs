//! Handler 层 OpenAPI 文档结构体
//!
//! 集中存放仅用于 utoipa OpenAPI 文档描述的结构体
//! （从 `agent_session_notification` 迁出，注解与输出保持完全一致）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// SSE 进度事件（用于 OpenAPI 文档）
///
/// 这是通过 SSE 流推送的实际事件结构，遵循标准 SSE 格式。
/// SSE 的 `data` 字段使用 `UnifiedSessionMessage` 结构体，包含完整的会话上下文信息：
///
/// ```text
/// event: agent_message_chunk
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"agent_message_chunk","data":{"content":{"type":"text","text":"Hello"},"index":0},"timestamp":"2024-12-16T10:30:00Z"}
///
/// event: tool_call
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"tool_call","data":{"tool_name":"read_file","tool_input":{"path":"test.rs"},"status":"started"},"timestamp":"2024-12-16T10:30:01Z"}
///
/// event: end_turn
/// data: {"session_id":"session456","message_type":"AgentSessionUpdate","sub_type":"end_turn","data":{"reason":"EndTurn","description":"正常结束"},"timestamp":"2024-12-16T10:30:05Z"}
/// ```
///
/// ---
///
/// ## 📝 重要说明
///
/// | 项目 | 说明 |
/// |------|------|
/// | **结构体用途** | 用于 OpenAPI 文档展示，描述 gRPC `ProgressEvent` 的完整信息 |
/// | **实际 SSE 格式** | 只有 `event` (= `sub_type`) 和 `data` (= `payload`) 两个字段 |
/// | **payload 类型** | 文档中为 `Value`（便于展示），实际传输为 JSON 字符串 |
/// | **元数据传输** | `message_type`, `request_id`, `timestamp` 在 gRPC 层传输，不直接出现在 SSE 流中 |
/// | **前端接收** | 使用 `EventSource`，通过 `event.type` 和 `event.data` 获取数据 |
///
/// ---
///
/// ## 🔄 数据流转换链路
///
/// ```text
/// [agent_runner]                    [rcoder]                      [前端]
/// UnifiedSessionMessage  ──gRPC──>  ProgressEvent  ──SSE──>  EventSource
///      │                                 │                        │
///      ├─ session_id ────────────────────┼────────> URL 路径传递   │
///      ├─ message_type ──────────────────┼────────> (gRPC 元数据)  │
///      ├─ sub_type ──────────────────────┼────────> event 字段 ───┤
///      ├─ data ──────────> payload ──────┼────────> data 字段 ────┤
///      ├─ timestamp ─────────────────────┼────────> (gRPC 元数据)  │
///      └─ request_id (在 data 中) ───────┴────────> (在 payload 中)│
/// ```
///
/// ---
///
/// ## 📊 message_type 与 sub_type 对应关系
///
/// | message_type | sub_type | 说明 |
/// |--------------|----------|------|
/// | `SessionPromptStart` | `prompt_start` | 用户发起对话，Agent 开始处理 |
/// | `SessionPromptEnd` | `end_turn` | Agent 正常完成任务 |
/// | `SessionPromptEnd` | `max_tokens` | 达到最大 token 数限制 |
/// | `SessionPromptEnd` | `max_turn_requests` | 达到最大请求数限制 |
/// | `SessionPromptEnd` | `refusal` | Agent 拒绝继续执行 |
/// | `SessionPromptEnd` | `cancelled` | 用户取消任务 |
/// | `SessionPromptEnd` | `error` | 执行过程中发生错误 |
/// | `AgentSessionUpdate` | `agent_message_chunk` | AI 响应文本片段 |
/// | `AgentSessionUpdate` | `agent_thought_chunk` | AI 思考过程片段 |
/// | `AgentSessionUpdate` | `user_message_chunk` | 用户消息片段 |
/// | `AgentSessionUpdate` | `tool_call` | 工具调用开始 |
/// | `AgentSessionUpdate` | `tool_call_update` | 工具调用状态更新 |
/// | `AgentSessionUpdate` | `plan` | 执行计划更新 |
/// | `AgentSessionUpdate` | `available_commands_update` | 可用命令列表更新 |
/// | `AgentSessionUpdate` | `current_mode_update` | 当前模式更新 |
/// | `Heartbeat` | `ping` | 心跳保活消息 |
///
/// ## 📦 完整 SSE 消息示例
///
/// ```json
/// {
///   "sessionId": "019b262c-e6d2-75d8-a374-2aa08bd93afd",
///   "messageType": "agentSessionUpdate",
///   "subType": "agent_message_chunk",
///   "data": {
///     "content": {"text": "你好，我来帮你...", "type": "text"},
///     "request_id": "d633d7b0ba9d4505ae6d87a5b274c580"
///   },
///   "timestamp": "2025-12-16T08:00:39.766Z"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEventDoc {
    /// 会话ID
    ///
    /// 与 URL 路径中的 `session_id` 参数一致，用于标识当前会话。
    #[schema(example = "019b262c-e6d2-75d8-a374-2aa08bd93afd")]
    pub session_id: String,

    /// 消息主类型
    ///
    /// 用于区分消息的生命周期阶段，便于前端进行状态管理。
    ///
    /// ## 可能的值
    ///
    /// | 值 | 说明 | 对应 subType |
    /// |----|------|--------------|
    /// | `sessionPromptStart` | 会话开始，Agent 开始处理用户请求 | `prompt_start` |
    /// | `sessionPromptEnd` | 会话结束，Agent 完成或终止处理 | `end_turn`, `max_tokens`, `cancelled`, `error` 等 |
    /// | `agentSessionUpdate` | 执行过程中的实时更新 | `agent_message_chunk`, `tool_call`, `plan` 等 |
    /// | `heartbeat` | 心跳消息，用于保持 SSE 连接 | `ping` |
    ///
    /// ## 前端状态机示例
    ///
    /// ```javascript
    /// eventSource.addEventListener('agent_message_chunk', (event) => {
    ///   const msg = JSON.parse(event.data);
    ///   switch (msg.messageType) {
    ///     case 'sessionPromptStart':
    ///       setStatus('processing');
    ///       break;
    ///     case 'agentSessionUpdate':
    ///       handleUpdate(msg.subType, msg.data);
    ///       break;
    ///     case 'sessionPromptEnd':
    ///       setStatus('completed');
    ///       break;
    ///     case 'heartbeat':
    ///       // 忽略或更新最后活跃时间
    ///       break;
    ///   }
    /// });
    /// ```
    #[schema(example = "agentSessionUpdate")]
    pub message_type: String,

    /// 消息子类型（作为 SSE 的 event 字段）
    ///
    /// 这是 SSE 事件的核心标识，前端应根据此字段决定如何处理 `data`。
    ///
    /// ## 完整的 subType 列表
    ///
    /// ### 会话生命周期事件
    /// | subType | messageType | 说明 |
    /// |---------|-------------|------|
    /// | `prompt_start` | sessionPromptStart | 会话开始 |
    /// | `end_turn` | sessionPromptEnd | 正常结束 |
    /// | `max_tokens` | sessionPromptEnd | token 限制 |
    /// | `max_turn_requests` | sessionPromptEnd | 请求数限制 |
    /// | `refusal` | sessionPromptEnd | Agent 拒绝 |
    /// | `cancelled` | sessionPromptEnd | 用户取消 |
    /// | `error` | sessionPromptEnd | 执行错误 |
    ///
    /// ### Agent 执行过程事件
    /// | subType | 说明 | 典型用途 |
    /// |---------|------|----------|
    /// | `agent_message_chunk` | AI 响应文本片段 | 流式显示 AI 回复 |
    /// | `agent_thought_chunk` | AI 思考过程片段 | 显示推理过程（可折叠） |
    /// | `user_message_chunk` | 用户消息片段 | 回显用户输入 |
    /// | `tool_call` | 工具调用开始 | 显示正在执行的操作 |
    /// | `tool_call_update` | 工具调用状态更新 | 显示工具执行结果 |
    /// | `plan` | 执行计划 | 显示任务分解步骤 |
    /// | `available_commands_update` | 可用命令更新 | 更新交互按钮 |
    /// | `current_mode_update` | 模式更新 | 显示当前工作模式 |
    ///
    /// ### 系统事件
    /// | subType | 说明 |
    /// |---------|------|
    /// | `ping` | 心跳保活 |
    ///
    /// ## 前端监听示例
    ///
    /// ```javascript
    /// const eventSource = new EventSource('/agent/progress/session_123');
    ///
    /// // 监听特定事件
    /// eventSource.addEventListener('agent_message_chunk', handleChunk);
    /// eventSource.addEventListener('tool_call', handleToolCall);
    /// eventSource.addEventListener('end_turn', handleComplete);
    /// ```
    #[schema(example = "agent_message_chunk")]
    pub sub_type: String,

    /// ACP 消息的完整 JSON 载荷
    ///
    /// 这是一个 JSON 对象，包含完整的 ACP (Agent Client Protocol) 消息数据。
    /// 具体结构取决于 `subType`，前端应根据 `subType` 解析此 JSON。
    ///
    /// ---
    ///
    /// ## 📋 各 subType 对应的 data 结构
    ///
    /// ### 1. `prompt_start` - 会话开始
    /// ```json
    /// {
    ///   "request_id": "req_123"  // 可选
    /// }
    /// ```
    ///
    /// ### 2. `end_turn` / `max_tokens` / `cancelled` 等 - 会话结束
    /// ```json
    /// {
    ///   "reason": "EndTurn",           // 停止原因枚举值
    ///   "description": "正常结束",      // 人类可读的描述
    ///   "error_message": "...",        // 可选，错误时才有
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    /// **reason 可能的值**: `EndTurn`, `MaxTokens`, `MaxTurnRequests`, `Refusal`, `Cancelled`
    ///
    /// ### 3. `error` - 执行错误
    /// ```json
    /// {
    ///   "code": -1,                    // 错误代码
    ///   "message": "执行失败: ...",     // 错误消息
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 4. `ping` - 心跳消息
    /// ```json
    /// {
    ///   "type": "heartbeat",
    ///   "message": "keep-alive",
    ///   "timestamp": "2024-01-01T00:00:00Z"
    /// }
    /// ```
    ///
    /// ### 5. `agent_message_chunk` - AI 响应文本片段
    /// ```json
    /// {
    ///   "content": {
    ///     "type": "text",              // 内容类型
    ///     "text": "你好，我来帮你..."   // 文本内容
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 6. `agent_thought_chunk` - AI 思考过程片段
    /// ```json
    /// {
    ///   "content": {
    ///     "type": "thinking",
    ///     "thinking": "正在分析用户的请求..."
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 7. `tool_call` - 工具调用
    /// ```json
    /// {
    ///   "tool_use_id": "tool_123",     // 工具调用 ID
    ///   "tool_name": "read_file",      // 工具名称
    ///   "tool_input": {                // 工具输入参数
    ///     "path": "src/main.rs"
    ///   },
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 8. `tool_call_update` - 工具调用状态更新
    /// ```json
    /// {
    ///   "tool_use_id": "tool_123",     // 工具调用 ID
    ///   "status": "running",           // 状态: running, success, error
    ///   "output": "...",               // 可选，工具输出
    ///   "error": "...",                // 可选，错误信息
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 9. `plan` - 执行计划
    /// ```json
    /// {
    ///   "steps": [                     // 计划步骤列表
    ///     {"description": "分析代码结构", "status": "completed"},
    ///     {"description": "修改文件", "status": "in_progress"},
    ///     {"description": "运行测试", "status": "pending"}
    ///   ],
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 10. `available_commands_update` - 可用命令更新
    /// ```json
    /// {
    ///   "available_commands": ["yes", "no", "explain"],
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    ///
    /// ### 11. `current_mode_update` - 当前模式更新
    /// ```json
    /// {
    ///   "current_mode_id": "code_review",
    ///   "request_id": "req_123"        // 可选
    /// }
    /// ```
    #[schema(
        example = json!({
            "content": {
                "type": "text",
                "text": "正在分析您的请求..."
            },
            "request_id": "req_123"
        })
    )]
    pub data: serde_json::Value,

    /// 事件时间戳（ISO 8601 格式）
    ///
    /// ## 格式
    ///
    /// - **类型**: ISO 8601 字符串
    /// - **时区**: UTC（以 `Z` 结尾）
    /// - **精度**: 毫秒
    ///
    /// ## 用途
    ///
    /// - **事件排序**: 确保事件按正确的时间顺序处理
    /// - **延迟计算**: 前端可计算网络延迟
    /// - **超时检测**: 检测是否有事件丢失或延迟过大
    /// - **日志记录**: 记录精确的事件发生时间
    ///
    /// ## 前端使用示例
    ///
    /// ```javascript
    /// eventSource.addEventListener('agent_message_chunk', (event) => {
    ///   const msg = JSON.parse(event.data);
    ///
    ///   // 直接解析 ISO 8601 字符串
    ///   const eventTime = new Date(msg.timestamp);
    ///
    ///   // 计算网络延迟
    ///   const latency = Date.now() - eventTime.getTime();
    ///   console.log(`事件延迟: ${latency}ms`);
    ///
    ///   // 格式化显示
    ///   const timeStr = eventTime.toLocaleTimeString();
    /// });
    /// ```
    ///
    /// ## 注意事项
    ///
    /// - 时间戳在 `agent_runner` 端生成，反映事件的实际发生时间
    /// - 由于网络传输，前端收到事件时可能有几十到几百毫秒的延迟
    #[schema(example = "2025-12-16T08:00:39.766Z")]
    pub timestamp: String,
}

/// SSE 错误事件（用于 OpenAPI 文档）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SseErrorEvent {
    /// 错误代码
    #[schema(example = "GRPC_CONNECTION_ERROR")]
    pub code: String,
    /// 错误消息
    #[schema(example = "无法连接到 Agent 服务")]
    pub message: String,
}
