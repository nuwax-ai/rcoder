//! session 创建 single-flight（从 session_manager.rs 拆出）。
//!
//! Leader/Follower 创建的内部类型（Handle/Guard/中止常量）与创建编排
//! （create_session / get_or_create_session / leader 路径）；single-flight 测试
//! 构造本模块私有 struct 字面量，随私有类型同档。

use super::AcpSessionManager;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{debug, info, warn};

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
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        let agent_info = self
            .create_session_internal(
                project_id.clone(),
                project_path,
                model_provider,
                start_config,
                service_uuid,
            )
            .await?;

        // 存储会话信息到 registry
        let session_id_str = agent_info.session_id().to_string();
        self.registry
            .insert(&project_id, &session_id_str, agent_info.clone());

        Ok(agent_info)
    }

    /// 内部方法：创建 Agent 会话但不插入到 registry
    ///
    /// 用于 entry API 优化，避免重复插入
    async fn create_session_internal(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        info!("Creating Agent session, project ID: {}", project_id);

        // 创建 SACP 启动器
        let launcher = ClaudeCodeLauncher::with_diagnostics_listener(
            self.notifier.clone(),
            self.model_env_resolver.clone(),
            self.permission_handler.clone(),
            self.diagnostics_listener.clone(),
        );

        // 记录是否使用了 resume（仅用于日志）
        let has_resume = start_config.resume_session_id.is_some();
        if has_resume {
            info!(
                "📌 Starting Agent with resume: session_id={:?}",
                start_config.resume_session_id
            );
        }

        // 启动 Agent (SACP 版本)
        // 如果 resume 失败，直接返回错误，让上层（rcoder）决定是否降级重试
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
    /// - `model_provider`: 模型提供者配置
    /// - `start_config`: Agent 启动配置
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    /// - `bool`: 是否是新创建的会话
    ///
    /// # 并发安全性
    ///
    /// 使用"检查-创建-插入"三阶段模式避免在持有 entry 期间调用 `.await`：
    ///
    /// 1. **快速检查**：检查会话是否已存在且有效
    /// 2. **创建会话**：如果需要创建，在**不持有锁**的情况下创建会话（.await）
    /// 3. **原子性插入**：使用 entry API 原子性插入，如果其他线程已创建则使用已存在的
    ///
    /// 这样确保：
    /// - 不会在持有 DashMap entry 期间跨越 await 点
    /// - 同一 project_id 最多只会创建一个会话
    /// - 高并发下不会阻塞其他 project_id 的访问（DashMap 分段锁特性）
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        let project_id_key = project_id.to_string();

        // Phase A：session_id_hint 快速复用。不可复用（通道死/模型变）时统一进入
        // 下方的创建合流——停旧由 Leader 独占执行（并发下只有一人停+建，不变式
        // 从概率性变确定性）。
        if let Some(ref hint_sid) = session_id_hint
            && let Some(existing) = self.registry.get_entry_by_session(hint_sid)
            && existing.project_id() == project_id
            && !existing.is_channel_closed()
            && !existing.is_model_config_changed(&model_provider)
        {
            info!(
                "[SESSION] Reusing existing session via session_id_hint: project_id={}, session_id={}",
                project_id, hint_sid
            );
            return Ok((existing, false));
        }

        // Phase B：project entry 快速复用（Pending 占位符除外——它需要被替换）
        if let Some(existing) = self.registry.get(&project_id_key)
            && *existing.status() != AgentStatus::Pending
            && !existing.is_channel_closed()
            && !existing.is_model_config_changed(&model_provider)
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
        let timeout =
            Duration::from_secs(start_config.acp_session_create_timeout_secs.unwrap_or(60));
        match role {
            SessionCreateRole::Leader(handle) => {
                self.create_as_leader(
                    handle,
                    &project_id_key,
                    project_path,
                    model_provider,
                    start_config,
                    service_uuid,
                )
                .await
            }
            SessionCreateRole::Follower(handle) => {
                self.join_as_follower(handle, &project_id_key, timeout, &model_provider)
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
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        let mut guard = SessionCreateGuard {
            map: self.in_flight.clone(),
            key: project_id.to_string(),
            handle: handle.clone(),
            outcome: None,
        };
        let result = self
            .create_session_leader_path(
                project_id,
                project_path,
                model_provider,
                start_config,
                service_uuid,
            )
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
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool), String> {
        // 停旧（若需）：Pending 占位符（dummy 通道）跳过；通道死/模型变才停。
        // model_changed 停旧是切模型正确性的关键：双 opencode 进程同时持有同一
        // session 会导致新进程 prompt 立即 service failure（切模型场景实测）。
        if let Some(existing) = self.registry.get(project_id)
            && *existing.status() != AgentStatus::Pending
        {
            let session_id_str = existing.session_id().to_string();
            let channel_closed = existing.is_channel_closed();
            let model_changed = existing.is_model_config_changed(&model_provider);
            if channel_closed {
                info!(
                    "⚠️ [SESSION] Session channel closed, rebuilding: project_id={}, old session_id={}",
                    project_id, session_id_str
                );
            }
            if model_changed {
                info!(
                    "🔄 [SESSION] Model config changed, restarting Agent session: project_id={}, old session_id={}",
                    project_id, session_id_str
                );
                // 🔪 显式停止旧 Agent 进程（graceful_stop 内部 CAS 幂等）
                if let Some(handle) = existing.lifecycle_handle() {
                    if let Err(e) = handle.graceful_stop().await {
                        warn!(
                            "[SESSION] graceful_stop old agent failed (continuing rebuild): {}",
                            e
                        );
                    } else {
                        info!(
                            "[SESSION] old agent stopped before rebuild: project_id={}, old session_id={}",
                            project_id, session_id_str
                        );
                    }
                }
            }
        }

        // 创建（spawn agent 进程 + 连接任务，阻塞至 session 就绪）
        let new_session = self
            .create_session_internal(
                project_id.to_string(),
                project_path,
                model_provider,
                start_config,
                service_uuid,
            )
            .await
            .map_err(|e| e.to_string())?;

        // 全量提交（三 map：覆盖 Pending 占位符/死 entry，正反向映射一致）
        let new_session_id = new_session.session_id().to_string();
        self.registry
            .insert(project_id, &new_session_id, new_session.clone());
        info!(
            "✅ [SESSION] Session committed (single-flight leader): project_id={}, session_id={}",
            project_id, new_session_id
        );
        Ok((new_session, true))
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
                if entry.is_channel_closed() || entry.is_model_config_changed(model_provider) {
                    return Err(anyhow::anyhow!(
                        "in-flight session outcome stale (dead channel or model mismatch), caller retry will lead: project_id={}",
                        project_id
                    ));
                }
                Ok((entry, is_new))
            })
    }
}

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
mod prompt_channel_tests {
    use super::*;
    use crate::acp::CancelNotificationRequestWrapper;
    use crate::session::session_manager::send_prompt_to_entry;
    use agent_client_protocol::schema::v1::{PromptRequest, SessionId};
    use chrono::Utc;
    use shared_types::{AgentStatus, ModelProviderConfig, ProjectAndAgentInfo};
    use tokio::sync::mpsc;

