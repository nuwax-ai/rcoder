//! app-files 域请求结构（rcoder `app_files` 转发链的容器侧内部契约，
//! handler 壳在 handlers/userapp_app_files.rs）。
//!
//! 字段为 `pub`（models 是 crate 内公共层）；user_id 均为可选审计字段
//! （rcoder 转发链不携带）。

use serde::Deserialize;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct AppFilesUploadForm {
    /// UserApp 应用 ID（定位 = resolve_userapp_dev；单 app 模式须与归属一致）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅日志审计，不参与容器内定位）
    pub user_id: String,
    /// app 根相对目标（压缩包=解压目录；单文件=文件路径）
    pub target: String,
    /// 压缩包解压后单层归一（默认 false）
    pub flatten: Option<bool>,
    #[schema(format = Binary)]
    /// 上传内容（zip / tar.gz / 单文件，魔数自动识别）
    pub file: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AppFilesUploadFromUrlBody {
    /// 制品/文件下载地址（HTTP(S)）
    pub url: String,
    /// app 根相对目标
    pub target: String,
    /// 压缩包解压后单层归一（默认 false）
    #[serde(default)]
    pub flatten: bool,
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 用户 ID（仅审计日志，可选——rcoder 转发链不携带；9252a29 曾改必填造成
    /// rcoder 转发 422 断链，回退为可选审计字段）。
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct AppFilesListParams {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 用户 ID（仅审计日志，可选——rcoder 转发链不携带）。
    #[serde(default)]
    pub user_id: Option<String>,
    /// app 根相对子目录（缺省列根）
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AppFilesDeleteBody {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 用户 ID（仅审计日志，可选——rcoder 转发链不携带）。
    #[serde(default)]
    pub user_id: Option<String>,
    /// app 根相对文件/目录
    pub path: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AppFilesClearBody {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 用户 ID（仅审计日志，可选）。
    #[serde(default)]
    pub user_id: Option<String>,
}
