//! OpenAPI-only multipart 占位 schema：仅出现在 `#[utoipa::path]` 的
//! `request_body(content = ...)` 文档声明里，运行时 multipart 字段由 handler
//! 手工提取（garde Fields 结构校验），这些 Form 类型从不实例化。

use utoipa::ToSchema;

use super::commons::BinaryFile;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadSingleFileForm {
    pub project_id: String,
    pub code_version: String,
    pub file_path: String,
    #[schema(format = Binary)]
    pub file: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadBatchFilesForm {
    pub project_id: String,
    pub code_version: String,
    pub file_paths: Vec<String>,
    pub files: Vec<BinaryFile>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadAttachmentForm {
    pub project_id: String,
    pub file_name: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadProjectForm {
    pub project_id: String,
    pub code_version: String,
    #[schema(format = Binary)]
    pub file: String,
    pub pid: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushProjectSkillsForm {
    pub project_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileForm {
    pub user_id: String,
    pub c_id: String,
    pub file_path: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    pub file_paths: Vec<String>,
    pub files: Vec<BinaryFile>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceForm {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceV2Form {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    pub mcp_servers_config: Option<String>,
    pub hooks_config: Option<String>,
    pub permissions_config: Option<String>,
    pub hook_scripts: Option<String>,
    /// 智能体 ID (非空时走实体存储 + 软链)
    pub agent_id: Option<String>,
    /// 技能名 → URL 映射 (JSON; 下载前可跳过已存在的)
    pub skill_url_map: Option<String>,
    /// 配置技能名全集 (JSON; 用于差集删除)
    pub skill_names: Option<Vec<String>>,
    /// 强制更新的技能名 (JSON; 传入时按需安装)
    pub update_skill_names: Option<Vec<String>>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitProjectTemplateForm {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: String,
    pub enable_git: Option<bool>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushSkillsForm {
    pub user_id: String,
    pub c_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    /// 智能体 ID (有则可能走实体存储; 须同时满足会话已是软链)
    pub agent_id: Option<String>,
}
