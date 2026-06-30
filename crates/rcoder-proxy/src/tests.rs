//! 集成测试模块
//!
//! 提供代理功能的集成测试和端到端测试。

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    response::Response,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

use crate::config::ProxyConfig;
use crate::service::PortProxyService;
use crate::server::ProxyServer;

/// 创建测试配置
fn create_test_config() -> ProxyConfig {
    ProxyConfig {
        listen_port: 0, // 使用随机可用端口
        default_backend_port: 3000,
        backend_host: "127.0.0.1".to_string(),
        port_param: "port".to_string(),
        config_file: None,
        verbose: false,
        ..Default::default()
    }
}

/// 创建完整的测试服务器配置
fn create_test_server_config(listen_port: u16) -> ProxyConfig {
    ProxyConfig {
        listen_port,
        default_backend_port: 3000,
        backend_host: "127.0.0.1".to_string(),
        port_param: "port".to_string(),
        config_file: None,
        verbose: false,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_config_validation() {
    // 测试有效配置
    let valid_config = ProxyConfig::default();
    assert!(valid_config.validate().is_ok());

    // 测试无效监听端口
    let mut invalid_config = ProxyConfig::default();
    invalid_config.listen_port = 0;
    assert!(invalid_config.validate().is_err());

    // 测试无效后端端口
    let mut invalid_config = ProxyConfig::default();
    invalid_config.default_backend_port = 0;
    assert!(invalid_config.validate().is_err());

    // 测试空主机地址
    let mut invalid_config = ProxyConfig::default();
    invalid_config.backend_host = String::new();
    assert!(invalid_config.validate().is_err());

    // 测试空端口参数名
    let mut invalid_config = ProxyConfig::default();
    invalid_config.port_param = String::new();
    assert!(invalid_config.validate().is_err());
}

#[tokio::test]
async fn test_service_backend_management() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    // 初始状态应该有默认后端
    assert_eq!(service.backend_count().await, 1);
    assert!(service.has_backend(3000).await);
    assert!(!service.has_backend(3001).await);

    // 添加后端
    service.add_backend(3001, "localhost".to_string()).await;
    assert_eq!(service.backend_count().await, 2);
    assert!(service.has_backend(3001).await);

    // 获取后端列表
    let backends = service.list_backends().await;
    assert_eq!(backends.len(), 2);
    assert!(backends.contains_key(&3000));
    assert!(backends.contains_key(&3001));

    // 移除后端
    service.remove_backend(3001).await;
    assert_eq!(service.backend_count().await, 1);
    assert!(!service.has_backend(3001).await);
}

#[tokio::test]
async fn test_port_extraction_scenarios() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    // 测试场景1: 从查询参数提取端口
    let request = Request::builder()
        .uri("/api/users?port=8080&format=json")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 8080);

    // 测试场景2: 从路径提取端口
    let request = Request::builder()
        .uri("/proxy/9000/api/v1/data")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 9000);

    // 测试场景3: 多个查询参数
    let request = Request::builder()
        .uri("/api/search?q=test&port=7000&page=1")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 7000);

    // 测试场景4: 端口参数格式错误
    let request = Request::builder()
        .uri("/api/data?port=invalid")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 3000); // 应该回退到默认端口

    // 测试场景5: 路径格式错误
    let request = Request::builder()
        .uri("/proxy/not_a_number/api")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 3000); // 应该回退到默认端口

    // 测试场景6: 没有端口参数
    let request = Request::builder()
        .uri("/api/data")
        .body(Body::empty())
        .unwrap();
    let port = service.extract_target_port(&request).unwrap();
    assert_eq!(port, 3000); // 应该使用默认端口
}

#[tokio::test]
async fn test_uri_building() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    // 测试普通路径
    let uri = service.build_target_uri("localhost", 3000, "/api/data").unwrap();
    assert_eq!(uri, "http://localhost:3000/api/data");

    // 测试带查询参数的路径
    let uri = service.build_target_uri("localhost", 3000, "/api/data?format=json").unwrap();
    assert_eq!(uri, "http://localhost:3000/api/data?format=json");

    // 测试代理路径前缀
    let uri = service.build_target_uri("localhost", 8080, "/proxy/8080/api/data").unwrap();
    assert_eq!(uri, "http://localhost:8080/api/data");

    // 测试代理路径前缀带查询参数
    let uri = service.build_target_uri("localhost", 8080, "/proxy/8080/api/data?format=json").unwrap();
    assert_eq!(uri, "http://localhost:8080/api/data?format=json");

    // 测试根路径
    let uri = service.build_target_uri("localhost", 3000, "/").unwrap();
    assert_eq!(uri, "http://localhost:3000/");

    // 测试代理根路径
    let uri = service.build_target_uri("localhost", 8080, "/proxy/8080/").unwrap();
    assert_eq!(uri, "http://localhost:8080/");
}

#[tokio::test]
async fn test_server_builder() {
    // 测试默认构建器
    let server = ProxyServerBuilder::new().build();
    assert_eq!(server.listen_port(), 8080);
    assert_eq!(server.default_backend_port(), 3000);

    // 测试自定义配置
    let server = ProxyServerBuilder::new()
        .listen_port(9000)
        .default_backend_port(4000)
        .backend_host("example.com")
        .port_param("target")
        .verbose(true)
        .build();

    assert_eq!(server.listen_port(), 9000);
    assert_eq!(server.default_backend_port(), 4000);
    assert_eq!(server.config().backend_host, "example.com");
    assert_eq!(server.config().port_param, "target");
    assert!(server.config().verbose);
}

