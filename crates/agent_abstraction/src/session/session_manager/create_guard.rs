//! session 创建 single-flight（从 session_manager.rs 拆出）。
//!
//! Leader/Follower 创建的内部类型（Handle/Guard/中止常量）与创建编排
//! （create_session / get_or_create_session / leader 路径）；single-flight 测试
//! 构造本模块私有 struct 字面量，随私有类型同档（prompt_channel_tests 已归
//! session_manager.rs——其测试对象 send_prompt_to_entry 属 session 域）。

use super::{
    AcpSessionManager, SessionCreationConfig, effective_model_config_changed,
    require_session_identity,
};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::launcher::ClaudeCodeLauncher;
use crate::traits::{AgentStartConfig, SessionNotifier, SessionRegistry};
use chrono::Utc;
use dashmap::DashMap;
use shared_types::{
    AgentLifecycle, AgentStatus, ModelProviderConfig, ProjectAndAgentInfo, SessionEntry,
};

impl<N: SessionNotifier + 'static, R: SessionRegistry> AcpSessionManager<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// 创建新的 Agent 会话 (SACP 版本)
    ///
    /// 启动 Agent 进程并建立 SACP 连接
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目路径
    /// - `model_provider`: 模型提供者配置
    /// - `start_config`: Agent 启动配置
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    pub async fn create_session(
        &self,
        project_id: String,
        project_path: PathBuf,
        _session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        mut start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        let deadline = start_config.session_create_deadline.unwrap_or_else(|| {
            tokio::time::Instant::now()
                + Duration::from_secs(
                    start_config
                        .acp_session_create_timeout_secs
                        .unwrap_or(shared_types::DEFAULT_ACP_SESSION_CREATE_TIMEOUT_SECS),
                )
        });
        let transition = Arc::new(
            tokio::time::timeout_at(
                deadline,
                crate::session::lock_execution_transition(&project_id),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!("agent execution transition timed out for project {project_id}")
            })?,
        );
        start_config.session_create_deadline = Some(deadline);
        start_config.startup_transition = Some(transition.clone());
        // Both public creation and single-flight creation use the same
        // physical retirement and post-factory ownership boundary. Preserve
        // this API's fresh-process behavior and its explicit resume settings.
        self.create_session_leader_path(
            &project_id,
            project_path,
            SessionCreationConfig {
                model_provider,
                start_config,
                service_uuid,
                force_new_session: true,
                api_key_manager: None,
                project_uuid_map: None,
            },
        )
        .await
        .map(|(entry, _)| entry)
        .map_err(anyhow::Error::msg)
    }

    /// 内部方法：创建 Agent 会话但不插入到 registry
    ///
    /// 用于 entry API 优化，避免重复插入
    async fn create_session_internal(
        &self,
        project_id: String,
        project_path: PathBuf,
        config: SessionCreationConfig,
    ) -> Result<R::Entry> {
        let SessionCreationConfig {
            model_provider,
            start_config,
            service_uuid,
            api_key_manager,
            project_uuid_map,
            ..
        } = config;
        info!("Creating Agent session, project ID: {}", project_id);

        // 创建 SACP 启动器
        let launcher = ClaudeCodeLauncher::with_diagnostics_listener(
            self.notifier.clone(),
            self.model_env_resolver.clone(),
            self.permission_handler.clone(),
            self.diagnostics_listener.clone(),
        )
        .with_api_config_maps(api_key_manager, project_uuid_map);

        // 记录是否使用了 resume（仅用于日志）
        let has_resume = start_config.resume_session_id.is_some();
        if has_resume {
            info!(
                "📌 Starting Agent with resume: session_id={:?}",
                start_config.resume_session_id
            );
        }

        // 启动 Agent (SACP 版本)
        // launcher 在总预算内尝试 LoadSession，再以 NewSession 降级；
        // 最终创建失败时确认子进程退出后返回真实错误。
        let connection_info = launcher
            .launch(
                project_id.clone(),
                project_path.clone(),
                model_provider.clone(),
                start_config.clone(),
                self.registry.clone(),
                service_uuid,
            )
            .await?;

        info!(
            "✅ Agent session created successfully, session ID: {}",
            connection_info.session_id
        );

        // 创建 ProjectAndAgentInfo
        let lifecycle_handle =
            Some(connection_info.lifecycle_guard.clone() as Arc<dyn AgentLifecycle>);
        let now = Utc::now();
        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.clone(),
            session_id: connection_info.session_id.clone(),
            prompt_tx: connection_info.prompt_tx,
            cancel_tx: connection_info.cancel_tx,
            model_provider: model_provider.clone(),
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: now,
            created_at: now,
            stop_handle: lifecycle_handle,
            agent_binary_snapshot: None,
        };

        // 返回 agent_info（不插入 registry，由调用方处理）
        Ok(agent_info.into())
    }

    /// 获取或创建会话 (SACP 版本)
    ///
    /// 如果会话已存在且模型配置未变化，则复用；否则创建新会话
    ///
    /// # 优化说明
    /// 获取或创建会话
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目路径
    /// - `session_id_hint`: 会话 ID 提示（用于恢复现有会话）
    /// - `config`: 模型、启动环境、关联 UUID 与 Fresh 策略
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    /// - `bool`: 是否是新创建的会话
    ///
    /// # 并发安全性
    ///
    /// 快速复用后，创建请求按 project_id 合流为 leader/follower；leader 在
    /// 执行转换锁内完成停旧、退出确认、启动和登记。DashMap guard 不跨 await，
    /// 不同 project 独立；控制请求在相同锁内完成旧实例清理。
    /// 比较影响实际进程及其代理绑定的有效模型配置。
    fn effective_model_changed(
        &self,
        existing: Option<&ModelProviderConfig>,
        new: Option<&ModelProviderConfig>,
    ) -> bool {
        effective_model_config_changed(existing, new)
    }

    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        mut config: SessionCreationConfig,
    ) -> Result<(R::Entry, bool)> {
        let session_id_hint = shared_types::SessionHint::classify(session_id_hint.as_deref())
            .as_session_id()
            .map(str::to_owned);
        config.start_config.resume_session_id =
            shared_types::SessionHint::classify(config.start_config.resume_session_id.as_deref())
                .as_session_id()
                .map(str::to_owned);
        let project_id_key = project_id.to_string();
        if let Some(hint) = session_id_hint.as_deref()
            && let Some(owner) = self.registry.get_project_by_session(hint)
            && owner != project_id
        {
            tracing::warn!(
                project_id,
                session_id = hint,
                owner,
                "foreign session hint ignored; creating a fresh session"
            );
            config.force_new_session = true;
        }
        if config.force_new_session {
            config.start_config.resume_session_id = None;
        }

        // Phase A：session_id_hint 快速复用。不可复用（通道死/模型变）时统一进入
        // 下方的创建合流——停旧由 Leader 独占执行（并发下只有一人停+建，不变式
        // 从概率性变确定性）。
        if !config.force_new_session
            && let Some(ref hint_sid) = session_id_hint
            && let Some(existing) = self.registry.get_entry_by_session(hint_sid)
            && existing.project_id() == project_id
            && require_session_identity(existing.session_id()).is_ok()
            && *existing.status() != AgentStatus::Terminating
            && !existing.is_channel_closed()
            && !self
                .effective_model_changed(existing.model_provider(), config.model_provider.as_ref())
            && session_id_hint
                .as_deref()
                .is_none_or(|hint| existing.session_id().to_string() == hint)
        {
            info!(
                "[SESSION] Reusing existing session via session_id_hint: project_id={}, session_id={}",
                project_id, hint_sid
            );
            return Ok((existing, false));
        }

        // Phase B: process ownership remains observable, but only a real
        // session identity may enter the prompt reuse path.
        if !config.force_new_session
            && let Some(existing) = self.registry.get(&project_id_key)
            && require_session_identity(existing.session_id()).is_ok()
            && *existing.status() != AgentStatus::Pending
            && *existing.status() != AgentStatus::Terminating
            && !existing.is_channel_closed()
            && !self
                .effective_model_changed(existing.model_provider(), config.model_provider.as_ref())
            && session_id_hint
                .as_deref()
                .is_none_or(|hint| existing.session_id().to_string() == hint)
        {
            info!("Reuse Agent session, project ID: {}", project_id);
            return Ok((existing, false));
        }

        // Phase C：需要创建/重建 → single-flight 决出 Leader/Follower。
        // 同 project 并发 chat 共享同一次 spawn（消除双 spawn + 败者 SIGKILL
        // 健康进程）；只在同步作用域内持有 DashMap entry guard，禁止跨 await。
        let role = match self.in_flight.entry(project_id_key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                SessionCreateRole::Follower(e.get().clone())
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let (tx, _rx) = watch::channel(None);
                let handle = Arc::new(SessionCreateHandle { tx });
                e.insert(handle.clone());
                SessionCreateRole::Leader(handle)
            }
        };
        let timeout = Duration::from_secs(
            config
                .start_config
                .acp_session_create_timeout_secs
                .unwrap_or(shared_types::DEFAULT_ACP_SESSION_CREATE_TIMEOUT_SECS),
        );
        match role {
            SessionCreateRole::Leader(handle) => {
                self.create_as_leader(handle, &project_id_key, project_path, config)
                    .await
            }
            SessionCreateRole::Follower(handle) => {
                self.join_as_follower(handle, &project_id_key, timeout, &config.model_provider)
                    .await
            }
        }
    }

    /// Leader 路径：CreateGuard 保证退出时（含 panic）必广播 outcome 并移除条目。
    async fn create_as_leader(
        &self,
        handle: Arc<SessionCreateHandle<R::Entry>>,
        project_id: &str,
        project_path: PathBuf,
        mut config: SessionCreationConfig,
    ) -> Result<(R::Entry, bool)> {
        let mut guard = SessionCreateGuard {
            map: self.in_flight.clone(),
            key: project_id.to_string(),
            handle,
            outcome: None,
        };
        let budget = Duration::from_secs(
            config
                .start_config
                .acp_session_create_timeout_secs
                .unwrap_or(shared_types::DEFAULT_ACP_SESSION_CREATE_TIMEOUT_SECS),
        );
        let deadline = config
            .start_config
            .session_create_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + budget);
        config.start_config.session_create_deadline = Some(deadline);
        let transition = tokio::time::timeout_at(
            deadline,
            crate::session::lock_execution_transition(project_id),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("agent execution transition timed out for project {project_id}")
        })?;
        let transition = Arc::new(transition);
        config.start_config.startup_transition = Some(transition.clone());
        // launcher 自己处理 handshake deadline 并等待失败子进程退出。
        // 这里不再用同一 deadline 抢先 drop launcher，single-flight 保持到清理完成。
        let result = self
            .create_session_leader_path(project_id, project_path, config)
            .await;
        guard.outcome = Some(result.clone());
        result.map_err(anyhow::Error::msg)
    }

    /// Leader 的实际创建序列：停旧（若需）→ spawn → 全量提交（三 map）。
    ///
    /// 提交用 `registry.insert()`（AgentSessionRegistry 实现为 register()，
    /// project/session 正反向映射一次一致）——替代旧的 entry.insert 单 map 写
    /// + 事后补写反向映射的双写窗口。
    async fn create_session_leader_path(
        &self,
        project_id: &str,
        project_path: PathBuf,
        config: SessionCreationConfig,
    ) -> Result<(R::Entry, bool), String> {
        let reconciliation = config.start_config.clone();
        let model_provider = config.model_provider.clone();
        create_after_reconciliation(
            self.registry.as_ref(),
            project_id,
            &model_provider,
            &reconciliation,
            config.force_new_session,
            || self.create_session_internal(project_id.to_owned(), project_path, config),
        )
        .await
    }

    /// Follower 路径：subscribe 等 Leader 广播（CreateGuard drop 必 send 一次）。
    async fn join_as_follower(
        &self,
        handle: Arc<SessionCreateHandle<R::Entry>>,
        project_id: &str,
        timeout: Duration,
        model_provider: &Option<ModelProviderConfig>,
    ) -> Result<(R::Entry, bool)> {
        info!(
            "[SESSION] joining in-flight session creation (single-flight follower): project_id={}",
            project_id
        );
        let mut rx = handle.tx.subscribe();
        // leader 可能已完成（borrow 直接拿到 Some）
        let outcome = if let Some(outcome) = rx.borrow().clone() {
            outcome
        } else {
            match tokio::time::timeout(timeout + SESSION_CREATE_FOLLOWER_GRACE, rx.changed()).await
            {
                Ok(Ok(())) => rx
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| Err("no outcome".into())),
                Ok(Err(_)) => Err(SESSION_CREATE_LEADER_ABORTED.to_string()),
                Err(_) => Err("session create join timeout".to_string()),
            }
        };
        outcome
            .map(|(entry, _)| {
                // 复用 Leader 建好的会话：reuse 语义（is_new=false）——避免
                // follower 的 chat 再次 register/重复 lifecycle watcher
                info!(
                    "[SESSION] joined leader's session (single-flight): project_id={}, session_id={}",
                    project_id,
                    entry.session_id()
                );
                (entry, false)
            })
            .map_err(anyhow::Error::msg)
            .and_then(|(entry, is_new)| {
                // 复检：Leader 刚建好即死 / 并发请求模型配置不同（并发切模型）
                if require_session_identity(entry.session_id()).is_err()
                    || entry.is_channel_closed()
                    || self.effective_model_changed(entry.model_provider(), model_provider.as_ref())
                {
                    return Err(anyhow::anyhow!(
                        "in-flight session outcome stale (unusable identity, dead channel or model mismatch), caller retry will lead: project_id={}",
                        project_id
                    ));
                }
                Ok((entry, is_new))
            })
    }
}

