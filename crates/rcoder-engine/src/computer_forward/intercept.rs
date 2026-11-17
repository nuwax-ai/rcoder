//! computer 域 per-agent RBD 转发拦截层（P1 接线）。
//!
//! 挂载于 file-server 路由前置（与 userapp `computer_intercept` 同层），
//! 决策序（任一不命中即 `next.run` 原样放行——flag 关闭时与现状逐字节一致）：
//!
//! ```text
//! flag 开? → 路径在透传清单? → 三级定位有目标? → 混住判据(PVC 存在)?
//!   → wake(幂等 ensure agent, ≤8s 预算) → 全流式转发 http://{pod_ip}:8086
//! ```
//!
//! 放行分支的语义：POST body 类路由无 header 时（Java 未集成的存量流量）
//! 落回 rcoder 本地共享卷直读——共享用户正确；RBD 用户的此类请求在 Java
//! 补 `x-user-id` header 前不被拦截（集成期已知的窗口，见 plan §5.1 协作时序）。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use shared_types::ServiceType;

use crate::app_state::AppState;
use crate::service::computer_container_manager::{
    ComputerContainerManager, ContainerCreateOptions,
};

use super::{is_pass_through_path, resolve_target, TargetResolution};

/// wake 预算：spike 实测冷启动（STS 0→1 到文件可服务）5.0-9.5s，常态 5-6s。
/// 超预算返回 503+Retry-After；K8s 侧创建继续，下一请求幂等 ensure 走快路径。
const WAKE_BUDGET: Duration = Duration::from_secs(8);
/// Retry-After 建议值（秒）：覆盖冷启动尾延 + 调用方退避下限。
const RETRY_AFTER_SECS: &str = "10";
/// 快路径探活预算：稳态容器 8086 必在线, 只兜 readiness 尾窗（亚秒级）。
const FAST_PATH_PROBE: Duration = Duration::from_millis(750);
/// wake 后探活预算：phase Running → 8086 监听的实测间隔 ~200ms-2s。
const WAKE_PROBE: Duration = Duration::from_secs(3);

/// 拦截决策（纯逻辑, 供单测）：四道闸门全开才转发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptDecision {
    /// 原样放行到 rcoder 本地 file-server（共享卷直读）。
    PassThrough,
    /// 转发到目标 agent 容器内嵌 file-server。
    Forward,
}

fn decide(flag_on: bool, path_in_table: bool, target_present: bool, is_rbd_user: bool) -> InterceptDecision {
    if flag_on && path_in_table && target_present && is_rbd_user {
        InterceptDecision::Forward
    } else {
        InterceptDecision::PassThrough
    }
}

/// computer 域 RBD 转发拦截（axum middleware）。
pub async fn computer_rbd_intercept(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if !shared_types::per_agent_rbd_enabled() {
        return next.run(req).await;
    }
    // Docker Compose 形态硬短路: RBD 判据层在非 K8s runtime 下无意义
    // (trait 默认 state=NotMigrated 恒放行是行为兜底, 这里是显式防线——
    // 即便 env 误设也不会让后续闸门逻辑在 Docker 形态下有任何行为面)。
    // cfg! 是编译期常量, Docker 构建下此分支恒真、零运行时开销。
    if !shared_types::is_kubernetes_runtime() {
        return next.run(req).await;
    }
    // userapp 标记请求让行（userapp 转发层优先——同用户可能同时存在 computer
    // agent 与 userapp 容器, PVC 名前缀不同但判据链不该被交叉触发）
    if req
        .headers()
        .get(shared_types::SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(shared_types::is_userapp_service_type_value)
    {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if !is_pass_through_path(path) {
        return next.run(req).await;
    }
    let user_id_header = req
        .headers()
        .get(shared_types::USER_ID_HEADER)
        .and_then(|v| v.to_str().ok());
    let target = match resolve_target(user_id_header, req.uri().query(), path) {
        TargetResolution::Found(target) => target,
        TargetResolution::Absent => return next.run(req).await,
        TargetResolution::Invalid => {
            // 有定位来源但值非法: 显式 400——静默放行会让 RBD 用户请求落
            // 本地共享卷读到空数据(看似成功的错误响应)
            tracing::warn!(
                path,
                "[COMPUTER_FORWARD] invalid user identifier in request, rejecting"
            );
            return (axum::http::StatusCode::BAD_REQUEST, "invalid user identifier").into_response();
        }
    };
    // 混住判据: PVC 存在 **且 migrated 标记生效**才转发。
    // 只有 PVC 存在(phase-a 预同步中/已切流未完成)不够——那期间用户仍在
    // 共享卷读写, 转发会把请求送进 RBD 旧副本(读到过时数据)。
    let migration_state = match state
        .runtime()
        .workspace_migration_state(&target.user_id, &ServiceType::ComputerAgentRunner)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                user_id = %target.user_id,
                error = %error,
                "[COMPUTER_FORWARD] migration state check failed, pass through"
            );
            return next.run(req).await;
        }
    };
    let is_rbd_user =
        migration_state == shared_types::WorkspaceMigrationState::Migrated;
    if decide(true, true, true, is_rbd_user) == InterceptDecision::PassThrough {
        return next.run(req).await;
    }
    tracing::info!(
        user_id = %target.user_id,
        source = ?target.source,
        "[COMPUTER_FORWARD] {} -> agent container",
        req.uri().path()
    );
    forward_to_agent(&state, &target.user_id, req).await
}

