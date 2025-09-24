//! HTTP 路由和处理器模块

pub mod chat_handler;
pub mod health_handler;

use std::sync::Arc;

use crate::{handler::chat_handler::AppState, middleware};
use axum::{
    Router,
    routing::{get, post},
};

pub type SharedState = Arc<AppState>;

/// 创建 Axum 路由
pub fn create_router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health_handler::health_check))
        .route("/chat", post(chat_handler::handle_chat))
        .with_state(state)
}