/// Caller holds the project execution transition. Keeping the factory inside
/// this sequence prevents a retry from spawning before the retained child exits.
async fn create_after_reconciliation<R, F, Fut>(
    registry: &R,
    project_id: &str,
    model_provider: &Option<ModelProviderConfig>,
    start_config: &AgentStartConfig,
    force_new_session: bool,
    create: F,
) -> Result<(R::Entry, bool), String>
where
    R: SessionRegistry,
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<R::Entry>> + Send,
{
    // An unusable historical SID does not hide a physical execution.
    // Retire that exact lifecycle before creating a usable replacement.
    // model_changed 停旧是切模型正确性的关键：双 opencode 进程同时持有同一
    // session 会导致新进程 prompt 立即 service failure（切模型场景实测）。
    if let Some(existing) = registry.get(project_id) {
        let session_id_str = existing.session_id().to_string();
        let established = require_session_identity(existing.session_id()).is_ok();
        let channel_closed = existing.is_channel_closed();
        let model_changed =
            effective_model_config_changed(existing.model_provider(), model_provider.as_ref());
        let resume_changed = start_config
            .resume_session_id
            .as_deref()
            .is_some_and(|hint| existing.session_id().to_string() != hint);
        let stopping = *existing.status() == AgentStatus::Terminating;
        if !force_new_session
            && established
            && !channel_closed
            && !model_changed
            && !resume_changed
            && !stopping
        {
            return Ok((existing, false));
        }
        if channel_closed {
            info!(
                "⚠️ [SESSION] Session channel closed, rebuilding: project_id={}, old session_id={}",
                project_id, session_id_str
            );
        }
        if !established
            || model_changed
            || force_new_session
            || channel_closed
            || resume_changed
            || stopping
        {
            if !channel_closed && existing.lifecycle_handle().is_none() {
                return Err(format!(
                    "Existing agent physical exit cannot be confirmed: lifecycle handle is missing for project {project_id}"
                ));
            }
            info!(
                "🔄 [SESSION] Model config changed, restarting Agent session: project_id={}, old session_id={}",
                project_id, session_id_str
            );
            // 🔪 显式停止旧 Agent 进程（graceful_stop 内部 CAS 幂等）
            if let Some(handle) = existing.lifecycle_handle() {
                let deadline = start_config.session_create_deadline.ok_or_else(|| {
                    format!("session creation deadline missing for project {project_id}")
                })?;
                registry.update_agent_status_for_session(
                    project_id,
                    &session_id_str,
                    None,
                    AgentStatus::Terminating,
                );
                // stop 内部脱离调用者执行 TERM/KILL/reaper。预算耗尽不移除
                // registry，后续重试仍必须观察这一个旧执行并确认退出。
                tokio::time::timeout_at(deadline, async {
                        handle.graceful_stop().await?;
                        handle.wait_for_exit().await
                    }).await
                        .map_err(|_| format!("old agent exit deadline exceeded for project {project_id}, session {session_id_str}"))?
                        .map_err(|error| format!("old agent exit unconfirmed for project {project_id}, session {session_id_str}: {error}"))?;
                info!(
                    "[SESSION] old agent exited before rebuild: project_id={}, old session_id={}",
                    project_id, session_id_str
                );
            }
        }
    }

    let new_session = create().await.map_err(|error| error.to_string())?;
    let new_session_id = new_session.session_id().to_string();
    let registered_owner = registry.get_project_by_session(&new_session_id);
    confirm_created_session_identity(
        registry,
        project_id,
        &new_session,
        registered_owner.as_deref(),
    )
    .await
    .map_err(|error| error.to_string())?;
    registry.insert(project_id, &new_session_id, new_session.clone());
    info!(
        "[SESSION] Session committed after physical retirement: project_id={}, session_id={}",
        project_id, new_session_id
    );
    Ok((new_session, true))
}

