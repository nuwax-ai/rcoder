//! 路由表匹配回归（从 router.rs 内联 tests 模块拆出；
//! `test_route_type_equality` / `test_route_type_debug` 随枚举本体迁 route_type.rs，
//! `test_get_routes_documentation` 随死 API get_routes_documentation 删除）。

use super::*;

#[test]
fn test_create_router() {
    let router = create_router().unwrap();

    // 测试 VNC 路由
    let matched = router
        .at("/computer/vnc/user_123/proj_456/vnc.html")
        .unwrap();
    assert_eq!(*matched.value, RouteType::VncProxy);
    assert_eq!(matched.params.get("user_id"), Some("user_123"));
    assert_eq!(matched.params.get("project_id"), Some("proj_456"));
    assert_eq!(matched.params.get("path"), Some("vnc.html"));

    // 测试 userApp 生产应用流量代理（免端口，user_id 不参与解析）
    let matched = router
        .at("/proxy/userapp/prod/u6/app-abc/api/users")
        .unwrap();
    assert_eq!(*matched.value, RouteType::ProdAppProxy);
    assert_eq!(matched.params.get("user_id"), Some("u6"));
    assert_eq!(matched.params.get("app_id"), Some("app-abc"));
    assert_eq!(matched.params.get("path"), Some("api/users"));

    // 测试端口代理路由
    let matched = router.at("/proxy/8080/api/status").unwrap();
    assert_eq!(*matched.value, RouteType::PortProxy);
    assert_eq!(matched.params.get("port"), Some("8080"));
    assert_eq!(matched.params.get("path"), Some("api/status"));

    // userApp 开发应用流量代理（带 path）+ 无尾随 path 兜底
    let matched = router
        .at("/proxy/userapp/dev/u6/app-abc123/api/users")
        .unwrap();
    assert_eq!(*matched.value, RouteType::DevAppProxy);
    assert_eq!(matched.params.get("user_id"), Some("u6"));
    assert_eq!(matched.params.get("app_id"), Some("app-abc123"));
    assert_eq!(matched.params.get("path"), Some("api/users"));
    let matched = router.at("/proxy/userapp/dev/u6/app-abc123").unwrap();
    assert_eq!(*matched.value, RouteType::DevAppProxy);
}

#[test]
fn test_vnc_route_variations() {
    let router = create_router().unwrap();

    // WebSocket 路径
    let matched = router
        .at("/computer/vnc/user_123/proj_456/websockify")
        .unwrap();
    assert_eq!(*matched.value, RouteType::VncProxy);
    assert_eq!(matched.params.get("path"), Some("websockify"));

    // 多级子路径
    let matched = router
        .at("/computer/vnc/user_123/proj_456/api/v1/status")
        .unwrap();
    assert_eq!(*matched.value, RouteType::VncProxy);
    assert_eq!(matched.params.get("path"), Some("api/v1/status"));
}

#[test]
fn test_port_proxy_route_variations() {
    let router = create_router().unwrap();

    // 不同端口
    for port in [3000, 8080, 9000, 5173] {
        let path = format!("/proxy/{}/api", port);
        let matched = router.at(&path).unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
        assert_eq!(matched.params.get("port"), Some(port.to_string().as_str()));
    }
}

