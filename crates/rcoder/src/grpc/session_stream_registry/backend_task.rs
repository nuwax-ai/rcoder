//! 后台 gRPC 接收 task 与合成事件构造（从 session_stream_registry.rs 拆出）。
//!
//! spawn_backend_task 维持「每 session 一条 agent_runner SubscribeProgress 流」：
//! get_client → get_status（epoch 比较）→ subscribe_progress(from_seq=last_seq)
//! → 收事件 dispatch。流断/出错重试（MAX_RETRIES），彻底失败以合成终态事件收尾
//! （经 dispatch_event 统一路径：broadcast + 清 ring）。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::grpc::GrpcChannelPool;
use crate::grpc::locale_metadata::new_request_with_locale;
use crate::grpc::session_stream_registry::{MAX_RETRIES, SharedStream};
use crate::grpc::session_stream_registry::{
    make_cursor_reset_event, make_prompt_end_event, make_stream_error_event,
    make_terminal_error_event,
};
use shared_types::grpc::{GetStatusRequest, ProgressRequest};

/// 启动后台 gRPC 接收 task（一条 agent_runner SubscribeProgress 流）。
///
/// 流程：get_client → get_status(idle 检查) → subscribe_progress(from_seq=last_seq)
/// → 收事件 dispatch。流断/出错重试（max_retries），彻底失败则 broadcast 错误事件后退出。
pub(crate) fn spawn_backend_task(
    shared: Arc<SharedStream>,
    grpc_addr: String,
    pool: Arc<GrpcChannelPool>,
    locale: &'static str,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let session_id = shared.session_id.clone();
        let cancel = shared.cancel_token.clone();
        drop(activity_updater); // activity 已通过 dispatch_event 内部节流调用

        for attempt in 1..=MAX_RETRIES {
            if cancel.is_cancelled() {
                return;
            }
            // 1. 从连接池获取客户端
            let mut client = match pool.get_client(&grpc_addr).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "[SessionStream] get_client failed (attempt {}/{}): {}",
                        attempt, MAX_RETRIES, e
                    );
                    pool.remove(&grpc_addr).await;
                    if attempt < MAX_RETRIES {
                        continue;
                    }
                    // 重试耗尽:必须发终态错误事件,否则 SharedStream 持 sender 不 Closed,
                    // 已连上的 HTTP SSE 客户端会永久 hang 在 recv()。错误文案用【当前】失败(#16a)。
                    let err_ev = make_terminal_error_event(shared.diag_ctx.as_ref(), locale).await;
                    // dispatch_event：broadcast + 终端清 ring（故障路径的半轮残留不得跨轮）
                    shared.dispatch_event(Arc::new(err_ev));
                    return;
                }
            };

            // 2. get_status idle 检查（保留与旧 sse_stream 一致的语义）
            let status_req = new_request_with_locale(
                GetStatusRequest {
                    project_id: String::new(),
                    session_id: session_id.clone(),
                },
                locale,
            );
            match client.get_status(status_req).await {
                Ok(resp) => {
                    let inner = resp.into_inner();
                    info!(
                        "[SessionStream] GetStatus: status={}, is_found={}, session_id={}",
                        inner.status, inner.is_found, session_id
                    );
                    if inner.is_found && inner.status == "idle" {
                        // 不再合成 SessionPromptEnd 断流——实测竞态：chat 发出后立即订阅
                        // SSE（前端标准时序），agent 的 busy 状态翻转滞后于 prompt 传递，
                        // 误判 idle 会杀掉整轮流（prompt_start/chunks 全部收不到）。
                        // idle 的真实代价只是流挂着收 keep-alive，由 IDLE_CLEANUP_SECS
                        // (30s) 兜底清理；真 turn 结束时终端事件会正常广播。
                        info!(
                            "[SessionStream] agent idle at subscribe time, proceeding to subscribe (chat→SSE race tolerated): session_id={}",
                            session_id
                        );
                    }
                    // epoch 比较(#15):同 epoch → 保留 last_seq(增量订阅);
                    // epoch 变化(agent 重启/worker panic 重建)→ 重置 last_seq + 清 ring + cursor-reset
                    if let Some(ref new_epoch) = inner.stream_epoch {
                        let changed = {
                            let mut guard = shared.epoch.lock();
                            match &*guard {
                                None => {
                                    *guard = Some(new_epoch.clone());
                                    false
                                }
                                Some(old) if old == new_epoch => false,
                                Some(_) => {
                                    *guard = Some(new_epoch.clone());
                                    true
                                }
                            }
                        };
                        if changed {
                            warn!(
                                "[SessionStream] epoch changed → reset last_seq + clear ring + cursor-reset: session_id={}",
                                session_id
                            );
                            shared.last_seq.store(0, Ordering::Release);
                            shared.clear_ring();
                            // cursor-reset 哨兵同时进 ring + broadcast：ring 让断线重连客户端
                            // 经 replay_since 收到它（broadcast 只投递订阅后的消息，重连客户端收不到）。
                            let reset_ev = Arc::new(make_cursor_reset_event());
                            shared.push_reset_to_ring(Arc::clone(&reset_ev));
                            if let Err(send_err) = shared.broadcast_tx.send(reset_ev) {
                                warn!(
                                    "[SessionStream] broadcast send failed (no subscriber): {send_err}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[SessionStream] get_status failed, continue to subscribe: {}",
                        e
                    );
                }
            }

            // 3. subscribe_progress，from_seq = last_seq（增量订阅）
            let from_seq = shared.last_seq.load(Ordering::Acquire);
            let req = new_request_with_locale(
                ProgressRequest {
                    session_id: session_id.clone(),
                    from_seq: Some(from_seq),
                },
                locale,
            );
            match client.subscribe_progress(req).await {
                Ok(resp) => {
                    info!(
                        "[SessionStream] SubscribeProgress established: session_id={}, from_seq={}",
                        session_id, from_seq
                    );
                    let mut stream = resp.into_inner();
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                info!("[SessionStream] cancelled, stopping: session_id={}", session_id);
                                return;
                            }
                            msg = stream.message() => match msg {
                                Ok(Some(ev)) => {
                                    debug!(
                                        "[SessionStream] event: session_id={}, seq={}, type={}, sub={}",
                                        session_id, ev.seq, ev.message_type, ev.sub_type
                                    );
                                    shared.dispatch_event(Arc::new(ev));
                                }
                                Ok(None) => {
                                    info!(
                                        "[SessionStream] agent_runner stream ended normally: session_id={}",
                                        session_id
                                    );
                                    // 兜底：若 agent_runner 未推 SessionPromptEnd 就关流，客户端转发 task 会 hang
                                    // （broadcast 不会 Closed，因 SharedStream 持有 sender）。补一个 terminal 事件唤醒退出。
                                    // 走 dispatch_event：agent 侧终端即清可能因流断没送达，ring 里的半轮
                                    // 由这里统一清（终端 seq=0 → dispatch 只 broadcast+清 ring，不污染游标）。
                                    shared.dispatch_event(Arc::new(make_prompt_end_event()));
                                    return;
                                }
                                Err(e) => {
                                    error!(
                                        "[SessionStream] stream error: session_id={}, code={}, msg={}",
                                        session_id, e.code(), e.message()
                                    );
                                    // 有 epoch 时:不在此重置 last_seq(下次 GetStatus epoch 比较决定,#15)。
                                    // 无 epoch(旧 agent_runner 不发 stream_epoch)→ 保留旧行为:重置 last_seq=0
                                    // 兜底重启(全量 replay 有重复但不丢数据;新 agent_runner 有 epoch 时由比较决定)。
                                    if shared.epoch.lock().is_none() {
                                        shared.last_seq.store(0, Ordering::Release);
                                    }
                                    if attempt < MAX_RETRIES {
                                        pool.remove(&grpc_addr).await;
                                        break; // 内层 loop 退出，外层重试
                                    }
                                    let err_ev = make_stream_error_event(e.code(), e.message());
                                    shared.dispatch_event(Arc::new(err_ev));
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[SessionStream] subscribe_progress failed (attempt {}/{}): {}",
                        attempt, MAX_RETRIES, e
                    );
                    if attempt < MAX_RETRIES {
                        pool.remove(&grpc_addr).await;
                        continue;
                    }
                    // 终态事件报告【当前】阶段错误,不用累积的过期错误(#16a)。
                    let err_ev = make_terminal_error_event(shared.diag_ctx.as_ref(), locale).await;
                    // dispatch_event：broadcast + 终端清 ring（故障路径的半轮残留不得跨轮）
                    shared.dispatch_event(Arc::new(err_ev));
                    return;
                }
            }
        }
    })
}
