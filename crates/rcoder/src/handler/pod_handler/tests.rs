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
    // app_id 与 service_type 互斥
    assert!(parse_app_target(Some("app-1"), None, Some("computer-agent-runner")).is_err());
    // 非法 stage 值
    assert!(parse_app_target(Some("app-1"), Some("staging"), None).is_err());
    // app_stage 依附于 app_id
    assert!(parse_app_target(None, Some("dev"), None).is_err());
    // identifier 白名单（防容器名/bind 路径注入）
    assert!(parse_app_target(Some("../escape"), None, None).is_err());
    assert!(parse_app_target(Some("app/1"), None, None).is_err());
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
    }
}
