//! 测试 Fixtures
//!
//! 提供测试用的构造器和辅助工具

use crate::service::AgentRequest;
use agent_abstraction::PromptMessage;
use shared_types::{Attachment, ServiceType};
use std::path::PathBuf;

/// 测试请求构造器
///
/// # Example
///
/// ```rust
/// use agent_runner::testing::fixtures::TestRequestBuilder;
///
/// // 创建基础测试请求
/// let (req, resp_rx) = TestRequestBuilder::new()
///     .project_id("test-project")
///     .content("Hello, Agent!")
///     .build();
/// ```
pub struct TestRequestBuilder {
    project_id: String,
    content: String,
    request_id: String,
    attachments: Vec<Attachment>,
    session_id: Option<String>,
    project_path: Option<PathBuf>,
}

impl Default for TestRequestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRequestBuilder {
    /// 创建新的测试请求构造器
    pub fn new() -> Self {
        Self {
            project_id: "test-project".to_string(),
            content: "Hello, Agent!".to_string(),
            request_id: uuid::Uuid::new_v4().to_string().replace("-", ""),
            attachments: vec![],
            session_id: None,
            project_path: None,
        }
    }

    /// 设置 project_id
    pub fn project_id(mut self, id: &str) -> Self {
        self.project_id = id.to_string();
        self
    }

    /// 设置请求内容
    pub fn content(mut self, content: &str) -> Self {
        self.content = content.to_string();
        self
    }

    /// 设置 request_id
    pub fn request_id(mut self, request_id: &str) -> Self {
        self.request_id = request_id.to_string();
        self
    }

    /// 设置 session_id
    pub fn session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    /// 设置项目路径
    pub fn project_path(mut self, path: &str) -> Self {
        self.project_path = Some(PathBuf::from(path));
        self
    }

    /// 添加附件
    pub fn add_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// 构建 PromptMessage（用于测试验证）
    ///
    /// 返回构建的 PromptMessage，可以直接访问字段进行断言验证
    pub fn build_prompt_message(&self) -> PromptMessage {
        PromptMessage {
            content: self.content.clone(),
            project_id: self.project_id.clone(),
            project_path: self
                .project_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp/test")),
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            attachments: self.attachments.clone(),
            data_source_attachments: vec![],
            service_type: ServiceType::RCoder,
            user_id: None,
            system_prompt_override: None,
            user_prompt_template_override: None,
            agent_config_override: None,
        }
    }

    /// 构建测试请求
    pub fn build(self) -> AgentRequest {
        let prompt_message = self.build_prompt_message();
        AgentRequest::new(prompt_message, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_request_builder_default() {
        let builder = TestRequestBuilder::new();
        let prompt = builder.build_prompt_message();

        // 验证默认值
        assert_eq!(prompt.project_id, "test-project");
        assert_eq!(prompt.content, "Hello, Agent!");
        assert!(!prompt.request_id.is_empty(), "request_id 应该自动生成");
        assert_eq!(prompt.project_path, PathBuf::from("/tmp/test"));
        assert!(prompt.session_id.is_none());
        assert!(prompt.attachments.is_empty());
        assert_eq!(prompt.service_type, ServiceType::RCoder);
    }

    #[test]
    fn test_test_request_builder_with_project_id() {
        let prompt = TestRequestBuilder::new()
            .project_id("custom-project")
            .build_prompt_message();

        assert_eq!(prompt.project_id, "custom-project");
    }

    #[test]
    fn test_test_request_builder_with_content() {
        let prompt = TestRequestBuilder::new()
            .content("Custom content")
            .build_prompt_message();

        assert_eq!(prompt.content, "Custom content");
    }

    #[test]
    fn test_test_request_builder_with_session_id() {
        let prompt = TestRequestBuilder::new()
            .session_id("test-session-123")
            .build_prompt_message();

        assert_eq!(prompt.session_id, Some("test-session-123".to_string()));
    }

    #[test]
    fn test_test_request_builder_with_request_id() {
        let prompt = TestRequestBuilder::new()
            .request_id("custom-request-id")
            .build_prompt_message();

        assert_eq!(prompt.request_id, "custom-request-id");
    }

    #[test]
    fn test_test_request_builder_with_project_path() {
        let prompt = TestRequestBuilder::new()
            .project_path("/home/user/project")
            .build_prompt_message();

        assert_eq!(prompt.project_path, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_test_request_builder_chain() {
        // 验证链式调用和所有字段设置
        let prompt = TestRequestBuilder::new()
            .project_id("my-project")
            .content("Test content")
            .request_id("custom-request-id")
            .session_id("session-456")
            .project_path("/home/user/project")
            .build_prompt_message();

        assert_eq!(prompt.project_id, "my-project");
        assert_eq!(prompt.content, "Test content");
        assert_eq!(prompt.request_id, "custom-request-id");
        assert_eq!(prompt.session_id, Some("session-456".to_string()));
        assert_eq!(prompt.project_path, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_test_request_builder_build_returns_agent_request() {
        let request = TestRequestBuilder::new().build();
        assert_eq!(request.prompt_message.project_id, "test-project");
        assert_eq!(request.prompt_message.content, "Hello, Agent!");
    }

    #[test]
    fn test_test_request_builder_request_id_auto_generated() {
        // 验证每次构建生成不同的 request_id
        let builder1 = TestRequestBuilder::new();
        let builder2 = TestRequestBuilder::new();

        let prompt1 = builder1.build_prompt_message();
        let prompt2 = builder2.build_prompt_message();

        assert_ne!(
            prompt1.request_id, prompt2.request_id,
            "每次构建应该生成不同的 request_id"
        );
    }
}
