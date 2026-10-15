//! 协调器服务对象：`PreviewCoordination` trait 的实现。
//!
//! 职责边界（不变量来源 spec.md）：
//! - 受理/发布/停止全部经权威存储 CAS；执行只发生在宿主（本机执行器或经
//!   `RemoteDispatch` 到宿主 Pod 的内部端点），目标地址来自权威库行；
//! - keep-alive 存活判定=ready+新鲜心跳或宿主 verify；死实例统一受理重建，
//!   绝不在非宿主副本无条件重建；降级信封 HTTP 200 + success:false；
//! - activity 只进内存累积器（30s 批量刷盘，GREATEST 不回退）；
//! - Unknown 恢复必须凭宿主 Pod 不存在证据，不凭 TTL。

use std::sync::Arc;
use std::time::Duration;

use shared_types::preview::degraded_reason;
use shared_types::{
    AcceptStartInput, AcceptStartOutcome, ExecutorLogChunk, ExecutorStartTicket,
    ExecutorStopOutcome, ExecutorVerifyReport, PREVIEW_PORT_MAX, PREVIEW_PORT_MIN,
    PREVIEW_PORT_RESERVED_MAX, PREVIEW_PORT_RESERVED_MIN, PreviewCoordination,
    PreviewCoordinationError, PreviewForwardCheck, PreviewHostIdentity, PreviewInstanceRecord,
    PreviewInstanceState, PreviewKeepAliveEnvelope, PreviewKeepAliveRequest, PreviewKeyInput,
    PreviewListEntry, PreviewPortAllocation, PreviewPortPoolStatus, PreviewProjectIdentity,
    PreviewRestartEnvelope, PreviewRestartRequest, PreviewRouteResolution, PreviewStartEnvelope,
    PreviewStartRequest, PreviewStopEnvelope, PreviewStopRequest, PreviewStoreError,
    is_preview_port, preview_key,
};

use crate::activity::ActivityAccumulator;
use crate::cache::RouteCache;
use crate::config::CoordinatorConfig;
use crate::dispatch::RemoteDispatch;
use crate::evidence::{HostEvidence, pod_uid_of};
use crate::identity::local_host_identity;

const START_PORT_ATTEMPTS: usize = 3;

fn unavailable(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Unavailable(context.to_string())
}

fn conflict(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Conflict(context.to_string())
}

fn invalid(context: impl std::fmt::Display) -> PreviewCoordinationError {
    PreviewCoordinationError::Invalid(context.to_string())
}

fn store_error(error: PreviewStoreError) -> PreviewCoordinationError {
    match error {
        PreviewStoreError::Unavailable(detail) => unavailable(format!("preview store: {detail}")),
        PreviewStoreError::Conflict(detail) => conflict(detail),
        PreviewStoreError::Invalid(detail) => invalid(detail),
    }
}

/// base path 规范化（与 file-server `process::normalize_base_path` 同规则；
/// 协调器不依赖 file-server，故此处同源复制并以此注释锚定）。
fn normalize_base_path(base: &str) -> String {
    let b = base.trim();
    if b.is_empty() {
        return "/".to_string();
    }
    if let Some(stripped) = b.strip_prefix('/') {
        let s = stripped.trim_end_matches('/');
        if s.is_empty() {
            return "/".to_string();
        }
        return format!("/{s}/");
    }
    format!("/{}/", b.trim_end_matches('/'))
}

/// 默认 base（端口组合；与 build_dev_args 默认一致）。
fn default_base(port: u16) -> String {
    format!("/proxy/{port}/")
}

fn compute_key(identity: &PreviewProjectIdentity) -> String {
    preview_key(&PreviewKeyInput {
        project_id: &identity.project_id,
        tenant_id: identity.tenant_id.as_deref(),
        space_id: identity.space_id.as_deref(),
        isolation_type: identity.isolation_type.as_deref(),
        resolved_path: &identity.resolved_path,
    })
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 协调路径的存储写回失败（CAS 让位/幂等收敛）只记日志。
fn note_store_result(result: Result<impl Send, PreviewStoreError>) {
    if let Err(error) = result {
        tracing::debug!("preview store write skipped (converged by CAS): {error}");
    }
}

// 拆分（file-server 大文件范式）：`coordinator` 服务对象与宿主证据恢复 /
// `start_stop` 受理启动与协调停止 / `coordination` PreviewCoordination
// trait 实现（方法保持无可见性限定）；共享自由函数/常量留本文件，测试
// 独立 `tests`。`crate::service::PreviewCoordinator` 旧路径经重导出不变；
// 跨块互访的私有字段/方法按原语义升 pub(super)。

mod coordination;
mod coordinator;
mod start_stop;
#[cfg(test)]
mod tests;

pub use coordinator::*;
