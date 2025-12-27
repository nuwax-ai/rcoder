//! Session 验证模块
//!
//! 提供基于 ACP API 的会话存在性检查功能。
//! 用于在 Resume 会话前预检查目标 session_id 是否有效。

use agent_client_protocol::{Agent, ClientSideConnection, ListSessionsRequest};
use tracing::{debug, info, warn};

/// 通过 list_sessions API 检查会话是否存在
///
/// 在尝试 Resume 会话前调用此函数，可以提前知道目标会话是否有效，
/// 避免无效 Resume 导致的超时等待。
///
/// # 参数
/// - `client_conn`: 已初始化的 ACP 连接
/// - `session_id`: 要检查的会话 ID
///
/// # 返回
/// - `Ok(true)`: 会话存在
/// - `Ok(false)`: 会话不存在
/// - `Err(_)`: API 调用失败（可能是 Agent 不支持 list_sessions）
///
/// # 示例
/// ```ignore
/// let exists = check_session_exists_via_api(&client_conn, "session-123").await?;
/// if exists {
///     // 可以安全地使用 resume 参数
/// } else {
///     // 跳过 resume，创建新会话
/// }
/// ```
pub async fn check_session_exists_via_api(
    client_conn: &ClientSideConnection,
    session_id: &str,
) -> Result<bool, agent_client_protocol::Error> {
    debug!("🔍 调用 list_sessions 检查会话是否存在: {}", session_id);

    let response = client_conn
        .list_sessions(ListSessionsRequest::new())
        .await?;

    let exists = response
        .sessions
        .iter()
        .any(|s| s.session_id.0.as_ref() == session_id);

    if exists {
        info!("✅ 会话存在（通过 API 验证）: {}", session_id);
    } else {
        warn!("⚠️ 会话不存在（通过 API 验证）: {}", session_id);
    }

    Ok(exists)
}

#[cfg(test)]
mod tests {
    // TODO: 添加单元测试（需要 mock ClientSideConnection）
}
