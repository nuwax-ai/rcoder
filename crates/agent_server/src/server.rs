//! HTTP 服务器模块

use crate::{
    api::{create_api_router, ApiState},
    config::AgentServerConfig,
    shutdown::{GracefulShutdown, ShutdownManager, SignalHandler},
    AgentManager, AgentServerError, AgentServerResult,
};
use anyhow::Result;
use std::sync::Arc;
use axum::{
    extract::Request,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing::{error, info, warn};

/// Agent Server
pub struct AgentServer {
    /// 配置
    config: AgentServerConfig,
    /// Agent 管理器
    agent_manager: Arc<AgentManager>,
    /// 关闭管理器
    shutdown_manager: ShutdownManager,
    /// 启动时间
    started_at: chrono::DateTime<chrono::Utc>,
}

impl AgentServer {
    /// 创建新的 Agent Server
    pub async fn new(config: AgentServerConfig) -> AgentServerResult<Self> {
        info!("创建 Agent Server");

        // 验证配置
        config.validate().map_err(|e| {
            AgentServerError::ConfigError(format!("配置验证失败: {}", e))
        })?;

        // 创建 Agent 管理器
        let (agent_manager, _shutdown_rx) = AgentManager::new(
            config.agent_type,
            config.project_id.clone(),
        );

        let agent_manager = Arc::new(agent_manager);

        // 初始化 Agent
        agent_manager.initialize().await?;

        // 创建关闭管理器
        let shutdown_manager = ShutdownManager::new();

        let server = Self {
            config,
            agent_manager,
            shutdown_manager,
            started_at: chrono::Utc::now(),
        };

        info!("Agent Server 创建完成");
        Ok(server)
    }

    /// 启动服务器
    pub async fn start(self) -> AgentServerResult<()> {
        info!("启动 Agent Server，端口: {}", self.config.port);

        // 启动 Agent
        self.agent_manager.start_agent().await?;

        // 创建 API 状态
        let api_state = ApiState {
            agent_manager: self.agent_manager.clone(),
            config: self.config.clone(),
            shutdown_manager: self.shutdown_manager.clone(),
        };

        // 创建路由
        let app = create_api_router(api_state, &self.config);

        // 绑定地址
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.port));
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            AgentServerError::HttpServerError(format!("绑定地址失败: {}", e))
        })?;

        info!("服务器监听地址: {}", addr);

        // 创建优雅关闭处理器
        let mut graceful_shutdown = GracefulShutdown::new(
            self.shutdown_manager.clone(),
            self.config.session_timeout_secs,
        );

        // 添加后台任务
        graceful_shutdown.add_task(tokio::spawn(async move {
            // 清理过期会话的任务
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(self.config.health_check_interval_secs),
            );

            loop {
                interval.tick().await;
                // TODO: 实现会话清理逻辑
                // self.agent_manager.cleanup_expired_sessions(self.config.session_timeout_secs).await;
            }
        }));

        // 启动优雅关闭任务
        let shutdown_task = {
            let server_clone = self.clone();
            tokio::spawn(async move {
                graceful_shutdown.wait_and_shutdown().await;
                info!("优雅关闭完成");
            })
        };

        // 启动 HTTP 服务器
        let server_future = axum::serve(listener, app).with_graceful_shutdown(async move {
            self.shutdown_manager.wait_for_shutdown().await;
            info!("收到关闭信号，开始优雅关闭 HTTP 服务器");
        });

        // 等待服务器完成或关闭信号
        match server_future.await {
            Ok(_) => {
                info!("HTTP 服务器正常结束");
            }
            Err(e) => {
                error!("HTTP 服务器错误: {}", e);
                return Err(AgentServerError::HttpServerError(format!("服务器错误: {}", e)));
            }
        }

        // 等待关闭任务完成
        let _ = shutdown_task.await;

        info!("Agent Server 已完全停止");
        Ok(())
    }

    /// 获取服务器状态
    pub async fn get_status(&self) -> AgentServerResult<ServerStatus> {
        let agent_status = self.agent_manager.get_agent_status().await?;
        let active_sessions = self.agent_manager.get_active_sessions_count().await;

        Ok(ServerStatus {
            is_running: matches!(agent_status, crate::agent::AgentStatus::Running),
            port: self.config.port,
            agent_type: self.config.agent_type,
            project_id: self.config.project_id.clone(),
            started_at: self.started_at,
            uptime_seconds: (chrono::Utc::now() - self.started_at).num_seconds() as u64,
            active_sessions,
            agent_status,
        })
    }

    /// 停止服务器
    pub async fn stop(&self) -> AgentServerResult<()> {
        info!("停止 Agent Server");

        // 发送停止信号
        self.shutdown_manager
            .send_shutdown(crate::shutdown::ShutdownSignal::Stop)
            .await
            .map_err(|e| {
                AgentServerError::Other(format!("发送停止信号失败: {}", e))
            })?;

        // 停止 Agent
        self.agent_manager.stop_agent().await?;

        info!("Agent Server 停止完成");
        Ok(())
    }
}

impl Clone for AgentServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            agent_manager: self.agent_manager.clone(),
            shutdown_manager: self.shutdown_manager.clone(),
            started_at: self.started_at,
        }
    }
}

/// 服务器状态
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerStatus {
    /// 是否运行中
    pub is_running: bool,
    /// 端口
    pub port: u16,
    /// Agent 类型
    pub agent_type: crate::config::AgentType,
    /// 项目 ID
    pub project_id: String,
    /// 启动时间
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// 运行时间 (秒)
    pub uptime_seconds: u64,
    /// 活跃会话数
    pub active_sessions: usize,
    /// Agent 状态
    pub agent_status: crate::agent::AgentStatus,
}

/// 启动服务器 (简化版本，用于 Docker 容器)
pub async fn start_server_simple(config: AgentServerConfig) -> Result<()> {
    info!("启动简化版 Agent Server");

    // 创建服务器
    let server = AgentServer::new(config).await?;

    // 设置信号处理
    let shutdown_manager = ShutdownManager::new();
    let _signal_handler = SignalHandler::new(shutdown_manager.clone())?;

    // 创建 API 状态
    let api_state = ApiState {
        agent_manager: server.agent_manager.clone(),
        config: server.config.clone(),
        shutdown_manager: shutdown_manager.clone(),
    };

    // 创建路由
    let app = create_api_router(api_state, &server.config);

    // 添加追踪中间件
    let app = app.layer(TraceLayer::new_for_http());

    // 绑定地址
    let addr = SocketAddr::from(([0, 0, 0, 0], server.config.port));
    let listener = TcpListener::bind(addr).await?;

    info!("服务器启动成功，监听地址: {}", addr);

    // 启动服务器并等待关闭信号
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_manager.wait_for_shutdown().await;
            info!("收到关闭信号，开始优雅关闭");
        })
        .await?;

    info!("服务器已停止");
    Ok(())
}