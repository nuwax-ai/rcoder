//! 应用管理处理器共享状态（注入到所有 handler 的 Arc<AppManagerState>）

use std::sync::Arc;

use crate::AppServiceTrait;

/// 应用状态（用于处理器）
#[derive(Clone)]
pub struct AppManagerState {
    pub app_service: Arc<dyn AppServiceTrait>,
    pub http_client: reqwest::Client,
}
