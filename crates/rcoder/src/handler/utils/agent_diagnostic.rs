//! agent pod 诊断 + 友好错误生成工具
//!
//! gRPC 连接 agent_runner 失败时,定位真实根因(OOMKilled / CrashLoopBackOff / 容器缺失 /
//! 启动中),并据此生成带根因的友好错误 —— 而非把裸 `transport error` 当作最终结论。
//!
//! 呈现原则(见 plan「错误呈现原则」):
//! - `transport error` 原文始终进 `tracing` 日志(排查必需),**不消灭**;
//! - 诊断出真实根因 → 错误码 [`ERR_AGENT_CONTAINER_UNAVAILABLE`],消息以根因为主;
//! - 诊断无根因(裸错误本身即线索)→ 错误码 [`ERR_GRPC_ERROR`],消息保留原文。
//!
//! [`ERR_AGENT_CONTAINER_UNAVAILABLE`]: shared_types::error_codes::ERR_AGENT_CONTAINER_UNAVAILABLE
//! [`ERR_GRPC_ERROR`]: shared_types::error_codes::ERR_GRPC_ERROR

use std::sync::Arc;
use std::time::{Duration, Instant};

use container_runtime_api::{AgentPodDiagnostic, ContainerRuntime};
use shared_types::ServiceType;
use shared_types::error_codes as ec;
use shared_types::error_codes::{get_error_message, get_i18n_message};
use tracing::warn;

/// 智能等待 pod ready 时的轮询间隔
const AGENT_READY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 诊断 agent pod 容器状态。
///
/// 失败不抛错(返回"未知"诊断 [`AgentPodDiagnostic::default`]),保证错误路径不二次失败 ——
/// 诊断本身绝不能让原本的连接错误变得更糟。
pub async fn diagnose(
    runtime: &Arc<dyn ContainerRuntime>,
    identifier: &str,
    service_type: ServiceType,
) -> AgentPodDiagnostic {
    match runtime.diagnose_agent_pod(identifier, &service_type).await {
        Ok(d) => d,
        Err(e) => {
            warn!("diagnose_agent_pod 失败,返回未知诊断: identifier={identifier}, err={e}");
            AgentPodDiagnostic::default()
        }
    }
}

/// gRPC 连接失败时,根据诊断结果生成 `(错误码, 错误消息)`。
///
/// 调用方据此构造各自领域的错误响应(chat → `HttpResult::error`,SSE → 错误事件,
/// agent-mgmt → `AppError`)。
///
/// # 参数
/// - `raw_err`:原始连接错误字符串(如 "Connection failed: transport error"),始终入日志。
pub async fn build_connection_error(
    runtime: &Arc<dyn ContainerRuntime>,
    identifier: &str,
    service_type: ServiceType,
    locale: &str,
    raw_err: &str,
) -> (String, String) {
    // 原文始终保留入日志(排查必需)
    warn!(
        "gRPC 连接失败(原文保留用于排查): identifier={identifier}, service_type={service_type:?}, err={raw_err}"
    );
    let d = diagnose(runtime, identifier, service_type).await;

    // 无根因且非启动中:transport 原文本身就是排查线索,保留它
    if !d.has_root_cause() && !d.is_starting_up() {
        let base = get_error_message(ec::ERR_GRPC_ERROR, locale);
        return (
            ec::ERR_GRPC_ERROR.to_string(),
            format!("{base} ({raw_err})"),
        );
    }

    // 有根因(或启动中):以根因为主,错误码 ERR_AGENT_CONTAINER_UNAVAILABLE
    let msg = if !d.exists {
        get_i18n_message("error.agent_container_not_found", locale)
    } else if d.is_oom() {
        // OOMKilled:附重启次数
        get_i18n_message("error.agent_container_oom", locale)
            .replace("{}", &d.restart_count.to_string())
    } else if d.is_crash_loop() {
        // CrashLoopBackOff:附退出码
        get_i18n_message("error.agent_container_crashloop", locale)
            .replace("{}", &d.last_exit_code.unwrap_or(-1).to_string())
    } else if d.is_starting_up() {
        get_i18n_message("error.agent_container_starting", locale)
    } else {
        // 其他根因(有 last_terminate_reason 等):基础串 + 可读 detail
        let base = get_i18n_message("error.agent_container_unavailable", locale);
        match &d.detail {
            Some(det) if !det.is_empty() => format!("{base} ({det})"),
            _ => base,
        }
    };

    (ec::ERR_AGENT_CONTAINER_UNAVAILABLE.to_string(), msg)
}

/// 智能等待 agent pod ready(容器冷启动 / OOM 重启后,等它就绪再重试 gRPC)。
///
/// 每 2s 诊断一次:ready 即返回 `true`;CrashLoopBackOff / 容器不存在(不可恢复)或超时
/// 返回 `false`(调用方转而走根因错误)。替代旧的"固定 sleep 3s × N"盲重试 ——
/// 旧策略对 30s+ 的启动窗口无能为力,这里一旦 ready 立即返回。
pub async fn wait_agent_ready(
    runtime: &Arc<dyn ContainerRuntime>,
    identifier: &str,
    service_type: ServiceType,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        // 直接调 diagnose_agent_pod,不经 diagnose() 包装 —— 后者在 K8s API 失败时返回
        // default(ready=true),会造成假阳性提前返回 "ready"。这里 API 失败视为"继续等待"。
        let d = match runtime.diagnose_agent_pod(identifier, &service_type).await {
            Ok(d) => d,
            Err(e) => {
                warn!("wait_agent_ready: diagnose 失败,继续等待: identifier={identifier}, err={e}");
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(AGENT_READY_POLL_INTERVAL).await;
                continue;
            }
        };
        if d.ready {
            return true;
        }
        // CrashLoopBackOff / 容器不存在:不可恢复,不再空等
        if !d.exists || d.is_crash_loop() {
            return false;
        }
        if Instant::now() >= deadline {
            return false; // 超时
        }
        tokio::time::sleep(AGENT_READY_POLL_INTERVAL).await;
    }
}
