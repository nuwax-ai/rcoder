//! Chat Handler 共享逻辑
//!
//! 封装 chat 请求处理的核心业务逻辑，供 gRPC 和 HTTP 复用。
//!
//! ## 结构
//!
//! `handle_chat_core` 仅负责编排，具体流程拆为子模块：
//! 1. [`probe`] —— 模型探活
//! 2. [`prepare`] —— 会话准备
//! 3. [`dispatch`] —— 任务下发
//! 4. [`finalize`] —— 结果组装
//!
//! 类型定义见 [`types`]，取消逻辑见同目录 `../cancel.rs`。

mod dispatch;
mod finalize;
mod prepare;
mod probe;
mod types;

pub use types::{ChatHandlerContext, ChatHandlerInput, ChatHandlerOutput};

use tracing::info;

// ---------------------------------------------------------------------------
// 编排主入口
// ---------------------------------------------------------------------------

/// 执行 Chat 请求的核心逻辑（编排四阶段：探活 → 准备 → 下发 → 组装）。
pub async fn handle_chat_core(
    input: ChatHandlerInput,
    context: &ChatHandlerContext,
) -> ChatHandlerOutput {
    let project_id = input.project_id.clone();
    let session_id = input.session_id.clone();
    let request_id = input.request_id.clone();

    info!(
        "[ChatHandler] Starting to process request: project_id={}, session_id={:?}, prompt_len={}, has_model_config={}",
        project_id,
        session_id,
        input.prompt.len(),
        input.model_config.is_some()
    );

    // ========== 阶段0: 模型探活(仅新建会话) ==========
    if let Some(blocked) = probe::run_model_probe(&input, &project_id, &session_id).await {
        return blocked;
    }

    // ========== 阶段1: 会话准备（ensure session/registry） ==========
    let preparation = match prepare::prepare_session(&input, &project_id, &session_id).await {
        Ok(preparation) => preparation,
        Err(output) => return output,
    };

    // ========== 阶段2: 任务下发 ==========
    let agent_request = match dispatch::dispatch_task(
        input,
        context,
        &project_id,
        &session_id,
        &request_id,
        &preparation,
    )
    .await
    {
        Ok(agent_request) => agent_request,
        Err(output) => return output,
    };

    // ========== 阶段3: 结果组装 ==========
    finalize::finalize_response(
        context,
        agent_request,
        preparation,
        project_id,
        session_id,
        request_id,
    )
    .await
}
