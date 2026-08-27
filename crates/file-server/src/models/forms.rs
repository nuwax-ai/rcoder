//! OpenAPI-only multipart 占位 schema：仅出现在 `#[utoipa::path]` 的
//! `request_body(content = ...)` 文档声明里，运行时 multipart 字段由 handler
//! 手工提取（garde Fields 结构校验），这些 Form 类型从不实例化。

use utoipa::ToSchema;

use super::commons::BinaryFile;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadSingleFileForm {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 代码版本号
    pub code_version: String,
    /// 上传目标相对路径
    pub file_path: String,
    /// 上传文件（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadBatchFilesForm {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 代码版本号
    pub code_version: String,
    /// 每个文件的目标相对路径（与 files 一一对应，重复字段）
    pub file_paths: Vec<String>,
    /// 上传文件列表（multipart 重复字段）
    pub files: Vec<BinaryFile>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadAttachmentForm {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 原始文件名（可选，缺省取上传文件名）
    pub file_name: Option<String>,
    /// 上传文件（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadProjectForm {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 代码版本号
    pub code_version: String,
    /// 项目 zip（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
    /// 关联进程 PID（可选，对齐 nuwax 字段位）
    pub pid: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushProjectSkillsForm {
    /// 项目 ID（workspace 根目录名）
    pub project_id: String,
    /// 技能 zip（可选，与 skillUrls 二选一）
    #[schema(format = Binary)]
    pub file: Option<String>,
    /// 技能 zip 的 URL 列表（JSON 数组或单值）
    pub skill_urls: Option<Vec<String>>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectForm {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 目标根目录覆盖（可选）
    pub custom_target_dir: Option<String>,
    /// 项目 zip（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileForm {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 上传目标相对路径
    pub file_path: String,
    /// 自定义目标目录（可选；缺省用 user/cid 推导的默认根）
    pub custom_target_dir: Option<String>,
    /// 上传文件（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesForm {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 自定义目标目录（可选；缺省用 user/cid 推导的默认根）
    pub custom_target_dir: Option<String>,
    /// 每个文件的目标相对路径（与 files 一一对应，重复字段）
    pub file_paths: Vec<String>,
    /// 上传文件列表（multipart 重复字段）
    pub files: Vec<BinaryFile>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceForm {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 工作区模板 zip（可选）
    #[schema(format = Binary)]
    pub file: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceV2Form {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 工作区模板 zip（可选）
    #[schema(format = Binary)]
    pub file: Option<String>,
    /// 技能 zip 的 URL 列表（JSON 数组或单值）
    pub skill_urls: Option<Vec<String>>,
    /// MCP servers 配置（JSON 串，透传 agent 装配）
    pub mcp_servers_config: Option<String>,
    /// Hooks 配置（JSON 串，透传 agent 装配）
    pub hooks_config: Option<String>,
    /// Permissions 配置（JSON 串，透传 agent 装配）
    pub permissions_config: Option<String>,
    /// Hook 脚本内容（透传 agent 装配）
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
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 模板 zip（multipart 二进制字段）
    #[schema(format = Binary)]
    pub file: String,
    /// 是否 git init（双开关：GIT_ENABLED 且为 true 才执行）
    pub enable_git: Option<bool>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushSkillsForm {
    /// 用户 ID（computer 树第一级 `{root}/{user_id}/{cId}`）
    pub user_id: String,
    /// 容器/实例 ID（computer 树第二级，Electron 全局根语义）
    pub c_id: String,
    /// 技能 zip（可选，与 skillUrls 二选一）
    #[schema(format = Binary)]
    pub file: Option<String>,
    /// 技能 zip 的 URL 列表（JSON 数组或单值）
    pub skill_urls: Option<Vec<String>>,
    /// 智能体 ID (有则可能走实体存储; 须同时满足会话已是软链)
    pub agent_id: Option<String>,
}
