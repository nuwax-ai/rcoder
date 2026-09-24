//! /fs/roots、/fs/children、/fs/mkdir、/fs/rename handlers：文件系统目录
//! 浏览与目录弹窗写操作（对齐 TS 1.5.3——不锚定工作空间、不带会话上下文，
//! 不解析 service）。

use garde::Validate;

use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::response::{FsChildrenResponse, FsMutationResponse, FsRootsResponse};
use crate::models::{FsChildrenQuery, FsMkdirRequest, FsRenameRequest};
use crate::service::fs_browser::FsMutatedDir;

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
    // 路径原样使用（不 trim，对齐 TS）：POSIX 下以空格结尾的目录名合法，
    // trim 会静默指向错误目录；garde not_blank 挡纯空白，其余交给绝对路径校验
    let (path, entries) = crate::service::fs_browser::list_fs_children(&q.path).await?;
    Ok(Json(FsChildrenResponse {
        success: true,
        path,
        entries,
    }))
}

/// 组装 mkdir/rename 的 wire 响应（`isDir`/`isSymlink` 为端点语义常量，
/// 对齐 TS utils 返回的同款固定值）。
fn mutated_response(dir: FsMutatedDir) -> FsMutationResponse {
    FsMutationResponse {
        success: true,
        name: dir.name,
        path: dir.path,
        parent_path: dir.parent_path,
        is_dir: true,
        is_symlink: false,
    }
}

/// 目录下新建一层目录
///
/// 目录选择弹窗"新建文件夹"：非递归（父目录须已存在）；重名/父缺失/非法名 → 400。
#[utoipa::path(
    post,
    path = "/fs/mkdir",
    request_body = FsMkdirRequest,
    responses((status = 200, description = "Created directory entry", body = FsMutationResponse)),
    tag = "Computer"
)]
pub(crate) async fn fs_mkdir(
    Json(body): Json<FsMkdirRequest>,
) -> Result<Json<FsMutationResponse>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    // JSON body 路径原样使用（不 trim，对齐 TS）：POSIX 下以空格结尾的目录名
    // 合法，trim 会静默指向错误目录；带空白前缀的相对路径由绝对路径校验拒绝
    let dir =
        crate::service::fs_browser::create_fs_directory(&body.parent_path, &body.dir_name).await?;
    Ok(Json(mutated_response(dir)))
}

/// 同目录重命名
///
/// 目录选择弹窗重命名：newName 仅名字（拒绝分隔符），不支持跨目录移动；重名/根目录/源缺失 → 400。
#[utoipa::path(
    post,
    path = "/fs/rename",
    request_body = FsRenameRequest,
    responses((status = 200, description = "Renamed directory entry", body = FsMutationResponse)),
    tag = "Computer"
)]
pub(crate) async fn fs_rename(
    Json(body): Json<FsRenameRequest>,
) -> Result<Json<FsMutationResponse>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let dir = crate::service::fs_browser::rename_fs_directory(&body.path, &body.new_name).await?;
    Ok(Json(mutated_response(dir)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    /// wire 形状端到端：mkdir/rename 走真实 axum router，锁定 camelCase
    /// 字段（`parentPath` 而非 `parent_path`）、success 信封与路径返回值。
    #[tokio::test]
    async fn mkdir_and_rename_wire_shape_is_camel_case() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().to_string_lossy().into_owned();
        // 期望值经 to_display_path 归一（Windows 反斜杠 → `/`），与响应同口径
        let display_parent = crate::service::fs_browser::to_display_path(&parent);
        let app = axum::Router::new()
            .route("/fs/mkdir", axum::routing::post(fs_mkdir))
            .route("/fs/rename", axum::routing::post(fs_rename));

        let body = serde_json::json!({ "parentPath": parent, "dirName": "新建目录" });
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::post("/fs/mkdir")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["success"], serde_json::json!(true));
        assert_eq!(value["name"], serde_json::json!("新建目录"));
        assert_eq!(value["parentPath"], serde_json::json!(display_parent));
        assert_eq!(value["isDir"], serde_json::json!(true));
        assert_eq!(value["isSymlink"], serde_json::json!(false));
        let created = value["path"].as_str().expect("path").to_string();
        assert!(created.ends_with("新建目录"));
        assert!(tokio::fs::try_exists(&created).await.unwrap());

        let body = serde_json::json!({ "path": created, "newName": "改名后" });
        let response = app
            .oneshot(
                axum::http::Request::post("/fs/rename")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["name"], serde_json::json!("改名后"));
        assert_eq!(value["parentPath"], serde_json::json!(display_parent));
        assert!(value["path"].as_str().expect("path").ends_with("改名后"));
        assert!(!tokio::fs::try_exists(&created).await.unwrap());
        assert!(
            tokio::fs::try_exists(tmp.path().join("改名后"))
                .await
                .unwrap()
        );
    }

    /// fs_children 不 trim 路径：尾空格目录（URL 编码 %20）原样浏览——
    /// 锁定 handler 层不做静默变形（POSIX 合法路径段）。
    #[tokio::test]
    async fn fs_children_serves_trailing_space_path_verbatim() {
        use tower::ServiceExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        tokio::fs::create_dir_all(tmp.path().join("dir "))
            .await
            .unwrap();
        let encoded = urlencoding_style_encode(&tmp.path().join("dir ").to_string_lossy());
        let app = axum::Router::new().route("/fs/children", axum::routing::get(fs_children));
        let response = app
            .oneshot(
                axum::http::Request::get(format!("/fs/children?path={encoded}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        // 回显路径保留尾空格（若 handler 曾 trim，浏览的会是 tmp 根而非 dir 子目录）
        let echoed = value["path"].as_str().expect("path");
        assert!(echoed.ends_with("dir "), "回显路径应保留尾空格: {echoed:?}");
    }

    /// query 参数的最小编码（空格 → %20，非 ASCII → UTF-8 字节百分号编码）。
    fn urlencoding_style_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }
}