#[derive(Debug)]
enum CreatedSessionRejection {
    Retired(String),
    ExitUnconfirmed(String),
}

impl std::fmt::Display for CreatedSessionRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retired(reason) | Self::ExitUnconfirmed(reason) => formatter.write_str(reason),
        }
    }
}

/// Retain a rejected factory's physical execution before any cleanup await.
/// Cancellation or an unconfirmed exit leaves that exact execution observable
/// to the next Start or Stop, without publishing its rejected session route.
async fn confirm_created_session_identity<R: SessionRegistry>(
    registry: &R,
    project_id: &str,
    entry: &R::Entry,
    registered_owner: Option<&str>,
) -> Result<(), CreatedSessionRejection>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    let rejection = match require_session_identity(entry.session_id()) {
        Err(error) => Some(error.to_string()),
        Ok(()) => registered_owner
            .filter(|owner| *owner != entry.project_id())
            .map(|owner| {
                format!(
                    "agent returned session {} already owned by project {owner}",
                    entry.session_id()
                )
            }),
    };
    let Some(rejection) = rejection else {
        return Ok(());
    };
    let mut retained: ProjectAndAgentInfo = entry.clone().into();
    retained.status = AgentStatus::Terminating;
    // Raw entry only: no SID index may be written or a foreign route replaced.
    // This synchronous write precedes even graceful_stop's first poll.
    drop(
        registry
            .entry(project_id.to_owned())
            .insert(retained.into()),
    );
    let handle = entry.lifecycle_handle().ok_or_else(|| {
        CreatedSessionRejection::ExitUnconfirmed(format!(
            "{rejection}; child exit unconfirmed: lifecycle handle is missing"
        ))
    })?;
    handle.graceful_stop().await.map_err(|error| {
        CreatedSessionRejection::ExitUnconfirmed(format!(
            "{rejection}; child exit unconfirmed while stopping: {error}"
        ))
    })?;
    handle.wait_for_exit().await.map_err(|error| {
        CreatedSessionRejection::ExitUnconfirmed(format!(
            "{rejection}; child exit unconfirmed while reaping: {error}"
        ))
    })?;
    // Same SID can identify a newer execution. Retire only the raw entity
    // whose exit was observed, leaving replacement entities and routes intact.
    if let dashmap::mapref::entry::Entry::Occupied(current) = registry.entry(project_id.to_owned())
        && current.get().session_id() == entry.session_id()
        && current.get().cancel_tx().same_channel(entry.cancel_tx())
        && current.get().prompt_tx().same_channel(entry.prompt_tx())
    {
        current.remove();
    }
    Err(CreatedSessionRejection::Retired(rejection))
}

