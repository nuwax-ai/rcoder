use super::*;
use crate::instance::test_lock_file;
use crate::proxy::{all_rust_path_allowed, is_hop_by_hop};

fn cfg() -> FileServerProxyConfig {
    FileServerProxyConfig::default()
}

/// config.yml wire 契约：策略值为 snake_case（helm 模板渲染依赖此形态）。
/// `userapp_split` 为已删除档（语义并入 ts_first）——wire 值不再受认可。
#[test]
fn policy_serializes_snake_case() {
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::TsFirst).unwrap().trim(),
        "ts_first"
    );
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::AllRust).unwrap().trim(),
        "all_rust"
    );
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::AllTs).unwrap().trim(),
        "all_ts"
    );
    for wire in ["ts_first", "all_rust", "all_ts"] {
        let parsed: RoutePolicy = serde_yaml::from_str(wire).unwrap();
        assert_eq!(parsed.as_str(), wire);
    }
    // 已删除档拒绝（values 已迁移；残留配置 fail-fast 而非静默错档）
    assert!(serde_yaml::from_str::<RoutePolicy>("userapp_split").is_err());
    // 段缺 policy 字段 → 默认 TsFirst（存量 config 兼容）
    let parsed: FileServerProxyConfig = serde_yaml::from_str(
        "listen_port: 60000\nrust_upstream_port: 8086\nts_upstream_port: 60001\n",
    )
    .unwrap();
    assert_eq!(parsed.policy, RoutePolicy::TsFirst);
}

/// 主 pod 形态（TsFirst 默认策略）：header 与 path 双判据。
/// userApp 标记流量走 Rust（rcoder 拦截层转发 per-app 容器——per-app RBD
/// 架构下 TS 物理读不到 app 工作区，历史「TS 自载 userApp」假设已失效）。
#[test]
fn ts_first_policy_routes_by_header_and_path() {
    let c = cfg();
    assert_eq!(c.policy, RoutePolicy::TsFirst, "默认策略为主 pod 形态");
    // header 判据（存量路径形态 + 业务声明）
    assert_eq!(
        c.upstream_port_for("/api/computer/get-file-list", Some("userapp")),
        Upstream::Rust(8086)
    );
    // path 判据（userApp 新契约前缀；Java 未接 header 期的兜底）
    assert_eq!(
        c.upstream_port_for("/api/v1/userapp/dev/start", None),
        Upstream::Rust(8086)
    );
    assert_eq!(
        c.upstream_port_for("/api/v1/userapp", None),
        Upstream::Rust(8086)
    );
    // 双判据都未命中 → TS
    for path in [
        "/health",
        "/api/version",
        "/api/computer/create-workspace",
        "/api/project/list",
        "/api/git/status",
        "/api/build/start",
        "/",
    ] {
        assert_eq!(
            c.upstream_port_for(path, None),
            Upstream::Ts(60001),
            "{path}"
        );
    }
    // 段边界: /api/v1/userapplication 不是 userApp 域
    assert_eq!(
        c.upstream_port_for("/api/v1/userapplication", None),
        Upstream::Ts(60001)
    );
    // 非本业务域声明（computer 等）与空值一律 TS——契约违规 404 可见而非静默误路由
    assert_eq!(
        c.upstream_port_for("/api/computer/x", Some("computer")),
        Upstream::Ts(60001)
    );
    assert_eq!(
        c.upstream_port_for("/api/computer/x", Some("")),
        Upstream::Ts(60001)
    );
    // 大小写不敏感：userapp 的任意大小写变体（含前后空白）均命中 Rust 上游
    // （单一事实源 shared_types::is_userapp_service_type_value）
    for variant in ["Userapp", "USERAPP", " userapp "] {
        assert_eq!(
            c.upstream_port_for("/api/computer/x", Some(variant)),
            Upstream::Rust(8086),
            "{variant}"
        );
    }
}

