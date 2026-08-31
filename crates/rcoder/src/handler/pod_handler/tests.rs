use super::helpers::*;
use super::types::*;
use super::*;

#[test]
fn test_pod_count_by_service_type_default() {
    let count = PodCountByServiceType {
        rcoder: 0,
        computer_agent_runner: 0,
    };
    assert_eq!(count.rcoder + count.computer_agent_runner, 0);
}

#[test]
fn test_pod_resource_limits_serialization() {
    let limits = ServiceResourceLimits {
        memory: Some(4294967296.0),
        cpu: Some(2.0),
        swap: Some(6442450944.0),
        storage_size: Some("10Gi".to_string()),
        ephemeral_storage_limit: None,
    };

    let json = serde_json::to_string(&limits).unwrap();
    assert!(json.contains("4294967296"));
    assert!(json.contains("2.0"));
    assert!(json.contains("6442450944"));
    assert!(json.contains("10Gi"));
}

#[test]
fn test_ensure_pod_response_serialization() {
    let response = EnsurePodResponse {
        created: true,
        container_info: PodContainerInfo {
            container_id: "abc123".to_string(),
            status: "running".to_string(),
        },
        message: "容器创建成功".to_string(),
    };

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("created"));
    assert!(json.contains("container_info"));
    assert!(json.contains("message"));
}

