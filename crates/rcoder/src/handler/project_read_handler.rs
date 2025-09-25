use anyhow::Result;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, error, info, instrument};

use crate::{model::*, router::AppState};

/// 项目读取请求结构
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectReadRequest {
    /// 项目 ID
    pub project_id: String,
}

/// 获取 project_id 的 workspace_path
async fn get_project_workspace(project_id: &str, projects_dir: &PathBuf) -> Result<PathBuf> {
    let project_dir = projects_dir.join(project_id);
    Ok(project_dir)
}

/// 处理项目读取请求
#[axum::debug_handler]
#[instrument(skip(state))]
pub async fn handle_project_read(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProjectReadRequest>,
) -> Result<crate::model::HttpResult<nuwax_parser::ProjectSourceCode>, crate::model::AppError> {
    let project_id = request.project_id;
    info!(
        "📖 [DEBUG] handle_project_read 开始处理请求: project_id={}",
        project_id
    );

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id, &state.config.projects_dir).await?;

    // 检查项目目录是否存在
    if !tokio::fs::metadata(&project_workspace).await.is_ok() {
        error!("❌ [DEBUG] 项目目录不存在: {:?}", project_workspace);
        return Ok(HttpResult::error(
            "PROJECT_NOT_FOUND",
            format!("项目目录不存在: {:?}", project_workspace).as_str(),
        ));
    }

    debug!("📁 [DEBUG] 项目工作目录: {:?}", project_workspace);

    // 使用 nuwax_parser 读取项目
    let reader = nuwax_parser::ProjectReader::default();
    let project_source_code = match reader.read_project(&project_workspace) {
        Ok(result) => result,
        Err(e) => {
            error!("❌ [DEBUG] 读取项目失败: {}", e);
            return Ok(HttpResult::error(
                "PROJECT_READ_FAILED",
                format!("读取项目失败: {}", e).as_str(),
            ));
        }
    };

    info!(
        "✅ [DEBUG] 成功读取项目，包含 {} 个文件",
        project_source_code.files.len()
    );

    Ok(HttpResult::success(project_source_code))
}