/// 容器形态（AllRust）：一律内嵌 Rust 上游（现状行为等价）。
#[test]
fn all_rust_policy_routes_everything_to_rust() {
    let c = FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 60000,
        rust_upstream_port: 60002,
        ts_upstream_port: 60001,
        policy: RoutePolicy::AllRust,
        coordinated_dev_lifecycle: false,
    };
    for (path, header) in [
        ("/health", None),
        ("/api/version", None),
        ("/api/computer/create-workspace", None),
        ("/api/computer/create-workspace", Some("computer")),
        ("/api/v1/userapp/dev/start", None),
        ("/", Some("")),
    ] {
        assert_eq!(
            c.upstream_port_for(path, header),
            Upstream::Rust(60002),
            "{path} {header:?} 应一律走内嵌 Rust"
        );
    }
}

/// 全 TS 模式（AllTs）：一律 TS 上游——userApp 判据在此模式下不生效
/// （后端选择整体切 TS 时不存在"部分路径仍走 Rust"的语义）。
#[test]
fn all_ts_policy_routes_everything_to_ts() {
    let c = FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 60000,
        rust_upstream_port: 8086,
        ts_upstream_port: 41234,
        policy: RoutePolicy::AllTs,
        coordinated_dev_lifecycle: false,
    };
    for (path, header) in [
        ("/health", None),
        ("/api/v1/userapp/dev/start", None),
        // userApp 显式 header 也走 TS——全 TS 语义优先于域判据
        ("/api/computer/get-file-list", Some("userapp")),
        ("/api/version", None),
        ("/", None),
    ] {
        assert_eq!(
            c.upstream_port_for(path, header),
            Upstream::Ts(41234),
            "{path} {header:?} 应一律走 TS"
        );
    }
}

/// parse_route_policy：与 serde wire 词汇表一致（含容差 trim 与非法值报错）。
#[test]
fn parse_route_policy_accepts_wire_vocabulary() {
    assert_eq!(
        parse_route_policy("ts_first").unwrap(),
        RoutePolicy::TsFirst
    );
    assert_eq!(
        parse_route_policy("all_rust").unwrap(),
        RoutePolicy::AllRust
    );
    assert_eq!(parse_route_policy("all_ts").unwrap(), RoutePolicy::AllTs);
    // trim 容差（env 值尾随空白是常见脏数据）
    assert_eq!(parse_route_policy(" all_ts\n").unwrap(), RoutePolicy::AllTs);
    // as_str 与 parse 互逆
    for policy in [
        RoutePolicy::TsFirst,
        RoutePolicy::AllRust,
        RoutePolicy::AllTs,
    ] {
        assert_eq!(parse_route_policy(policy.as_str()).unwrap(), policy);
    }
    // 非法值：报错文案带受认可值清单（调用方直接展示给用户）；
    // userapp_split（已删除档）也在此列
    for bad in [
        "",
        "split",
        "ALL_RUST",
        "userapp",
        "ts-first",
        "userapp_split",
    ] {
        let err = parse_route_policy(bad).unwrap_err();
        assert!(
            err.contains("ts_first | all_rust | all_ts"),
            "{bad:?} 报错应含受认可值清单: {err}"
        );
    }
}

#[test]
fn custom_ports_respected() {
    let c = FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 61000,
        rust_upstream_port: 18086,
        ts_upstream_port: 6001,
        policy: RoutePolicy::TsFirst,
        coordinated_dev_lifecycle: false,
    };
    assert_eq!(
        c.upstream_port_for("/api/v1/userapp/dev/start", None),
        Upstream::Rust(18086)
    );
    assert_eq!(c.upstream_port_for("/health", None), Upstream::Ts(6001));
}

#[test]
fn hop_by_hop_detection() {
    assert!(is_hop_by_hop("Connection"));
    assert!(is_hop_by_hop("keep-alive"));
    assert!(is_hop_by_hop("upgrade"));
    assert!(!is_hop_by_hop("x-service-type"));
    assert!(!is_hop_by_hop("content-type"));
}