#[test]
fn test_userapp_dev_terminal_routes() {
    let router = create_router().expect("router");

    // 开发域工具族（stage 段 dev）通配 + 兜底（无尾随 path）成对
    for (path, expected) in [
        ("/userapp/dev/ttyd/u1/app-1/ws", RouteType::DevTtydProxy),
        ("/userapp/dev/ttyd/u1/app-1", RouteType::DevTtydProxy),
        ("/userapp/dev/vnc/u1/app-1/vnc.html", RouteType::DevVncProxy),
        ("/userapp/dev/vnc/u1/app-1", RouteType::DevVncProxy),
        ("/userapp/dev/audio/u1/app-1/ws", RouteType::DevAudioProxy),
        ("/userapp/dev/audio/u1/app-1", RouteType::DevAudioProxy),
        ("/userapp/dev/ime/u1/app-1/connect", RouteType::DevImeProxy),
        ("/userapp/dev/ime/u1/app-1", RouteType::DevImeProxy),
    ] {
        let matched = router.at(path).expect(path);
        assert_eq!(*matched.value, expected, "path={path}");
    }

    // 生产域工具族（stage 段 prod，原 /runtime 段退役）：与开发域 stage 段区分
    for (path, expected, expected_app) in [
        (
            "/userapp/prod/ttyd/u1/app-1/ws/token",
            RouteType::RuntimeTtydProxy,
            "app-1",
        ),
        (
            "/userapp/prod/ttyd/u1/app-1",
            RouteType::RuntimeTtydProxy,
            "app-1",
        ),
        // 开发域路由不被 prod 段劫持
        (
            "/userapp/dev/ttyd/u1/app-1/ws",
            RouteType::DevTtydProxy,
            "app-1",
        ),
    ] {
        let matched = router.at(path).expect(path);
        assert_eq!(*matched.value, expected, "path={path}");
        assert_eq!(
            matched.params.get("app_id"),
            Some(expected_app),
            "app_id param path={path}"
        );
    }

    // 工具族新形态（{user_id}/{app_id} 双段）：app_id + 剩余 path 剥前缀语义不变
    for (path, expected, expected_user, expected_app, expected_rest) in [
        (
            "/userapp/dev/dbx/u1/app-1",
            RouteType::DevDbxProxy,
            "u1",
            "app-1",
            None,
        ),
        (
            "/userapp/dev/dbx/u1/app-1/api/auth/check",
            RouteType::DevDbxProxy,
            "u1",
            "app-1",
            Some("api/auth/check"),
        ),
        (
            "/userapp/dev/ttyd/u1/app-1/ws",
            RouteType::DevTtydProxy,
            "u1",
            "app-1",
            Some("ws"),
        ),
        (
            "/userapp/prod/dbx/u1/app-1",
            RouteType::ProdDbxProxy,
            "u1",
            "app-1",
            None,
        ),
    ] {
        let matched = router.at(path).expect(path);
        assert_eq!(*matched.value, expected, "path={path}");
        assert_eq!(
            matched.params.get("user_id"),
            Some(expected_user),
            "user_id param path={path}"
        );
        assert_eq!(
            matched.params.get("app_id"),
            Some(expected_app),
            "app_id param path={path}"
        );
        assert_eq!(
            matched.params.get("path"),
            expected_rest,
            "rest path param path={path}"
        );
    }

    // 单段 app_id 旧形态（无 user_id）已退役：根形态（无尾随 path）不得命中。
    // 注：深层旧 URL（如 /userapp/dev/dbx/app-1/api/...）会把 app-1 误解析进
    // user_id 槽 → 定位失败 404——Breaking 换代由 Java 同批改 URL 保证。
    for old_path in ["/userapp/prod/dbx/app-1", "/userapp/dev/ttyd/app-1"] {
        let path = crate::service::utils::normalize_path(old_path);
        let hits = router
            .at(path)
            .ok()
            .map(|m| {
                matches!(
                    *m.value,
                    RouteType::DevDbxProxy
                        | RouteType::ProdDbxProxy
                        | RouteType::DevTtydProxy
                        | RouteType::RuntimeTtydProxy
                )
            })
            .unwrap_or(false);
        assert!(
            !hits,
            "single-segment legacy path must be retired: {old_path}"
        );
    }

    // 旧路径风格已退役（clean break）：不得命中 userapp 家族任何变体。
    // 注：/proxy/apps/...、/proxy/dev/... 旧前缀会落进 /proxy/{port} 泛化路由
    //（port 段非数字 → handler 400），属预期行为，不算命中。
    for old_path in [
        "/userapp/ttyd/app-1/ws",
        "/userapp/pgweb/app-1/runtime",
        "/proxy/apps/u1/app-1/4000/x",
        "/proxy/devapps/u1/app-1/4000/x",
        "/proxy/dev/dbx/app-1/api/auth/check",
    ] {
        let path = crate::service::utils::normalize_path(old_path);
        let hits_userapp_family = router
            .at(path)
            .ok()
            .map(|m| {
                matches!(
                    *m.value,
                    RouteType::DevTtydProxy
                        | RouteType::DevVncProxy
                        | RouteType::DevAudioProxy
                        | RouteType::DevImeProxy
                        | RouteType::DevDbxProxy
                        | RouteType::ProdDbxProxy
                        | RouteType::RuntimeTtydProxy
                        | RouteType::ProdAppProxy
                        | RouteType::DevAppProxy
                )
            })
            .unwrap_or(false);
        assert!(
            !hits_userapp_family,
            "old-style path must not hit userapp family: {old_path}"
        );
    }

    // 与 /proxy/{port} 数字端口族互不干扰：数字端口仍走 PortProxy
    assert_eq!(
        *router.at("/proxy/8080/some/path").unwrap().value,
        RouteType::PortProxy
    );
}

