//! /fs/roots 与 /fs/children handlers：文件系统目录浏览（目录选择弹窗，
//! 对齐 TS f979df7——不锚定工作空间、不带会话上下文，不解析 service）。

use garde::Validate;

use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::FsChildrenQuery;
use crate::models::response::{FsChildrenResponse, FsRootsResponse};

/// 文件系统根目录列表
///
/// 浏览起点：根目录（win32 盘符 / 其余 `/`）+ home 快捷入口。
#[utoipa::path(
    get,
    path = "/fs/roots",
    responses((status = 200, description = "Roots + home", body = FsRootsResponse)),
    tag = "Computer"
)]
pub(crate) async fn fs_roots() -> Result<Json<FsRootsResponse>, AppError> {
    let (roots, home) = crate::service::fs_browser::list_fs_roots().await?;
    Ok(Json(FsRootsResponse {
        success: true,
        roots,
        home,
    }))
}

/// 列目录下一层子项
///
/// 按绝对路径列一层（目录 + 文件），目录在前自然排序；目录不存在/无权限 → 400。
#[utoipa::path(
    get,
    path = "/fs/children",
    params(FsChildrenQuery),
    responses((status = 200, description = "One-level entries", body = FsChildrenResponse)),
    tag = "Computer"
)]
pub(crate) async fn fs_children(
    Query(q): Query<FsChildrenQuery>,
) -> Result<Json<FsChildrenResponse>, AppError> {
    q.validate().map_err(crate::error::from_garde)?;
    let (path, entries) = crate::service::fs_browser::list_fs_children(q.path.trim()).await?;
    Ok(Json(FsChildrenResponse {
        success: true,
        path,
        entries,
    }))
}
