//! SubscribeProgress RPC 实现

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use shared_types::grpc::{ProgressEvent, ProgressRequest};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use crate::router::AppState;
use crate::service::SESSION_CACHE;
use crate::service::agent_registry::AGENT_REGISTRY;

use super::locale::locale_from_grpc_request;

pub type SubscribeProgressStream =
    Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

/// 空闲超时时间（30分钟）
/// 如果在这个时间内没有收到任何真实消息（不包括心跳），则关闭流
const IDLE_TIMEOUT_SECS: u64 = 1800;

/// 空连接主动断开阈值（session/project 维度）：
/// 持续无真实消息且该 session 的 agent 处于 Idle（或未注册）→ 发
/// SessionPromptEnd（前端约定的结束消息，客户端收到即停）并断开——
/// 消除"任务早已结束、客户端才连上 SSE"的空等（否则要挂到 30 分钟）。
/// 双条件互补：无消息排除流式输出中的连接；agent 状态排除静默长任务
/// （执行长命令/深度思考时无消息但 Active）。**并发竞态由状态机吸收**：
/// chat 刚到时 registry 置 Pending（正在启动）——Active/Pending 均不断，
/// 120s 阈值是状态滞后的第二道保险。
const NO_ACTIVE_TASK_DISCONNECT_SECS: u64 = 120;

/// 流退出原因，每个变体对应一个明确的退出路径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    /// 用户主动取消
    Cancelled,
    /// 客户端断开连接（tx.send 失败）
    ClientDisconnected,
    /// 收到 SessionPromptEnd 终止消息
    TerminalMessage,
    /// 消息通道关闭（receiver 返回 None）
    ChannelClosed,
    /// 空闲超时
    IdleTimeout,
    /// 心跳发送失败（客户端断开）
    HeartbeatFailed,
    /// 空连接断开（无消息 + 该 session 的 agent idle）
    NoActiveTask,
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::ClientDisconnected => write!(f, "client_disconnected"),
            Self::TerminalMessage => write!(f, "terminal_message"),
            Self::ChannelClosed => write!(f, "channel_closed"),
            Self::IdleTimeout => write!(f, "idle_timeout"),
            Self::HeartbeatFailed => write!(f, "heartbeat_failed"),
            Self::NoActiveTask => write!(f, "no_active_task"),
        }
    }
}

#[instrument(skip(_app_state, request))]
pub async fn subscribe_progress(
    _app_state: &Arc<AppState>,
    request: Request<ProgressRequest>,
) -> Result<Response<SubscribeProgressStream>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        let session_id = req.session_id.clone();
        // 订阅方的消费游标：只回放 seq > from_seq 的消息（增量补齐）。
        // 缺省（None / 0）= 全量回放（向后兼容）。
        let from_seq = req.from_seq.unwrap_or(0);

        info!(
            "[gRPC] SubscribeProgress started: session_id={}",
            session_id
        );

        let (tx, rx) = mpsc::channel::<Result<ProgressEvent, Status>>(100);
        let session_id_clone = session_id.clone();

        tokio::spawn(async move {
            use dashmap::mapref::entry::Entry;

            info!(
                "[gRPC] Looking up session in SESSION_CACHE: session_id={}",
                session_id_clone
            );
            // 🛡️ 关键修复：不在 DashMap entry() 持锁范围内调用 .await
            // view() 在闭包返回后立即释放锁，无 Ref 暴露
            let session_data = if let Some(existing) = SESSION_CACHE.view(&session_id_clone, |_, d| d.clone()) {
                info!(
                    "[gRPC] SESSION_CACHE found, reusing: session_id={}",
                    session_id_clone
                );
                existing
            } else {
                info!(
                    "[gRPC] SESSION_CACHE not found, creating new SessionData: session_id={}",
                    session_id_clone
                );
                let session_data = crate::service::SessionData::new(1000).await;
                match SESSION_CACHE.entry(session_id_clone.clone()) {
                    Entry::Occupied(entry) => {
                        info!(
                            "[gRPC] SESSION_CACHE exists after creation, reusing: session_id={}",
                            session_id_clone
                        );
                        entry.get().clone()
                    }
                    Entry::Vacant(entry) => {
                        info!(
                            "[gRPC] SESSION_CACHE created successfully: session_id={}",
                            session_id_clone
                        );
                        entry.insert(session_data.clone());
                        session_data
                    }
                }
            };

            info!(
                "[gRPC] Creating new SSE connection: session_id={}",
                session_id_clone
            );
            match session_data.create_new_connection(100, from_seq).await {
                Ok((conn_id, replay_messages, message_rx, cancellation_token)) => {
                    info!(
                        "[gRPC] Session connection created successfully: session_id={}, replay_count={}",
                        session_id_clone, replay_messages.len()
                    );

                    // 📼 回放 ring buffer 中的历史消息
                    for (seq, msg) in replay_messages {
                        let event = unified_message_to_progress_event(seq, &msg);
                        debug!(
                            "[gRPC] Replaying ProgressEvent: session_id={}, message_type={}, sub_type={}, payload={}",
                            session_id_clone, event.message_type, event.sub_type, event.payload
                        );
                        if tx.send(Ok(event)).await.is_err() {
                            debug!("[gRPC] Client disconnected during replay");
                            session_data.close_connection(conn_id);
                            return;
                        }
                    }

                    let reason = run_stream_loop(
                        &session_id_clone,
                        message_rx,
                        cancellation_token,
                        &tx,
                        locale,
                    )
                    .await;

                    info!(
                        "[gRPC] SubscribeProgress stream ended, cleaning up SSE sender: session_id={}, conn_id={conn_id}, reason={}",
                        session_id_clone, reason
                    );
                    session_data.close_connection(conn_id);
                }
                Err(e) => {
                    warn!("[gRPC] Failed to create session connection: {}", e);
                    if let Err(send_err) = tx
                        .send(Err(Status::internal(format!(
                            "{}: {}",
                            shared_types_i18n::get_i18n_message("grpc.subscribe.create_connection_failed", locale),
                            e
                        ))))
                        .await
                    {
                        warn!(
                            "[gRPC] Failed to send error status: session_id={}, error={}",
                            session_id_clone, send_err
                        );
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(stream) as SubscribeProgressStream
        ))
    })
    .await
}

