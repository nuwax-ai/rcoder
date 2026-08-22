//! Pod 容器管理 HTTP 处理器
//!
//! 提供 Pod 容器的统计、启动和保活功能。
//!
//! ## 接口列表
//! - `GET /computer/pod/count` - 获取容器数量统计
//! - `GET /computer/pod/list` - 获取所有容器信息（支持分页）
//! - `POST /computer/pod/ensure` - 启动/确保容器存在（幂等）
//! - `POST /computer/pod/keepalive` - 容器保活（刷新活动时间）

use axum::extract::State;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, instrument, warn};

use super::utils::{I18nJsonOrQuery, I18nQuery, container_identity_from_name};
use crate::router::AppState;
use crate::service::ComputerContainerManager;
use crate::service::computer_container_manager::ContainerCreateOptions;
use docker_manager::runtime_selection::RuntimeType;
// sync_single_vnc_backend 已移除，使用 ContainerLookupService 统一数据源
use crate::{AppError, HttpResult};
use shared_types::{
    ContainerBasicInfo, PodCountByServiceType, PodCountResponse, ProjectAndContainerInfo,
    ServiceResourceLimits, ServiceType, VncStatusResponse,
};

// pod_handler 目录化：类型/辅助/各 handler 按职责拆分（函数体原样搬迁，未做分解）
mod ensure;
mod helpers;
mod keepalive;
mod queries;
mod restart;
mod status;
#[cfg(test)]
mod tests;
mod types;

pub use ensure::*;
pub(crate) use helpers::resolve_resource_limits_from_config;
pub use keepalive::*;
pub use queries::*;
pub use restart::*;
pub use status::*;
pub use types::*;
