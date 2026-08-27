//! git 域请求体与 Query 参数（对齐 nuwax gitRoutes）。
//!
//! 写操作 body 经 `#[serde(flatten)]` 复用 `GitWriteBody` 基类（serde flatten
//! 与 deserialize_with 的交互是已知坑区，见 handlers/git/write.rs 测试）。
//! 字段为 `pub`（models 是 crate 内公共层）。

use garde::Validate;
use serde::Deserialize;

/// GET 路由公共查询 (workspaceType + project/computer 标识 + 多租户)。
#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct GitQuery {
    /// 工作区类型: `project`(项目工作区) / `computer`(Electron 容器根) 二选一
    pub workspace_type: Option<String>,
    /// 项目 ID（workspaceType=project 时必填）
    pub project_id: Option<String>,
    /// 用户 ID（workspaceType=computer 时必填）
    pub user_id: Option<String>,
    /// 容器/实例 ID（workspaceType=computer 时必填）
    pub c_id: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// POST 路由公共 body (写操作基类, 被 FilesBody / CommitBody 等经 serde flatten 复用)。
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitWriteBody {
    pub workspace_type: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub project_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub user_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub c_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileContentBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref", default)]
    pub ref_: Option<String>,
    pub file_path: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommitBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub message: String,
    #[serde(default)]
    #[garde(skip)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiffBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub mode: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevertBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub branch_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub start_point: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BranchNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub branch_name: String,
    /// branch-delete 强制删除未合并分支 (对齐 nuwax deleteBranch force)。
    #[serde(default)]
    #[garde(skip)]
    pub force: Option<bool>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub tag_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub tag_name: String,
}

/// git 提交历史查询（无 utoipa 注解：path 参数在 handler 注解里逐项声明，
/// 见 openapi.rs 测试 flattened_git_log_query_is_exposed_as_individual_parameters）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitLogQuery {
    #[serde(flatten)]
    pub base: GitQuery,
    pub max_count: Option<usize>,
    pub skip: Option<usize>,
    /// 指定分支 (对齐 nuwax git.log ref); 默认 HEAD。
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
}