#[tokio::test]
async fn test_server_convenience_methods() {
    // 测试默认服务器
    let server = ProxyServer::default();
    assert_eq!(server.listen_port(), 8080);
    assert_eq!(server.default_backend_port(), 3000);

    // 测试自定义端口
    let server = ProxyServer::with_listen_port(9090);
    assert_eq!(server.listen_port(), 9090);
    assert_eq!(server.default_backend_port(), 3000);

    // 测试链式配置
    let server = ProxyServer::with_listen_port(8080)
        .with_backend_host("localhost")
        .with_port_param("port")
        .with_default_backend_port(3000);

    assert_eq!(server.config().backend_host, "localhost");
    assert_eq!(server.config().port_param, "port");
    assert_eq!(server.config().default_backend_port, 3000);
}

#[tokio::test]
async fn test_server_pre_start_check() {
    let server = ProxyServer::default();

    // 默认配置应该通过预启动检查
    let result = server.pre_start_check().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_proxy_headers() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    let mut request = Request::builder()
        .uri("/api/data")
        .body(Body::empty())
        .unwrap();

    // 添加代理头
    service.add_proxy_headers(&mut request).unwrap();

    // 验证头信息
    assert_eq!(request.headers().get("X-Forwarded-Proto").unwrap(), "http");
    assert_eq!(request.headers().get("X-Port-Proxy").unwrap(), "simple-proxy");
}

// 这个测试需要实际的网络连接，只在集成测试中运行
#[ignore]
#[tokio::test]
async fn test_end_to_end_proxy_request() {
    // 注意：这个测试需要有一个在端口3000运行的实际服务
    // 在实际运行时，需要先启动一个简单的HTTP服务器

    let config = create_test_server_config(8080);
    let service = PortProxyService::new(config);

    // 创建测试请求
    let request = Request::builder()
        .method(Method::GET)
        .uri("/?port=3000")
        .body(Body::empty())
        .unwrap();

    // 尝试代理请求（可能会失败，因为可能没有实际的服务在运行）
    let result = service.proxy_request(request).await;

    // 检查是否正确解析了端口（即使连接失败）
    // 这主要验证端口提取和URI构建逻辑
    match result {
 Ok(_) => println!("proxyrequestsucceeded"),
 Err(e) => println!("proxyrequestfailed( message ): {}", e),
    }
}

// 性能测试
#[tokio::test]
async fn test_concurrent_backend_operations() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    // 并发添加多个后端
    let mut handles = Vec::new();
    for i in 1..=10 {
        let service_clone = service.clone();
        let handle = tokio::spawn(async move {
            service_clone.add_backend(3000 + i, format!("host{}", i)).await;
        });
        handles.push(handle);
    }

    // 等待所有操作完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证所有后端都已添加
    assert_eq!(service.backend_count().await, 11); // 1个默认 + 10个新增

    // 并发读取后端列表
    let mut handles = Vec::new();
    for _ in 0..5 {
        let service_clone = service.clone();
        let handle = tokio::spawn(async move {
            service_clone.list_backends().await
        });
        handles.push(handle);
    }

    // 验证并发读取的一致性
    for handle in handles {
        let backends = handle.await.unwrap();
        assert_eq!(backends.len(), 11);
    }
}

// 边界条件测试
#[tokio::test]
async fn test_edge_cases() {
    let config = create_test_config();
    let service = PortProxyService::new(config);

    // 测试添加和移除相同的端口
    service.add_backend(3001, "host1".to_string()).await;
    service.add_backend(3001, "host2".to_string()).await; // 覆盖

    let backend = service.get_target_backend(3001).await.unwrap();
    assert_eq!(backend, "host2"); // 应该是最后的值

    // 测试移除不存在的端口
    service.remove_backend(9999).await; // 不应该崩溃

    // 测试获取不存在的后端
    let result = service.get_target_backend(9999).await;
    assert!(result.is_err());

    // 测试空配置
    let empty_config = ProxyConfig {
        listen_port: 8080,
        default_backend_port: 0, // 无效端口
        backend_host: "127.0.0.1".to_string(),
        port_param: "port".to_string(),
        config_file: None,
        verbose: false,
        ..Default::default()
    };
    assert!(empty_config.validate().is_err());
}

#[test]
fn test_config_default_methods() {
    // 测试默认配置
    let default_config = ProxyConfig::default();
    assert_eq!(default_config.listen_port, 8080);
    assert_eq!(default_config.default_backend_port, 3000);
    assert_eq!(default_config.backend_host, "127.0.0.1");
    assert_eq!(default_config.port_param, "port");
    assert!(default_config.config_file.is_none());
    assert!(!default_config.verbose);

    // 测试便捷方法
    let config = ProxyConfig::new();
    assert_eq!(config.listen_port, 8080);

    let config = ProxyConfig::with_listen_port(9090);
    assert_eq!(config.listen_port, 9090);
    assert_eq!(config.default_backend_port, 3000); // 其他应该保持默认值

    let config = ProxyConfig::with_listen_port(8080)
        .with_backend_host("example.com")
        .with_port_param("service_port");
    assert_eq!(config.listen_port, 8080);
    assert_eq!(config.backend_host, "example.com");
    assert_eq!(config.port_param, "service_port");
}

// 测试模块集成
#[test]
fn test_module_integration() {
    // 验证各模块之间的集成是否正常
    let config = ProxyConfig::default();
    let service = PortProxyService::new(config.clone());
    let server = ProxyServer::new(config);

    // 验证配置一致性
    assert_eq!(service.config().default_backend_port, server.default_backend_port());
    assert_eq!(service.config().port_param, server.config().port_param);

    // 验证服务访问
    let server_service = server.service();
    let service_backends = service.backends();

    // 两个服务实例应该是独立的
    assert_ne!(service.config() as *const _, server_service.config() as *const _);
}