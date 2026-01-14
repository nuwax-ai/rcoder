//! Session 验证模块
//!
//! 提供基于 ACP API 的会话存在性检查功能。
//! 用于在 Resume 会话前预检查目标 session_id 是否有效。
//!
//! 特性：
//! - 使用 moka 缓存 list_sessions 结果，TTL 10 秒
//! - 减少 API 调用次数，提高响应速度

use agent_client_protocol::{Agent, ClientSideConnection, ListSessionsRequest};
use moka::future::Cache;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, info, warn};

/// 全局 session 存在性缓存
/// Key: project_path, Value: 该项目下的 session_id 集合
static SESSION_CACHE: OnceLock<Cache<String, HashSet<String>>> = OnceLock::new();

/// 获取或初始化缓存（TTL 10 秒）
fn get_session_cache() -> &'static Cache<String, HashSet<String>> {
    SESSION_CACHE.get_or_init(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(10))
            .build()
    })
}

/// 通过 list_sessions API 检查会话是否存在（带缓存）
///
/// 在尝试 Resume 会话前调用此函数，可以提前知道目标会话是否有效，
/// 避免无效 Resume 导致的超时等待。
///
/// # 参数
/// - `client_conn`: 已初始化的 ACP 连接
/// - `session_id`: 要检查的会话 ID
/// - `project_path`: 项目路径，用作缓存 key
///
/// # 返回
/// - `Ok(true)`: 会话存在
/// - `Ok(false)`: 会话不存在
/// - `Err(_)`: API 调用失败（可能是 Agent 不支持 list_sessions）
///
/// # 缓存行为
/// - 首次调用会请求 API 并缓存结果
/// - 后续调用（10秒内）直接从缓存读取
/// - 缓存按 project_path 隔离
///
/// # 示例
/// ```ignore
/// let exists = check_session_exists_via_api(&client_conn, "session-123", "/path/to/project").await?;
/// if exists {
///     // 可以安全地使用 resume 参数
/// } else {
///     // 跳过 resume，创建新会话
/// }
/// ```
pub async fn check_session_exists_via_api(
    client_conn: &ClientSideConnection,
    session_id: &str,
    project_path: &str,
) -> Result<bool, agent_client_protocol::Error> {
    let cache = get_session_cache();

    // 尝试从缓存获取
    if let Some(session_ids) = cache.get(project_path).await {
        let exists = session_ids.contains(session_id);
        debug!(
            "🔍 从缓存检查会话: {} -> {}",
            session_id,
            if exists { "存在" } else { "不存在" }
        );
        return Ok(exists);
    }

    // 缓存未命中，调用 API
    debug!("🔍 缓存未命中，调用 list_sessions API: {}", session_id);
    let response = client_conn
        .list_sessions(ListSessionsRequest::new())
        .await?;

    // 构建 session_id 集合并缓存
    let session_ids: HashSet<String> = response
        .sessions
        .iter()
        .map(|s| s.session_id.0.to_string())
        .collect();

    let exists = session_ids.contains(session_id);

    // 存入缓存
    cache.insert(project_path.to_string(), session_ids).await;

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
