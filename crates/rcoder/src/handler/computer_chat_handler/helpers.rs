use super::*;

/// 确保 project_id 对应的工作目录存在
///
/// Computer Agent Runner 的目录结构：
/// /app/computer-project-workspace/{user_id}/{project_id}/
///
/// 注意：这个目录已经在 docker-compose.yml 中挂载，可以直接在 rcoder 容器内创建
///
/// # 参数
/// - `isolation_type`: 隔离类型（可选）
/// - `tenant_id`: 租户 ID（可选）
/// - `space_id`: 空间 ID（可选）
/// - `user_id`: 用户 ID（当 isolation_type 为 project 时使用）
/// - `work_dir_id`: 工作目录标识符（可能是 project_id 或 agent_work_dir）
#[instrument(skip_all, fields(user_id = %user_id, work_dir_id = %work_dir_id))]
pub(super) async fn ensure_project_workspace_exists(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    user_id: &str,
    work_dir_id: &str,
) -> Result<(), AppError> {
    // 根据隔离类型构建工作空间路径
    let project_workspace_path = std::path::PathBuf::from(
        build_computer_workspace_path(isolation_type, tenant_id, space_id, user_id, work_dir_id)
            .map_err(|e| AppError::validation_error(&e.to_string()))?,
    );

    debug!(
        "📁 [COMPUTER_CHAT] Ensuring project workspace directory exists: {:?}",
        project_workspace_path
    );

    // 直接在 rcoder 容器内创建目录
    tokio::fs::create_dir_all(&project_workspace_path)
        .await
        .map_err(|e| {
            error!(
                "❌ [COMPUTER_CHAT] Failed to create project workspace: path={:?}, error={}",
                project_workspace_path, e
            );
            AppError::internal_server_error(&format!("Failed to create project workspace: {}", e))
        })?;

    debug!(
        "✅ [COMPUTER_CHAT] Project workspace directory created: user_id={}, work_dir_id={}, isolation_type={:?}, path={:?}",
        user_id, work_dir_id, isolation_type, project_workspace_path
    );

    Ok(())
}

/// 确保 存储 中存在 project_id 到容器的映射
///
/// 🛡️ 关键修复：容器创建成功后立即插入 存储 记录
///
/// 这样可以防止孤立容器清理器误判并清理刚创建的容器，因为：
/// 1. 孤立容器清理器会检查 存储 中是否存在该 user_id 关联的记录
/// 2. 如果记录不存在，容器会被判定为孤立并清理
/// 3. gRPC 请求是异步的，可能需要较长时间才能返回
///
/// # Arguments
/// * `state` - 应用状态
/// * `user_id` - 用户 ID
/// * `project_id` - 项目 ID
/// * `container_info` - 容器信息
/// * `request` - 聊天请求
pub(super) fn ensure_project_mapping_in_state(
    state: &Arc<AppState>,
    user_id: &str,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    request: &ComputerChatRequest,
) -> Result<(), AppError> {
    // 检查是否已存在该 project_id 的记录
    let existing_project = state.get_project(project_id);
    if let Some(ref existing) = existing_project {
        // 如果记录存在，检查容器ID是否变更
        if let Some(existing_container) = existing.container_info() {
            if existing_container.container_id == container_info.container_id {
                debug!(
                    "🔄 [COMPUTER_CHAT] project record already exists and container unchanged: project_id={}",
                    project_id
                );
                return Ok(());
            } else {
                info!(
                    "🔄 [COMPUTER_CHAT] Detected container change: project_id={}, old_cid={}, new_cid={}",
                    project_id, existing_container.container_id, container_info.container_id
                );
                // 容器变更，继续执行后续的插入/更新逻辑（insert_project 会执行 upsert）
            }
        } else {
            // 现有记录没有容器信息，继续更新
        }
    }

    // 创建新的 ProjectAndContainerInfo
    let mut project_info = shared_types::ProjectAndContainerInfo::new(project_id.to_string());

    // 设置 user_id（ComputerAgentRunner 模式）
    project_info.set_user_id(Some(user_id.to_string()));
    // 设置 pod_id（共享容器模式）
    project_info.set_pod_id(request.pod_id.clone());

    // 🛡️ 关键修复：如果现有记录有 session_id，保留它
    // 多 session 模型下：把 existing 的所有 session 都迁过来（容器变更场景）
    if let Some(ref existing) = existing_project {
        let existing_sessions = existing.sessions();
        if !existing_sessions.is_empty() {
            for sid in &existing_sessions {
                project_info.add_session(sid.clone());
            }
            debug!(
                "🔄 [COMPUTER_CHAT] Preserved {} existing session(s): project_id={}",
                existing_sessions.len(),
                project_id
            );
        }
    }

    // 更新容器信息
    project_info.update_extended_from_request(
        Some(container_info.clone()),
        request.model_provider.clone(),
        request.request_id.clone(),
        Some(shared_types::ServiceType::ComputerAgentRunner),
    );
    project_info.set_scope(
        request.tenant_id.clone(),
        request.space_id.clone(),
        request.isolation_type.clone(),
    );

    // immediately insert project record
    // 注意：如果有现有 session，必须使用 insert_with_session 来同步更新 session_index
    // 否则容器重建后，session_index 中会丢失 session 映射，导致 SSE 连接失败
    let project_info_arc = Arc::new(project_info);
    let existing_sessions: Vec<String> = existing_project
        .as_ref()
        .map(|p| p.sessions().into_iter().collect())
        .unwrap_or_default();

    if existing_sessions.is_empty() {
        // 没有现有 session，直接插入
        state
            .insert_project(project_id.to_string(), project_info_arc)
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;
    } else {
        // 有现有 session，使用 insert_with_session 同步更新 session_index
        // 对于多个 session，先插入项目，再逐个添加 session
        state
            .insert_project(project_id.to_string(), project_info_arc.clone())
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;

        // 逐个添加现有 session 到 session_index
        for sid in &existing_sessions {
            state.add_session_to_project(project_id, sid);
        }

        debug!(
            "🔄 [COMPUTER_CHAT] Synced {} existing session(s) to session_index: project_id={}",
            existing_sessions.len(),
            project_id
        );
    }

    info!(
        "🆕 [COMPUTER_CHAT] Inserted project record (immediately after container creation): user_id={}, project_id={}, container_id={}",
        user_id, project_id, container_info.container_id
    );

    Ok(())
}
