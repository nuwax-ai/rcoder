//! userApp 文件域转发层（rcoder 侧编排，实际处理在 per-app 开发容器内 file-server）。
//!
//! - [`forward`]：`/api/userapp/{*rest}` 通配透传 + `/api/computer/*` 拦截层
//!   （`X-Service-Type: userapp` 分流，反向代理转来的 TS 老路径原样透传）
//! - [`workspace`]：`POST /api/userapp/workspace` 创建项目显式入口
//! - [`db`]：`POST /api/userapp/db/{env}/align-credentials` PG 凭据对齐
//! - 本模块：路由聚合 + 开发容器 ensure-workspace 公共调用
//!
//! 容器定位/创建复用 [`crate::userapp_publish::agent_runner::ensure_userapp_builder`]
//! （幂等；注册 state.projects 防孤立清理）。

pub(crate) mod db;
mod forward;
pub(crate) mod workspace;

use std::sync::Arc;

use axum::routing::Router;
use axum::routing::{any, post};

use crate::router::AppState;

// 分流 header 常量（X-Service-Type / X-App-Id）定义在 shared_types（与容器内
// file-server 共用单一事实源）；本模块转发 computer_intercept 拦截层给主 Router
// 装配。chat body 的 service_type 词表由 shared_types::ChatServiceScope 枚举承载。
pub(crate) use forward::computer_intercept;
pub(crate) use forward::invalidate_probe_cache;

/// userApp 域转发路由（挂 rcoder 主 Router；`/api/userapp` 族不再来自 file-server
/// 本地路由——路由合并时已排除）。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/userapp/workspace", post(workspace::create_workspace))
        .route(
            "/api/userapp/db/{env}/align-credentials",
            post(db::align_credentials),
        )
        .route("/api/userapp/{*rest}", any(forward::forward_userapp))
}

/// 容器内 file-server 幂等建 workspace 目录（execute-command 等接口的 cwd 前置；
/// create-workspace / chat 开发对话 / db 对齐共用的公共调用）。
///
/// 全新容器的 file-server 有启动窗口（镜像全套 agent_runner+PG+file-server），
/// 连接类失败按 5s/10s/15s 退避重试（HTTP 4xx/5xx 业务错误不重试，直接上抛）。
/// 错误返回面向日志的描述串（调用方各自映射响应类型）。
pub(crate) async fn ensure_workspace_via_dev(
    addr: &str,
    app_id: &str,
    user_id: &str,
) -> Result<(), String> {
    // 五档退避最坏 120s：agent_runner(file-server 60000) 在宿主高负载（多 builder 并发
    // 构建/对话）下启动可超 30s——原三档 30s 上限在 e2e 六场景并行时实测不够
    // （后发容器被先发容器负载拖慢 → 60000 连接失败）。
    const BACKOFF_SECS: [u64; 5] = [5, 10, 15, 30, 60];
    let mut last_err = String::new();
    for (attempt, delay) in std::iter::once(0u64)
        .chain(BACKOFF_SECS.iter().copied())
        .enumerate()
    {
        if attempt > 0 {
            tracing::info!(
                "[USERAPP_FORWARD] ensure-workspace retry {}/5 after {delay}s (dev container starting): app_id={app_id}",
                attempt
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        let resp = crate::http_client::shared_client()
            .post(format!("{addr}/api/userapp/ensure-workspace"))
            .timeout(std::time::Duration::from_secs(30))
            .json(&serde_json::json!({"appId": app_id, "userId": user_id}))
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                // 业务错误（4xx/5xx 响应）重试无益，直接上抛
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("ensure-workspace returned {status}: {text}"));
            }
            Err(e) => {
                last_err = format!("dev container ensure-workspace failed: {e}");
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::workspace::CreateWorkspaceBody;

    #[test]
    fn create_workspace_body_is_camel_case() {
        let raw = serde_json::json!({"appId": "app-1", "userId": "u1"});
        let body: CreateWorkspaceBody = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(body.app_id, "app-1");
        assert_eq!(body.user_id, "u1");
    }
}
