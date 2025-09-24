use acp_adapter::StreamUpdate;

use super::progress_events::{ProgressEvent, ProgressEventSubType, ProgressEventType};

/// 将StreamUpdate转换为ProgressEvent
fn stream_update_to_progress_event(stream_update: StreamUpdate) -> ProgressEvent {
    let (event_type, sub_type, session_id, content) = match stream_update {
        StreamUpdate::UserMessageChunk {
            session_id,
            content,
        } => (
            ProgressEventType::TaskStarted,
            ProgressEventSubType::UserMessageChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::AgentMessageChunk {
            session_id,
            content,
        } => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentMessageChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::AgentThoughtChunk {
            session_id,
            content,
        } => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentThoughtChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::ToolCall {
            session_id,
            tool_call,
        } => (
            ProgressEventType::ToolCall,
            ProgressEventSubType::ToolCall,
            session_id.0.to_string(),
            format!("正在执行工具调用: {}", tool_call.title),
        ),
        StreamUpdate::ToolCallUpdate {
            session_id,
            tool_call_update,
        } => (
            ProgressEventType::ToolCallUpdate,
            ProgressEventSubType::ToolCallUpdate,
            session_id.0.to_string(),
            format!("工具调用更新: {}", tool_call_update.title),
        ),
        StreamUpdate::Plan { session_id, plan } => (
            ProgressEventType::PlanUpdate,
            ProgressEventSubType::PlanUpdate,
            session_id.0.to_string(),
            "Plan已更新".to_string(),
        ),
        StreamUpdate::AvailableCommandsUpdate {
            session_id,
            available_commands,
        } => (
            ProgressEventType::AvailableCommandsUpdate,
            ProgressEventSubType::AvailableCommandsUpdate,
            session_id.0.to_string(),
            format!("可用命令已更新，共{}个命令", available_commands.len()),
        ),
        StreamUpdate::CurrentModeUpdate {
            session_id,
            current_mode_id,
        } => (
            ProgressEventType::CurrentModeUpdate,
            ProgressEventSubType::CurrentModeUpdate,
            session_id.0.to_string(),
            format!("当前模式已更新为: {}", current_mode_id.0),
        ),
        StreamUpdate::PromptCompleted {
            session_id,
            stop_reason,
        } => (
            ProgressEventType::TaskCompleted,
            ProgressEventSubType::PromptCompleted,
            session_id.0.to_string(),
            format!("任务完成: {:?}", stop_reason),
        ),
        StreamUpdate::Error { session_id, error } => (
            ProgressEventType::TaskFailed,
            ProgressEventSubType::Error,
            session_id.0.to_string(),
            format!("任务失败: {}", error),
        ),
        _ => {
            // 对于其他不常用的事件类型，返回通用的任务执行事件
            (
                ProgressEventType::Executing,
                ProgressEventSubType::Unknown,
                "unknown".to_string(),
                "任务执行中".to_string(),
            )
        }
    };

    ProgressEvent::new(session_id, event_type, sub_type, content)
}