/// wake（幂等 ensure agent 容器, ≤WAKE_BUDGET）+ 全流式转发其 :8086。
async fn forward_to_agent(state: &AppState, user_id: &str, req: Request) -> Response {
    // ① 快路径: 容器 Running 且 :8086 探活成功 → 直接转发（稳态分享请求
    // 毫秒级; 跳过 ensure——ensure 即使命中 pod_cache 也要走锁与状态比对）。
    // 探活兜 readiness 尾窗（phase Running ≠ 8086 已监听, 实测差 ~200ms）;
    // 失败自然落入慢路径 wake。
    if let Some(addr) = running_agent_addr(state, user_id).await {
        if let Some(ready_addr) = wait_file_server_ready(&addr, FAST_PATH_PROBE).await {
            return stream_to_addr(&ready_addr, req).await;
        }
    }
    // ② 慢路径: wake（幂等 ensure, ≤WAKE_BUDGET）。
    //
    // limits 必须从 kubernetes_config 解析（与 /computer/chat、/pod/ensure
    // 同一真源）——传 None 会让 k8s_agent_create 回落 DEFAULT_AGENT_* 常量,
    // 同一用户两条创建路径拿到不同 limits（OOM 行为分裂 + 双真源）。
    let resource_limits = state
        .config
        .kubernetes_config
        .as_ref()
        .and_then(|kc| kc.get_service_config(&ServiceType::ComputerAgentRunner))
        .map(|sc| sc.resource_limits.clone());
    if resource_limits.is_none() {
        tracing::error!(
            user_id,
            "[COMPUTER_FORWARD] kubernetes_config 未配置 computer-agent-runner 的 \
             resource_limits, 拒绝 wake（与 pod_handler 同口径 fail-fast）"
        );
        return service_unavailable("agent container resource limits not configured");
    }
    let options = ContainerCreateOptions {
        user_id: user_id.to_string(),
        // ComputerAgentRunner 的容器标识只取 user_id（container_identifier）；
        // project_id 在此族无消费者，占位同值防御空串校验。
        project_id: user_id.to_string(),
        resource_limits,
        pod_id: None,
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type: ServiceType::ComputerAgentRunner,
    };
    let info = match tokio::time::timeout(
        WAKE_BUDGET,
        ComputerContainerManager::ensure_container_for_user_with_type(&options, state.runtime()),
    )
    .await
    {
        Ok(Ok((info, _created))) => info,
        Ok(Err(error)) => {
            tracing::warn!(user_id, %error, "[COMPUTER_FORWARD] wake ensure failed");
            return service_unavailable("agent container unavailable");
        }
        Err(_) => {
            tracing::info!(user_id, "[COMPUTER_FORWARD] wake budget exceeded, 503+retry");
            return service_unavailable("agent container is starting, retry shortly");
        }
    };
    let Some(ip) = info.container_ip.split(':').next().filter(|s| !s.is_empty()) else {
        return service_unavailable("agent container address not ready");
    };
    let addr = format!("{}:{}", ip, shared_types::HTTP_DEFAULT_PORT);
    // ensure 等的是 phase Running; 8086 监听晚 ~200ms-2s（readinessProbe
    // period=1s）。预算内探活, 探不到按 503 降级（预览页重试吸收）。
    match wait_file_server_ready(&addr, WAKE_PROBE).await {
        Some(ready_addr) => stream_to_addr(&ready_addr, req).await,
        None => service_unavailable("agent container is starting, retry shortly"),
    }
}

