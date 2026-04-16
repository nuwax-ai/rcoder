//! Prometheus 指标模块
//!
//! 提供 Prometheus 指标的定义、记录和导出功能。
//! 使用 `metrics` crate 作为 facade，`metrics-exporter-prometheus` 作为后端。

use metrics::{
    counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram, Unit,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::time::Duration;
use tracing::info;

// ============= 指标名称常量 =============

/// HTTP 请求总数
pub const HTTP_REQUESTS_TOTAL: &str = "http_requests_total";
/// HTTP 请求耗时
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "http_request_duration_seconds";

/// gRPC 请求总数
pub const GRPC_REQUESTS_TOTAL: &str = "grpc_requests_total";
/// gRPC 请求耗时
pub const GRPC_REQUEST_DURATION_SECONDS: &str = "grpc_request_duration_seconds";

/// Agent 任务总数
pub const AGENT_TASKS_TOTAL: &str = "agent_tasks_total";
/// Agent 任务耗时
pub const AGENT_TASK_DURATION_SECONDS: &str = "agent_task_duration_seconds";
/// 活跃任务数
pub const AGENT_ACTIVE_TASKS: &str = "agent_active_tasks";

// ============= 初始化 =============

/// Initializing Prometheus 指标系统
///
/// 安装 Prometheus recorder 并返回 handle，用于渲染指标。
///
/// # Returns
///
/// 返回 `PrometheusHandle`，可通过 `render()` 方法获取 Prometheus 格式的指标文本。
///
/// # Example
///
/// ```no_run
/// use rcoder_telemetry::prometheus::init_prometheus;
///
/// fn main() {
///     let handle = init_prometheus().expect("Failed to init prometheus");
///     let metrics_text = handle.render();
///     println!("{}", metrics_text);
/// }
/// ```
pub fn init_prometheus() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("Failed to install Prometheus recorder: {}", e))?;

    // 注册指标描述
    register_metric_descriptions();

    info!("[Prometheus] Metrics system initialization completed");

    Ok(handle)
}

/// 注册指标描述（元数据）
fn register_metric_descriptions() {
    // HTTP 指标
    describe_counter!(
        HTTP_REQUESTS_TOTAL,
        Unit::Count,
        "Total number of HTTP requests"
    );
    describe_histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        Unit::Seconds,
        "HTTP request duration in seconds"
    );

    // gRPC 指标
    describe_counter!(
        GRPC_REQUESTS_TOTAL,
        Unit::Count,
        "Total number of gRPC requests"
    );
    describe_histogram!(
        GRPC_REQUEST_DURATION_SECONDS,
        Unit::Seconds,
        "gRPC request duration in seconds"
    );

    // Agent 指标
    describe_counter!(
        AGENT_TASKS_TOTAL,
        Unit::Count,
        "Total number of agent tasks"
    );
    describe_histogram!(
        AGENT_TASK_DURATION_SECONDS,
        Unit::Seconds,
        "Agent task duration in seconds"
    );
    describe_gauge!(
        AGENT_ACTIVE_TASKS,
        Unit::Count,
        "Current number of active agent tasks"
    );
}

// ============= HTTP 指标 =============

/// 记录 HTTP 请求
///
/// # Arguments
///
/// * `method` - HTTP 方法（GET, POST 等）
/// * `path` - 请求路径
/// * `status` - 响应状态码
pub fn record_http_request(method: &str, path: &str, status: u16) {
    counter!(
        HTTP_REQUESTS_TOTAL,
        "method" => method.to_string(),
        "path" => path.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// 记录 HTTP 请求耗时
///
/// # Arguments
///
/// * `method` - HTTP 方法
/// * `path` - 请求路径
/// * `duration` - 请求耗时
pub fn record_http_duration(method: &str, path: &str, duration: Duration) {
    histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "method" => method.to_string(),
        "path" => path.to_string()
    )
    .record(duration.as_secs_f64());
}

// ============= gRPC 指标 =============

/// 记录 gRPC 请求
///
/// # Arguments
///
/// * `method` - gRPC 方法名（如 "Chat", "SubscribeProgress"）
/// * `status` - 状态（"ok", "error"）
pub fn record_grpc_request(method: &str, status: &str) {
    counter!(
        GRPC_REQUESTS_TOTAL,
        "method" => method.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// 记录 gRPC 请求耗时
///
/// # Arguments
///
/// * `method` - gRPC 方法名
/// * `duration` - 请求耗时
pub fn record_grpc_duration(method: &str, duration: Duration) {
    histogram!(
        GRPC_REQUEST_DURATION_SECONDS,
        "method" => method.to_string()
    )
    .record(duration.as_secs_f64());
}

// ============= Agent 指标 =============

/// 记录 Agent 任务
///
/// # Arguments
///
/// * `project_id` - 项目 ID
/// * `status` - 任务状态（"success", "error", "timeout"）
pub fn record_agent_task(project_id: &str, status: &str) {
    counter!(
        AGENT_TASKS_TOTAL,
        "project_id" => project_id.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// 记录 Agent 任务耗时
///
/// # Arguments
///
/// * `project_id` - 项目 ID
/// * `duration` - 任务耗时
pub fn record_agent_task_duration(project_id: &str, duration: Duration) {
    histogram!(
        AGENT_TASK_DURATION_SECONDS,
        "project_id" => project_id.to_string()
    )
    .record(duration.as_secs_f64());
}

/// 设置活跃任务数
///
/// # Arguments
///
/// * `count` - 当前活跃任务数
pub fn set_active_tasks(count: u64) {
    gauge!(AGENT_ACTIVE_TASKS).set(count as f64);
}

/// 增加活跃任务数
pub fn inc_active_tasks() {
    gauge!(AGENT_ACTIVE_TASKS).increment(1.0);
}

/// 减少活跃任务数
pub fn dec_active_tasks() {
    gauge!(AGENT_ACTIVE_TASKS).decrement(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：这些测试需要先初始化 metrics recorder
    // 在实际测试中，可能需要使用 mock recorder

    #[test]
    fn test_metric_names() {
        assert_eq!(HTTP_REQUESTS_TOTAL, "http_requests_total");
        assert_eq!(GRPC_REQUESTS_TOTAL, "grpc_requests_total");
        assert_eq!(AGENT_ACTIVE_TASKS, "agent_active_tasks");
    }
}
