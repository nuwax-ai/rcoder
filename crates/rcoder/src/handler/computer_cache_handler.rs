//! Computer user 缓存清理处理器
//!
//! 手动触发清空指定 user 的 `.cache`（共享卷
//! `/app/computer-project-workspace/{user_id}/.cache`）下所有内容，回收可再生缓存
//! （uv / pnpm / npm / 浏览器二进制等）占用的空间。只清 `.cache`，不动
//! `.npm` / `.config` / `.local` / project 等其他目录。
//!
//! `.cache` 是 agent 运行时的可再生缓存（XDG_CACHE_HOME 语义：可随时删除），
//! 清空后下次使用会自动重建，不影响用户数据。

use std::io::ErrorKind;
use std::path::Path;

use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::fs;
use tracing::{info, instrument, warn};
use utoipa::ToSchema;

use crate::{AppError, HttpResult, router::AppState};
use shared_types::current_request_locale;
use shared_types::error_codes::ERR_VALIDATION;

use super::utils::{I18nJsonOrQuery, user_dir};

/// 清理 user `.cache` 缓存请求
///
/// `user_id` 必填，定位 `.cache` 所在 workspace；其余字段对齐其他 computer 接口
/// （`RestartPodRequest`），当前不参与 `.cache` 路径解析——computer-agent 的
/// workspace 固定为 `/app/computer-project-workspace/{user_id}`。
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CacheCleanRequest {
    /// 用户唯一标识符（computer 路径必填，定位 `.cache` 所在 workspace；
    /// userApp 分派形态下可省略——owner 从 app 元数据解析）
    #[serde(default)]
    #[schema(example = "1754545591")]
    pub user_id: Option<String>,

    /// 容器唯一标识（可选，对齐接口惯例；不改变 `.cache` 路径解析）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID（可选，对齐接口惯例；不改变 `.cache` 路径解析）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID（可选，对齐接口惯例）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型（可选，对齐接口惯例）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型。可选值：`computer-agent-runner`（默认——清 computer workspace
    /// 的 .cache）、`userapp`（同义变体 `user-app` / `application` / `app`，大小
    /// 写不敏感——与 app_id 搭配，清 userApp 开发工作区内的 .cache）。
    /// userApp 容器类型由 app_stage 推导，勿传 `user-app-builder`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,

    /// userApp 应用 ID——存在即进入 userApp 分派，清 dev 工作区
    /// `dev/{owner}/{app_id}/.cache`（app 项目自身的构建缓存如 vite/webpack
    /// 输出）；computer 路径不消费本字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_001")]
    pub app_id: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——userApp 分派仅
    /// 支持 dev：构建缓存只存在于开发工作区，prod 运行
    /// 容器无构建缓存语义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// 清理 user `.cache` 缓存响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CacheCleanResponse {
    /// 用户 ID
    pub user_id: String,

    /// 成功删除的 `.cache` 顶层子项数量（不统计字节大小）
    #[schema(example = 12)]
    pub deleted_entries: u64,
}

