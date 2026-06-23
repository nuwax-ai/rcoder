//! 配置加载测试
//!
//! 独立的测试文件，用于验证配置文件的读取和解析逻辑。
//! 复制测试环境的配置，在本地读取和解析验证。

use shared_types::{MultiImageConfig, ServiceType};
use std::path::PathBuf;

/// 获取项目根目录
fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR 指向 crates/rcoder
    // 项目根目录是上两级目录
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent() // crates/
        .and_then(|p| p.parent()) // 项目根目录
        .expect("Could not find project root")
        .to_path_buf()
}

/// 测试从本地配置文件加载配置
#[test]
fn test_load_local_config_file() {
    let config_path = project_root().join("docker/config.yml");
    println!("Looking for config at: {:?}", config_path);
    assert!(
        config_path.exists(),
        "Config file not found at {:?}",
        config_path
    );

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

    // 验证服务数量
    assert_eq!(multi_config.services.len(), 2, "Expected 2 services");

    // 验证 web-agent-runner 配置
    let web_config = multi_config
        .get_service_config(&ServiceType::WebAgentRunner)
        .expect("web-agent-runner config not found");
    assert!(web_config.image.is_some(), "web-agent-runner image should be set");
    assert!(web_config.enabled, "web-agent-runner should be enabled");
    assert_eq!(web_config.service_type, ServiceType::WebAgentRunner);

    // 验证 computer-agent-runner 配置
    let computer_config = multi_config
        .get_service_config(&ServiceType::ComputerAgentRunner)
        .expect("computer-agent-runner config not found");
    assert!(computer_config.image.is_some(), "computer-agent-runner image should be set");
    assert!(computer_config.enabled, "computer-agent-runner should be enabled");
    assert_eq!(computer_config.service_type, ServiceType::ComputerAgentRunner);

    // 验证配置有效
    assert!(multi_config.validate().is_ok(), "Config validation failed");

    // 输出配置摘要
    println!("✅ Local config loaded successfully:");
    println!("  Services: {}", multi_config.services.len());
    println!("  Registry prefix: {:?}", multi_config.global_defaults.registry_prefix);
    for (key, svc) in &multi_config.services {
        println!("  - {}: service_type={}, image={:?}, enabled={}", key, svc.service_type, svc.image, svc.enabled);
    }
}

/// 测试服务名称兼容性
#[test]
fn test_service_name_compatibility() {
    // 测试服务名称是 "rcoder"，但 service_type 字段是 "WebAgentRunner" 的配置
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
    assert!(web_config.is_some(), "Should find config for WebAgentRunner");
    assert_eq!(
        web_config.unwrap().image,
        Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder:latest".to_string())
    );

    let computer_config = multi_config.get_service_config(&ServiceType::ComputerAgentRunner);
    assert!(computer_config.is_some(), "Should find config for ComputerAgentRunner");
    assert_eq!(
        computer_config.unwrap().image,
        Some("nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-test/rcoder-computer-agent-runner:latest".to_string())
    );

    // 验证配置有效
    assert!(multi_config.validate().is_ok());

    println!("✅ Service name compatibility test passed");
}

/// 测试 ServiceType 序列化/反序列化
#[test]
fn test_service_type_serde() {
    // 测试大驼峰格式
    let config1 = r#"{"service_type": "WebAgentRunner", "image": "test:latest"}"#;
    let parsed1: serde_json::Value = serde_json::from_str(config1).unwrap();
    let st1: ServiceType = serde_json::from_value(parsed1["service_type"].clone()).unwrap();
    assert_eq!(st1, ServiceType::WebAgentRunner);

    // 测试中划线格式
    let config2 = r#"{"service_type": "web-agent-runner", "image": "test:latest"}"#;
    let parsed2: serde_json::Value = serde_json::from_str(config2).unwrap();
    let st2: ServiceType = serde_json::from_value(parsed2["service_type"].clone()).unwrap();
    assert_eq!(st2, ServiceType::WebAgentRunner);

    // 测试旧枚举名
    let config3 = r#"{"service_type": "RCoder", "image": "test:latest"}"#;
    let parsed3: serde_json::Value = serde_json::from_str(config3).unwrap();
    let st3: ServiceType = serde_json::from_value(parsed3["service_type"].clone()).unwrap();
    assert_eq!(st3, ServiceType::WebAgentRunner);

    // 测试 ComputerAgentRunner
    let config4 = r#"{"service_type": "ComputerAgentRunner", "image": "test:latest"}"#;
    let parsed4: serde_json::Value = serde_json::from_str(config4).unwrap();
    let st4: ServiceType = serde_json::from_value(parsed4["service_type"].clone()).unwrap();
    assert_eq!(st4, ServiceType::ComputerAgentRunner);

    println!("✅ ServiceType serde test passed");
}