#[cfg(test)]
mod identity_tests;

/// follower 等待 leader 的额外宽限（leader 的 CreateGuard drop 必先广播，
/// follower 不应先超时——宽限覆盖 leader 提交/清理的尾部耗时）
const SESSION_CREATE_FOLLOWER_GRACE: Duration = Duration::from_secs(10);
/// leader 异常退出（panic）时广播给 follower 的失败原因
pub(super) const SESSION_CREATE_LEADER_ABORTED: &str = "session create leader aborted";

/// 会话创建 single-flight 的结果（照搬 `AppActivityRegistry` wake 模式）
type SessionCreateOutcome<E> = Result<(E, bool), String>;

/// 进行中的会话创建句柄（leader 持 `tx`，follower `subscribe` 后等结果）
pub(super) struct SessionCreateHandle<E> {
    tx: watch::Sender<Option<SessionCreateOutcome<E>>>,
}

/// RAII 守卫：leader 路径持有。drop 时（含 panic unwind）广播 outcome
/// （panic 未写入 → 广播 Err 快速通知，follower 不干等 dead-man 超时）
/// 并以 `Arc::ptr_eq` 防误删地移除 in_flight 条目。
pub(super) struct SessionCreateGuard<E> {
    map: Arc<DashMap<String, Arc<SessionCreateHandle<E>>>>,
    key: String,
    handle: Arc<SessionCreateHandle<E>>,
    /// leader 完成后写入 outcome；panic 时仍为 None → drop 发 aborted
    outcome: Option<SessionCreateOutcome<E>>,
}