/// 清空指定 user 的 `.cache` 缓存
///
/// 清空 `/app/computer-project-workspace/{user_id}/.cache` 下所有内容（保留 `.cache` 空目录）。
/// 这些是 agent 运行时的可再生缓存，删除后下次使用会自动重建，不影响用户数据。
/// 仅清 `.cache`，不动 `.npm` / `.config` / `.local` / project 等其他目录。
///
/// 已知限制：不检测 user 是否活跃；若该 user 的 agent 正在 `pnpm install`，
/// 清 `.cache` 可能使本次安装失败（缓存可再生，重试即恢复）。
#[utoipa::path(
    post,
    path = "/computer/cache/clean",
    request_body(
        content = CacheCleanRequest,
        description = "清理 user .cache 缓存请求（user_id 必填，其余可选）",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "清理完成", body = HttpResult<CacheCleanResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "computer",
    operation_id = "computer_cache_clean",
    summary = "清理 user 的 .cache 缓存",
    description = "清空 /app/computer-project-workspace/{user_id}/.cache 下所有内容（可再生缓存），\
                   回收空间。只清 .cache，不动其他目录。computer-agent 的 workspace 固定按 user_id，\
                   pod_id/tenant_id/space_id 等可选字段对齐接口惯例但不改变路径解析。支持 userApp 分派：service_type=userapp + app_id 清开发工作区 dev/{owner}/{app_id}/.cache（app 项目构建缓存，仅 dev 阶段）。"
)]
#[instrument(skip_all, fields(user_id = ?request.user_id.as_deref()))]
pub async fn computer_cache_clean(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<CacheCleanRequest>,
) -> Result<HttpResult<CacheCleanResponse>, AppError> {
    let locale = current_request_locale();

    // 0. userApp 分派（service_type=userapp + app_id；构建缓存
    //    只存在于 dev 开发工作区）。user_id 此形态下可省略——owner 从 app
    //    元数据解析（显式传 > metadata > fail-fast，与 ensure_userapp_builder 同源）
    match super::pod_handler::parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(super::pod_handler::AppTarget::NotApp) => {}
        Ok(super::pod_handler::AppTarget::Dev(app_id)) => {
            info!("[CACHE_CLEAN] userApp dev dispatch: app_id={app_id}");
            return cache_clean_userapp_dev(state.as_ref(), &app_id, &request).await;
        }
        Ok(super::pod_handler::AppTarget::Prod(_)) => {
            return Ok(super::pod_handler::invalid_app_target_response(locale, "app_stage 'prod' is not supported: agent 会话仅存在于 dev 阶段 (UserappBuilder 开发容器)"));
        }
        Err(e) => return Ok(super::pod_handler::invalid_app_target_response(locale, &e)),
    }

    // 1. 校验 user_id（computer 路径必填）
    let user_id = request
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(user_id) = user_id else {
        warn!("[CACHE_CLEAN] rejected: empty user_id");
        return Ok(HttpResult::error_with_message(
            ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    };

    // 2. 校验 service_type（仅支持 computer-agent-runner）
    if let Some(ref st) = request.service_type
        && !st.trim().eq_ignore_ascii_case("computer-agent-runner")
    {
        warn!("[CACHE_CLEAN] rejected: unsupported service_type={}", st);
        return Ok(HttpResult::error_with_message(
            ERR_VALIDATION,
            locale,
            "only service_type=computer-agent-runner is supported",
        ));
    }

    // 3. 解析 .cache 路径（user_dir 内置 validate_identifier，拒绝 ../ 穿越注入）
    let user_path = match user_dir(user_id) {
        Ok(p) => p,
        Err(e) => {
            warn!("[CACHE_CLEAN] rejected: invalid user_id={}: {}", user_id, e);
            return Ok(HttpResult::error_with_message(
                ERR_VALIDATION,
                locale,
                &e.to_string(),
            ));
        }
    };
    let cache_dir = Path::new(&user_path).join(".cache");

    // 4. 清空 .cache/*（保留 .cache 本身）
    let deleted = clean_cache_dir(&cache_dir).await?;

    info!(
        "[CACHE_CLEAN] cleaned user_id={}, deleted_entries={}",
        user_id, deleted
    );

    Ok(HttpResult::success(CacheCleanResponse {
        user_id: user_id.to_string(),
        deleted_entries: deleted,
    }))
}

/// 清空指定目录下的所有直接子项（保留目录本身）。
///
/// - 目录不存在（`NotFound`）→ 幂等返回 `Ok(0)`
/// - 每个直接子项：目录用 `remove_dir_all`，文件 / 符号链接用 `remove_file`
/// - 返回成功删除的子项数；单个子项删除失败只记 `warn` 不中断（优先整体回收）
/// - 遍历自身的 IO 错误（权限等）向上传播（→ `AppError` 500）
async fn clean_cache_dir(cache_dir: &Path) -> std::io::Result<u64> {
    let mut entries = match fs::read_dir(cache_dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut deleted: u64 = 0;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // file_type() 不跟随符号链接：真目录走 remove_dir_all；
        // 符号链接（无论指向文件还是目录）/ 文件 / 特殊文件都走 remove_file，
        // 只删 entry 本身——绝不跟随符号链接递归删目标（避免误删 .cache 外数据）。
        let file_type = entry.file_type().await?;
        let res = if file_type.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        };
        match res {
            Ok(_) => deleted += 1,
            Err(e) => warn!("[CACHE_CLEAN] failed to remove {}: {}", path.display(), e),
        }
    }
    Ok(deleted)
}

