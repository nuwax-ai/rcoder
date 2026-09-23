//! Custom Page 预览路由单测：路由注册矩阵 + 跨 Pod 转发两阶段（决策/重写）分支。
//!
//! 完整的"宿主校验 410/503/404"与 HMR ws 端到端行为由 Compose/K8s E2E 覆盖
//! （真实 vite + 真实 pingora 栈）；此处锁定可单测的确定性部分。
//!
//! 时序模型对齐 pingora 真实阶段序：`upstream_peer`（选上游+连接）先于
//! `upstream_request_filter`（改写请求）——预览 Forward 决策必须发生在
//! upstream_peer 阶段（历史缺陷：决策在 request 阶段，非宿主副本在 peer
//! 阶段连 127.0.0.1:vite_port 即 502，Forward 永无执行机会）。测试按该
//! 时序串联两阶段，回归即红。

use std::sync::Arc;

use matchit::Router;

use crate::route_type::RouteType;
use crate::router::create_router;
use crate::service::handlers::port_proxy::{handle_port_proxy_request, handle_port_proxy_upstream};
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
            peer_proxy_port: 8088,
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

fn fixed_backends() -> Arc<arc_swap::ArcSwap<std::collections::HashMap<u16, String>>> {
    Arc::new(arc_swap::ArcSwap::from_pointee(
        std::collections::HashMap::new(),
    ))
}

/// 跑一遍 upstream_peer 阶段（决策；params 借用存活于本作用域）。
async fn run_upstream_phase(
    router: &Router<RouteType>,
    slot: &Arc<arc_swap::ArcSwapOption<Arc<PreviewRouteDeps>>>,
    uri: &str,
    ctx: &mut TrackingCtx,
) -> Box<pingora_core::upstreams::peer::HttpPeer> {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        shared_types::published::register_direct(
            "127.0.0.1",
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        );
    }
    let matched = router.at(uri).expect("route match");
    handle_port_proxy_upstream(
        ctx,
        matched.params,
        &fixed_backends(),
        "127.0.0.1",
        &Arc::new(crate::service::types::ProxyMetrics::default()),
        slot,
    )
    .await
    .expect("upstream phase")
}

/// 跑一遍 upstream_request_filter 阶段（消费决策改写请求）。
async fn run_request_phase(
    router: &Router<RouteType>,
    uri: &str,
    ctx: &mut TrackingCtx,
) -> pingora_http::RequestHeader {
    let (mut header, original) = make_request(uri);
    let matched = router.at(uri).expect("route match");
    handle_port_proxy_request(&mut header, &original, matched.params, true, ctx)
        .await
        .expect("request phase");
    header
}

