//! SessionWorker：session_cache 的单线程命令循环（从 session_cache.rs 拆出）。
//!
//! 所有 ring/订阅者状态变更经 [`SessionCommand`] 串行处理——这是订阅原子化
//! （快照+注册零窗口）与 Push 投递的并发安全基础。worker 独占的状态
//! （producer/consumer/buffered_len/next_seq）只在 [`SessionWorker::run`] 内存在。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    ConnectionRegistry, ConnectionState, MAX_LOG_TRUNCATE_LEN, MAX_SUBSCRIBERS,
    UnifiedSessionMessage, truncate_message_for_log,
};

pub(crate) struct SessionWorker {
    max_size: usize,
    command_rx: mpsc::Receiver<SessionCommand>,
    // 🎯 多订阅者注册表（与 SessionData 共享）
    connections: ConnectionRegistry,
    /// conn_id 分配器（与 SessionData 共享；Subscribe 命令在 worker 内分配）
    next_conn_id: Arc<AtomicU64>,
}

impl SessionWorker {
    pub(super) fn spawn(
        max_size: usize,
        command_rx: mpsc::Receiver<SessionCommand>,
        connections: ConnectionRegistry,
        next_conn_id: Arc<AtomicU64>,
    ) -> tokio::task::JoinHandle<()> {
        let start_time = std::time::Instant::now();
        debug!(
            "[SessionWorker::spawn] Starting SessionWorker creation, max_size={}",
            max_size
        );

        let worker = SessionWorker {
            max_size,
            command_rx,
            connections,
            next_conn_id,
        };

        let spawn_start = std::time::Instant::now();
        let handle = tokio::spawn(worker.run());
        debug!(
            "[SessionWorker::spawn] tokio::spawn took: {:?}",
            spawn_start.elapsed()
        );
        debug!(
            "[SessionWorker::spawn] Total spawn took: {:?}",
            start_time.elapsed()
        );

        handle
    }

