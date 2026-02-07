mod acp_agent;
pub mod cleanup_task;

use crate::CancelNotificationRequestWrapper;
// 导出 agent_worker 相关类型和函数
// AgentRequest 是 SACP 版本的新类型，LocalSetAgentRequest 是向后兼容别名
#[allow(deprecated)]
pub use acp_agent::{AgentRequest, LocalSetAgentRequest, agent_worker_with_heartbeat};
use shared_types::AgentLifecycleGuard;
// SACP 类型导入
use sacp::schema::{PromptRequest, SessionId};
use dashmap::DashMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use rcoder_proxy::{PingoraServerManager, ProxyConfig as PingoraProxyConfig};
use tracing::{error, info};
use crate::config::ProxyConfig;

/// Pingora 启动结果
pub struct PingoraStartResult {
    /// Pingora 服务器管理器包装器（用于发送关闭信号）
    server_manager: Arc<tokio::sync::Mutex<PingoraServerManager>>,
}

impl PingoraStartResult {
    /// 停止 Pingora 服务器
    pub async fn stop(&mut self) {
        let mut guard = self.server_manager.lock().await;
        let _ = guard.stop().await;
    }
}

/// 启动 Pingora 代理服务
///
/// 封装 Pingora 的创建和启动逻辑，供 main.rs 和 http_server/start.rs 复用
#[must_use]
pub fn start_pingora(
    proxy_config: &ProxyConfig,
    shared_api_key_manager: Arc<dashmap::DashMap<String, shared_types::ModelProviderConfig>>,
) -> PingoraStartResult {
    info!(
        "🚀 启动 Pingora 反向代理服务，监听端口: {}",
        proxy_config.listen_port
    );
    info!(
        "🔄 代理路由格式: /proxy/{{port}}{{/path}} - 例如: /proxy/{}/health",
        proxy_config.default_backend_port
    );

    let pingora_config = PingoraProxyConfig {
        listen_port: proxy_config.listen_port,
        default_backend_port: proxy_config.default_backend_port,
        backend_host: proxy_config.backend_host.clone(),
        port_param: proxy_config.port_param.clone(),
        config_file: None,
        verbose: false,
    };

    // 创建 Pingora 服务器管理器
    let server_manager = PingoraServerManager::new(pingora_config)
        .with_api_key_manager(shared_api_key_manager);

    let pingora_service = server_manager.service();

    // 启动健康检查循环（按配置）
    if proxy_config.health_check.enabled {
        let hc = &proxy_config.health_check;
        pingora_service.start_health_check_loop(hc.interval_seconds, hc.timeout_seconds * 1000);
    }

    // 包装为 Arc<Mutex<...>> 以便共享
    let server_manager = Arc::new(tokio::sync::Mutex::new(server_manager));
    let server_manager_for_spawn = server_manager.clone();

    // 在后台任务中启动 Pingora
    tokio::spawn(async move {
        // 从 Arc<Mutex<...>> 中获取锁并启动
        let mut guard = server_manager_for_spawn.lock().await;
        if let Err(e) = guard.start().await {
            error!("Pingora 代理服务器启动失败: {}", e);
        }
    });

    info!("✅ Pingora 代理服务已启动在端口 {}", proxy_config.listen_port);

    PingoraStartResult {
        server_manager,
    }
}

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// ACP协议的连接信息
pub struct AcpConnectionInfo {
    /// 会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道（使用新类型）
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// Agent停止句柄（将被包装为守卫并放入 ProjectAndAgentInfo）
    pub stop_handle: Option<Arc<AgentLifecycleGuard>>,
}