impl<E> Drop for SessionCreateGuard<E> {
    fn drop(&mut self) {
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| Err(SESSION_CREATE_LEADER_ABORTED.to_string()));
        if let Err(e) = self.handle.tx.send(Some(outcome)) {
            debug!("[SESSION] create outcome send failed (no follower): {e}");
        }
        if let dashmap::mapref::entry::Entry::Occupied(entry) = self.map.entry(self.key.clone())
            && Arc::ptr_eq(entry.get(), &self.handle)
        {
            entry.remove();
        }
    }
}

enum SessionCreateRole<E> {
    Leader(Arc<SessionCreateHandle<E>>),
    Follower(Arc<SessionCreateHandle<E>>),
}

#[cfg(test)]
mod single_flight_tests {
    use super::*;
    use tokio::sync::watch;

    fn make_guard<E>(
        map: Arc<DashMap<String, Arc<SessionCreateHandle<E>>>>,
        key: &str,
    ) -> (Arc<SessionCreateHandle<E>>, SessionCreateGuard<E>) {
        let (tx, _rx) = watch::channel(None);
        let handle = Arc::new(SessionCreateHandle { tx });
        map.insert(key.to_string(), handle.clone());
        let guard = SessionCreateGuard {
            map,
            key: key.to_string(),
            handle: handle.clone(),
            outcome: None,
        };
        (handle, guard)
    }

