//! Docker Agent 集成测试
//!
//! 测试 Docker Agent 与 RCoder 系统的集成功能

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::model::{AgentType, ChatPrompt, ChatPromptBuilder};
    use shared_types::ModelProviderConfig;

    #[tokio::test]
    #[ignore] // 需要 Docker 环境，手动运行
    async fn test_docker_agent_creation() {
        // 创建测试项目目录
        let project_id = "test_docker_agent";
        let workspace_dir = "./project_workspace";
        let project_path = format!("{}/{}", workspace_dir, project_id);

        // 确保项目目录存在
        tokio::fs::create_dir_all(&project_path).await.unwrap();

        // 创建 ChatPrompt
        let chat_prompt = ChatPromptBuilder::default()
            .project_id(project_id.to_string())
            .project_path(PathBuf::from(&project_path))
            .prompt("测试 Docker Agent 功能".to_string())
            .agent_type(AgentType::Docker)
            .model_provider(Some(ModelProviderConfig {
                name: "anthropic".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                api_key: "test_key".to_string(),
                default_model: "claude-3-5-sonnet-20241022".to_string(),
                requires_openai_auth: false,
            }))
            .build()
            .unwrap();

        // 验证 ChatPrompt 创建成功
        assert_eq!(chat_prompt.project_id, project_id);
        assert_eq!(chat_prompt.agent_type, AgentType::Docker);
        assert!(chat_prompt.model_provider.is_some());

        // 清理测试目录
        tokio::fs::remove_dir_all(&project_path).await.unwrap_or_default();
    }

    #[tokio::test]
    #[ignore] // 需要 Docker 环境，手动运行
    async fn test_docker_container_config() {
        let project_id = "test_config";
        let workspace_dir = "./project_workspace";

        // 创建 Docker 配置
        let config = docker_manager::DockerUtils::create_config_from_project_id(
            project_id,
            workspace_dir,
            Some("registry.yichamao.com/rcoder:latest".to_string()),
        );

        // 验证配置
        assert_eq!(config.project_id, project_id);
        assert_eq!(config.image, "registry.yichamao.com/rcoder:latest");
        assert!(config.host_path.contains(project_id));
        assert_eq!(config.container_path, "/app/workspace");
        assert_eq!(config.network_mode, "host");

        // 验证环境变量
        assert!(config.env_vars.contains_key("RUST_LOG"));
        assert!(config.env_vars.contains_key("TZ"));
    }

    #[tokio::test]
    async fn test_agent_type_selection() {
        // 测试通过环境变量选择 Docker Agent
        std::env::set_var("USE_DOCKER_AGENT", "true");

        let model_provider = shared_types::ModelProviderConfig {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "test_key".to_string(),
            default_model: "claude-3-5-sonnet-20241022".to_string(),
            requires_openai_auth: false,
        };

        let agent_type = AgentType::from_model_provider(Some(&model_provider));
        assert_eq!(agent_type, AgentType::Docker);

        // 清理环境变量
        std::env::remove_var("USE_DOCKER_AGENT");
    }

    #[tokio::test]
    async fn test_agent_type_name() {
        assert_eq!(AgentType::Claude.agent_type_name(), "claude");
        assert_eq!(AgentType::Codex.agent_type_name(), "codex");
        assert_eq!(AgentType::Docker.agent_type_name(), "docker");
    }

    #[tokio::test]
    async fn test_chat_prompt_with_model_provider() {
        let model_provider = ModelProviderConfig {
            name: "test_provider".to_string(),
            base_url: "https://api.test.com".to_string(),
            api_key: "test_api_key".to_string(),
            default_model: "test-model".to_string(),
            requires_openai_auth: false,
        };

        let chat_prompt = ChatPromptBuilder::default()
            .project_id("test_project".to_string())
            .project_path(PathBuf::from("./test_project"))
            .prompt("Test prompt".to_string())
            .agent_type(AgentType::Docker)
            .model_provider(Some(model_provider.clone()))
            .build()
            .unwrap();

        assert_eq!(chat_prompt.project_id, "test_project");
        assert_eq!(chat_prompt.agent_type, AgentType::Docker);
        assert!(chat_prompt.model_provider.is_some());

        let retrieved_config = chat_prompt.model_provider.unwrap();
        assert_eq!(retrieved_config.name, model_provider.name);
        assert_eq!(retrieved_config.api_key, model_provider.api_key);
    }

    mod docker_integration {
        use super::*;
        use docker_manager::{DockerManager, DockerManagerConfig};

        #[tokio::test]
        #[ignore] // 需要 Docker 环境，手动运行
        async fn test_docker_manager_lifecycle() {
            // 创建 Docker 管理器
            let config = DockerManagerConfig::default();
            let docker_manager = DockerManager::new(config).await.unwrap();

            // 测试项目配置
            let project_id = "test_lifecycle";
            let workspace_dir = "./project_workspace";

            // 创建测试目录
            let project_path = format!("{}/{}", workspace_dir, project_id);
            tokio::fs::create_dir_all(&project_path).await.unwrap();

            // 创建容器配置
            let container_config = docker_manager::DockerUtils::create_config_from_project_id(
                project_id,
                workspace_dir,
                None, // 使用默认镜像
            );

            // 创建容器
            let container_info = docker_manager.create_container(container_config).await.unwrap();
            assert!(!container_info.container_id.is_empty());
            assert_eq!(container_info.project_id, project_id);
            assert_eq!(container_info.status, docker_manager::ContainerStatus::Running);

            // 等待容器启动
            tokio::time::sleep(Duration::from_secs(3)).await;

            // 检查容器状态
            let status = docker_manager.update_container_status(project_id).await.unwrap();
            assert!(status.is_some());

            // 获取容器信息
            let retrieved_info = docker_manager.get_container_info(project_id).unwrap();
            assert_eq!(retrieved_info.container_id, container_info.container_id);

            // 停止容器
            docker_manager.stop_container(project_id).await.unwrap();

            // 验证容器已移除
            let info_after_stop = docker_manager.get_container_info(project_id);
            assert!(info_after_stop.is_none());

            // 清理测试目录
            tokio::fs::remove_dir_all(&project_path).await.unwrap_or_default();
        }
    }

    mod performance_tests {
        use super::*;
        use std::time::Instant;

        #[tokio::test]
        #[ignore] // 性能测试，手动运行
        async fn test_multiple_container_creation() {
            let config = DockerManagerConfig::default();
            let docker_manager = DockerManager::new(config).await.unwrap();

            let start_time = Instant::now();
            let num_containers = 5;

            for i in 0..num_containers {
                let project_id = format!("perf_test_{}", i);
                let workspace_dir = "./project_workspace";

                // 创建项目目录
                let project_path = format!("{}/{}", workspace_dir, project_id);
                tokio::fs::create_dir_all(&project_path).await.unwrap();

                // 创建容器
                let container_config = docker_manager::DockerUtils::create_config_from_project_id(
                    &project_id,
                    workspace_dir,
                    None,
                );

                let container_info = docker_manager.create_container(container_config).await.unwrap();
                assert_eq!(container_info.project_id, project_id);
            }

            let creation_time = start_time.elapsed();
            println!("创建 {} 个容器耗时: {:?}", num_containers, creation_time);

            // 清理所有容器
            for i in 0..num_containers {
                let project_id = format!("perf_test_{}", i);
                docker_manager.stop_container(&project_id).await.unwrap();

                // 清理项目目录
                let project_path = format!("./project_workspace/{}", project_id);
                tokio::fs::remove_dir_all(&project_path).await.unwrap_or_default();
            }

            // 验证平均创建时间合理（例如每个容器不超过10秒）
            let avg_time_per_container = creation_time / num_containers;
            assert!(avg_time_per_container < Duration::from_secs(10));
        }
    }
}