/// 运行 SSE 流主循环，返回退出原因
///
/// 每个分支通过 `return` 显式返回 `ExitReason`，编译器可追踪所有路径。
/// 独立函数避免了 `tokio::select!` 宏展开导致的 definite-assignment 盲区。
async fn run_stream_loop(
    session_id: &str,
    mut message_rx: mpsc::Receiver<(u64, shared_types::UnifiedSessionMessage)>,
    cancellation_token: tokio_util::sync::CancellationToken,
    tx: &mpsc::Sender<Result<ProgressEvent, Status>>,
    locale: &'static str,
) -> ExitReason {
    // tokio Instant：运行时与 std 等价；paused 时钟下可推进（本循环可单测）
    let mut last_message_time = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                info!("[gRPC] Session connection cancelled, sending SessionPromptEnd: session_id={}", session_id);

                use agent_client_protocol::schema::v1::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.to_string(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some(shared_types_i18n::get_i18n_message("grpc.subscribe.user_cancelled", locale)),
                    request_id: None,
                });
                let unified_message = notify.to_unified_message();
                let end_event = unified_message_to_progress_event(0, &unified_message);

                if let Err(e) = tx.send(Ok(end_event)).await {
                    warn!("[gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id, e);
                }

                return ExitReason::Cancelled;
            }
            msg = message_rx.recv() => {
                match msg {
                    Some((seq, unified_message)) => {
                        // 重置空闲超时计时器
                        last_message_time = tokio::time::Instant::now();

                        // 终端关流只认 end_turn/error：cancelled 仅是本轮中断，
                        // agent 随后可能继续执行（切模型场景：迟到的取消余波
                        // PromptEnd(Cancelled) 曾把活跃订阅误杀，事件全部退化为
                        // ring buffer 积压，客户端空流）。cancelled 后由下一个
                        // end_turn 或空闲超时自然收流。
                        let is_terminal_message = matches!(
                            unified_message.message_type,
                            crate::model::SessionMessageType::SessionPromptEnd
                        ) && matches!(
                            unified_message.sub_type.as_str(),
                            "end_turn" | "error"
                        );

                        let event = unified_message_to_progress_event(seq, &unified_message);
                        debug!(
                            "[gRPC] Sending ProgressEvent: session_id={}, message_type={}, sub_type={}, payload={}",
                            session_id, event.message_type, event.sub_type, event.payload
                        );
                        if tx.send(Ok(event)).await.is_err() {
                            debug!("[gRPC] Client disconnected");
                            return ExitReason::ClientDisconnected;
                        }

                        if is_terminal_message {
                            info!(
                                "[gRPC] Received SessionPromptEnd, closing stream: session_id={}, sub_type={}",
                                session_id, unified_message.sub_type
                            );
                            return ExitReason::TerminalMessage;
                        }
                    }
                    None => {
                        debug!("[gRPC] Session channel closed, sending SessionPromptEnd event");
                        let end_event = ProgressEvent {
                            message_type: "SessionPromptEnd".to_string(),
                            sub_type: "end_turn".to_string(),
                            payload: format!(
                                r#"{{"reason":"EndTurn","description":"{}"}}"#,
                                shared_types_i18n::get_i18n_message("grpc.subscribe.no_active_task", locale)
                            ),
                            request_id: None,
                            seq: 0,
                            timestamp: chrono::Utc::now().timestamp_millis(),
                        };
                        if let Err(e) = tx.send(Ok(end_event)).await {
                            warn!("[gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id, e);
                        }
                        return ExitReason::ChannelClosed;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                // 空连接主动断开（session/project 维度，先于 30 分钟兜底）：
                // 持续无真实消息 + 该 session 的 agent Idle（或未注册）→ 发
                // SessionPromptEnd（前端约定结束消息）断开。Active/Pending
                // （含静默长任务与刚启动窗口）不重置计时——状态由下个 tick
                // 重新裁决，消息到达时才刷新计时。
                if last_message_time.elapsed() >= Duration::from_secs(NO_ACTIVE_TASK_DISCONNECT_SECS)
                    && AGENT_REGISTRY
                        .view_agent_info_by_session(session_id, |info| info.status)
                        .is_none_or(|st| st == shared_types::AgentStatus::Idle)
                {
                    info!(
                        "[gRPC] No active task (idle {}s+, agent idle, no real message), closing stream: session_id={}",
                        NO_ACTIVE_TASK_DISCONNECT_SECS, session_id
                    );
                    let end_event = ProgressEvent {
                        message_type: "SessionPromptEnd".to_string(),
                        sub_type: "end_turn".to_string(),
                        payload: format!(
                            r#"{{"reason":"NoActiveTask","description":"{}"}}"#,
                            shared_types_i18n::get_i18n_message("grpc.subscribe.no_active_task", locale)
                        ),
                        request_id: None,
                        seq: 0,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    };
                    if let Err(e) = tx.send(Ok(end_event)).await {
                        warn!("subscribe_progress event send failed (subscriber gone): {e}");
                    }
                    return ExitReason::NoActiveTask;
                }
                // 检查空闲超时
                if last_message_time.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    info!(
                        "[gRPC] Idle timeout ({}s) reached, closing stream: session_id={}",
                        IDLE_TIMEOUT_SECS, session_id
                    );
                    let timeout_event = ProgressEvent {
                        message_type: "SessionPromptEnd".to_string(),
                        sub_type: "idle_timeout".to_string(),
                        payload: format!(
                            r#"{{"reason":"IdleTimeout","description":"{}"}}"#,
                            shared_types_i18n::get_i18n_message("grpc.subscribe.idle_timeout", locale)
                        ),
                        request_id: None,
                        seq: 0,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    };
                    if let Err(e) = tx.send(Ok(timeout_event)).await {
                        warn!("subscribe_progress event send failed (subscriber gone): {e}");
                    }
                    return ExitReason::IdleTimeout;
                }

                let heartbeat = ProgressEvent {
                    message_type: "Heartbeat".to_string(),
                    sub_type: "ping".to_string(),
                    payload: r#"{"type":"heartbeat","message":"keep-alive"}"#.to_string(),
                    request_id: None,
                    seq: 0,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };

                if tx.send(Ok(heartbeat)).await.is_err() {
                    debug!("[gRPC] Failed to send heartbeat; client disconnected");
                    return ExitReason::HeartbeatFailed;
                }
            }
        }
    }
}