    /// Leader 正常完成：guard drop 广播 outcome，follower 唤醒
    #[tokio::test]
    async fn guard_drop_broadcasts_outcome() {
        let map = Arc::new(DashMap::new());
        let (handle, mut guard) = make_guard::<u8>(map.clone(), "p1");
        let rx = handle.tx.subscribe();
        guard.outcome = Some(Ok((42u8, true)));
        drop(guard);
        assert_eq!(*rx.borrow(), Some(Ok((42, true))));
        assert!(!map.contains_key("p1"), "entry removed after drop");
    }

    /// Leader panic（未写 outcome）：drop 广播 aborted，follower 快速失败不干等
    #[tokio::test]
    async fn guard_drop_without_outcome_broadcasts_aborted() {
        let map = Arc::new(DashMap::new());
        let (handle, guard) = make_guard::<u8>(map.clone(), "p1");
        let rx = handle.tx.subscribe();
        drop(guard);
        assert!(matches!(*rx.borrow(), Some(Err(ref e)) if e == SESSION_CREATE_LEADER_ABORTED));
    }

    /// ptr_eq 防误删：条目已被新一轮 Leader 替换时，旧 guard drop 不删新条目
    #[tokio::test]
    async fn stale_guard_does_not_remove_new_leader_entry() {
        let map = Arc::new(DashMap::new());
        let (_old_handle, guard) = make_guard(map.clone(), "p1");
        // 新一轮 Leader 抢先注册（旧 guard 尚未 drop 的异常序列）
        let (tx, _rx) = watch::channel(None);
        let new_handle = Arc::new(SessionCreateHandle::<u8> { tx });
        map.insert("p1".to_string(), new_handle.clone());
        drop(guard);
        assert!(
            map.contains_key("p1"),
            "new leader's entry must survive stale guard drop"
        );
        assert!(Arc::ptr_eq(map.get("p1").unwrap().value(), &new_handle));
    }

    /// follower 在 leader 完成前 subscribe：changed() 唤醒并取到 outcome
    #[tokio::test]
    async fn follower_wakes_on_leader_completion() {
        let map = Arc::new(DashMap::new());
        let (handle, mut guard) = make_guard::<u8>(map.clone(), "p1");
        let mut rx = handle.tx.subscribe();
        assert!(rx.borrow().is_none(), "not yet finished");
        let waiter = tokio::spawn(async move {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                if let Some(outcome) = rx.borrow().clone() {
                    return Some(outcome);
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        guard.outcome = Some(Ok((7u8, true)));
        drop(guard);
        let outcome = waiter.await.unwrap().expect("waiter got outcome");
        assert!(matches!(outcome, Ok((7, true))));
    }
}
