//! ttyd 终端 query 参数契约测试（computer + userapp dev 两条链）。
//!
//! 行为级反例先行：`?service_type=` / `?cwd=` 为浏览器原生 WS 客户端唯一可用的
//! 业务场景与初始目录载体。契约规则见 `ttyd_params` 模块文档与
//! `resolve_computer_service_type` 的合并矩阵。

use crate::router::{RouteType, create_router};
use crate::service::types::TrackingCtx;

fn make_request(uri: &str) -> (pingora_http::RequestHeader, http::Uri) {
    let original: http::Uri = uri.parse().expect("uri");
    let mut header = pingora_http::RequestHeader::build(&http::Method::GET, uri.as_bytes(), None)
        .expect("header");
    header.insert_header("Host", "example.com").expect("host");
    (header, original)
}

/// 跑 computer ttyd 请求阶段，返回改写后的 header（成功）。
async fn run_computer_phase(
    uri: &str,
    extra_headers: &[(&'static str, &'static str)],
) -> pingora_http::RequestHeader {
    let router = create_router().expect("router");
    let (mut header, original) = make_request(uri);
    for (name, value) in extra_headers {
        header.insert_header(*name, *value).expect("extra header");
    }
    let matched = router.at(uri).expect("route match");
    assert!(matches!(matched.value, RouteType::TtydProxy));
    let ctx = TrackingCtx::new();
    super::ttyd::handle_ttyd_request(&mut header, &original, matched.params, &ctx)
        .await
        .expect("request phase");
    header
}

/// 跑 computer ttyd 请求阶段，断言失败（400 类）。
async fn run_computer_phase_err(uri: &str, extra_headers: &[(&'static str, &'static str)]) {
    let router = create_router().expect("router");
    let (mut header, original) = make_request(uri);
    for (name, value) in extra_headers {
        header.insert_header(*name, *value).expect("extra header");
    }
    let matched = router.at(uri).expect("route match");
    let ctx = TrackingCtx::new();
    let result =
        super::ttyd::handle_ttyd_request(&mut header, &original, matched.params, &ctx).await;
    assert!(result.is_err(), "expect reject: {uri}");
}

/// 跑 userapp dev ttyd 请求阶段。
async fn run_dev_phase(uri: &str) -> Result<pingora_http::RequestHeader, Box<pingora_core::Error>> {
    let router = create_router().expect("router");
    let (mut header, original) = make_request(uri);
    let matched = router.at(uri).expect("route match");
    assert!(matches!(matched.value, RouteType::DevTtydProxy));
    let ctx = TrackingCtx::new();
    super::dev_terminal::handle_dev_ttyd_request(&mut header, &original, matched.params, &ctx)
        .await
        .map(|()| header)
}

fn header_value<'a>(header: &'a pingora_http::RequestHeader, name: &str) -> Option<&'a str> {
    // 字节级 UTF-8：x-ttyd-cwd 可承载中文（HeaderValue::to_str 对非 ASCII Err）
    header
        .headers
        .get(name)
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
}

// ── service_type（computer 链） ─────────────────────────────────────────────

/// query 指定 normalProject → 注入本义值（修复前 query 被忽略，恒默认值）。
#[tokio::test]
async fn service_type_query_selects_normal_project() {
    let header = run_computer_phase(
        "/computer/ttyd/u1/p1/ws?service_type=computer-normal-project",
        &[],
    )
    .await;
    assert_eq!(
        header_value(&header, "x-ttyd-service-type"),
        Some("computer-normal-project")
    );
}

/// 无 query 无 header → 默认 computer-agent-runner（现状不变）。
#[tokio::test]
async fn no_inputs_default_service_type() {
    let header = run_computer_phase("/computer/ttyd/u1/p1/ws", &[]).await;
    assert_eq!(
        header_value(&header, "x-ttyd-service-type"),
        Some("computer-agent-runner")
    );
}

/// query 值不可解析 → 400（显式错误输入 fail fast）。
#[tokio::test]
async fn service_type_query_garbage_rejected() {
    run_computer_phase_err("/computer/ttyd/u1/p1/ws?service_type=not-a-type", &[]).await;
}

/// query 值合法但非 Computer 族 → 400。
#[tokio::test]
async fn service_type_query_non_computer_rejected() {
    run_computer_phase_err("/computer/ttyd/u1/p1/ws?service_type=web-agent-runner", &[]).await;
}

/// header 与 query 并存且不一致 → 400（意图矛盾不静默择一）。
#[tokio::test]
async fn service_type_header_query_conflict_rejected() {
    run_computer_phase_err(
        "/computer/ttyd/u1/p1/ws?service_type=computer-normal-project",
        &[("X-Ttyd-Service-Type", "computer-agent-runner")],
    )
    .await;
}