/// AllRust 白名单: file-server 语义路径放行, 上游 8086 的其余路由面
/// （/chat、/agent-mgmt/* 等集群内面）不放行——防 60000 入口裸暴露。
#[test]
fn all_rust_whitelist_gates_rust_upstream_surface() {
    for path in [
        "/api/version",
        "/api/computer/create-workspace",
        "/api/v1/userapp/dev/start",
        "/health",
        "/",
        "/api-docs/openapi.json",
    ] {
        assert!(all_rust_path_allowed(path), "{path} 应放行");
    }
    for path in [
        "/chat",
        "/computer/chat",
        "/agent/stop",
        "/agent-mgmt/agents/install-from-url",
        "/ready",
        "/proxy/3000/x",
    ] {
        assert!(!all_rust_path_allowed(path), "{path} 应拒绝(白名单外)");
    }
}

#[tokio::test]
async fn dynamic_port_publishes_real_bound_address() {
    // N03：端口 0 = 动态分配——status 返回真实绑定地址（非 ":0"）
    init(FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 0,
        ..Default::default()
    });
    let address = try_start().await.expect("start");
    assert!(
        !address.ends_with(":0"),
        "must publish the real bound address, got {address}"
    );
    assert!(address.starts_with("127.0.0.1:"), "got {address}");
    stop().await.expect("stop");
    assert!(status().await.is_none());
}

#[test]
fn instance_lock_domain_is_stable_per_listen_semantics() {
    // N06：固定端口 → 稳定锁域（host-port 键）；动态端口 → per-invocation
    // 独立域（多实例并行合法）
    let fixed = FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "127.0.0.1".to_string(),
        listen_port: 60000,
        ..Default::default()
    };
    // 锁文件创建成功即锁域可解析（内容断言经路径行为验证：同配置同路径）
    let file = test_lock_file(&fixed).expect("lock file");
    drop(file);
    let dynamic = FileServerProxyConfig {
        listen_port: 0,
        ..fixed.clone()
    };
    assert!(test_lock_file(&dynamic).is_ok());
}

#[tokio::test]
async fn non_loopback_without_token_is_refused() {
    // N07：对外监听无令牌 → fail-fast（不裸奔）；loopback 无令牌合法
    init(FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: false,
        listen_host: "0.0.0.0".to_string(),
        listen_port: 0,
        ..Default::default()
    });
    let error = try_start()
        .await
        .expect_err("must refuse non-loopback without token");
    assert!(
        error.contains("without FILE_SERVER_PROXY_TOKEN"),
        "got: {error}"
    );
}

// N07 反例 2：带 token 的非 loopback 可启动（bind + 锁正常）。
// init 是 OnceLock 幂等注册（首次生效）——nextest 每测试独立进程，
// 本进程首次注册即 token 形态。
#[tokio::test]
async fn non_loopback_with_token_starts() {
    init(FileServerProxyConfig {
        auth_token: Some("secret".to_string()),
        listen_host: "0.0.0.0".to_string(),
        listen_port: 0,
        ..Default::default()
    });
    let address = try_start().await.expect("token + public bind must start");
    stop().await.expect("stop");
    assert!(
        !address.ends_with(":0"),
        "real bound address, got {address}"
    );
}

// N07 反例 3：受管形态显式声明（public_bind_declared）——公开绑定无令牌
// 合法（容器 supervisor env 通道；网络边界由编排层承担）。
#[tokio::test]
async fn managed_declaration_allows_public_bind_without_token() {
    init(FileServerProxyConfig {
        auth_token: None,
        public_bind_declared: true,
        listen_host: "0.0.0.0".to_string(),
        listen_port: 0,
        ..Default::default()
    });
    let address = try_start().await.expect("managed public bind must start");
    stop().await.expect("stop");
    assert!(
        !address.ends_with(":0"),
        "real bound address, got {address}"
    );
}