/// 容器 Running 时的转发地址（None = 不在跑/查询失败, 走 wake 慢路径）。
async fn running_agent_addr(state: &AppState, user_id: &str) -> Option<String> {
    let info = state
        .runtime()
        .get_container_info_by_identifier(user_id, &ServiceType::ComputerAgentRunner)
        .await
        .ok()??;
    if info.status != "Running" {
        return None;
    }
    let ip = info.container_ip.split(':').next()?.trim();
    (!ip.is_empty()).then(|| format!("{}:{}", ip, shared_types::HTTP_DEFAULT_PORT))
}

/// 探活 agent 容器内嵌 file-server :8086（TCP connect, 250ms 间隔）。
/// 返回 Some(addr)=可转发; None=预算内未就绪。
async fn wait_file_server_ready(addr: &str, budget: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        // connect 必须带自身超时: 无超时时 SYN 被丢(pod 刚删无 endpoint,
        // Cilium drop 而非 RST)会让单次 connect 挂满内核 tcp_syn_retries
        // (~127s)——预算检查在 connect 之后, 750ms/3s 预算全部失效。
        // 剩余预算即本次 connect 的超时上限。
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        if tokio::time::timeout(remaining, tokio::net::TcpStream::connect(addr))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Some(addr.to_string());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// 全流式转发（与 userapp_forward::upstream 内核同语义的 computer 域实现：
/// 该内核私有且耦合 userapp 凭据/错误族, 待后续统一抽取; hop-by-hop 剥离
/// 按 RFC 9110 静态表 ∪ Connection 动态列举）。
async fn stream_to_addr(addr: &str, req: Request) -> Response {
    let target = format!("http://{addr}{}", req.uri());
    let (mut parts, body) = req.into_parts();
    parts.headers.remove("host");
    parts.headers.remove("x-api-key");
    let listed = connection_listed(&parts.headers);
    let mut outbound = crate::http_client::forward_client()
        .request(parts.method.clone(), &target);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name.as_str(), &listed) {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    let upstream = match outbound
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(error) => {
            tracing::warn!(addr, %error, "[COMPUTER_FORWARD] upstream request failed");
            return service_unavailable("agent container file-server unreachable, retry shortly");
        }
    };
    let status = upstream.status();
    let resp_listed = connection_listed(upstream.headers());
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_hop_by_hop(name.as_str(), &resp_listed) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|error| {
            tracing::error!(addr, %error, "[COMPUTER_FORWARD] build upstream response failed");
            axum::http::StatusCode::BAD_GATEWAY.into_response()
        })
}

fn service_unavailable(message: &str) -> Response {
    Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .header("Retry-After", RETRY_AFTER_SECS)
        .header("Content-Type", "application/json")
        .body(Body::from(format!(
            "{{\"success\":false,\"code\":\"CONTAINER_STARTING\",\"message\":\"{message}\"}}"
        )))
        .unwrap_or_else(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response())
}

/// 逐跳头静态表（RFC 9110 §7.6.1）。
const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "host",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authenticate",
    "proxy-authorization",
];

fn is_hop_by_hop(name: &str, connection_listed: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
        || connection_listed.iter().any(|listed| listed.eq_ignore_ascii_case(name))
}

fn connection_listed(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_all_gates_required() {
        assert_eq!(decide(true, true, true, true), InterceptDecision::Forward);
        // 任一闸门关闭 → 放行（flag 关 = 现状逐字节一致的关键）
        assert_eq!(decide(false, true, true, true), InterceptDecision::PassThrough);
        assert_eq!(decide(true, false, true, true), InterceptDecision::PassThrough);
        assert_eq!(decide(true, true, false, true), InterceptDecision::PassThrough);
        assert_eq!(decide(true, true, true, false), InterceptDecision::PassThrough);
    }

    #[test]
    fn hop_by_hop_static_and_dynamic() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "X-Custom, keep-alive".parse().unwrap());
        let listed = connection_listed(&headers);
        assert!(is_hop_by_hop("te", &listed));
        assert!(is_hop_by_hop("x-custom", &listed)); // Connection 动态列举
        assert!(!is_hop_by_hop("content-type", &listed));
        assert!(!is_hop_by_hop("x-app-id", &listed));
    }
}