/// header 与 query 一致 → 正常（同名变体归一到同一枚举）。
#[tokio::test]
async fn service_type_header_query_agree() {
    let header = run_computer_phase(
        "/computer/ttyd/u1/p1/ws?service_type=ComputerNormalProject",
        &[("X-Ttyd-Service-Type", "computer-normal-project")],
    )
    .await;
    assert_eq!(
        header_value(&header, "x-ttyd-service-type"),
        Some("computer-normal-project")
    );
}

// ── cwd（两条链） ──────────────────────────────────────────────────────────

/// computer 链：cwd 绝对路径 → 注入归一化值（修复前无此通道）。
#[tokio::test]
async fn cwd_query_injects_header() {
    let header = run_computer_phase(
        "/computer/ttyd/u1/p1/ws?cwd=%2Fhome%2Fuser%2FnormalProject%2Fp1",
        &[],
    )
    .await;
    assert_eq!(
        header_value(&header, "x-ttyd-cwd"),
        Some("/home/user/normalProject/p1")
    );
}

/// form 语义：`+` = 空格，空格路径可注入（多平台归一放行，传输编码由
/// agent_runner 侧承担）。
#[tokio::test]
async fn cwd_query_with_space_and_plus() {
    let header = run_computer_phase("/computer/ttyd/u1/p1/ws?cwd=%2Fhome%2Fmy+dir", &[]).await;
    assert_eq!(header_value(&header, "x-ttyd-cwd"), Some("/home/my dir"));
}

/// 中文路径 cwd：归一放行 + pingora header 写侧接受非 visible-ASCII 值
/// （agent_runner 读侧须字节级 UTF-8——HeaderValue::to_str 对中文返回 Err，
/// 此为复查修复的锚点测试）。
#[tokio::test]
async fn cwd_query_chinese_path_injects_header() {
    let header = run_computer_phase(
        "/computer/ttyd/u1/p1/ws?cwd=%2Fhome%2Fuser%2F%E6%88%91%E7%9A%84%E9%A1%B9%E7%9B%AE",
        &[],
    )
    .await;
    assert_eq!(
        header_value(&header, "x-ttyd-cwd"),
        Some("/home/user/我的项目")
    );
}

/// 相对路径 / 点段 cwd → 400。
#[tokio::test]
async fn cwd_relative_rejected() {
    run_computer_phase_err("/computer/ttyd/u1/p1/ws?cwd=home/user", &[]).await;
    run_computer_phase_err("/computer/ttyd/u1/p1/ws?cwd=%2Fhome%2F..%2Fetc", &[]).await;
}

/// 客户端伪造 x-ttyd-cwd 且无 query → 必须被剥离（不能透传 ws_terminal）。
#[tokio::test]
async fn forged_cwd_header_stripped_without_query() {
    let header = run_computer_phase("/computer/ttyd/u1/p1/ws", &[("X-Ttyd-Cwd", "/etc")]).await;
    assert!(
        header_value(&header, "x-ttyd-cwd").is_none(),
        "客户端伪造 x-ttyd-cwd 不得存活"
    );
}

/// 客户端伪造 x-ttyd-cwd + 合法 query cwd → 服务端校验值覆盖伪造值。
#[tokio::test]
async fn forged_cwd_header_overridden_by_query() {
    let header = run_computer_phase(
        "/computer/ttyd/u1/p1/ws?cwd=%2Fhome%2Fuser%2Fp1",
        &[("X-Ttyd-Cwd", "/etc")],
    )
    .await;
    assert_eq!(header_value(&header, "x-ttyd-cwd"), Some("/home/user/p1"));
}

/// userapp dev 链：cwd 注入；service_type 出现即 400（定位键恒为 app_id）。
#[tokio::test]
async fn dev_chain_cwd_ok_service_type_rejected() {
    let header = run_dev_phase(
        "/api/v1/userapp/proxy/ttyd/dev/u1/app1/ws?cwd=%2Fhome%2Fuser%2Fapp1%2Fsub+dir",
    )
    .await
    .expect("dev request phase");
    assert_eq!(
        header_value(&header, "x-ttyd-cwd"),
        Some("/home/user/app1/sub dir")
    );
    assert_eq!(
        header_value(&header, "x-ttyd-service-type"),
        Some("user-app-builder"),
        "dev 链服务类型恒定，不受 query 影响"
    );

    let err = run_dev_phase(
        "/api/v1/userapp/proxy/ttyd/dev/u1/app1/ws?service_type=computer-agent-runner",
    )
    .await
    .expect_err("dev 链 service_type 必须拒绝");
    assert!(
        matches!(
            err.etype(),
            pingora_core::ErrorType::HTTPStatus(code) if *code == 400
        ),
        "应回 400（typed）：{err}"
    );
}

/// userapp dev 链：客户端伪造 x-ttyd-cwd 同样剥离。
#[tokio::test]
async fn dev_chain_forged_cwd_stripped() {
    let header = run_dev_phase("/api/v1/userapp/proxy/ttyd/dev/u1/app1/ws")
        .await
        .expect("dev request phase");
    assert!(header_value(&header, "x-ttyd-cwd").is_none());
}