#[test]
fn test_validate_resource_limits_valid() {
    let limits = ServiceResourceLimits {
        memory: Some(4294967296.0), // 4GB
        cpu: Some(2.0),
        swap: Some(6442450944.0), // 6GB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_none_values() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_cpu_zero() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_negative() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(-1.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_too_large() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(200.0),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_memory_too_small() {
    let limits = ServiceResourceLimits {
        memory: Some(256_000_000.0), // 256MB
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_memory_too_large() {
    let limits = ServiceResourceLimits {
        memory: Some(256_000_000_000.0), // 256GB
        cpu: None,
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_swap_less_than_memory() {
    let limits = ServiceResourceLimits {
        memory: Some(8_589_934_592.0), // 8GB
        cpu: None,
        swap: Some(4_294_967_296.0), // 4GB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    // swap<memory 已改为 resolve 阶段 normalize_swap 自动规整,validate 不再拒绝
    assert!(validate_resource_limits(&limits).is_ok());
}

#[test]
fn test_validate_resource_limits_swap_too_small() {
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: None,
        swap: Some(256_000_000.0), // 256MB
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_err());
}

#[test]
fn test_validate_resource_limits_cpu_boundary() {
    // 测试边界值：0.1 应该失败（小于等于 0）
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.1),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());

    // 测试边界值：0.01 应该通过
    let limits = ServiceResourceLimits {
        memory: None,
        cpu: Some(0.01),
        swap: None,
        storage_size: None,
        ephemeral_storage_limit: None,
    };
    assert!(validate_resource_limits(&limits).is_ok());
}

// ============================================================================
// userApp 分派（parse_app_target）
// ============================================================================

#[test]
fn app_target_no_app_id_falls_through_to_agent_path() {
    // 无 app_id 无 app_stage → agent/computer 既有路径
    assert!(matches!(
        parse_app_target(None, None, Some("computer-agent-runner")),
        Ok(AppTarget::NotApp)
    ));
}

#[test]
fn app_target_dev_and_prod_dispatch() {
    assert!(matches!(
        parse_app_target(Some("app-1"), None, None),
        Ok(AppTarget::Dev(id)) if id == "app-1"
    ));
    assert!(matches!(
        parse_app_target(Some("app-1"), Some("dev"), None),
        Ok(AppTarget::Dev(id)) if id == "app-1"
    ));
    assert!(matches!(
        parse_app_target(Some("app-1"), Some("prod"), None),
        Ok(AppTarget::Prod(id)) if id == "app-1"
    ));
    // 空串 app_id 视为未传（回 agent 路径）
    assert!(matches!(
        parse_app_target(Some("  "), None, None),
        Ok(AppTarget::NotApp)
    ));
}

#[test]
fn app_target_validates_stage_and_conflicts() {
    // app_id 与非 userapp 的 service_type 互斥
    assert!(parse_app_target(Some("app-1"), None, Some("computer-agent-runner")).is_err());
    // 非法 stage 值
    assert!(parse_app_target(Some("app-1"), Some("staging"), None).is_err());
    // app_stage 依附于 app_id
    assert!(parse_app_target(None, Some("dev"), None).is_err());
    // identifier 白名单（防容器名/bind 路径注入）
    assert!(parse_app_target(Some("../escape"), None, None).is_err());
    assert!(parse_app_target(Some("app/1"), None, None).is_err());
}

/// userApp 场景统一三字段形态：service_type=userapp（大小写不敏感）与 app_id
/// 搭配放行分派；无 app_id 时单独的 userapp 标记报错（防误走 agent 路径空查）。
#[test]
fn app_target_accepts_userapp_service_type_alongside_app_id() {
    // userapp 搭配 app_id → 正常分派（缺省/显式 dev 与 prod）
    assert!(matches!(
        parse_app_target(Some("app-1"), None, Some("userapp")),
        Ok(AppTarget::Dev(id)) if id == "app-1"
    ));
    assert!(matches!(
        parse_app_target(Some("app-1"), Some("prod"), Some("userapp")),
        Ok(AppTarget::Prod(id)) if id == "app-1"
    ));
    // 大小写不敏感 + 既有 ServiceType 变体同义
    for variant in ["USERAPP", "Userapp", "user-app"] {
        assert!(
            matches!(
                parse_app_target(Some("app-1"), None, Some(variant)),
                Ok(AppTarget::Dev(_))
            ),
            "service_type={variant:?} 应视为 userapp 变体放行"
        );
    }
    // userapp 标记缺 app_id → 报错（不走 agent 路径空查 Userapp 容器）
    assert!(parse_app_target(None, None, Some("userapp")).is_err());
}

/// agent 族分派（status/stop/cancel/notify-resolved/cache-clean 共用）：
/// wire 形态 = service_type=userapp + **project_id 兼任 app_id** + app_stage
/// （缺省 dev，prod 拒绝——agent 会话仅存在于 dev 阶段）。
#[test]
fn agent_userapp_dispatch_resolves_project_id_as_app_id() {
    // userapp + project_id → Some(app_id)（内部以 app_id 语义消费 project_id 值）
    for variant in ["userapp", "USERAPP", "Userapp", "user-app"] {
        assert_eq!(
            parse_agent_userapp_dispatch(Some(variant), Some("app-1"), None),
            Ok(Some("app-1".to_string())),
            "service_type={variant:?} 应视为 userapp 变体分派"
        );
    }
    // 非 userapp / 未传 service_type → None 直通 computer 既有路径
    assert_eq!(
        parse_agent_userapp_dispatch(None, Some("p1"), None),
        Ok(None)
    );
    assert_eq!(
        parse_agent_userapp_dispatch(Some("computer-agent-runner"), Some("p1"), None),
        Ok(None)
    );
    // app_stage 缺省 dev；prod / 非法值拒绝
    assert_eq!(
        parse_agent_userapp_dispatch(Some("userapp"), Some("app-1"), Some("dev")),
        Ok(Some("app-1".to_string()))
    );
    assert!(parse_agent_userapp_dispatch(Some("userapp"), Some("app-1"), Some("prod")).is_err());
    assert!(parse_agent_userapp_dispatch(Some("userapp"), Some("app-1"), Some("staging")).is_err());
}

/// 入参语义防误用：user-app-builder（UserappBuilder 变体）不是有效入参——
/// userApp 容器类型由 app_stage 推导，传 builder 显式报错引导正确形态
/// （静默直通会把 project_id 兼任的 app_id 当普通项目 ID 查出误导错误）。
#[test]
fn agent_userapp_dispatch_rejects_builder_variant() {
    for variant in ["user-app-builder", "USER-APP-BUILDER"] {
        let err = parse_agent_userapp_dispatch(Some(variant), Some("app-1"), None)
            .expect_err("user-app-builder 应显式拒绝");
        assert!(
            err.contains("app_stage 推导"),
            "报错应引导 app_stage 推导语义: {err}"
        );
    }
    // 未知值仍宽松直通 computer 路径（cache 场景需 computer-agent-runner 直通）
    assert_eq!(
        parse_agent_userapp_dispatch(Some("computer-agent-runner"), Some("p1"), None),
        Ok(None)
    );
}

/// agent 族分派的校验面：userapp 缺 project_id 报错、project_id 走 identifier
/// 白名单（防容器名/路径拼接注入）、空白串视为未传。
#[test]
fn agent_userapp_dispatch_validates_inputs() {
    // userapp 标记缺 project_id（兼任 app_id）→ 报错
    assert!(parse_agent_userapp_dispatch(Some("userapp"), None, None).is_err());
    assert!(parse_agent_userapp_dispatch(Some("userapp"), Some("  "), None).is_err());
    // identifier 白名单
    assert!(parse_agent_userapp_dispatch(Some("userapp"), Some("../escape"), None).is_err());
    assert!(parse_agent_userapp_dispatch(Some("userapp"), Some("app/1"), None).is_err());
}

/// 契约钉住：userApp 请求只传 app_id/app_stage 即可反序列化（user_id/project_id
/// 有 serde default 兜底，agent 路径空值校验在后）——Java 最小请求形态。
#[test]
fn userapp_minimal_request_deserializes_without_user_or_project() {
    for raw in [
        r#"{"app_id":"app-1"}"#,
        r#"{"app_id":"app-1","app_stage":"dev"}"#,
        r#"{"app_id":"app-1","app_stage":"prod"}"#,
    ] {
        let ensured: EnsurePodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("EnsurePodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(ensured.user_id, "");
        assert_eq!(ensured.app_id.as_deref(), Some("app-1"));
        let ka: KeepalivePodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("KeepalivePodRequest {raw} 应可反序列化: {e}"));
        assert!(ka.app_stage.is_some() || ka.app_stage.is_none());
        let rs: RestartPodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("RestartPodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(rs.project_id, "");
        let sp: StopPodRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("StopPodRequest {raw} 应可反序列化: {e}"));
        assert_eq!(sp.user_id, "");
        assert_eq!(sp.app_id.as_deref(), Some("app-1"));
    }
}

/// 契约钉住：stop 的 agent 路径完整形态（user_id/project_id/service_type）+
/// query 形态（I18nJsonOrQuery 兜底）均可反序列化。
#[test]
fn stop_pod_request_deserializes_agent_path_forms() {
    let sp: StopPodRequest = serde_json::from_str(
        r#"{"user_id":"user_123","project_id":"proj_456","service_type":"web-agent-runner"}"#,
    )
    .unwrap_or_else(|e| panic!("StopPodRequest agent 形态应可反序列化: {e}"));
    assert_eq!(sp.user_id, "user_123");
    assert_eq!(sp.project_id, "proj_456");
    assert_eq!(sp.service_type.as_deref(), Some("web-agent-runner"));
    assert!(sp.pod_id.is_none() && sp.app_id.is_none() && sp.app_stage.is_none());

    let qs: StopPodRequest = serde_urlencoded::from_str("user_id=user_123&project_id=proj_456")
        .unwrap_or_else(|e| panic!("StopPodRequest query 形态应可反序列化: {e}"));
    assert_eq!(qs.user_id, "user_123");
    assert_eq!(qs.project_id, "proj_456");
}

/// 契约钉住：GET 两接口的 userApp 三字段 query 形态可反序列化（userapp 与
/// app_id/app_stage 搭配；user_id/project_id 不传为 None）——Java 统一传参形态。
#[test]
fn userapp_query_deserializes_with_three_field_form() {
    // serde_urlencoded 是 axum Query 底层（I18nQuery 纯透传），用同一引擎验证。
    for raw in [
        "service_type=userapp&app_id=app-1",
        "service_type=userapp&app_id=app-1&app_stage=dev",
        "service_type=userapp&app_id=app-1&app_stage=prod",
    ] {
        let ps: PodStatusQuery = serde_urlencoded::from_str(raw)
            .unwrap_or_else(|e| panic!("PodStatusQuery {raw} 应可反序列化: {e}"));
        assert_eq!(ps.service_type.as_deref(), Some("userapp"));
        assert_eq!(ps.app_id.as_deref(), Some("app-1"));
        assert!(ps.user_id.is_none() && ps.project_id.is_none());

        let vs: VncStatusQuery = serde_urlencoded::from_str(raw)
            .unwrap_or_else(|e| panic!("VncStatusQuery {raw} 应可反序列化: {e}"));
        assert_eq!(vs.service_type.as_deref(), Some("userapp"));
        assert_eq!(vs.app_id.as_deref(), Some("app-1"));
    }
    // 仅 app_id（service_type/app_stage 缺省）也可——与 POST 三兄弟最小契约对齐
    let ps: PodStatusQuery = serde_urlencoded::from_str("app_id=app-1")
        .unwrap_or_else(|e| panic!("PodStatusQuery 应可反序列化: {e}"));
    assert_eq!(ps.app_id.as_deref(), Some("app-1"));
    assert!(ps.service_type.is_none() && ps.app_stage.is_none());
}
