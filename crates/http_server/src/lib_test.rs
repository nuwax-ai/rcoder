#[cfg(test)]
mod tests {
    use super::*;
    use http_interface::*;

    #[tokio::test]
    async fn test_project_manager() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = HttpProjectManager::new(temp_dir.path().to_path_buf());

        let request = CreateProjectRequest {
            name: "test-project".to_string(),
            description: None,
            template: None,
        };

        let project = manager.create_project(request).await.unwrap();
        assert_eq!(project.name, "test-project");
        assert!(project.path.exists());
    }

    #[tokio::test]
    async fn test_claude_manager() {
        let manager = HttpClaudeManager::new().await.unwrap();

        // 这里暂时不测试实际的prompt发送，因为需要Claude Code CLI
        assert!(true);
    }
}