/// userApp dev 分派：清 app 开发工作区的 `.cache`（app 项目自身的构建缓存，
/// 如 vite/webpack 输出）。
///
/// 目标路径 = `{USERAPP_WORKSPACE_ROOT}/dev/{owner}/{app_id}/.cache`（dev 四
/// 目录中工作区段的隐藏缓存目录）。owner 三档解析与 ensure_userapp_builder
/// 同源（显式 user_id > app 元数据 > fail-fast，绝不兜底 app_id——防宿主树
/// 挂错位置不可回收）；owner 过 identifier 白名单（防路径穿越）。
async fn cache_clean_userapp_dev(
    state: &AppState,
    app_id: &str,
    request: &CacheCleanRequest,
) -> Result<HttpResult<CacheCleanResponse>, AppError> {
    // owner 解析：显式 user_id > metadata（get_app_owner）> fail-fast
    let metadata_owner = state.app_service.get_app_owner(app_id).await;
    let owner = crate::userapp_builder::resolve_owner(
        request.user_id.as_deref(),
        metadata_owner.as_deref(),
    )
    .map_err(|e| {
        warn!("[CACHE_CLEAN][USERAPP] cannot resolve owner: app_id={app_id}: {e}");
        AppError::with_message(
            ERR_VALIDATION,
            "cannot resolve owner user_id for app; pass user_id explicitly",
        )
    })?;
    shared_types::validate_identifier(&owner, "user_id").map_err(|e| {
        warn!("[CACHE_CLEAN][USERAPP] rejected: invalid owner={owner}: {e}");
        AppError::with_message(ERR_VALIDATION, e.to_string())
    })?;

    let cache_dir = Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
        .join("dev")
        .join(&owner)
        .join(app_id)
        .join(".cache");

    // 复用 computer 路径的清理实现（幂等、不跟随符号链接、保留 .cache 本身）
    let deleted = clean_cache_dir(&cache_dir).await?;

    info!(
        "[CACHE_CLEAN][USERAPP] cleaned app dev workspace cache: app_id={app_id}, owner={owner}, deleted_entries={deleted}",
    );

    Ok(HttpResult::success(CacheCleanResponse {
        user_id: owner,
        deleted_entries: deleted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clean_cache_dir_removes_all_children_keeps_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join(".cache");
        fs::create_dir_all(&cache).await.unwrap();

        // 顶层文件
        fs::write(cache.join("a.txt"), b"x").await.unwrap();
        fs::write(cache.join("b.json"), b"{}").await.unwrap();
        // 顶层目录（含嵌套内容）
        fs::create_dir_all(cache.join("uv").join("store"))
            .await
            .unwrap();
        fs::write(cache.join("uv").join("store").join("pkg"), b"y")
            .await
            .unwrap();
        fs::create_dir_all(cache.join("pnpm")).await.unwrap();

        let deleted = clean_cache_dir(&cache).await.unwrap();
        // a.txt + b.json + uv/ + pnpm/ = 4 个顶层子项
        assert_eq!(deleted, 4);

        // .cache 本身保留且为空
        assert!(cache.is_dir());
        let mut it = fs::read_dir(&cache).await.unwrap();
        assert!(it.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clean_cache_dir_missing_returns_zero() {
        // .cache 不存在 → 幂等 Ok(0)，不报错
        let deleted = clean_cache_dir(Path::new("/nonexistent/.cache"))
            .await
            .unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn clean_cache_dir_empty_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join(".cache");
        fs::create_dir_all(&cache).await.unwrap();

        let deleted = clean_cache_dir(&cache).await.unwrap();
        assert_eq!(deleted, 0);
        assert!(cache.is_dir());
    }

    /// 回归：.cache 下有符号链接指向目录时，绝不能跟随它删除目标目录内容。
    #[cfg(unix)]
    #[tokio::test]
    async fn clean_cache_dir_symlink_to_dir_not_followed() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join(".cache");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&cache).await.unwrap();
        fs::create_dir_all(outside.join("keep")).await.unwrap();
        fs::write(outside.join("keep").join("data"), b"precious")
            .await
            .unwrap();

        // .cache/link_to_dir -> outside（指向目录）：只能删符号链接本身，不能碰 outside
        symlink(&outside, cache.join("link_to_dir")).unwrap();
        fs::write(cache.join("file.txt"), b"x").await.unwrap();

        let deleted = clean_cache_dir(&cache).await.unwrap();
        // link_to_dir（符号链接当一项删）+ file.txt = 2 项
        assert_eq!(deleted, 2);
        let mut it = fs::read_dir(&cache).await.unwrap();
        assert!(it.next_entry().await.unwrap().is_none());

        // outside 目标内容完好（符号链接未被跟随）
        assert!(outside.join("keep").join("data").exists());
        assert_eq!(
            fs::read(outside.join("keep").join("data")).await.unwrap(),
            b"precious"
        );
    }

    /// 契约钉住：userApp 分派 wire 形态 = service_type=userapp +
    /// app_id 定位 + app_stage 可缺省（user_id 可省略——owner
    /// 从 app 元数据解析）；既有 computer 形态（user_id 必传）不受影响。
    #[test]
    fn cache_clean_request_deserializes_userapp_wire_form() {
        let raw = r#"{"service_type":"userapp","app_id":"app-1"}"#;
        let req: CacheCleanRequest = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("userApp 形态 {raw} 应可反序列化: {e}"));
        assert_eq!(req.service_type.as_deref(), Some("userapp"));
        assert_eq!(req.app_id.as_deref(), Some("app-1"));
        assert!(req.app_stage.is_none());
        assert!(req.user_id.is_none());

        let legacy = r#"{"user_id":"1754545591"}"#;
        let req: CacheCleanRequest = serde_json::from_str(legacy).unwrap();
        assert_eq!(req.user_id.as_deref(), Some("1754545591"));
        assert!(req.service_type.is_none());
        assert!(req.app_id.is_none());
    }
}
