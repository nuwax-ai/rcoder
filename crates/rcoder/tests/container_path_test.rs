#[cfg(test)]
mod tests {
    use shared_types::service_config::default_rcoder_service_config;
    use std::collections::HashMap;
    #[test]
    fn test_default_container_path_template() {
        let config = default_rcoder_service_config();
        assert_eq!(
            config.container_path_template,
            "/app/project_workspace/{project_id}"
        );
    }

    #[test]
    fn test_resolve_container_path_with_project_id() {
        let config = default_rcoder_service_config();

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-123".to_string());
        variables.insert("service_type".to_string(), "rcoder".to_string());

        let resolved = config.resolve_container_path(&variables);
        assert_eq!(resolved, "/app/project_workspace/test-project-123");
    }

    #[test]
    fn test_resolve_container_path_with_service_type() {
        let config = default_rcoder_service_config();

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-123".to_string());
        variables.insert("service_type".to_string(), "agent-runner".to_string());

        let resolved = config.resolve_container_path(&variables);
        // 默认模板不包含 {service_type} 变量，所以应该保持不变
        assert_eq!(resolved, "/app/project_workspace/test-project-123");
    }

    #[test]
    fn test_custom_container_path_template() {
        let mut config = default_rcoder_service_config();
        config.container_path_template = "/app/workspace/{project_id}".to_string();

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-456".to_string());

        let resolved = config.resolve_container_path(&variables);
        assert_eq!(resolved, "/app/workspace/test-project-456");
    }

    #[test]
    fn test_user_workspace_path_template() {
        let mut config = default_rcoder_service_config();
        config.container_path_template =
            "/app/user_workspace/{user_id}/projects/{project_id}".to_string();

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-789".to_string());
        variables.insert("user_id".to_string(), "user-123".to_string());

        let resolved = config.resolve_container_path(&variables);
        assert_eq!(
            resolved,
            "/app/user_workspace/user-123/projects/test-project-789"
        );
    }

    #[test]
    fn test_empty_variables() {
        let config = default_rcoder_service_config();

        let empty_variables = HashMap::new();
        let resolved = config.resolve_container_path(&empty_variables);

        // 即使没有变量，模板应该保持不变
        assert_eq!(resolved, "/app/project_workspace/{project_id}");
    }

    #[test]
    fn test_multiple_variables() {
        let mut config = default_rcoder_service_config();
        config.container_path_template =
            "/data/{service_type}/{project_id}/{workspace_dir}".to_string();

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project".to_string());
        variables.insert("service_type".to_string(), "agent".to_string());
        variables.insert("workspace_dir".to_string(), "workspace".to_string());

        let resolved = config.resolve_container_path(&variables);
        assert_eq!(resolved, "/data/agent/test-project/workspace");
    }
}
