//! Custom Page 预览路由单测：路由注册矩阵 + 跨 Pod 转发重写分支。
//!
//! 完整的"宿主校验 410/503/404"与 HMR ws 端到端行为由 Compose/K8s E2E 覆盖
//! （真实 vite + 真实 pingora 栈）；此处锁定可单测的确定性部分。

use std::sync::Arc;

use matchit::Router;

use crate::route_type::RouteType;
use crate::router::create_router;
use crate::service::handlers::port_proxy::handle_port_proxy_request;
use crate::service::types::{PreviewRouteDeps, TrackingCtx};

/// 协调桩：resolve_route 可编程，其余方法按"不可达"失败语义返回。
struct StubCoordination {
    resolution: shared_types::PreviewRouteResolution,
}

#[async_trait::async_trait]
impl shared_types::PreviewCoordination for StubCoordination {
    async fn start_dev(
        &self,
        _req: shared_types::PreviewStartRequest,
    ) -> Result<shared_types::PreviewStartEnvelope, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn stop_dev(
        &self,
        _req: &shared_types::PreviewStopRequest,
    ) -> Result<shared_types::PreviewStopEnvelope, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn restart_dev(
        &self,
        _req: shared_types::PreviewRestartRequest,
    ) -> Result<shared_types::PreviewRestartEnvelope, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn keep_alive_dev(
        &self,
        _req: &shared_types::PreviewKeepAliveRequest,
    ) -> Result<shared_types::PreviewKeepAliveEnvelope, shared_types::PreviewCoordinationError>
    {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn list_dev(
        &self,
    ) -> Result<Vec<shared_types::PreviewListEntry>, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn read_dev_log(
        &self,
        _project_id: &str,
        _log_type: &str,
        _start_index: usize,
    ) -> Result<shared_types::ExecutorLogChunk, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn port_pool_status(
        &self,
    ) -> Result<shared_types::PreviewPortPoolStatus, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn resolve_route(&self, _port: u16) -> shared_types::PreviewRouteResolution {
        self.resolution.clone()
    }
    async fn invalidate_route(&self, _port: u16) {}
    async fn check_forward(
        &self,
        _instance_id: &str,
        _port: u16,
    ) -> shared_types::PreviewForwardCheck {
        shared_types::PreviewForwardCheck::Allowed
    }
    async fn internal_stop(
        &self,
        _preview_key: &str,
        _instance_id: &str,
        _operation_id: &str,
        _revision: i64,
    ) -> Result<shared_types::ExecutorStopOutcome, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
    async fn internal_verify(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<shared_types::ExecutorVerifyReport, shared_types::PreviewCoordinationError> {
        Err(shared_types::PreviewCoordinationError::Invalid(
            "stub".into(),
        ))
    }
}

fn slot_with(
    resolution: shared_types::PreviewRouteResolution,
) -> Arc<arc_swap::ArcSwapOption<Arc<PreviewRouteDeps>>> {
    Arc::new(arc_swap::ArcSwapOption::from(Some(Arc::new(Arc::new(
        PreviewRouteDeps {
            coordination: Arc::new(StubCoordination { resolution }),
            peer_api_port: 8086,
            internal_token: "unit-test-token".into(),
        },
    )))))
}

fn empty_slot() -> Arc<arc_swap::ArcSwapOption<Arc<PreviewRouteDeps>>> {
    Arc::new(arc_swap::ArcSwapOption::from(None))
}

fn make_request(uri: &str) -> (pingora_http::RequestHeader, http::Uri) {
    let original: http::Uri = uri.parse().expect("uri");
    let mut header = pingora_http::RequestHeader::build(&http::Method::GET, uri.as_bytes(), None)
        .expect("header");
    header.insert_header("Host", "example.com").expect("host");
    (header, original)
}

/// 跑一遍 request 阶段（matchit 匹配与 handler 同作用域——params 借用存活）。
async fn run_request_phase(
    router: &Router<RouteType>,
    slot: &Arc<arc_swap::ArcSwapOption<Arc<PreviewRouteDeps>>>,
    uri: &str,
) -> (pingora_http::RequestHeader, TrackingCtx) {
    let (mut header, original) = make_request(uri);
    let matched = router.at(uri).expect("route match");
    let mut ctx = TrackingCtx::new();
    handle_port_proxy_request(&mut header, &original, matched.params, true, slot, &mut ctx)
        .await
        .expect("request phase");
    (header, ctx)
}

#[test]
fn preview_forward_route_registration_matrix() {
    let router = create_router().expect("router");
    for path in [
        "/internal/preview-forward/inst-1/4200/page/index.html",
        "/internal/preview-forward/inst-1/4200",
    ] {
        let matched = router.at(path).expect(path);
        assert!(matches!(matched.value, RouteType::PreviewForward), "{path}");
    }
    // 非 internal 前缀不受影响
    assert!(matches!(
        router.at("/proxy/4200/page/").expect("proxy").value,
        RouteType::PortProxy
    ));
}

#[tokio::test]
async fn port_proxy_forwards_to_remote_host_when_resolved() {
    let router = create_router().expect("router");
    let slot = slot_with(shared_types::PreviewRouteResolution::Forward {
        instance_id: "inst-1".into(),
        port: 4200,
        host_ip: "10.1.2.3".into(),
    });
    let (header, ctx) = run_request_phase(&router, &slot, "/proxy/4200/page/index.html?a=1").await;

    assert_eq!(
        header.uri.path(),
        "/internal/preview-forward/inst-1/4200/page/index.html"
    );
    assert_eq!(header.uri.query(), Some("a=1"), "query 保留");
    assert_eq!(
        header
            .headers
            .get("x-preview-internal-token")
            .and_then(|v| v.to_str().ok()),
        Some("unit-test-token"),
        "客户端伪造令牌被进程令牌覆盖"
    );
    assert_eq!(
        ctx.preview_peer,
        Some(("10.1.2.3".to_string(), 8086)),
        "上游覆盖为宿主 Pod 主 API 端口"
    );
    assert_eq!(ctx.preview_origin_port, Some(4200));
}

#[tokio::test]
async fn port_proxy_keeps_legacy_path_when_not_forwarded() {
    let router = create_router().expect("router");
    // 三种不转发解析：本机宿主 / 非预览 / 存储不可用降级
    for resolution in [
        shared_types::PreviewRouteResolution::Local {
            instance_id: "i".into(),
            port: 4200,
        },
        shared_types::PreviewRouteResolution::NotPreview,
        shared_types::PreviewRouteResolution::Unavailable,
    ] {
        let slot = slot_with(resolution);
        let (header, ctx) = run_request_phase(&router, &slot, "/proxy/4200/page/").await;
        assert_eq!(header.uri.path(), "/page/", "路径照旧剥离端口前缀");
        assert!(ctx.preview_peer.is_none(), "不覆盖上游");
    }
}

#[tokio::test]
async fn port_proxy_keeps_legacy_path_when_slot_empty() {
    let router = create_router().expect("router");
    let slot = empty_slot();
    let (header, ctx) = run_request_phase(&router, &slot, "/proxy/4200/page/").await;
    assert_eq!(header.uri.path(), "/page/");
    assert!(ctx.preview_peer.is_none());
}

#[tokio::test]
async fn forwarded_path_preserves_trailing_slash_and_deep_rest() {
    let router = create_router().expect("router");
    let slot = slot_with(shared_types::PreviewRouteResolution::Forward {
        instance_id: "inst-2".into(),
        port: 4321,
        host_ip: "10.1.2.4".into(),
    });
    let (header, _ctx) = run_request_phase(&router, &slot, "/proxy/4321/@vite/client").await;
    assert_eq!(
        header.uri.path(),
        "/internal/preview-forward/inst-2/4321/@vite/client",
        "深层资产路径（含 @ 前缀段）原样保留"
    );
}