#[test]
fn test_route_not_found() {
    let router = create_router().unwrap();

    // 不匹配的路径应该返回错误
    assert!(router.at("/unknown/path").is_err());
    assert!(router.at("/computer/desktop").is_err());
    // 注意：/api/xxx/yyy 现在会匹配到 ApiProxy 路由
}

#[test]
fn test_api_proxy_route() {
    let router = create_router().unwrap();

    // 测试 Anthropic API 路由
    let matched = router.at("/api/anthropic/v1/messages").unwrap();
    assert_eq!(*matched.value, RouteType::ApiProxy);
    assert_eq!(matched.params.get("service_name"), Some("anthropic"));
    assert_eq!(matched.params.get("path"), Some("v1/messages"));

    // 测试 OpenAI API 路由
    let matched = router.at("/api/openai/v1/chat/completions").unwrap();
    assert_eq!(*matched.value, RouteType::ApiProxy);
    assert_eq!(matched.params.get("service_name"), Some("openai"));
    assert_eq!(matched.params.get("path"), Some("v1/chat/completions"));

    // 测试多级路径
    let matched = router.at("/api/custom/v2/org/project/messages").unwrap();
    assert_eq!(*matched.value, RouteType::ApiProxy);
    assert_eq!(matched.params.get("service_name"), Some("custom"));
    assert_eq!(matched.params.get("path"), Some("v2/org/project/messages"));
}

#[test]
fn test_audio_route_matching() {
    let router = create_router().unwrap();

    // 测试音频 WebSocket 路由
    let matched = router.at("/computer/audio/user_123/proj_456/ws").unwrap();
    assert_eq!(*matched.value, RouteType::AudioProxy);
    assert_eq!(matched.params.get("user_id"), Some("user_123"));
    assert_eq!(matched.params.get("project_id"), Some("proj_456"));
    assert_eq!(matched.params.get("path"), Some("ws"));

    // 测试音频 HTTP 路由 (带文件名)
    let matched = router
        .at("/computer/audio/user_123/proj_456/index.html")
        .unwrap();
    assert_eq!(*matched.value, RouteType::AudioProxy);
    assert_eq!(matched.params.get("path"), Some("index.html"));

    // 测试带子路径的 WebSocket
    let matched = router
        .at("/computer/audio/user_123/proj_456/ws/token")
        .unwrap();
    assert_eq!(*matched.value, RouteType::AudioProxy);
    assert_eq!(matched.params.get("path"), Some("ws/token"));

    // 注意：尾斜杠路径 (如 "/computer/audio/user_123/proj_456/") 不匹配 {*path} 通配符
    // 这是 matchit 的限制，{*path} 需要至少一个字符
    // 实际场景中客户端通常不会发送尾斜杠到这些路径
}

#[test]
fn test_ime_route_matching() {
    let router = create_router().unwrap();

    // 测试带子路径的 IME 路由
    let matched = router.at("/computer/ime/alice/myproject/connect").unwrap();
    assert_eq!(*matched.value, RouteType::ImeProxy);
    assert_eq!(matched.params.get("user_id"), Some("alice"));
    assert_eq!(matched.params.get("project_id"), Some("myproject"));
    assert_eq!(matched.params.get("path"), Some("connect"));

    // 注意：尾斜杠路径不匹配 {*path} 通配符，需要至少一个字符
}

