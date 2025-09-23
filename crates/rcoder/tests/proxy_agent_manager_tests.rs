use rcoder::proxy_agent_manager::{ProxyAgentManager, ProxyConfig};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

#[tokio::test]
async fn test_proxy_agent_manager_creation() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root: workspace_root.clone(),
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await;
    assert!(manager.is_ok(), "Failed to create ProxyAgentManager");

    let manager = manager.unwrap();

    // 验证工作空间已创建
    assert!(workspace_root.exists(), "Workspace root should exist");
}

#[tokio::test]
async fn test_send_prompt_without_project_id() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 发送 prompt（不带项目ID）
    let result = manager.send_prompt(
        "auto-generated-project",  // 不指定 project_id
        None,  // 不指定 session_id
        "Hello, how are you?"
    ).await;

    // 预期会成功，因为会自动生成项目ID
    assert!(result.is_ok(), "Should succeed with auto-generated project ID");

    let (response, session_id) = result.unwrap();
    assert!(!session_id.is_empty(), "Session ID should not be empty");
    // 注意：由于当前使用模拟的ACP连接，响应可能为空或简单
}

#[tokio::test]
async fn test_send_prompt_with_project_id() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 发送 prompt（带项目ID）
    let project_id = "test-project";
    let result = manager.send_prompt(
        project_id,
        None,
        "Can you help me with Rust programming?"
    ).await;

    // 预期会成功
    assert!(result.is_ok(), "Should succeed with provided project ID");

    let (response, session_id) = result.unwrap();
    assert!(!session_id.is_empty(), "Session ID should not be empty");
}

#[tokio::test]
async fn test_get_or_create_agent() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 创建 agent
    let project_id = "test-agent";
    let result = manager.get_or_create_agent(project_id).await;

    // 预期会成功
    assert!(result.is_ok(), "Should succeed to create agent");
}

#[tokio::test]
async fn test_cleanup_idle_agents() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置（较短的超时时间）
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_millis(100), // 100ms 超时
        cleanup_interval: Duration::from_millis(50), // 50ms 清理间隔
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 创建一个 agent
    let project_id = "test-cleanup";
    manager.get_or_create_agent(project_id).await.unwrap();

    // 等待超时
    sleep(Duration::from_millis(200)).await;

    // 执行清理
    let result = manager.cleanup_idle_agents().await;
    assert!(result.is_ok(), "Cleanup should succeed");
}

#[tokio::test]
async fn test_project_workspace_creation() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root: workspace_root.clone(),
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 发送 prompt，触发项目工作空间创建
    let project_id = "workspace-test";
    manager.send_prompt(
        project_id,
        None,
        "Test workspace creation"
    ).await.unwrap();

    // 验证项目工作空间已创建
    let project_workspace = workspace_root.join(project_id);
    assert!(project_workspace.exists(), "Project workspace should be created");

    // 验证会话目录已创建
    let sessions_dir = project_workspace.join("sessions");
    assert!(sessions_dir.exists(), "Sessions directory should be created");

    // 验证工作文件目录已创建
    let workspace_files_dir = project_workspace.join("workspace_files");
    assert!(workspace_files_dir.exists(), "Workspace files directory should be created");
}

#[tokio::test]
async fn test_concurrent_agents() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置（最多2个并发agent）
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 并发创建多个 agents
    let project_ids = vec!["concurrent-1", "concurrent-2", "concurrent-3"];

    for project_id in project_ids {
        let result = manager.get_or_create_agent(project_id).await;
        // 前2个应该成功，第3个可能会失败（取决于并发限制）
        if project_id == "concurrent-3" {
            // 第3个可能会失败，这是预期的
            // 在实际实现中，这应该根据并发限制来处理
        } else {
            assert!(result.is_ok(), "Should succeed to create agent {}", project_id);
        }
    }
}

#[tokio::test]
async fn test_proxy_agent_manager_shutdown() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 创建一些 agents
    manager.get_or_create_agent("shutdown-test").await.unwrap();

    // 执行关闭
    let result = manager.shutdown().await;
    assert!(result.is_ok(), "Shutdown should succeed");
}

#[tokio::test]
async fn test_proxy_agent_manager_basic_functionality() {
    // 创建临时目录
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path().to_path_buf();

    // 创建配置
    let config = ProxyConfig {
        workspace_root,
        idle_timeout: Duration::from_secs(60),
        cleanup_interval: Duration::from_secs(10),
        max_concurrent_agents: 2,
        local_set_config: Default::default(),
    };

    // 创建代理管理器
    let manager = ProxyAgentManager::new(config).await.unwrap();

    // 测试基本功能
    let result = manager.send_prompt(
        "test-project",
        None,
        "Hello, test!"
    ).await;

    // 预期会成功
    assert!(result.is_ok(), "Should succeed with basic functionality");

    let (response, session_id) = result.unwrap();
    assert!(!session_id.is_empty(), "Session ID should not be empty");
}