    /// 构造仅含通道语义的测试 entry（lifecycle 等与本测试无关字段留 None）
    fn test_entry(
        project_id: &str,
        prompt_tx: mpsc::Sender<PromptRequest>,
        cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
    ) -> ProjectAndAgentInfo {
        let now = Utc::now();
        ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::from("ses_test"),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: now,
            created_at: now,
            stop_handle: None,
            agent_binary_snapshot: None,
        }
    }

    /// 通道接收端存活的 entry：is_channel_closed 必须为 false——
    /// get_or_create_session 第三阶段据此裁决"用 existing"，语义锁死
    #[test]
    fn live_channel_is_not_closed() {
        let (prompt_tx, prompt_rx) = mpsc::channel(1);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let entry = test_entry("p1", prompt_tx, cancel_tx);
        assert!(!entry.is_channel_closed());
        drop(prompt_rx); // 防优化告警：显式持有到断言后
    }

    /// 接收端 drop（连接任务退出）→ is_channel_closed 为 true——
    /// 第三阶段据此触发"替换死 entry"，SendError 修复的裁决依据
    #[test]
    fn dropped_receiver_marks_channel_closed() {
        let (prompt_tx, prompt_rx) = mpsc::channel(1);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let entry = test_entry("p1", prompt_tx, cancel_tx);
        drop(prompt_rx);
        assert!(entry.is_channel_closed());
    }

    /// send_prompt_to_entry 对死通道的错误必须可 downcast 到
    /// mpsc SendError——acp_worker 的重试判定（修复 3）依赖此类型链
    #[tokio::test]
    async fn send_to_dead_entry_error_downcasts_to_send_error() {
        let (prompt_tx, prompt_rx) = mpsc::channel(1);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        drop(prompt_rx);
        let entry = test_entry("p1", prompt_tx, cancel_tx);

        let request = PromptRequest::new(SessionId::from("ses_test"), Vec::new());
        // Err 类型即 SendError（函数签名保证），Err 即"通道死亡"——修复 3 的重试依据
        send_prompt_to_entry(&entry, request)
            .await
            .expect_err("dead channel must error");
    }

    /// 活通道直发成功（修复 2 的主路径）
    #[tokio::test]
    async fn send_to_live_entry_succeeds() {
        let (prompt_tx, mut prompt_rx) = mpsc::channel(1);
        let (cancel_tx, _cancel_rx) = mpsc::channel(1);
        let entry = test_entry("p1", prompt_tx, cancel_tx);

        let request = PromptRequest::new(SessionId::from("ses_test"), Vec::new());
        send_prompt_to_entry(&entry, request)
            .await
            .expect("live channel send");
        assert!(prompt_rx.try_recv().is_ok());
    }

    // ModelProviderConfig 引用占位（构造完整 entry 的后续场景用）
    #[allow(dead_code)]
    fn _model_config_marker(_: Option<ModelProviderConfig>) {}
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