#[test]
fn test_audio_and_ime_route_not_conflict() {
    let router = create_router().unwrap();

    // 确保音频和 IME 路由不会互相干扰
    let audio_matched = router.at("/computer/audio/user_123/proj_456/ws").unwrap();
    assert_eq!(*audio_matched.value, RouteType::AudioProxy);

    let ime_matched = router
        .at("/computer/ime/user_123/proj_456/connect")
        .unwrap();
    assert_eq!(*ime_matched.value, RouteType::ImeProxy);

    // 确保不同的路径参数被正确解析
    assert_eq!(audio_matched.params.get("path"), Some("ws"));
    assert_eq!(ime_matched.params.get("path"), Some("connect"));
}

#[test]
fn test_ttyd_route_matching() {
    let router = create_router().unwrap();

    // WebSocket 路径
    let matched = router.at("/computer/ttyd/alice/myproject/ws").unwrap();
    assert_eq!(*matched.value, RouteType::TtydProxy);
    assert_eq!(matched.params.get("user_id"), Some("alice"));
    assert_eq!(matched.params.get("project_id"), Some("myproject"));
    assert_eq!(matched.params.get("path"), Some("ws"));

    // HTTP index.html 路径
    let matched = router
        .at("/computer/ttyd/alice/myproject/index.html")
        .unwrap();
    assert_eq!(*matched.value, RouteType::TtydProxy);
    assert_eq!(matched.params.get("path"), Some("index.html"));

    // 多级子路径（如 ws/token）
    let matched = router
        .at("/computer/ttyd/alice/myproject/ws/token")
        .unwrap();
    assert_eq!(*matched.value, RouteType::TtydProxy);
    assert_eq!(matched.params.get("path"), Some("ws/token"));
}

#[test]
fn test_audio_ime_ttyd_route_not_conflict() {
    let router = create_router().unwrap();

    let audio_matched = router.at("/computer/audio/u/p/ws").unwrap();
    assert_eq!(*audio_matched.value, RouteType::AudioProxy);

    let ime_matched = router.at("/computer/ime/u/p/connect").unwrap();
    assert_eq!(*ime_matched.value, RouteType::ImeProxy);

    let ttyd_matched = router.at("/computer/ttyd/u/p/ws").unwrap();
    assert_eq!(*ttyd_matched.value, RouteType::TtydProxy);

    // 路径参数各自正确解析，互不串台
    assert_eq!(audio_matched.params.get("path"), Some("ws"));
    assert_eq!(ime_matched.params.get("path"), Some("connect"));
    assert_eq!(ttyd_matched.params.get("path"), Some("ws"));
}

#[test]
fn test_web_ttyd_route_matching() {
    let router = create_router().unwrap();

    // 基本 WebSocket 路径
    let matched = router.at("/web/ttyd/user_123/proj_456/ws").unwrap();
    assert_eq!(*matched.value, RouteType::WebTtydProxy);
    assert_eq!(matched.params.get("user_id"), Some("user_123"));
    assert_eq!(matched.params.get("project_id"), Some("proj_456"));
    assert_eq!(matched.params.get("path"), Some("ws"));

    // index.html 路径
    let matched = router.at("/web/ttyd/user_123/proj_456/index.html").unwrap();
    assert_eq!(*matched.value, RouteType::WebTtydProxy);
    assert_eq!(matched.params.get("path"), Some("index.html"));

    // 多级路径（ws/token）
    let matched = router.at("/web/ttyd/user_123/proj_456/ws/token").unwrap();
    assert_eq!(*matched.value, RouteType::WebTtydProxy);
    assert_eq!(matched.params.get("path"), Some("ws/token"));
}

#[test]
fn test_web_ttyd_and_computer_ttyd_not_conflict() {
    let router = create_router().unwrap();

    // 确保 web ttyd 和 computer ttyd 路由不会互相干扰
    let web_matched = router.at("/web/ttyd/u/p/ws").unwrap();
    assert_eq!(*web_matched.value, RouteType::WebTtydProxy);

    let computer_matched = router.at("/computer/ttyd/u/p/ws").unwrap();
    assert_eq!(*computer_matched.value, RouteType::TtydProxy);

    // 路径参数各自正确解析
    assert_eq!(web_matched.params.get("path"), Some("ws"));
    assert_eq!(computer_matched.params.get("path"), Some("ws"));
}