    async fn run(mut self) {
        // ring buffer 存 (seq, message)：seq 是 session 级单调递增的序号，
        // 供订阅方增量 replay 与去重（见 SessionCommand::ReplaySince）。
        let (mut producer, mut consumer) =
            HeapRb::<(u64, UnifiedSessionMessage)>::new(self.max_size).split();
        let mut buffered_len = 0usize;
        // session 级单调递增的消息序号。注意：Clear 命令清空 ring buffer 内容但【不重置】
        // next_seq——否则新一轮 prompt 的 seq 从 1 重来，会与订阅方持有的 last_seq 撞车导致漏发。
        let mut next_seq: u64 = 1;

        while let Some(cmd) = self.command_rx.recv().await {
            match cmd {
                SessionCommand::Subscribe {
                    buffer_size,
                    from_seq,
                    ack,
                } => {
                    // 原子 seam：此刻无并发 Push——快照与注册之间零窗口。
                    let (tx, rx) = mpsc::channel(buffer_size);
                    let token = CancellationToken::new();
                    let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
                    self.connections.insert(
                        conn_id,
                        ConnectionState {
                            sender: tx,
                            cancel: token.clone(),
                        },
                    );
                    // 超上限逐最旧（conn_id 单调，最小 id 即最早）
                    while self.connections.len() > MAX_SUBSCRIBERS {
                        let Some(oldest) = self
                            .connections
                            .iter()
                            .min_by_key(|e| *e.key())
                            .map(|e| *e.key())
                        else {
                            break;
                        };
                        if oldest == conn_id {
                            break;
                        }
                        if let Some((_, evicted)) = self.connections.remove(&oldest) {
                            evicted.cancel.cancel();
                            warn!(
                                "[SessionWorker] subscriber limit {MAX_SUBSCRIBERS} reached, evicted oldest conn {oldest}"
                            );
                        }
                    }
                    // 快照：注册完成后取——注册后新 Push 只走 sender，
                    // 快照只含注册前的 ring 内容，两路无重叠无缺口。
                    let snapshot: Vec<(u64, UnifiedSessionMessage)> = consumer
                        .iter()
                        .filter(|(s, _)| *s > from_seq)
                        .cloned()
                        .collect();
                    debug!(
                        "[SessionWorker] Subscribe: conn {conn_id}, from_seq={from_seq}, replay {} of {} buffered",
                        snapshot.len(),
                        consumer.occupied_len()
                    );
                    if ack
                        .send(SubscribeResult {
                            conn_id,
                            replay_messages: snapshot,
                            rx,
                            token: token.clone(),
                        })
                        .is_err()
                    {
                        // 订阅者已放弃：回滚注册
                        self.connections.remove(&conn_id);
                        warn!(
                            "[SessionWorker] subscriber gone before ack, rolled back conn {conn_id}"
                        );
                    }
                }
                SessionCommand::Push { message } => {
                    debug!(
                        "[SessionWorker] Push message: message_type={:?}, sub_type={}, data={}",
                        message.message_type, message.sub_type, message.data
                    );

                    let should_buffer = !matches!(
                        message.message_type,
                        crate::model::SessionMessageType::Heartbeat
                    );

                    // 入 buffer 的消息分配 session 级单调 seq；Heartbeat 等不入 buffer 的消息用
                    // seq=0（哨兵：订阅方据此跳过去重，不更新消费游标）。
                    let seq = if should_buffer {
                        let s = next_seq;
                        next_seq += 1;
                        s
                    } else {
                        0
                    };

                    if should_buffer {
                        if producer.is_full() {
                            drop(consumer.try_pop());
                            buffered_len = buffered_len.saturating_sub(1);
                        }
                        if producer.try_push((seq, message.clone())).is_ok() {
                            buffered_len += 1;
                        } else {
                            warn!("Ring buffer push failed; real-time delivery only");
                        }
                    }

                    // 遍历全部订阅者投递；Closed 的连接在迭代结束后按 id 精确移除
                    // （不在迭代中 remove——DashMap 迭代持 shard 锁，同 shard remove 会死锁）
                    let mut closed_conns: Vec<u64> = Vec::new();
                    for entry in self.connections.iter() {
                        use tokio::sync::mpsc::error::TrySendError;
                        if let Err(send_err) = entry.value().sender.try_send((seq, message.clone()))
                        {
                            match send_err {
                                TrySendError::Full(_) => {
                                    // buffer 满（客户端暂时慢）：不禁用 sender；ring buffer 已备份
                                    warn!(
                                        "SSE sender buffer full, message buffered: message_type={:?}, sub_type={}",
                                        message.message_type, message.sub_type,
                                    );
                                }
                                TrySendError::Closed(_) => {
                                    // receiver 已断开：记录 id，迭代后移除（防重复失败刷日志）
                                    closed_conns.push(*entry.key());
                                    warn!(
                                        "SSE subscriber {} receiver dropped, will remove: message_type={:?}, sub_type={}",
                                        entry.key(),
                                        message.message_type,
                                        message.sub_type,
                                    );
                                }
                            }
                        }
                    }
                    for id in closed_conns {
                        if let Some((_, conn)) = self.connections.remove(&id) {
                            conn.cancel.cancel();
                        }
                    }
                    if self.connections.is_empty() {
                        // 无订阅者，跳过实时推送（消息已在 ring buffer 备份）
                        info!(
                            "SSE sender missing, skipping real-time delivery (message buffered in ring buffer): message_type={:?}, sub_type={}, data={}",
                            message.message_type,
                            message.sub_type,
                            truncate_message_for_log(&message.data, MAX_LOG_TRUNCATE_LEN)
                        );
                    }

                    // 终端即清（单消费者轮语义）：SessionPromptEnd 实时推送给在场订阅者后
                    // 立即清空 ring——轮结束后连入的消费者不得 replay 到本轮消息。
                    // 注：agent 异常退出的 SessionPromptError 在 Notify→Message 转换层
                    // 已归一化为 SessionPromptEnd(sub_type="error")，此处天然覆盖异常路径。
                    // （ring 缓存的职责收窄为"chat 发出→SSE 连上"的时间差缓冲。）
                    if should_buffer
                        && matches!(
                            message.message_type,
                            crate::model::SessionMessageType::SessionPromptEnd
                        )
                    {
                        let mut cleared = 0usize;
                        while consumer.try_pop().is_some() {
                            cleared += 1;
                        }
                        buffered_len = 0;
                        info!(
                            "[SessionWorker] terminal event delivered, ring cleared immediately: cleared={cleared}"
                        );
                    }
                }
                SessionCommand::Clear { ack } => {
                    // 仅清空 ring buffer 内容；next_seq 保持单调（见 run 开头注释），不随 clear 重置。
                    // 轮间清理 = chat prepare（prompt 前清）+ 终端即清（SessionPromptEnd 后清）
                    // 两点保证；cancel 在途的旧轮尾巴混入属毫秒级已知边界（不产生重复，
                    // 前端按 messageId 聚合归入旧消息）。
                    let mut cleared = 0usize;
                    while consumer.try_pop().is_some() {
                        cleared += 1;
                    }
                    buffered_len = 0;
                    let _ = ack.send(cleared);
                }
                SessionCommand::MessageCount { ack } => {
                    let _ = ack.send(buffered_len);
                }
                SessionCommand::ReplaySince { from_seq, ack } => {
                    // 非破坏性读取：用 consumer.iter() 扫描，不 pop，ring buffer 内容与顺序不变。
                    // 只返回 seq > from_seq 的消息（增量补齐），消除"已收过的消息被重放"的重复。
                    let occupied = consumer.occupied_len();
                    let snapshot: Vec<(u64, UnifiedSessionMessage)> = consumer
                        .iter()
                        .filter(|(s, _)| *s > from_seq)
                        .cloned()
                        .collect();
                    debug!(
                        "[SessionWorker] ReplaySince: from_seq={}, matched {} of {} buffered messages (non-destructive)",
                        from_seq,
                        snapshot.len(),
                        occupied
                    );
                    if let Err(e) = ack.send(snapshot) {
                        warn!("session_cache ack send failed (subscriber gone): {e:?}");
                    }
                }
            }
        }

        debug!("[SessionWorker] stopped");
    }
}