/// 按 pingora 真实时序串联两阶段（决策 → 改写）。
async fn run_both_phases(
    slot: &Arc<arc_swap::ArcSwapOption<Arc<PreviewRouteDeps>>>,
    uri: &str,
) -> (
    pingora_http::RequestHeader,
    TrackingCtx,
    Box<pingora_core::upstreams::peer::HttpPeer>,
) {
    let router = create_router().expect("router");
    let mut ctx = TrackingCtx::new();
    let peer = run_upstream_phase(&router, slot, uri, &mut ctx).await;
    let header = run_request_phase(&router, uri, &mut ctx).await;
    (header, ctx, peer)
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

/// 时序锁（本修复的回归锚）：Forward 决策必须发生在 upstream_peer 阶段——
/// 历史缺陷正是决策晚于 peer 选择，非宿主副本连 127.0.0.1:vite_port 即 502。
#[tokio::test]
async fn forward_decision_lands_in_upstream_peer_phase() {
    let router = create_router().expect("router");
    let slot = slot_with(shared_types::PreviewRouteResolution::Forward {
        instance_id: "inst-1".into(),
        port: 4200,
        host_ip: "10.1.2.3".into(),
    });
    // 仅跑 peer 阶段——验证决策产物在 ctx 中就绪
    let mut ctx = TrackingCtx::new();
    let peer =
        run_upstream_phase(&router, &slot, "/proxy/4200/page/index.html?a=1", &mut ctx).await;

    assert_eq!(ctx.preview_peer, Some(("10.1.2.3".to_string(), 8088)));
    assert_eq!(ctx.preview_rewrite, Some(("inst-1".into(), 4200)));
    assert_eq!(ctx.preview_origin_port, Some(4200));
    assert_eq!(ctx.target_port, Some(8088));
    assert_eq!(ctx.upstream_host.as_deref(), Some("10.1.2.3"));
    assert_eq!(
        ctx.preview_internal_token.as_deref(),
        Some("unit-test-token"),
        "token 由 peer 阶段写入 ctx，request 阶段直接消费"
    );
    assert_eq!(peer.to_string(), "addr: 10.1.2.3:8088, scheme: HTTP");

    // request 阶段消费：take() 一次性清空 ctx 中的 preview 状态
    let header = run_request_phase(&router, "/proxy/4200/page/index.html?a=1", &mut ctx).await;
    assert_eq!(
        header.uri.path(),
        "/internal/preview-forward/inst-1/4200/page/index.html"
    );
    assert_eq!(
        header
            .headers
            .get("x-preview-internal-token")
            .and_then(|v| v.to_str().ok()),
        Some("unit-test-token"),
        "客户端伪造令牌被进程令牌覆盖"
    );
    assert!(ctx.preview_rewrite.is_none(), "take() 已消费");
    assert!(ctx.preview_internal_token.is_none(), "take() 已消费");
}

#[tokio::test]
async fn forward_rewrite_consumes_upstream_decision() {
    let slot = slot_with(shared_types::PreviewRouteResolution::Forward {
        instance_id: "inst-1".into(),
        port: 4200,
        host_ip: "10.1.2.3".into(),
    });
    let (header, _ctx, _peer) = run_both_phases(&slot, "/proxy/4200/page/index.html?a=1").await;

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
    // take() 一次性消费：request 阶段后 ctx 应为空（防止重复消费或内存泄漏）
    assert!(_ctx.preview_rewrite.is_none(), "take() 已消费");
    assert!(_ctx.preview_internal_token.is_none(), "take() 已消费");
}

#[tokio::test]
async fn legacy_path_when_not_forwarded() {
    let router = create_router().expect("router");
    // 三种不转发解析：本机宿主 / 非预览 / 存储不可用降级——peer 阶段即回环本机端口
    for resolution in [
        shared_types::PreviewRouteResolution::Local {
            instance_id: "i".into(),
            port: 4200,
        },
        shared_types::PreviewRouteResolution::NotPreview,
        shared_types::PreviewRouteResolution::Unavailable,
    ] {
        let slot = slot_with(resolution);
        let mut ctx = TrackingCtx::new();
        let peer = run_upstream_phase(&router, &slot, "/proxy/4200/page/", &mut ctx).await;
        assert_eq!(
            peer.to_string(),
            "addr: 127.0.0.1:4200, scheme: HTTP",
            "回环本机端口"
        );
        assert!(ctx.preview_peer.is_none(), "不覆盖上游");
        assert!(ctx.preview_rewrite.is_none(), "不产生重写要素");
        let header = run_request_phase(&router, "/proxy/4200/page/", &mut ctx).await;
        assert_eq!(header.uri.path(), "/page/", "路径照旧剥离端口前缀");
    }
}

#[tokio::test]
async fn legacy_path_when_slot_empty() {
    let router = create_router().expect("router");
    let slot = empty_slot();
    let mut ctx = TrackingCtx::new();
    let peer = run_upstream_phase(&router, &slot, "/proxy/4200/page/", &mut ctx).await;
    assert_eq!(peer.to_string(), "addr: 127.0.0.1:4200, scheme: HTTP");
    let header = run_request_phase(&router, "/proxy/4200/page/", &mut ctx).await;
    assert_eq!(header.uri.path(), "/page/");
    assert!(ctx.preview_peer.is_none());
}

#[tokio::test]
async fn forwarded_path_preserves_trailing_slash_and_deep_rest() {
    let slot = slot_with(shared_types::PreviewRouteResolution::Forward {
        instance_id: "inst-2".into(),
        port: 4321,
        host_ip: "10.1.2.4".into(),
    });
    let (header, _ctx, _peer) = run_both_phases(&slot, "/proxy/4321/@vite/client").await;
    assert_eq!(
        header.uri.path(),
        "/internal/preview-forward/inst-2/4321/@vite/client",
        "深层资产路径（含 @ 前缀段）原样保留"
    );
}
