
use super::*;
use crate::proxy::{all_rust_path_allowed, is_hop_by_hop};

fn cfg() -> FileServerProxyConfig {
    FileServerProxyConfig::default()
}

/// config.yml wire 契约：策略值为 snake_case（helm 模板渲染依赖此形态）。
#[test]
fn policy_serializes_snake_case() {
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::UserappSplit)
            .unwrap()
            .trim(),
        "userapp_split"
    );
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::AllRust).unwrap().trim(),
        "all_rust"
    );
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::AllTs).unwrap().trim(),
        "all_ts"
    );
    assert_eq!(
        serde_yaml::to_string(&RoutePolicy::TsFirst).unwrap().trim(),
        "ts_first"
    );
    let parsed: RoutePolicy = serde_yaml::from_str("all_rust").unwrap();
    assert_eq!(parsed, RoutePolicy::AllRust);
    let parsed: RoutePolicy = serde_yaml::from_str("all_ts").unwrap();
    assert_eq!(parsed, RoutePolicy::AllTs);
    let parsed: RoutePolicy = serde_yaml::from_str("ts_first").unwrap();
    assert_eq!(parsed, RoutePolicy::TsFirst);
    // 段缺 policy 字段 → 默认 UserappSplit（存量 config 兼容）
    let parsed: FileServerProxyConfig = serde_yaml::from_str(
        "listen_port: 60000\nrust_upstream_port: 8086\nts_upstream_port: 60001\n",
    )
    .unwrap();
    assert_eq!(parsed.policy, RoutePolicy::UserappSplit);
}

/// 主 pod 形态（UserappSplit 默认策略）：header 与 path 双判据。
#[test]
fn userapp_split_policy_routes_by_header_and_path() {
    let c = cfg();
    assert_eq!(c.policy, RoutePolicy::UserappSplit, "默认策略为主 pod 形态");
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
        listen_port: 60000,
        rust_upstream_port: 60002,
        ts_upstream_port: 60001,
        policy: RoutePolicy::AllRust,
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
        listen_port: 60000,
        rust_upstream_port: 8086,
        ts_upstream_port: 41234,
        policy: RoutePolicy::AllTs,
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
        parse_route_policy("userapp_split").unwrap(),
        RoutePolicy::UserappSplit
    );
    assert_eq!(
        parse_route_policy("all_rust").unwrap(),
        RoutePolicy::AllRust
    );
    assert_eq!(parse_route_policy("all_ts").unwrap(), RoutePolicy::AllTs);
    assert_eq!(
        parse_route_policy("ts_first").unwrap(),
        RoutePolicy::TsFirst
    );
    // trim 容差（env 值尾随空白是常见脏数据）
    assert_eq!(parse_route_policy(" all_ts\n").unwrap(), RoutePolicy::AllTs);
    // as_str 与 parse 互逆
    for policy in [
        RoutePolicy::UserappSplit,
        RoutePolicy::AllRust,
        RoutePolicy::AllTs,
        RoutePolicy::TsFirst,
    ] {
        assert_eq!(parse_route_policy(policy.as_str()).unwrap(), policy);
    }
    // 非法值：报错文案带受认可值清单（调用方直接展示给用户）
    for bad in ["", "split", "ALL_RUST", "userapp", "ts-first"] {
        let err = parse_route_policy(bad).unwrap_err();
        assert!(
            err.contains("userapp_split | all_rust | all_ts | ts_first"),
            "{bad:?} 报错应含受认可值清单: {err}"
        );
    }
}

/// TS 优先模式（TsFirst）：存量同名接口全走 TS——**含 userApp 标记**
/// （header 判据失效，由 TS 以 service_type 入参消费）；仅 Rust 独有的
/// `/api/v1/userapp*` 走 rust。与 UserappSplit 的差异点就在 header 判据。
#[test]
fn ts_first_policy_routes_legacy_to_ts_even_with_userapp_header() {
    let c = FileServerProxyConfig {
        listen_port: 60000,
        rust_upstream_port: 8086,
        ts_upstream_port: 60001,
        policy: RoutePolicy::TsFirst,
    };
    // Rust 独有接口 → rust
    for path in [
        "/api/v1/userapp",
        "/api/v1/userapp/dev/start",
        "/api/v1/userapp/files",
    ] {
        assert_eq!(
            c.upstream_port_for(path, None),
            Upstream::Rust(8086),
            "{path}"
        );
    }
    // 存量同名接口 → TS；带 userApp 标记也走 TS（差异点/语义铁证）
    for (path, header) in [
        ("/api/computer/get-file-list", None),
        ("/api/computer/get-file-list", Some("userapp")),
        ("/api/project/content", Some("userapp")),
        ("/health", None),
        ("/api/version", None),
        ("/api/v1/userapplication", None),
    ] {
        assert_eq!(
            c.upstream_port_for(path, header),
            Upstream::Ts(60001),
            "{path} {header:?} 应走 TS"
        );
    }
}

#[test]
fn custom_ports_respected() {
    let c = FileServerProxyConfig {
        listen_port: 61000,
        rust_upstream_port: 18086,
        ts_upstream_port: 6001,
        policy: RoutePolicy::UserappSplit,
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
