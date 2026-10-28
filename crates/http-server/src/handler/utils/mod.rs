//! Handler 工具模块
//!
//! 提供 handler 层共享的工具函数和常量。
//! agent 诊断 / gRPC 地址 / workspace 路径三族已迁 rcoder-engine（引擎侧
//! grpc/cleanup_task/service 消费），此处 re-export 保持 handler 内
//! `crate::handler::utils::X` 路径兼容（Phase 0 零行为搬迁）。

mod agent_mgmt_forward;
mod i18n_extractors;
mod locale;

#[allow(unused_imports)]
pub use rcoder_engine::utils::{
    build_computer_workspace_path, build_workspace_path, map_container_work_dir_to_host,
    project_dir, user_dir,
};
#[allow(unused_imports)]
pub use rcoder_engine::utils::{
    build_connection_error, diagnose, root_cause_message, wait_agent_ready,
};
// DiagCtx 定义在共享 crate container-runtime-api,这里 re-export 供 rcoder 内部统一引用
// (规避 lib/bin 双实例类型分裂)。
pub use container_runtime_api::DiagCtx;

#[allow(unused_imports)]
pub use agent_mgmt_forward::{
    AgentMgmtForwardCtx, InstallAgentParams, check_agent, get_agent, install_agent, list_agents,
    status_to_app_error, uninstall_agent,
};
pub use i18n_extractors::{I18nJsonOrQuery, I18nPath, I18nQuery};
pub use locale::get_locale_from_headers;
#[allow(unused_imports)]
pub use rcoder_engine::utils::{
    container_identity_from_name, extract_grpc_addr, extract_grpc_addr_with_port,
};
pub(crate) use workload_reuse::{admission_unconfirmed_app_error, refresh_workload_fence};

// 内部使用：路径验证（通过路径函数自动调用，无需外部直接使用）
#[allow(unused_imports)]
pub use rcoder_engine::utils::{PathValidationError, is_known_identifier, validate_identifier};

/// Keep the existing shutdown flight gate with an accepted task after its HTTP
/// caller disconnects. The shared counter covers ordinary Agent writes too.
pub(crate) fn owned_operation_flight(
    state: &crate::app_state::AppState,
) -> Result<shared_types::FlightGuard, crate::AppError> {
    state.userapp_op_flight.guard().map_err(|_| {
        crate::AppError::with_message(
            shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
            "Service is shutting down",
        )
    })
}

/// Detach the accepted operation's lifetime without losing request context.
/// Tokio spawn does not inherit task-locals or the currently entered span.
pub(crate) fn spawn_owned_operation<T>(
    flight: shared_types::FlightGuard,
    operation: impl Future<Output = T> + Send + 'static,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
{
    use tracing::Instrument as _;
    let locale = shared_types::current_request_locale();
    tokio::spawn(
        async move {
            let _flight = flight;
            shared_types::scope_request_locale(locale, operation).await
        }
        .in_current_span(),
    )
}

#[cfg(test)]
mod owned_operation_tests;
mod workload_reuse;
