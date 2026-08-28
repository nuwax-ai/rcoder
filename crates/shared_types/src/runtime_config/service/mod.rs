//! 服务镜像配置（目录化：resource/image/defaults 按域分组；
//! runtime_config 约定"对外只走 crate 根 re-export"，下游零路径依赖）。

mod defaults;
mod image;
mod resource;

pub use defaults::{default_agent_runner_service_config, default_rcoder_service_config};
pub use image::{ConfigValidationResult, ServiceImageConfig, ServiceMountConfig};
pub use resource::{ServiceResourceLimits, ServiceSecurityConfig};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::ServiceType;

    use super::*;

    /// ServiceSecurityConfig 反序列化：未配置字段应为 None
    #[test]
    fn test_security_config_deserialize() {
        let json = r#"{"privileged":true,"cap_add":["SYS_PTRACE"],"security_opt":["seccomp=unconfined"],"pids_limit":200}"#;
        let sec: ServiceSecurityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(sec.privileged, Some(true));
        assert_eq!(sec.cap_add, Some(vec!["SYS_PTRACE".to_string()]));
        assert_eq!(
            sec.security_opt,
            Some(vec!["seccomp=unconfined".to_string()])
        );
        assert_eq!(sec.pids_limit, Some(200));
        assert_eq!(sec.cap_drop, None);
        assert_eq!(sec.init, None);
    }

    /// 默认服务配置（代码内构造）security 必须为 None —— 不改变现有默认行为
    #[test]
    fn test_default_service_config_security_is_none() {
        assert!(default_agent_runner_service_config().security.is_none());
        assert!(default_rcoder_service_config().security.is_none());
    }

    /// ServiceImageConfig 带 security 的 round-trip：验证 security 字段 serde 贯通
    #[test]
    fn test_service_image_config_security_roundtrip() {
        let mut cfg = default_agent_runner_service_config();
        cfg.security = Some(ServiceSecurityConfig {
            privileged: Some(false),
            cap_add: Some(vec!["SYS_PTRACE".to_string()]),
            cap_drop: None,
            security_opt: Some(vec!["seccomp=unconfined".to_string()]),
            pids_limit: None,
            init: None,
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: ServiceImageConfig = serde_json::from_str(&json).unwrap();
        let sec = cfg2.security.expect("security preserved after roundtrip");
        assert_eq!(sec.cap_add, Some(vec!["SYS_PTRACE".to_string()]));
        assert_eq!(
            sec.security_opt,
            Some(vec!["seccomp=unconfined".to_string()])
        );
    }

    /// serde alias 兼容：旧字段名（memory_limit/cpu_limit/swap_limit）经 alias 反序列化到
    /// 新字段（memory/cpu/swap）。保证 config.yml 旧键名 + 旧 HTTP 请求不破坏。
    #[test]
    fn test_resource_limits_serde_alias() {
        // 旧字段名（config.yml 现状）经 alias 解析到新字段
        let json_old = r#"{"memory_limit":1e9,"cpu_limit":2.0,"swap_limit":2e9}"#;
        let limits_old: ServiceResourceLimits = serde_json::from_str(json_old).unwrap();
        assert_eq!(limits_old.memory, Some(1e9));
        assert_eq!(limits_old.cpu, Some(2.0));
        assert_eq!(limits_old.swap, Some(2e9));

        // 新字段名直接解析
        let json_new = r#"{"memory":1e9,"cpu":2.0,"swap":2e9}"#;
        let limits_new: ServiceResourceLimits = serde_json::from_str(json_new).unwrap();
        assert_eq!(limits_new.memory, Some(1e9));
        assert_eq!(limits_new.cpu, Some(2.0));
        assert_eq!(limits_new.swap, Some(2e9));

        // 序列化用新字段名（不带 _limit）
        let s = serde_json::to_string(&limits_old).unwrap();
        assert!(
            s.contains("\"memory\""),
            "serialized should use new field name: {s}"
        );
        assert!(
            !s.contains("memory_limit"),
            "serialized should not use alias: {s}"
        );
    }

    #[test]
    fn test_config_validation() {
        let mut config = default_rcoder_service_config();

        // 为测试设置镜像配置
        config.arm64_image = Some("test-image:arm64".to_string());
        config.amd64_image = Some("test-image:amd64".to_string());

        // 有效配置
        assert!(matches!(config.validate(), ConfigValidationResult::Valid));

        // 无效配置：所有镜像为空
        let mut invalid_config = config.clone();
        invalid_config.image = None;
        invalid_config.arm64_image = None;
        invalid_config.amd64_image = None;
        invalid_config.default_image = None;
        assert!(matches!(
            invalid_config.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_environment_merge() {
        let config = default_rcoder_service_config();

        let mut base_env = HashMap::new();
        base_env.insert("BASE_VAR".to_string(), "base_value".to_string());
        base_env.insert("RUST_LOG".to_string(), "debug".to_string()); // 重叠

        let merged = config.merge_environment(&base_env);

        assert_eq!(merged.get("BASE_VAR"), Some(&"base_value".to_string()));
        // 服务特定环境变量应该覆盖基础变量
        assert_eq!(merged.get("RUST_LOG"), Some(&"info".to_string())); // RCoder 配置是 info
        assert_eq!(merged.get("SERVICE_MODE"), Some(&"full".to_string()));
    }

    #[test]
    fn test_mount_validation() {
        // 创建一个有挂载点的配置用于测试
        let config_with_mounts = ServiceImageConfig {
            service_type: ServiceType::WebAgentRunner,
            image: None,
            arm64_image: Some("test-image:arm64".to_string()),
            amd64_image: Some("test-image:amd64".to_string()),
            default_image: Some("test-image:latest".to_string()),
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(),
            mounts: vec![ServiceMountConfig {
                container_path: "/app/workspace".to_string(),
                host_path: "/host/workspace".to_string(),
                read_only: false,
                mount_type: "bind".to_string(),
                resolve_from: None,
            }],
            command: vec![],
            entrypoint: None,
            resource_limits: ServiceResourceLimits::new(None, None, None, None, None),
            work_dir: "/app".to_string(),
            network_mode: "bridge".to_string(),
            container_path_template: "/app/project_workspace/{project_id}".to_string(),
            workspace_resolution_path: None,
            security: None,
        };

        for mount in &config_with_mounts.mounts {
            assert!(matches!(mount.validate(), ConfigValidationResult::Valid));
        }

        // 测试无效挂载
        let mut invalid_mount = config_with_mounts.mounts[0].clone();
        invalid_mount.container_path = "".to_string();
        assert!(matches!(
            invalid_mount.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_mount_path_resolution() {
        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-123".to_string());
        variables.insert("workspace_dir".to_string(), "/app/workspace".to_string());

        let mount = ServiceMountConfig {
            container_path: "/app/workspace/{project_id}".to_string(),
            host_path: "{workspace_dir}/projects/{project_id}".to_string(),
            read_only: false,
            mount_type: "bind".to_string(),
            resolve_from: None,
        };

        let resolved = mount.resolve_host_path(&variables);
        assert_eq!(resolved, "/app/workspace/projects/test-project-123");
    }

    #[test]
    fn test_get_summary() {
        let config = default_rcoder_service_config();
        let summary = config.get_summary();

        assert!(summary.contains("web-agent-runner"));
        assert!(summary.contains("Enabled: true"));
        // 镜像配置为空时，summary 不包含镜像地址
    }

    #[test]
    fn test_container_prefix_with_image_tag_prefix() {
        // 测试使用 image_tag_prefix 的情况
        let config = default_agent_runner_service_config();
        assert_eq!(config.container_prefix(), "computer-agent-runner");
    }

    #[test]
    fn test_container_prefix_fallback_to_service_type() {
        // 测试没有 image_tag_prefix 时回退到 service_type 默认值
        let mut config = default_rcoder_service_config();
        config.image_tag_prefix = None;
        assert_eq!(config.container_prefix(), "web-agent-runner");
    }

    #[test]
    fn test_container_prefix_rcoder() {
        // WebAgentRunner 配置使用 web-agent-runner 前缀
        let config = default_rcoder_service_config();
        assert_eq!(config.container_prefix(), "web-agent-runner");
    }

    /// 测试 ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 的差异
    ///
    /// 这是导致 VNC 状态查询返回 CONTAINER_NOT_FOUND 的根因：
    /// - ServiceType::container_prefix() 返回硬编码的 "computer-agent-runner"
    /// - ServiceConfig::container_prefix() 读取配置的 image_tag_prefix "computer-agent-runner"
    /// - 容器创建使用后者，而错误的查询代码使用前者，导致名称不匹配
    #[test]
    fn test_container_prefix_difference_causes_container_not_found() {
        // 硬编码的 ServiceType 前缀（错误的查询方式）
        let service_type_prefix = ServiceType::ComputerAgentRunner.container_prefix();
        assert_eq!(service_type_prefix, "computer-agent-runner");

        // 配置化的 ServiceConfig 前缀（正确的创建方式）
        let config = default_agent_runner_service_config();
        let config_prefix = config.container_prefix();
        assert_eq!(config_prefix, "computer-agent-runner");

        // 两者应该相同
        assert_eq!(
            service_type_prefix, config_prefix,
            "ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 应该相同"
        );

        // 展示如果用错误的前缀构造容器名会导致什么问题
        let user_id = "1743762321";
        let container_name = format!("{}-{}", service_type_prefix, user_id);

        assert_eq!(container_name, "computer-agent-runner-1743762321");
    }

    #[test]
    fn test_resource_limits_validation_valid() {
        let valid = ServiceResourceLimits {
            memory: Some(1_000_000_000.0), // 1GB
            cpu: Some(2.0),
            swap: Some(2_000_000_000.0), // 2GB
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_small() {
        let invalid = ServiceResourceLimits {
            memory: Some(256_000_000.0), // 256MB - 太小
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(invalid.validate().unwrap_err().contains("at least 512MB"));
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_large() {
        let invalid = ServiceResourceLimits {
            memory: Some(100_000_000_000.0), // 100GB - 太大
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("cannot exceed 64GB")
        );
    }

    #[test]
    fn test_resource_limits_validation_invalid_cpu_too_small() {
        let invalid = ServiceResourceLimits {
            memory: None,
            cpu: Some(0.1), // 太小
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("at least 0.5 cores")
        );
    }

    #[test]
    fn test_resource_limits_normalize_swap_less_than_memory() {
        // swap < memory 不再导致 validate 失败(已改为自动规整)
        let rl = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: None,
            swap: Some(1_000_000_000.0), // 1GB < memory
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        assert!(rl.validate().is_ok());

        // normalize_swap:swap < memory → swap = memory × 2
        let (fixed, changed) = rl.normalize_swap();
        assert!(changed);
        assert_eq!(fixed.swap, Some(4_000_000_000.0));
        assert_eq!(fixed.memory, Some(2_000_000_000.0)); // memory 不变

        // swap >= memory 时不修正
        let ok = ServiceResourceLimits {
            memory: Some(2_000_000_000.0),
            cpu: None,
            swap: Some(4_000_000_000.0),
            storage_size: None,
            ephemeral_storage_limit: None,
        };
        let (same, changed2) = ok.normalize_swap();
        assert!(!changed2);
        assert_eq!(same.swap, Some(4_000_000_000.0));
    }

    #[test]
    fn test_resource_limits_merge() {
        let default_limits = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: Some(2.0),
            swap: Some(4_000_000_000.0), // 4GB
            storage_size: None,
            ephemeral_storage_limit: None,
        };

        let override_limits = ServiceResourceLimits {
            memory: Some(4_000_000_000.0), // 覆盖：4GB
            cpu: None,                     // 不覆盖
            swap: Some(8_000_000_000.0),   // 覆盖：8GB
            storage_size: Some("20Gi".to_string()),
            ephemeral_storage_limit: None,
        };

        let merged = default_limits.merge_with(&override_limits);
        assert_eq!(merged.memory, Some(4_000_000_000.0));
        assert_eq!(merged.cpu, Some(2.0)); // 保留默认
        assert_eq!(merged.swap, Some(8_000_000_000.0));
        assert_eq!(merged.storage_size, Some("20Gi".to_string()));
    }

    #[test]
    fn test_resource_limits_merge_all_none() {
        let default_limits = ServiceResourceLimits {
            memory: Some(2_000_000_000.0), // 2GB
            cpu: Some(2.0),
            swap: Some(4_000_000_000.0), // 4GB
            storage_size: Some("10Gi".to_string()),
            ephemeral_storage_limit: None,
        };

        let override_limits = ServiceResourceLimits {
            memory: None,
            cpu: None,
            swap: None,
            storage_size: None,
            ephemeral_storage_limit: None,
        };

        let merged = default_limits.merge_with(&override_limits);
        // 所有字段都应该保留默认值
        assert_eq!(merged.memory, Some(2_000_000_000.0));
        assert_eq!(merged.cpu, Some(2.0));
        assert_eq!(merged.swap, Some(4_000_000_000.0));
        assert_eq!(merged.storage_size, Some("10Gi".to_string()));
    }

    #[test]
    fn test_workspace_resolution_path_rcoder() {
        let config = default_rcoder_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/project_workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/computer-project-workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_explicit_override() {
        let mut config = default_rcoder_service_config();
        config.workspace_resolution_path = Some("/custom/path".to_string());
        assert_eq!(config.effective_workspace_resolution_path(), "/custom/path");
    }

    #[test]
    fn test_workspace_container_path_rcoder() {
        let config = default_rcoder_service_config();
        // RCoder: PROJECT_WORKSPACE_BASE="/app/project_workspace"
        assert_eq!(config.workspace_container_path(), "/app/project_workspace");
    }

    #[test]
    fn test_workspace_container_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // ComputerAgentRunner: PROJECT_WORKSPACE_BASE="/home/user"
        assert_eq!(config.workspace_container_path(), "/home/user");
    }

    #[test]
    fn test_workspace_container_path_fallback() {
        let mut config = default_rcoder_service_config();
        config.environment.remove("PROJECT_WORKSPACE_BASE");
        // 无环境变量时回退到 effective_workspace_resolution_path
        assert_eq!(
            config.workspace_container_path(),
            config.effective_workspace_resolution_path()
        );
    }
}
