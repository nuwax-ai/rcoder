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