fn unified_message_to_progress_event(
    seq: u64,
    message: &shared_types::UnifiedSessionMessage,
) -> ProgressEvent {
    let timestamp = message.timestamp.timestamp_millis();

    ProgressEvent {
        message_type: format!("{:?}", message.message_type),
        sub_type: message.sub_type.clone(),
        payload: serde_json::to_string(&message.data).unwrap_or_default(),
        request_id: message
            .data
            .get("request_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        seq,
        timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_agent_info(status: shared_types::AgentStatus) -> shared_types::ProjectAndAgentInfo {
        shared_types::ProjectAndAgentInfo {
            project_id: "proj-t".to_string(),
            session_id: agent_client_protocol::schema::v1::SessionId::new(Arc::from("ses-t")),
            prompt_tx: mpsc::channel(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY).0,
            cancel_tx: mpsc::channel(shared_types::AGENT_CANCEL_CHANNEL_CAPACITY).0,
            model_provider: None,
            request_id: None,
            status,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
            agent_binary_snapshot: None,
        }
    }

    /// 空连接断开：session 无注册 agent（None）+ 不喂消息 → 先收心跳保活，
    /// NO_ACTIVE_TASK_DISCONNECT_SECS 后收 SessionPromptEnd/end_turn 并以
    /// NoActiveTask 退出（paused 时钟自动推进 30s tick）。
    #[tokio::test(start_paused = true)]
    async fn idle_subscription_without_agent_closes_with_prompt_end() {
        let (_msg_tx, msg_rx) = mpsc::channel(4);
        let (ev_tx, mut ev_rx) = mpsc::channel(32);
        let cancel = tokio_util::sync::CancellationToken::new();

        let handle = tokio::spawn(async move {
            let tx = ev_tx.clone();
            run_stream_loop("ses-none", msg_rx, cancel, &tx, "en-US").await
        });

        let reason = handle.await.unwrap();
        assert_eq!(reason, ExitReason::NoActiveTask);

        // 排干心跳后，最后一条必须是 SessionPromptEnd/end_turn（前端约定的
        // 结束消息形态——收到即不再重连）
        let mut last = None;
        while let Ok(ev) = ev_rx.try_recv() {
            last = Some(ev.unwrap());
        }
        let last = last.expect("at least the terminal event");
        assert_eq!(last.message_type, "SessionPromptEnd");
        assert_eq!(last.sub_type, "end_turn");
    }

    /// Active 保护：agent 在跑（即使静默无消息）不得触发空连接断开——
    /// cancel 收尾证明循环存活到了外部取消。
    #[tokio::test(start_paused = true)]
    async fn active_agent_keeps_silent_subscription_alive() {
        AGENT_REGISTRY.register(
            "proj-active",
            "ses-active",
            make_agent_info(shared_types::AgentStatus::Active),
        );
        let (_msg_tx, msg_rx) = mpsc::channel(4);
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        let cancel = tokio_util::sync::CancellationToken::new();

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            let tx = ev_tx.clone();
            let reason = run_stream_loop("ses-active", msg_rx, cancel_clone, &tx, "en-US").await;
            drop(ev_tx);
            reason
        });

        // 让循环跑过若干个 30s tick（远超 NO_ACTIVE_TASK_DISCONNECT_SECS）
        tokio::time::advance(Duration::from_secs(600)).await;
        cancel.cancel();
        let reason = handle.await.unwrap();
        assert_eq!(
            reason,
            ExitReason::Cancelled,
            "active agent must not be disconnected"
        );
        // 期间只有心跳 + cancel 分支自身的 SessionPromptEnd（收尾终态）；
        // 绝不能出现 no-active-task 断开的 end_turn 形态
        while let Ok(ev) = ev_rx.try_recv() {
            let ev = ev.unwrap();
            assert_ne!(
                (ev.message_type.as_str(), ev.sub_type.as_str()),
                ("SessionPromptEnd", "end_turn"),
                "no-active-task disconnect must not fire while agent is active"
            );
        }
        AGENT_REGISTRY.remove_by_project("proj-active");
    }

    /// Pending 保护（并发竞态窗口）：chat 刚到、agent 正在启动——即使状态
    /// 滞后零消息也不得断流。
    #[tokio::test(start_paused = true)]
    async fn pending_agent_keeps_subscription_alive() {
        AGENT_REGISTRY.register(
            "proj-pending",
            "ses-pending",
            make_agent_info(shared_types::AgentStatus::Pending),
        );
        let (_msg_tx, msg_rx) = mpsc::channel(4);
        let (ev_tx, _ev_rx) = mpsc::channel(64);
        let cancel = tokio_util::sync::CancellationToken::new();

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            let tx = ev_tx.clone();
            run_stream_loop("ses-pending", msg_rx, cancel_clone, &tx, "en-US").await
        });

        tokio::time::advance(Duration::from_secs(600)).await;
        cancel.cancel();
        let reason = handle.await.unwrap();
        assert_eq!(
            reason,
            ExitReason::Cancelled,
            "pending (starting) agent must not be disconnected"
        );
        AGENT_REGISTRY.remove_by_project("proj-pending");
    }
}
