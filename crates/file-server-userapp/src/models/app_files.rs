//! app-files 域请求结构（rcoder `app_files` 转发链的容器侧内部契约，
//! handler 壳在 handlers/userapp_app_files.rs）。
//!
//! 字段为 `pub`（models 是 crate 内公共层）；app_id 定位、user_id 必填
//! （宿主卷分区定位 + dev 容器懒创建显式 owner 档）。

use serde::Deserialize;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
pub struct AppFilesUploadForm {
    /// UserApp 应用 ID（定位 = resolve_userapp_dev；单 app 模式须与归属一致）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧为挂载分区组成段）
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
    /// 归属用户 ID（必填；rcoder 转发链现已携带——dev 容器懒创建显式 owner
    /// 档与分区定位双消费）。
    pub user_id: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct AppFilesListParams {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 宿主机数据卷分区归属目录名（必填；rcoder 转发链现已携带——懒唤醒挂载定位）。
    pub user_id: String,
    /// app 根相对子目录（缺省列根）
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AppFilesDeleteBody {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 宿主机数据卷分区归属目录名（必填；rcoder 转发链现已携带——懒唤醒挂载定位）。
    pub user_id: String,
    /// app 根相对文件/目录
    pub path: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AppFilesClearBody {
    /// UserApp 应用 ID（定位）。
    pub app_id: String,
    /// 宿主机数据卷分区归属目录名（必填；懒唤醒挂载定位）。
    pub user_id: String,
}
