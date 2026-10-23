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
#[cfg(test)]
use crate::service::SESSION_CACHE;
use crate::service::agent_registry::AGENT_REGISTRY;
use crate::service::{get_or_create_session_data, require_cache_session_id};

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
        require_cache_session_id(&session_id)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let session_data = get_or_create_session_data(&session_id)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        // 订阅方的消费游标：只回放 seq > from_seq 的消息（增量补齐）。
        // 缺省（None / 0）= 全量回放（向后兼容）。
        let from_seq = req.from_seq.unwrap_or(0);

        info!(
            "[gRPC] SubscribeProgress started: session_id={}",
            session_id
        );

        let (tx, rx) = mpsc::channel::<Result<ProgressEvent, Status>>(100);
        let session_id_clone = session_id.clone();

        // 观测接入（dial9）：订阅中段转发任务经 rcoder-obs 门面 spawn（feature
        // 关=直通 tokio::spawn）。locale 已显式捕获进闭包、连接清理
        // （close_connection）与取消 token 逻辑不变。
        rcoder_obs::spawn(async move {
            info!(
                "[gRPC] Creating new SSE connection: session_id={}",
                session_id_clone
            );
            match session_data.create_new_connection_with_terminal(100, from_seq).await {
                Ok(subscription) => {
                    let conn_id = subscription.conn_id;
                    let replay_messages = subscription.replay_messages;
                    let message_rx = subscription.rx;
                    let cancellation_token = subscription.token;
                    let terminal = subscription.terminal;
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
                        Some(terminal),
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
    terminal: Option<tokio::sync::watch::Receiver<Option<shared_types::UnifiedSessionMessage>>>,
    tx: &mpsc::Sender<Result<ProgressEvent, Status>>,
    locale: &'static str,
) -> ExitReason {
    // tokio Instant：运行时与 std 等价；paused 时钟下可推进（本循环可单测）
    let mut last_message_time = tokio::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                info!("[gRPC] Session connection cancelled, sending SessionPromptEnd: session_id={}", session_id);

                use agent_client_protocol::schema::v1::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let captured = terminal.as_ref().and_then(|terminal| terminal.borrow().clone());
                let unified_message = captured.unwrap_or_else(|| SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.to_string(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some(shared_types_i18n::get_i18n_message("grpc.subscribe.user_cancelled", locale)),
                    request_id: None,
                }).to_unified_message());
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

                        // 终端关流判定用共享契约（shared_types）：end_turn/error/
                        // max_tokens/max_turn_requests/refusal 都是本轮真实终态;
                        // cancelled 仅是本轮中断，agent 随后可能继续执行（切模型
                        // 场景：迟到的取消余波 PromptEnd(Cancelled) 曾把活跃订阅
                        // 误杀，事件全部退化为 ring buffer 积压，客户端空流）。
                        // cancelled 后由下一个终态或空闲超时自然收流。
                        let is_terminal_message = matches!(
                            unified_message.message_type,
                            crate::model::SessionMessageType::SessionPromptEnd
                        ) && shared_types::is_prompt_terminal_sub_type(&unified_message.sub_type);

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
                        if let Some(captured) = terminal.as_ref().and_then(|terminal| terminal.borrow().clone()) {
                            if let Err(error) = tx.send(Ok(unified_message_to_progress_event(0, &captured))).await {
                                warn!(session_id, %error, "failed to send captured channel-close terminal");
                            }
                            return ExitReason::ChannelClosed;
                        }
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

    #[tokio::test]
    async fn queued_control_terminal_token_fallback_keeps_captured_request_id() {
        use super::super::cleanup::CapturedSessionCleanup;
        let sid = format!("control-token-fallback-{}", uuid::Uuid::new_v4());
        let data = crate::service::SessionData::new(64).await;
        let old = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        SESSION_CACHE.insert(sid.clone(), data.clone());
        let cleanup = CapturedSessionCleanup::capture(&sid, Some("original-control-rid".into()));
        let new = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        // Push 只入队，未开始 poll 流即关闭；token 分支先于消息读取。
        let terminal = cleanup
            .send_prompt_end(
                agent_client_protocol::schema::v1::StopReason::Cancelled,
                None,
            )
            .await;
        cleanup.close(&terminal);
        let (tx, mut rx) = mpsc::channel(8);
        let reason =
            run_stream_loop(&sid, old.rx, old.token, Some(old.terminal), &tx, "en-US").await;
        assert_eq!(reason, ExitReason::Cancelled);
        let event = rx.recv().await.unwrap().unwrap();
        assert_eq!(
            event.seq, 0,
            "the token fallback, not sequenced Push, must win"
        );
        assert_eq!(event.sub_type, "cancelled");
        assert_eq!(event.request_id.as_deref(), Some("original-control-rid"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&event.payload).unwrap()["request_id"],
            "original-control-rid"
        );
        assert!(!new.token.is_cancelled());
        assert!(new.terminal.borrow().is_none());
        SESSION_CACHE.remove(&sid);
    }

    #[tokio::test]
    async fn backpressured_control_terminal_records_identity_before_worker_cancel() {
        use super::super::cleanup::CapturedSessionCleanup;
        let sid = format!("control-full-fallback-{}", uuid::Uuid::new_v4());
        let data = crate::service::SessionData::new(64).await;
        let old = data
            .create_new_connection_with_terminal(1, 0)
            .await
            .unwrap();
        SESSION_CACHE.insert(sid.clone(), data.clone());
        data.push_message(
            shared_types::SessionNotify::SessionPromptStart(shared_types::SessionPromptStart {
                session_id: sid.clone(),
                request_id: Some("queued-original-rid".into()),
            })
            .to_unified_message(),
        )
        .await
        .unwrap();
        assert_eq!(data.message_count().await, 1); // 满队列保留第一条，终态无法 try_send。
        let cleanup = CapturedSessionCleanup::capture(&sid, Some("queued-original-rid".into()));
        let new = data
            .create_new_connection_with_terminal(8, 0)
            .await
            .unwrap();
        let terminal = cleanup
            .send_prompt_end(
                agent_client_protocol::schema::v1::StopReason::Cancelled,
                None,
            )
            .await;
        let _ = data.message_count().await; // worker 处理 terminal 后移除慢连接并 cancel。
        assert!(old.token.is_cancelled());
        assert_eq!(
            old.terminal.borrow().as_ref().unwrap().data["request_id"],
            "queued-original-rid"
        );
        cleanup.close(&terminal); // 此时旧 conn 已不在 registry 中，不能靠这里补写 RID。
        let (tx, mut rx) = mpsc::channel(8);
        let reason =
            run_stream_loop(&sid, old.rx, old.token, Some(old.terminal), &tx, "en-US").await;
        assert_eq!(reason, ExitReason::Cancelled);
        assert_eq!(
            rx.recv().await.unwrap().unwrap().request_id.as_deref(),
            Some("queued-original-rid")
        );
        assert!(!new.token.is_cancelled());
        assert!(new.terminal.borrow().is_none());
        SESSION_CACHE.remove(&sid);
    }

    #[tokio::test]
    async fn channel_closed_fallback_uses_captured_terminal_instead_of_latest_request() {
        let (sender, receiver) = mpsc::channel(1);
        drop(sender);
        let terminal =
            shared_types::SessionNotify::SessionPromptEnd(shared_types::SessionPromptEnd {
                session_id: "captured-channel-close".into(),
                stop_reason: agent_client_protocol::schema::v1::StopReason::Cancelled,
                error_message: None,
                request_id: Some("captured-channel-rid".into()),
            })
            .to_unified_message();
        let (_terminal_sender, terminal_receiver) = tokio::sync::watch::channel(Some(terminal));
        let (tx, mut rx) = mpsc::channel(8);
        let reason = run_stream_loop(
            "captured-channel-close",
            receiver,
            tokio_util::sync::CancellationToken::new(),
            Some(terminal_receiver),
            &tx,
            "en-US",
        )
        .await;
        assert_eq!(reason, ExitReason::ChannelClosed);
        let event = rx.recv().await.unwrap().unwrap();
        assert_eq!(event.sub_type, "cancelled");
        assert_eq!(event.request_id.as_deref(), Some("captured-channel-rid"));
    }

    /// R06 反例: 真实 notifier 会产生 max_tokens/max_turn_requests/refusal 终态;
    /// 只认 end_turn/error 时, 真实 Prompt 已结束流仍悬挂, 通道关闭后还会合成
    /// 第二个 end_turn。修复后应及时以 TerminalMessage 收流、末事件保持原 sub_type。
    #[tokio::test]
    async fn quota_and_refusal_stop_reasons_close_the_subscription_stream() {
        use agent_client_protocol::schema::v1::StopReason;

        fn prompt_end(reason: StopReason) -> shared_types::UnifiedSessionMessage {
            shared_types::SessionNotify::SessionPromptEnd(shared_types::SessionPromptEnd {
                session_id: "ses-r06".into(),
                stop_reason: reason,
                error_message: None,
                request_id: Some("rid-r06".into()),
            })
            .to_unified_message()
        }

        for (reason, expected_sub) in [
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::MaxTurnRequests, "max_turn_requests"),
            (StopReason::Refusal, "refusal"),
            (StopReason::EndTurn, "end_turn"),
        ] {
            let (msg_tx, msg_rx) = mpsc::channel(4);
            let (ev_tx, mut ev_rx) = mpsc::channel(32);
            let end = prompt_end(reason);
            msg_tx.send((7, end)).await.unwrap();
            drop(msg_tx);
            let outcome = run_stream_loop(
                "ses-r06",
                msg_rx,
                tokio_util::sync::CancellationToken::new(),
                None,
                &ev_tx,
                "en-US",
            )
            .await;
            drop(ev_tx); // 排空前释放发送端, recv 才会终止
            assert_eq!(outcome, ExitReason::TerminalMessage, "{expected_sub}");
            let mut events = Vec::new();
            while let Some(ev) = ev_rx.recv().await {
                events.push(ev.unwrap());
            }
            assert_eq!(
                events.len(),
                1,
                "{expected_sub}: no synthetic second terminal may follow"
            );
            assert_eq!(events[0].seq, 7, "{expected_sub}");
            assert_eq!(events[0].message_type, "SessionPromptEnd", "{expected_sub}");
            assert_eq!(events[0].sub_type, expected_sub);
        }
    }

    /// R06 边界: cancelled 不关流——cancelled 后新的 PromptStart 仍能到达,
    /// 由后续 end_turn 正常收流。
    #[tokio::test]
    async fn cancelled_keeps_stream_open_for_next_prompt_start() {
        use agent_client_protocol::schema::v1::StopReason;

        let sid = "ses-r06-cancelled";
        let (msg_tx, msg_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(32);
        let messages = [
            shared_types::SessionNotify::SessionPromptStart(shared_types::SessionPromptStart {
                session_id: sid.into(),
                request_id: Some("r1".into()),
            })
            .to_unified_message(),
            shared_types::SessionNotify::SessionPromptEnd(shared_types::SessionPromptEnd {
                session_id: sid.into(),
                stop_reason: StopReason::Cancelled,
                error_message: None,
                request_id: Some("r1".into()),
            })
            .to_unified_message(),
            shared_types::SessionNotify::SessionPromptStart(shared_types::SessionPromptStart {
                session_id: sid.into(),
                request_id: Some("r2".into()),
            })
            .to_unified_message(),
            shared_types::SessionNotify::SessionPromptEnd(shared_types::SessionPromptEnd {
                session_id: sid.into(),
                stop_reason: StopReason::EndTurn,
                error_message: None,
                request_id: Some("r2".into()),
            })
            .to_unified_message(),
        ];
        for (seq, message) in messages.into_iter().enumerate() {
            msg_tx.send((seq as u64 + 1, message)).await.unwrap();
        }
        drop(msg_tx);
        let outcome = run_stream_loop(
            sid,
            msg_rx,
            tokio_util::sync::CancellationToken::new(),
            None,
            &ev_tx,
            "en-US",
        )
        .await;
        drop(ev_tx); // 排空前释放发送端, recv 才会终止
        assert_eq!(outcome, ExitReason::TerminalMessage);
        let mut subs = Vec::new();
        while let Some(ev) = ev_rx.recv().await {
            subs.push(ev.unwrap().sub_type);
        }
        assert_eq!(
            subs,
            vec!["prompt_start", "cancelled", "prompt_start", "end_turn"]
        );
    }
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
            run_stream_loop("ses-none", msg_rx, cancel, None, &tx, "en-US").await
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
            let reason =
                run_stream_loop("ses-active", msg_rx, cancel_clone, None, &tx, "en-US").await;
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
            run_stream_loop("ses-pending", msg_rx, cancel_clone, None, &tx, "en-US").await
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

    /// dial9 spawner 证据测试（observability T3.4/T3.5）：进程内挂 dial9 runtime
    /// 直调 `subscribe_progress`，让 SessionWorker（worker.rs spawn 点）与订阅转发
    /// 任务（本文件 spawn 点）真实运行并产生 instrumented 事件。
    ///
    /// 运行方式（断言由 `tools/dial9-probe` 对 trace 解码完成，本测试只负责
    /// 产生真实事件；缺 env 直接失败，不做空转"通过"）：
    /// ```bash
    /// DIAL9_EVIDENCE_DIR=/tmp/dial9-evidence \
    ///   cargo nextest run -p agent_runner --features dial9 \
    ///   --run-ignored=only dial9_spawn_evidence
    /// tools/dial9-probe --min-instrumented 2 --expect-wake-pairs /tmp/dial9-evidence
    /// ```
    #[cfg(feature = "dial9")]
    #[test]
    #[ignore = "证据测试：需 DIAL9_EVIDENCE_DIR 且 --features dial9（详见测试注释）"]
    fn dial9_spawn_evidence() {
        let evidence_dir = std::env::var("DIAL9_EVIDENCE_DIR")
            .expect("需要 DIAL9_EVIDENCE_DIR=<trace 输出目录>（并用 --run-ignored 显式运行）");
        // SAFETY: 测试进程内单线程环境写 env（dial9 runtime_from_env 读取），
        // 豁免通道仅限测试模块。
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("DIAL9_ENABLED", "1");
            std::env::set_var("DIAL9_TRACE_DIR", &evidence_dir);
        }

        let (recorder, runtime) = crate::dial9_obs::runtime_from_env().expect("dial9 runtime");
        runtime.block_on(async {
            let state = std::sync::Arc::new(evidence_app_state());
            let request = Request::new(ProgressRequest {
                session_id: "dial9-evidence-session".to_string(),
                from_seq: Some(0),
            });
            let response = subscribe_progress(&state, request)
                .await
                .expect("subscribe_progress establishes");
            use tokio_stream::StreamExt as _;
            let mut stream = response.into_inner();
            // 等 worker 完成 Subscribe 注册；仅有 SESSION_CACHE 条目仍可能
            // 早于注册，终态 Push 若先被消费就会清空 ring，无法向本订阅投递。
            let session_ready = 'ready: {
                for _ in 0..30 {
                    if let Some(session) =
                        SESSION_CACHE.view("dial9-evidence-session", |_, d| d.clone())
                        && session.connections_len() == 1
                    {
                        break 'ready true;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                false
            };
            assert!(session_ready, "worker 应在 3s 内注册本订阅");
            // 真实唤醒链：push 一条消息 → command 通道 send → 唤醒 SessionWorker
            // （worker 从 recv 挂起点被 wake）→ 订阅者通道投递 → 转发任务被 wake。
            let request_id = "dial9-evidence-push";
            crate::service::push_session_update_with_project(
                "proj-dial9-evidence",
                "dial9-evidence-session",
                shared_types::SessionNotify::SessionPromptEnd(shared_types::SessionPromptEnd {
                    session_id: "dial9-evidence-session".to_string(),
                    stop_reason: agent_client_protocol::schema::v1::StopReason::EndTurn,
                    error_message: None,
                    request_id: Some(request_id.to_string()),
                }),
            )
            .await
            .expect("push session update");
            // 收到终态事件（转发任务被唤醒并投递）即证明整条唤醒链真实工作。
            let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("push 后 5s 内应经转发任务收到事件（wake 链断言）")
                .expect("订阅不得在投递本次 push 前关闭")
                .expect("订阅必须投递 ProgressEvent，不能返回 gRPC 错误");
            assert_eq!(event.message_type, "SessionPromptEnd");
            assert_eq!(event.sub_type, "end_turn");
            assert!(event.seq > 0, "必须是 worker 分配游标的真实 push 事件");
            assert_eq!(event.request_id.as_deref(), Some(request_id));
        });
        drop(runtime);
        recorder.graceful_shutdown(Duration::from_secs(5));
        eprintln!(
            "trace 已落 {evidence_dir}；断言：tools/dial9-probe \
             --min-instrumented 2 --expect-wake-pairs {evidence_dir}"
        );
    }

    /// subscribe_progress 不消费 AppState 内容（参数为 `_app_state`），构造
    /// 最小可用实例仅为了让真实签名路径完整执行。
    #[cfg(feature = "dial9")]
    fn evidence_app_state() -> AppState {
        use std::sync::Arc;
        use std::sync::LazyLock;

        static EMPTY_KEYS: LazyLock<
            Arc<dashmap::DashMap<String, shared_types::ModelProviderConfig>>,
        > = LazyLock::new(|| Arc::new(dashmap::DashMap::new()));

        AppState {
            sessions: Arc::new(dashmap::DashMap::new()),
            config: crate::config::AppConfig::default(),
            agent_session_service: Arc::new(crate::service::AgentSessionService::new(
                Arc::new(agent_abstraction::launcher::DirectModelRuntimeEnvResolver),
                60,
            )),
            api_key_manager: Arc::new(crate::api_key_manager::ApiKeyManager::new()),
            shared_api_key_manager: Arc::clone(&EMPTY_KEYS),
            project_uuid_map: Arc::new(dashmap::DashMap::new()),
            agent_mgmt_registry: Arc::new(crate::agent_mgmt::AgentRegistry::empty(
                Default::default(),
            )),
            agent_mgmt_path_manager: Default::default(),
            #[cfg(feature = "proxy")]
            pingora_service: None,
        }
    }
}
