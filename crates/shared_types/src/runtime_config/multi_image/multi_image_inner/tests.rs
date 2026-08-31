    use super::*;
    use crate::default_rcoder_service_config;

    #[test]
    fn test_default_multi_image_config() {
        let config = MultiImageConfig::default();

        // 验证默认配置
        assert!(matches!(
            config.selection_strategy,
            ImageSelectionStrategy::ServiceOnly
        ));
        assert_eq!(config.services.len(), 2); // rcoder + computer-agent-runner
        assert!(config.is_service_enabled(&ServiceType::WebAgentRunner));
        assert!(config.is_service_enabled(&ServiceType::ComputerAgentRunner)); // 默认启用

        // 验证配置摘要
        let summary = config.get_summary();
        assert!(summary.contains("2/2")); // 2个启用，总共2个
    }

    #[test]
    fn test_config_validation() {
        let mut config = MultiImageConfig::default();

        // 为测试设置镜像配置
        for service_config in config.services.values_mut() {
            service_config.arm64_image = Some("test-image:arm64".to_string());
            service_config.amd64_image = Some("test-image:amd64".to_string());
        }

        // 有效配置应该通过验证
        assert!(config.validate().is_ok());

        // 测试无效配置
        let mut invalid_config = config.clone();
        invalid_config.services.clear(); // 清空所有服务
        assert!(invalid_config.validate().is_err());
    }

    #[test]
    fn test_service_management() {
        let mut config = MultiImageConfig::default();

        // 测试服务启用/禁用
        assert!(
            config
                .set_service_enabled(&ServiceType::WebAgentRunner, false)
                .is_ok()
        );
        assert!(!config.is_service_enabled(&ServiceType::WebAgentRunner));

        assert!(
            config
                .set_service_enabled(&ServiceType::WebAgentRunner, true)
                .is_ok()
        );
        assert!(config.is_service_enabled(&ServiceType::WebAgentRunner));

        // 测试不存在的服务
        assert!(
            config
                .set_service_enabled(&ServiceType::ComputerAgentRunner, true)
                .is_ok()
        ); // 存在
    }

    #[test]
    fn test_legacy_config_creation() {
        let config = create_legacy_multi_image_config(
            Some("custom-registry.com/rcoder:latest".to_string()),
            None,
            None,
            None,
        );

        // 验证传统镜像配置被正确应用
        let rcoder_config = config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            rcoder_config.image,
            Some("custom-registry.com/rcoder:latest".to_string())
        );

        // 验证只有 WebAgentRunner 服务
        assert_eq!(config.services.len(), 1);
        assert!(config.services.contains_key("web-agent-runner"));
    }

    #[test]
    fn test_project_overrides() {
        let mut overrides = ProjectImageOverrides {
            images: HashMap::new(),
            enabled_services: vec!["web-agent-runner".to_string()],
            environment: HashMap::new(),
        };

        overrides.images.insert(
            "web-agent-runner".to_string(),
            "custom-web-agent-runner:latest".to_string(),
        );
        overrides
            .environment
            .insert("DEBUG".to_string(), "true".to_string());

        assert!(overrides.validate().is_ok());

        // 测试应用配置
        let mut service_config = default_rcoder_service_config();
        overrides
            .apply_to_service_config(&ServiceType::WebAgentRunner, &mut service_config)
            .unwrap();

        assert_eq!(
            service_config.image,
            Some("custom-web-agent-runner:latest".to_string())
        );
        assert!(service_config.environment.contains_key("DEBUG"));
    }

    #[test]
    fn test_apply_global_defaults() {
        let mut config = MultiImageConfig::default();

        // 设置全局默认配置
        config.global_defaults.image = Some("global-default:latest".to_string());

        // 应用全局默认配置
        config.apply_global_defaults();

        // 验证配置被应用
        for service_config in config.services.values() {
            assert_eq!(
                service_config.image,
                Some("global-default:latest".to_string())
            );
        }
    }

    #[test]
    fn test_registry_prefix() {
        let mut config = MultiImageConfig::default();

        // 测试默认前缀（空字符串）
        assert_eq!(config.get_registry_prefix(), "");

        // 测试自定义前缀
        config.global_defaults.registry_prefix = Some("my-registry.com".to_string());
        assert_eq!(config.get_registry_prefix(), "my-registry.com");
    }

    #[test]
    fn test_config_file_loading() {
        // 测试从 JSON 配置加载配置
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev"
  },
  "services": {
    "web-agent-runner": {
      "service_type": "web-agent-runner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "image_tag_prefix": "dev-master-rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "computer-agent-runner",
      "image": "dev-rcoder-agent-runner:latest",
      "arm64_image": "dev-rcoder-agent-runner:latest",
      "amd64_image": "dev-rcoder-agent-runner:latest",
      "default_image": "dev-rcoder-agent-runner:latest",
      "image_tag_prefix": "dev-rcoder-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证 web-agent-runner 配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            web_config.image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest".to_string())
        );
        assert!(web_config.enabled);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .unwrap();
        assert_eq!(
            computer_config.image,
            Some("dev-rcoder-agent-runner:latest".to_string())
        );
        assert!(computer_config.enabled);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }

    #[test]
    fn test_config_with_legacy_service_key() {
        // 测试服务名称是 "rcoder"，但 service_type 字段是 "web-agent-runner" 的配置
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev"
  },
  "services": {
    "rcoder": {
      "service_type": "web-agent-runner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest",
      "image_tag_prefix": "dev-master-rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "computer-agent-runner",
      "image": "dev-rcoder-agent-runner:latest",
      "arm64_image": "dev-rcoder-agent-runner:latest",
      "amd64_image": "dev-rcoder-agent-runner:latest",
      "default_image": "dev-rcoder-agent-runner:latest",
      "image_tag_prefix": "dev-rcoder-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证通过新的服务名称可以找到配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .unwrap();
        assert_eq!(
            web_config.image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/dev-master-rcoder:latest".to_string())
        );
        assert!(web_config.enabled);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .unwrap();
        assert_eq!(
            computer_config.image,
            Some("dev-rcoder-agent-runner:latest".to_string())
        );
        assert!(computer_config.enabled);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }

    #[test]
    fn test_config_from_local_config_file() {
        // 测试从本地配置文件 docker/config.yml 加载配置
        // 这是本地开发测试使用的配置文件
        // 使用相对路径读取项目根目录下的 docker/config.yml
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = std::path::Path::new(manifest_dir)
            .ancestors()
            .find(|p| p.join("docker/config.yml").exists())
            .expect("Could not find project root with docker/config.yml")
            .join("docker/config.yml");

        let config_content = std::fs::read_to_string(&config_path)
            .unwrap_or_else(|e| panic!("Failed to read config file at {:?}: {}", config_path, e));

        // 解析 YAML 配置
        let config: serde_yaml::Value = serde_yaml::from_str(&config_content)
            .unwrap_or_else(|e| panic!("Failed to parse YAML config: {}", e));

        // 提取 multi_image_config 部分
        let multi_image_config = config
            .get("docker_config")
            .and_then(|dc| dc.get("multi_image_config"))
            .expect("multi_image_config not found in config file");

        // 转换为 MultiImageConfig
        let multi_config: MultiImageConfig = serde_yaml::from_value(multi_image_config.clone())
            .unwrap_or_else(|e| panic!("Failed to parse multi_image_config: {}", e));

        // 验证服务数量(web-agent-runner + computer-agent-runner + user-app-builder)
        assert_eq!(multi_config.services.len(), 3);

        // 验证 web-agent-runner 配置
        let web_config = multi_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .expect("web-agent-runner config not found");
        assert!(web_config.image.is_some());
        assert!(web_config.enabled);
        assert_eq!(web_config.service_type, ServiceType::WebAgentRunner);

        // 验证 computer-agent-runner 配置
        let computer_config = multi_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .expect("computer-agent-runner config not found");
        assert!(computer_config.image.is_some());
        assert!(computer_config.enabled);
        assert_eq!(
            computer_config.service_type,
            ServiceType::ComputerAgentRunner
        );

        // 验证 user-app-builder 配置(路 B)
        let builder_config = multi_config
            .get_service_config(&ServiceType::UserappBuilder)
            .expect("user-app-builder config not found");
        assert!(builder_config.image.is_some());
        assert!(builder_config.enabled);
        assert_eq!(builder_config.service_type, ServiceType::UserappBuilder);

        // 验证配置有效
        assert!(multi_config.validate().is_ok());

        // 验证通过 ServiceType 枚举可以找到配置
        assert!(
            multi_config
                .get_service_config(&ServiceType::WebAgentRunner)
                .is_some()
        );
        assert!(
            multi_config
                .get_service_config(&ServiceType::ComputerAgentRunner)
                .is_some()
        );

        // 输出配置摘要
        println!(
            "✅ Local config loaded: {} services, registry_prefix={:?}",
            multi_config.services.len(),
            multi_config.global_defaults.registry_prefix
        );
        for (key, svc) in &multi_config.services {
            println!(
                "  - {}: service_type={}, image={:?}, enabled={}",
                key, svc.service_type, svc.image, svc.enabled
            );
        }
    }

    #[test]
    fn test_config_with_rcoder_key_and_web_agent_runner_type() {
        // 测试服务名称是 "rcoder"，但 service_type 字段是 "WebAgentRunner" 的配置
        // 这是测试环境使用的配置格式
        let config_json = r#"
{
  "global_defaults": {
    "registry_prefix": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test"
  },
  "services": {
    "rcoder": {
      "service_type": "WebAgentRunner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest",
      "image_tag_prefix": "rcoder",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    },
    "computer-agent-runner": {
      "service_type": "ComputerAgentRunner",
      "image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "arm64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "amd64_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "default_image": "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest",
      "image_tag_prefix": "computer-agent-runner",
      "enabled": true,
      "environment": {},
      "mounts": [],
      "command": [],
      "resource_limits": {},
      "work_dir": "/app",
      "network_mode": "bridge"
    }
  },
  "selection_strategy": "ServiceOnly",
  "cache_config": {
    "enabled": true,
    "ttl_seconds": 3600,
    "max_entries": 50
  }
}
"#;

        let multi_config: MultiImageConfig = serde_json::from_str(config_json).unwrap();

        // 验证服务数量
        assert_eq!(multi_config.services.len(), 2);

        // 验证通过 ServiceType 枚举可以找到配置
        let web_config = multi_config.get_service_config(&ServiceType::WebAgentRunner);
        assert!(
            web_config.is_some(),
            "Should find config for WebAgentRunner"
        );
        assert_eq!(
            web_config.unwrap().image,
            Some(
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest"
                    .to_string()
            )
        );

        let computer_config = multi_config.get_service_config(&ServiceType::ComputerAgentRunner);
        assert!(
            computer_config.is_some(),
            "Should find config for ComputerAgentRunner"
        );
        assert_eq!(
            computer_config.unwrap().image,
            Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest".to_string())
        );

        // 验证配置有效
        assert!(multi_config.validate().is_ok());
    }
