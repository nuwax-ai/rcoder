//! 阶段1: 会话准备
//!
//! 对应原流程步骤 1 ~ 4.5：
//! - 步骤 1: 查询现有 Agent 状态（session_id 优先，回退 project_id）
//! - 步骤 1.5: 检查 agent 二进制是否存在 + 版本检测（缺失时兜底自装）
//! - 步骤 2: Agent Busy 时取消当前任务
//! - 步骤 3: 创建 PendingGuard（RAII）
//! - 步骤 4: 清理无效 session / 复用时清空 ring buffer
//! - 步骤 4.5: Auto-Reload 检测（强制重启 ACP agent 进程）

use std::path::PathBuf;

use shared_types::error_codes;
use tracing::{error, info, warn};

use super::types::{ChatHandlerInput, ChatHandlerOutput, SessionPreparation};
use crate::agent_mgmt::checker;
use crate::service::cancel::cancel_current_task;
use crate::service::{AGENT_REGISTRY, PendingGuard, SESSION_CACHE};

/// 会话准备:Agent 状态查询、版本检测、Busy 取消、PendingGuard、session 清理、Auto-Reload。
pub(super) async fn prepare_session(
    input: &ChatHandlerInput,
    project_id: &str,
    session_id: &Option<String>,
) -> Result<SessionPreparation, ChatHandlerOutput> {
    // ========== 步骤1: 查询现有 Agent 状态 ==========
    // 优先通过 session_id 查找，回退到 project_id 查找
    // 用 view 闭包访问 agent_info:闭包返回即释放读锁,无 Ref 暴露 —— 结构上杜绝守卫跨
    // 下面 install_agent / get_agent_version 的 await(闭包是同步 FnOnce,无法在里面 await)。
    let agent_busy = if let Some(sid) = session_id {
        info!(
            "[ChatHandler] Looking up Agent by session_id: session_id={}",
            sid
        );
        AGENT_REGISTRY.view_agent_info_by_session(sid, |info| {
            (
                info.status,
                info.cancel_tx.clone(),
                info.session_id.to_string(),
            )
        })
    } else {
        None
    };
    let agent_busy = agent_busy.or_else(|| {
        info!(
            "[ChatHandler] Looking up Agent by project_id: project_id={}",
            project_id
        );
        AGENT_REGISTRY.view_agent_info(project_id, |info| {
            (
                info.status,
                info.cancel_tx.clone(),
                info.session_id.to_string(),
            )
        })
    });

    // ========== 步骤1.5: 检查 agent 二进制是否存在 + 版本检测 ==========
    let agent_version = if let Some(ref agent_config) = input.agent_config_override {
        if let Some(ref server) = agent_config.agent_server {
            if let Some(ref command) = server.command {
                if let Err(e) = checker::check_agent_exists(command) {
                    error!("[ChatHandler] Agent not found: {}", e);
                    return Err(ChatHandlerOutput::error(
                        project_id.to_string(),
                        session_id.clone().unwrap_or_default(),
                        e,
                        error_codes::ERR_AGENT_MGMT_NOT_FOUND.to_string(),
                    ));
                }

                // 兜底自装：bundle 缺失时（正常情况 rcoder 已装好，走不到这里）主动安装；
                // 若缺下载信息（无 platforms）无法自装，则 fail-fast，避免 spawn node 崩溃 → ACP 50s 超时。
                // 判据布局无关：{install_root}/{agent_id}/{version} 目录存在且非空。
                let install_root = crate::agent_mgmt::path_manager::PathManager::new()
                    .install_dir()
                    .to_path_buf();
                let agent_id = server.agent_id.as_deref().unwrap_or("");
                let version = server.version.as_deref().unwrap_or("");

                // 有 agent_id + version 才能定位安装目录、判定是否缺失
                if !agent_id.is_empty()
                    && !version.is_empty()
                    && !agent_provisioning::is_agent_installed(&install_root, agent_id, version)
                {
                    let has_platforms = server.platforms.as_ref().is_some_and(|p| !p.is_empty());
                    if has_platforms {
                        // 能自装：下载 + 解压到 install_root
                        warn!(
                            "[ChatHandler] bundle missing, triggering fallback self-install: \
                             agent_id={}, version={}, install_root={}",
                            agent_id,
                            version,
                            install_root.display()
                        );
                        let cache_dir = install_root
                            .parent()
                            .map(|p| p.join(".acp-agent-cache"))
                            .unwrap_or_else(|| PathBuf::from("/tmp/.acp-agent-cache"));
                        let mgr = match agent_provisioning::AgentDownloadManager::new(cache_dir) {
                            Ok(m) => m,
                            Err(e) => {
                                error!(
                                    "[ChatHandler] fallback self-install: cache dir init failed: {}",
                                    e
                                );
                                return Err(ChatHandlerOutput::error(
                                    project_id.to_string(),
                                    session_id.clone().unwrap_or_default(),
                                    format!("agent cache dir unavailable: {}", e),
                                    error_codes::ERR_AGENT_MGMT_INSTALL_FAILED.to_string(),
                                ));
                            }
                        };
                        let args = server.args.clone().unwrap_or_default();
                        let platforms = server.platforms.clone().unwrap_or_default();
                        if let Err(e) = agent_provisioning::install_agent(
                            &mgr,
                            agent_id,
                            version,
                            command,
                            &args,
                            &platforms,
                            &install_root,
                        )
                        .await
                        {
                            error!(
                                "[ChatHandler] fallback self-install FAILED: agent_id={}, \
                                 version={}, error={:?}",
                                agent_id, version, e
                            );
                            return Err(ChatHandlerOutput::error(
                                project_id.to_string(),
                                session_id.clone().unwrap_or_default(),
                                format!("agent bundle missing and self-install failed: {}", e),
                                error_codes::ERR_AGENT_MGMT_INSTALL_FAILED.to_string(),
                            ));
                        }
                        warn!(
                            "[ChatHandler] fallback self-install OK: agent_id={}, version={}",
                            agent_id, version
                        );
                    } else {
                        // bundle 缺失且无 platforms（无法自装）→ fail-fast，而非 spawn 后 50s 超时
                        error!(
                            "[ChatHandler] agent bundle missing and cannot self-install \
                             (no platforms): agent_id={}, version={}, install_root={}",
                            agent_id,
                            version,
                            install_root.display()
                        );
                        return Err(ChatHandlerOutput::error(
                            project_id.to_string(),
                            session_id.clone().unwrap_or_default(),
                            format!(
                                "agent bundle missing and no install info: agent_id={}, version={}",
                                agent_id, version
                            ),
                            error_codes::ERR_AGENT_MGMT_INSTALL_FAILED.to_string(),
                        ));
                    }
                }

                checker::get_agent_version(command).await
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // 默认 agent
        if let Err(e) = checker::check_agent_exists(shared_types::DEFAULT_AGENT_ID) {
            error!("[ChatHandler] Default agent not found: {}", e);
            return Err(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.clone().unwrap_or_default(),
                e,
                error_codes::ERR_AGENT_MGMT_NOT_FOUND.to_string(),
            ));
        }
        checker::get_agent_version(shared_types::DEFAULT_AGENT_ID).await
    };
    if let Some(ref v) = agent_version {
        info!("[ChatHandler] Agent version detected: {}", v);
    }

    // ========== 步骤2: 检查 Agent Busy 状态，如果忙则取消当前任务 ==========
    use crate::model::AgentStatus;
    if let Some((status, cancel_tx, agent_session_id)) = agent_busy
        && (status == AgentStatus::Active || status == AgentStatus::Pending)
    {
        info!(
            "[ChatHandler] Agent Busy, cancelling current task: project_id={}, status={:?}, session_id={:?}",
            project_id, status, session_id
        );

        // cancel_tx 已 owned(上面 clone 出来);actual_session_id 优先用请求里的,空则用 agent 的
        let actual_session_id = session_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or(agent_session_id);

        // 取消当前任务
        if let Err(cancel_error) =
            cancel_current_task(&cancel_tx, &actual_session_id, project_id).await
        {
            // 取消失败，返回错误
            error!(
                "[ChatHandler] Failed to cancel current task: project_id={}, error={:?}",
                project_id, cancel_error
            );
            return Err(cancel_error);
        }

        info!(
            "[ChatHandler] Current task cancelled, proceeding with new request: project_id={}",
            project_id
        );
    }

    // ========== 步骤3: 创建 PendingGuard（RAII）==========
    // 自动在作用域结束时清理，避免状态泄漏
    let pending_guard = PendingGuard::new(&AGENT_REGISTRY, project_id);
    info!(
        "[ChatHandler] Created PendingGuard: project_id={}",
        project_id
    );

    // ========== 步骤4: 清理无效 session ==========
    // 只在 session 不存在时才清理无效的 session_id
    if let Some(sid) = session_id {
        let session_exists = AGENT_REGISTRY.contains_session(sid);
        info!(
            "[ChatHandler] Step 4: session_id={}, session_exists_in_registry={}",
            sid, session_exists
        );

        if !session_exists {
            // registry 查不到 ≠ session 永久无效：切模型重建窗口 registry 短暂
            // 无 entry 是正常状态，此刻 remove SESSION_CACHE 会把刚建立的
            // SSE 订阅 sender 一起删掉（之后所有事件退化为 ring 积压、客户端
            // 空流）。改为只清 ring——SessionData 的生命周期由 idle cleanup 统一管理。
            if let Some(sd) = SESSION_CACHE.view(sid, |_, d| d.clone()) {
                let cleared = sd.clear_message_buffer().await;
                info!(
                    "[ChatHandler] session not in registry (rebuild window?), kept SessionData with ring cleared ({} messages): session_id={}",
                    cleared, sid
                );
            } else {
                info!(
                    "[ChatHandler] session not in registry and no SessionData cached: session_id={}",
                    sid
                );
            }
        } else if session_exists {
            info!("[ChatHandler] Reusing existing session: session_id={}", sid);
            // 🧹 清空 ring buffer，防止回放过期的历史消息
            // view() 在闭包返回后立即释放锁，无 Ref 暴露
            if let Some(sd) = SESSION_CACHE.view(sid, |_, d| d.clone()) {
                info!(
                    "[ChatHandler] SESSION_CACHE found for session_id={}, attempting to clear ring buffer",
                    sid
                );
                let cleared = sd.clear_message_buffer().await;
                if cleared > 0 {
                    info!(
                        "[ChatHandler] Cleared {} stale messages from ring buffer for new conversation: session_id={}",
                        cleared, sid
                    );
                } else {
                    info!(
                        "[ChatHandler] Ring buffer already empty for session_id={}",
                        sid
                    );
                }
            } else {
                info!(
                    "[ChatHandler] SESSION_CACHE not found for session_id={}",
                    sid
                );
            }
        }
    } else {
        info!("[ChatHandler] Step 4: session_id is None, skipping clear logic");
    }

    // ========== 步骤 4.5: Auto-Reload 检测（简化版） ==========
    // 当 auto_reload.enabled=true 时，强制重启 ACP agent 进程。
    // 重启后传入 resume_session_id 尝试恢复历史上下文。
    let mut was_reloaded = false;
    let mut old_session_id_for_resume = None;

    if let Some(agent_config) = &input.agent_config_override
        && let Some(auto_reload_config) = &agent_config.auto_reload
        && auto_reload_config.enabled
        && let Some(agent_server) = &agent_config.agent_server
        && let Some(_command) = agent_server.command.as_deref()
    {
        // Re-lookup from registry (original binding may have been consumed in Step 2)
        let agent_info_for_reload = AGENT_REGISTRY.get_agent_info(project_id);

        if let Some(agent_info) = agent_info_for_reload {
            // Extract needed data, then IMMEDIATELY drop Ref before .await
            let stop_handle = agent_info.stop_handle.clone();
            let old_session_id = agent_info.session_id.to_string();
            drop(agent_info); // Release DashMap read lock BEFORE .await

            // 保存旧 session_id 用于 resume
            if !old_session_id.is_empty() {
                old_session_id_for_resume = Some(old_session_id.clone());
            }

            info!(
                "[ChatHandler] Auto-reload: forcing restart, project_id={}, old_session_id={}",
                project_id, old_session_id
            );

            // 1. Stop old agent subprocess
            if let Some(handle) = &stop_handle
                && let Err(e) = handle.graceful_stop().await
            {
                warn!(
                    "[ChatHandler] graceful_stop failed during reload: {}, forcing cancel",
                    e
                );
                handle.cancel(); // CancellationToken fallback — infallible, no process signal dependency
            }

            // 2. Remove from AGENT_REGISTRY
            AGENT_REGISTRY.remove_by_project(project_id);

            // 3. Notify SSE stream + clean SESSION_CACHE
            if !old_session_id.is_empty() {
                use crate::service::push_session_update_with_project;
                use agent_client_protocol::schema::v1::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: old_session_id.clone(),
                    stop_reason: StopReason::EndTurn,
                    error_message: Some("Auto-reload: restarting agent".into()),
                    request_id: None,
                });
                if let Err(e) =
                    push_session_update_with_project(project_id, &old_session_id, notify).await
                {
                    warn!(
                        "[ChatHandler] failed to push auto-reload SessionPromptEnd notification: project_id={}, session_id={}, error={}",
                        project_id, old_session_id, e
                    );
                }

                // view() 在闭包返回后立即释放锁，无 Ref 暴露
                if let Some(sd) = SESSION_CACHE.view(&old_session_id, |_, d| d.clone()) {
                    sd.close_all_connections();
                }
                SESSION_CACHE.remove(&old_session_id);
            }

            was_reloaded = true;
            info!(
                "[ChatHandler] Auto-reload complete, will create new session: \
                 project_id={}, resume_session_id={:?}",
                project_id, old_session_id_for_resume
            );
        }
    }

    Ok(SessionPreparation {
        pending_guard,
        agent_version,
        was_reloaded,
        resume_session_id: old_session_id_for_resume,
    })
}
