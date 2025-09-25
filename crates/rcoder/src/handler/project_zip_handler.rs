use anyhow::Result;
use axum::{Json, extract::State, response::{IntoResponse, Response}};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};

use crate::{model::*, router::AppState};

/// 项目压缩请求结构
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectZipRequest {
    /// 项目 ID
    pub project_id: String,
}

/// 获取 project_id 的 workspace_path
async fn get_project_workspace(project_id: &str, projects_dir: &PathBuf) -> Result<PathBuf> {
    let project_dir = projects_dir.join(project_id);
    Ok(project_dir)
}

/// 处理项目压缩请求
#[axum::debug_handler]
#[instrument(skip(state))]
pub async fn handle_project_zip(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProjectZipRequest>,
) -> Result<crate::model::HttpResult<String>, crate::model::AppError> {
    info!(
        "🗜️ [DEBUG] handle_project_zip 开始处理请求: project_id={}",
        request.project_id
    );

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&request.project_id, &state.config.projects_dir).await?;

    // 检查项目目录是否存在
    if !tokio::fs::metadata(&project_workspace).await.is_ok() {
        error!("❌ [DEBUG] 项目目录不存在: {:?}", project_workspace);
        return Ok(HttpResult::error(
            "PROJECT_NOT_FOUND",
            format!("项目目录不存在: {:?}", project_workspace).as_str(),
        ));
    }

    debug!("📁 [DEBUG] 项目工作目录: {:?}", project_workspace);

    // 使用 nuwax_parser 压缩项目
    let zipper = nuwax_parser::ProjectZipper::new();
    let zip_path = match zipper.zip_project(&project_workspace, None) {
        Ok(path) => path,
        Err(e) => {
            error!("❌ [DEBUG] 压缩项目失败: {}", e);
            return Ok(HttpResult::error(
                "PROJECT_ZIP_FAILED",
                format!("压缩项目失败: {}", e).as_str(),
            ));
        }
    };

    info!("✅ [DEBUG] 成功压缩项目: {:?}", zip_path);

    // 返回 ZIP 文件的下载 URL
    let download_url = format!("/project/download/{}", request.project_id);
    Ok(HttpResult::success(download_url))
}

/// 处理项目文件下载请求
#[axum::debug_handler]
#[instrument(skip(state))]
pub async fn handle_project_download(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(project_id): axum::extract::Path<String>,
) -> Result<Response, crate::model::AppError> {
    info!(
        "📥 [DEBUG] handle_project_download 开始处理请求: project_id={}",
        project_id
    );

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id, &state.config.projects_dir).await?;

    // 检查项目目录是否存在
    if !tokio::fs::metadata(&project_workspace).await.is_ok() {
        error!("❌ [DEBUG] 项目目录不存在: {:?}", project_workspace);
        return Err(crate::model::AppError::from(
            anyhow::anyhow!("项目目录不存在: {:?}", project_workspace),
        ));
    }

    // 查找或创建 ZIP 文件
    let zip_file_name = format!("{}.zip", project_id);
    let zip_path = state.config.projects_dir.join(&zip_file_name);

    // 如果 ZIP 文件不存在，则创建它
    if !zip_path.exists() {
        debug!("🗜️ [DEBUG] 创建 ZIP 文件: {:?}", zip_path);
        let zipper = nuwax_parser::ProjectZipper::new();
        zipper.zip_project(&project_workspace, Some(&zip_path)).map_err(|e| {
            crate::model::AppError::from(anyhow::anyhow!("压缩项目失败: {}", e))
        })?;
    }

    debug!("📥 [DEBUG] 发送 ZIP 文件: {:?}", zip_path);

    // 读取 ZIP 文件内容
    let zip_content = tokio::fs::read(&zip_path).await.map_err(|e| {
        crate::model::AppError::from(anyhow::anyhow!("读取 ZIP 文件失败: {}", e))
    })?;

    // 设置响应头
    let headers = [
        ("Content-Type", "application/zip"),
        ("Content-Disposition", &format!("attachment; filename=\"{}\"", zip_file_name)),
        ("Cache-Control", "no-cache, no-store, must-revalidate"),
        ("Pragma", "no-cache"),
        ("Expires", "0"),
    ];

    Ok((headers, zip_content).into_response())
}