#[derive(Debug)]
/// 订阅建立结果（Subscribe 命令的 ack 载荷）：
/// replay 快照 + 实时接收端 + 连接标识/取消令牌。
pub(crate) struct SubscribeResult {
    pub(crate) conn_id: u64,
    pub(crate) replay_messages: Vec<(u64, UnifiedSessionMessage)>,
    pub(crate) rx: mpsc::Receiver<(u64, UnifiedSessionMessage)>,
    pub(crate) token: CancellationToken,
}

pub(crate) enum SessionCommand {
    Push {
        message: UnifiedSessionMessage,
    },
    /// 原子化订阅：在 worker 单线程内完成「replay 快照 + sender 注册」——
    /// Push 与 Subscribe 串行处理，消除"注册后、快照前"窗口内消息既进 ring
    /// 又进 sender 的双路投递（gRPC 流重复 → rcoder ring 重复条目 →
    /// 无游标重连时吐给客户端，即偶发的 4/492 重叠窗口重复根因）。
    Subscribe {
        buffer_size: usize,
        from_seq: u64,
        ack: oneshot::Sender<SubscribeResult>,
    },
    Clear {
        ack: oneshot::Sender<usize>,
    },
    MessageCount {
        ack: oneshot::Sender<usize>,
    },
    /// 增量 replay：只返回 ring buffer 中 seq > from_seq 的消息（非破坏性读取）。
    ReplaySince {
        from_seq: u64,
        ack: oneshot::Sender<Vec<(u64, UnifiedSessionMessage)>>,
    },
}
