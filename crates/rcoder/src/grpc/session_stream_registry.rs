//! Session 级共享 SSE 流注册表
//!
//! 对每个 session_id 维护【一条】共享的 agent_runner `SubscribeProgress` 流，
//! 多个 HTTP SSE 客户端通过 `broadcast` fan-out 共享同一条流的输出，
//! 从根上消除"每个 HTTP SSE 请求新建一条 agent_runner 流 → 各自全量 replay → 重复消息"。
//!
//! 配合 agent_runner 的 seq 增量 replay（`ProgressEvent.seq`）：
//! - 共享流后台 task 建立/重建时，向 agent_runner 传 `from_seq = last_seq`，只拉缺失部分。
//! - HTTP 客户端按各自的消费游标从 ring 增量补齐，并跳过 broadcast 中 `seq <= 已收最大值`
//!   的重叠消息（补齐与订阅之间的窗口去重）。

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer};
use shared_types::grpc::{GetStatusRequest, ProgressEvent, ProgressRequest};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tracing::{debug, error, info, warn};

use super::GrpcChannelPool;
use super::new_request_with_locale;
use crate::handler::utils::{DiagCtx, diagnose, root_cause_message};

/// broadcast 每个 receiver 的缓冲（高频 agent_message_chunk 时给慢消费者足够窗口）
const BROADCAST_CAPACITY: usize = 256;
/// rcoder 侧历史 ring buffer 容量（与 agent_runner 一致，用于新客户端增量补齐 / Lagged 回退）
const RING_CAPACITY: usize = 1000;
/// 最后一个 HTTP 客户端断开后，延迟清理共享流的时间（处理前端短暂断线重连）
const IDLE_CLEANUP_SECS: u64 = 30;
/// activity_updater 节流间隔（与 sse_stream 一致）
const ACTIVITY_UPDATE_THROTTLE_SECS: i64 = 10;
/// agent_runner 流重试次数
const MAX_RETRIES: u32 = 2;

type SharedEvent = Arc<ProgressEvent>;

/// SSE 共享流关闭回调类型（参数为 grpc_addr）。
/// 容器销毁路径（reaper/restart/ensure/destroyer）按地址关闭前端进度流。
pub type ShutdownSseFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Session → 共享流注册表（rcoder 进程级单例，挂在 `AppState`）。
pub struct SessionStreamRegistry {
    streams: DashMap<String, Arc<SharedStream>>,
    /// per-session 创建锁：序列化 `get_or_create` 的慢速路径，避免并发创建多个 SharedStream。
    /// 必要性：agent_runner 的 `current_connection` 是单连接模型（`create_new_connection` cancel 旧 token），
    /// 若并发创建 N 个 SharedStream，它们各自建立的 agent_runner SubscribeProgress 流会互相 cancel 抖动，
    /// 导致 registry 持有的流被反复 cancel、客户端收不到稳定事件。
    create_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl SessionStreamRegistry {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
            create_locks: DashMap::new(),
        }
    }

    /// 仅当 registry 中仍是指定实例时移除并关闭，避免快慢路径行为不一致。
    fn remove_and_shutdown(&self, session_id: &str, expected: &Arc<SharedStream>) -> bool {
        let Some((_, removed)) = self
            .streams
            .remove_if(session_id, |_, current| Arc::ptr_eq(current, expected))
        else {
            return false;
        };
        removed.shutdown();
        true
    }

    /// 强制关闭某 session 的共享流（容器销毁/项目删除时调用）。
    /// 与 [`remove_and_shutdown`] 的 ptr_eq 精确清理不同，这是销毁语义：无条件移除该 session 的流，
    /// 让后台 gRPC task 尽快退出，避免对已失效地址重试到 MAX_RETRIES。
    pub fn shutdown_session(&self, session_id: &str) -> bool {
        if let Some((_, removed)) = self.streams.remove(session_id) {
            removed.shutdown();
            self.remove_unused_create_lock(session_id);
            true
        } else {
            false
        }
    }

    /// 获取或创建 session 的共享流。
    ///
    /// 并发安全：参照 `session_cache::push_session_update` 的快速路径(`view`) + 慢速路径(`entry` 外 await)，
    /// 严守"不在 DashMap entry 持锁范围内 await"（项目规范，避免 shard 锁跨 yield 死锁）。
    ///
    /// grpc_addr 变化（容器重建）或后台 task 已退出时，作废旧流重建。
    pub async fn get_or_create(
        self: &Arc<Self>,
        session_id: &str,
        grpc_addr: &str,
        pool: Arc<GrpcChannelPool>,
        locale: &'static str,
        activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
        diag_ctx: Option<Arc<DiagCtx>>,
    ) -> Arc<SharedStream> {
        // 快速路径：存在 + grpc_addr 匹配 + 后台 task 存活 → 复用
        if let Some(existing) = self.streams.view(session_id, |_, v| v.clone()) {
            if existing.matches_addr(grpc_addr) && existing.is_alive() {
                return existing;
            }
            // grpc_addr 变化（容器重建）或 task 已死 → 移除并 shutdown 旧 task（cancel 后台
            // gRPC task，避免它继续重试已失效的旧 grpc_addr 而短暂泄漏资源）。
            self.remove_and_shutdown(session_id, &existing);
        }

        // 慢速路径：per-session 创建锁序列化，避免并发创建多个 SharedStream
        // （见 create_locks 字段注释：agent_runner 单连接模型下多个流会互相 cancel 抖动）。
        let lock = self
            .create_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _create_guard = lock.lock().await;

        // double-check：持锁后，可能已有其他并发请求创建了可复用的流
        if let Some(existing) = self.streams.view(session_id, |_, v| v.clone()) {
            if existing.matches_addr(grpc_addr) && existing.is_alive() {
                return existing;
            }
            // 这里可能移除另一个创建者刚插入但 grpc_addr 不匹配的活跃流，必须同步 cancel
            // 它的后台 task；只 remove 会让 task 持有 Arc 并继续运行到自然断流。
            self.remove_and_shutdown(session_id, &existing);
        }

        // 锁内创建（含 spawn 后台 task 的 await）；此时无并发创建者，安全。
        let new_stream = SharedStream::new(
            session_id.to_string(),
            grpc_addr.to_string(),
            pool,
            locale,
            activity_updater,
            diag_ctx,
        )
        .await;
        self.streams
            .insert(session_id.to_string(), new_stream.clone());
        new_stream
    }

    /// 按 grpc_addr 批量关闭共享流（容器销毁路径：reaper/restart/ensure/destroyer 调用）。
    ///
    /// 与 [`shutdown_session`] 的按 session_id 关闭不同，此方法用于"project/session 记录可能
    /// 已被清空、只剩 grpc_addr 可用"的销毁路径。销毁语义：无条件移除匹配地址的流，先发终态
    /// SessionPromptEnd 事件再 cancel 后台 task。幂等：重复调用返回 0。
    pub fn shutdown_streams_by_addr(&self, grpc_addr: &str) -> usize {
        // 两阶段：先迭代收集 (session_id, Arc)，再逐个 remove_if(ptr_eq) + shutdown。
        // 避免在 DashMap 迭代持 shard 锁期间执行 shutdown 的 broadcast send。
        let matches: Vec<(String, Arc<SharedStream>)> = self
            .streams
            .iter()
            .filter(|entry| entry.value().matches_addr(grpc_addr))
            .map(|entry| (entry.key().clone(), Arc::clone(entry.value())))
            .collect();
        let total = matches.len();
        let mut closed = 0usize;
        for (session_id, shared) in matches {
            // ptr_eq 确保只移除迭代时看到的同一实例（并发 get_or_create 可能已替换为新流）
            if let Some((_, removed)) = self
                .streams
                .remove_if(&session_id, |_, current| Arc::ptr_eq(current, &shared))
            {
                removed.shutdown();
                self.remove_unused_create_lock(&session_id);
                closed += 1;
            }
        }
        info!(
            "[SessionStream] shutdown_streams_by_addr: grpc_addr={}, matched={}, closed={}",
            grpc_addr, total, closed
        );
        closed
    }

    /// 当前活跃共享流数量（测试 / 观测用）
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    fn remove_unused_create_lock(&self, session_id: &str) {
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.create_locks.entry(session_id.to_string())
            && Arc::strong_count(entry.get()) == 1
        {
            // entry guard prevents a concurrent get_or_create from cloning this Arc between the
            // strong-count check and removal. Any current/waiting creator contributes another Arc.
            entry.remove();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }
}

impl Default for SessionStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 一个 session 的共享流：一条 agent_runner `SubscribeProgress` 后台 task + `broadcast` fan-out + 历史 ring。
pub struct SharedStream {
    session_id: String,
    grpc_addr: String,
    broadcast_tx: broadcast::Sender<SharedEvent>,
    ring: Mutex<HeapRb<(u64, SharedEvent)>>,
    ref_count: AtomicUsize,
    last_seq: AtomicU64,
    /// agent_runner 该 session 的 stream epoch(GetStatus 返回)。同 epoch → 保留 last_seq 增量订阅;
    /// epoch 变化(agent 重启/worker panic 重建)→ 重置 last_seq + 清 ring + cursor-reset(#15)。
    epoch: Mutex<Option<String>>,
    last_activity_secs: AtomicI64,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
    cancel_token: CancellationToken,
    task_handle: OnceLock<JoinHandle<()>>,
    /// 诊断上下文:后台 task 重试耗尽发终态错误事件时,据此做 OOM/crashloop 等精准诊断,
    /// 替代通用文案。None(测试/无 runtime)→ 通用"Compute environment temporarily unavailable"。
    diag_ctx: Option<Arc<DiagCtx>>,
}

impl SharedStream {
    async fn new(
        session_id: String,
        grpc_addr: String,
        pool: Arc<GrpcChannelPool>,
        locale: &'static str,
        activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
        diag_ctx: Option<Arc<DiagCtx>>,
    ) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let shared = Arc::new(Self {
            session_id: session_id.clone(),
            grpc_addr: grpc_addr.clone(),
            broadcast_tx,
            ring: Mutex::new(HeapRb::<(u64, SharedEvent)>::new(RING_CAPACITY)),
            ref_count: AtomicUsize::new(0),
            last_seq: AtomicU64::new(0),
            epoch: Mutex::new(None),
            last_activity_secs: AtomicI64::new(0),
            activity_updater: Arc::clone(&activity_updater),
            cancel_token: CancellationToken::new(),
            task_handle: OnceLock::new(),
            diag_ctx: diag_ctx.clone(),
        });

        let handle = spawn_backend_task(
            Arc::clone(&shared),
            grpc_addr,
            pool,
            locale,
            activity_updater,
        );
        if let Err(handle) = shared.task_handle.set(handle) {
            handle.abort();
            warn!(session_id = %shared.session_id, "backend task handle was initialized twice");
        }
        shared
    }

    fn matches_addr(&self, grpc_addr: &str) -> bool {
        self.grpc_addr == grpc_addr
    }

    /// 后台 task 仍存活（未 cancel 且 JoinHandle 未结束）
    fn is_alive(&self) -> bool {
        if self.cancel_token.is_cancelled() {
            return false;
        }
        match self.task_handle.get() {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }

    /// 注册一个 HTTP 客户端消费者（ref_count +1），返回 RAII guard。
    /// guard 持有 `Arc<SharedStream>`，drop 时直接减【自己的】ref_count（见 ClientGuard::drop）。
    pub fn acquire_client(self: &Arc<Self>, registry: Arc<SessionStreamRegistry>) -> ClientGuard {
        self.ref_count.fetch_add(1, Ordering::AcqRel);
        ClientGuard {
            registry,
            shared: Arc::clone(self),
            session_id: self.session_id.clone(),
        }
    }

    /// 当前已观察到的最大 seq（HTTP 客户端 ring 补齐的起点；agent_runner 流重建时的 from_seq）
    pub fn last_seq(&self) -> u64 {
        self.last_seq.load(Ordering::Acquire)
    }

    /// 订阅 broadcast（实时事件）
    pub fn subscribe(&self) -> broadcast::Receiver<SharedEvent> {
        self.broadcast_tx.subscribe()
    }

    /// 增量补齐：从 ring 读取 `seq > from_seq` 的事件（非破坏性，按 seq 升序）。
    /// 用于 HTTP 客户端建连时补齐错过的历史；与 broadcast 实时流配合时需按 seq 去重重叠窗口。
    pub fn replay_since(&self, from_seq: u64) -> Vec<SharedEvent> {
        let ring = self.ring.lock();
        ring.iter()
            // seq=0 是 cursor-reset 哨兵：无条件返回，让断线重连客户端也能收到它并由
            // forward_to_client 重置去重游标（修复 epoch 变更后重连丢事件）。
            .filter(|(s, _)| *s == 0 || *s > from_seq)
            .map(|(_, ev)| Arc::clone(ev))
            .collect()
    }

    /// 后台 task 收到事件时调用：真实消息(seq>=1)存 ring + 更新 last_seq + 节流刷新 activity；
    /// 所有消息（含 seq=0 合成消息）broadcast 给客户端。
    fn dispatch_event(&self, ev: SharedEvent) {
        let seq = ev.seq;
        if seq > 0 {
            {
                let mut ring = self.ring.lock();
                if ring.is_full() {
                    ring.try_pop();
                }
                if ring.try_push((seq, Arc::clone(&ev))).is_err() {
                    warn!(
                        "[SessionStream] ring push failed (full?), real-time only: session_id={}, seq={}",
                        self.session_id, seq
                    );
                }
            }
            self.last_seq.store(seq, Ordering::Release);
            maybe_update_activity(
                &self.activity_updater,
                &self.session_id,
                &self.last_activity_secs,
            );
        }
        // broadcast：无 receiver 时 send 返回 Err（客户端全断时事件只留 ring）。
        // 高频路径 (每个流式 chunk 都经过), 且"无订阅者"是正常态 (客户端全断后
        // 流要等 idle 清理窗口才移除), 用 debug 避免日志刷屏。
        if let Err(send_err) = self.broadcast_tx.send(ev) {
            debug!("[SessionStream] broadcast send failed (no subscriber): {send_err}");
        }
    }

    /// 清空历史 ring(epoch 变化时调用,丢弃旧 epoch 的事件,避免新 epoch 重放旧事件)。
    fn clear_ring(&self) {
        let mut ring = self.ring.lock();
        *ring = HeapRb::<(u64, SharedEvent)>::new(RING_CAPACITY);
    }

    /// 把 cursor-reset 哨兵(seq=0)写入 ring。
    /// dispatch_event 跳过 seq=0 故单独推送；目的是让断线重连客户端经 replay_since 取到哨兵，
    /// 进而由 forward_to_client 重置去重游标（broadcast 只投递订阅后的消息，重连客户端收不到）。
    fn push_reset_to_ring(&self, ev: SharedEvent) {
        let mut ring = self.ring.lock();
        if ring.is_full() {
            ring.try_pop();
        }
        drop(ring.try_push((0, ev)));
    }

    fn shutdown(&self) {
        // 先通知所有连着的客户端：流即将结束。避免 task 被 cancel 后 broadcast Receiver 不 Closed
        // （SharedStream 仍持有 sender）导致客户端转发 task hang。发 SessionPromptEnd 让
        // forward_to_client 检测终端后优雅退出。
        let end_ev = Arc::new(ProgressEvent {
            message_type: "SessionPromptEnd".to_string(),
            sub_type: "stream_ended".to_string(),
            payload:
                r#"{"reason":"StreamEnded","description":"Session stream replaced or cleaned up"}"#
                    .to_string(),
            request_id: None,
            seq: 0,
            timestamp: now_millis(),
        });
        if let Err(send_err) = self.broadcast_tx.send(end_ev) {
            warn!("[SessionStream] broadcast send failed (no subscriber): {send_err}");
        }
        self.cancel_token.cancel();
        // 不 await task：避免 get_or_create（HTTP 请求路径）阻塞——后台 task 在 get_client/get_status/
        // subscribe_progress 等连接阶段不响应 cancel，若 await 会卡住 HTTP 请求。task 会在 stream 循环
        // 响应 cancel 退出，或连接失败重试（MAX_RETRIES）耗尽退出；其持有的 Arc 随 task 退出回收，
        // streams 已 remove_if，无泄漏。
    }
}

/// HTTP 客户端消费 guard：持有 `Arc<SharedStream>`，drop 时直接减【自己的】ref_count
/// （不按 session_id 反查 streams——grpc_addr 变化后 streams 里可能是新 SharedStream，反查会误减新的，
///  且旧的永不减导致泄漏）。最后一个客户端离开时延迟清理，`remove_if` 用 `ptr_eq` 确保只删自己的 shared。
pub struct ClientGuard {
    registry: Arc<SessionStreamRegistry>,
    shared: Arc<SharedStream>,
    session_id: String,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let prev = self.shared.ref_count.fetch_sub(1, Ordering::AcqRel);
        if prev > 1 {
            return; // 仍有其他客户端连着
        }
        // 最后一个客户端离开 → 延迟清理（期间若有新客户端连入，ref_count>0，不清理）
        let registry = Arc::clone(&self.registry);
        let session_id = std::mem::take(&mut self.session_id);
        let shared = Arc::clone(&self.shared);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(Duration::from_secs(IDLE_CLEANUP_SECS)).await;
                if shared.ref_count.load(Ordering::Acquire) != 0 {
                    return; // 期间有新客户端连入，放弃清理
                }
                // remove_if 用 ptr_eq：grpc_addr 变化后 streams 里可能是新 SharedStream，不能误删；
                // 若本 shared 已不在 streams（被 grpc_addr 变化替换），返回 None，这里只让 Arc 随 drop 回收。
                if let Some((_, removed)) = registry
                    .streams
                    .remove_if(&session_id, |_, v| Arc::ptr_eq(v, &shared))
                {
                    removed.shutdown();
                    // 仅在没有创建者持有/等待这把锁时移除，避免同一 session 短暂出现两把锁，
                    // 破坏 get_or_create 的 single-flight 保证。
                    registry.remove_unused_create_lock(&session_id);
                    info!(
                        "[SessionStream] idle cleanup after {}s: session_id={}",
                        IDLE_CLEANUP_SECS, session_id
                    );
                }
            });
        }
    }
}

/// 启动后台 gRPC 接收 task（一条 agent_runner SubscribeProgress 流）。
///
/// 流程：get_client → get_status(idle 检查) → subscribe_progress(from_seq=last_seq)
/// → 收事件 dispatch。流断/出错重试（max_retries），彻底失败则 broadcast 错误事件后退出。
fn spawn_backend_task(
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
                    if let Err(send_err) = shared.broadcast_tx.send(Arc::new(err_ev)) {
                        warn!("[SessionStream] broadcast send failed (no subscriber): {send_err}");
                    }
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
                        info!(
                            "[SessionStream] agent idle, sending SessionPromptEnd: session_id={}",
                            session_id
                        );
                        let ev = make_prompt_end_event();
                        if let Err(send_err) = shared.broadcast_tx.send(Arc::new(ev)) {
                            warn!(
                                "[SessionStream] broadcast send failed (no subscriber): {send_err}"
                            );
                        }
                        return;
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
                                    if let Err(send_err) = shared
                                        .broadcast_tx
                                        .send(Arc::new(make_prompt_end_event()))
                                    {
                                        warn!(
                                            "[SessionStream] broadcast send failed (no subscriber): {send_err}"
                                        );
                                    }
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
                                    if let Err(send_err) =
                                        shared.broadcast_tx.send(Arc::new(err_ev))
                                    {
                                        warn!(
                                            "[SessionStream] broadcast send failed (no subscriber): {send_err}"
                                        );
                                    }
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
                    if let Err(send_err) = shared.broadcast_tx.send(Arc::new(err_ev)) {
                        warn!("[SessionStream] broadcast send failed (no subscriber): {send_err}");
                    }
                    return;
                }
            }
        }
    })
}

/// 节流刷新 session 活跃时间（防 idle cleaner 误杀；与 sse_stream 语义一致）。
fn maybe_update_activity(
    updater: &Arc<dyn Fn(&str) + Send + Sync>,
    session_id: &str,
    last_update_secs: &AtomicI64,
) {
    let now_secs = chrono::Utc::now().timestamp();
    let last = last_update_secs.load(Ordering::Relaxed);
    if now_secs - last < ACTIVITY_UPDATE_THROTTLE_SECS {
        return;
    }
    if last_update_secs
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    updater(session_id);
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// agent idle 时的 SessionPromptEnd 事件（seq=0 合成消息，客户端无条件转发）
fn make_prompt_end_event() -> ProgressEvent {
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "end_turn".to_string(),
        payload: r#"{"reason":"EndTurn","description":"Agent has no task in execution"}"#
            .to_string(),
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// agent_runner 流传输中出错（seq=0 合成消息）
fn make_stream_error_event(code: Code, _message: &str) -> ProgressEvent {
    let error_code = map_tonic_code(code);
    // 用 serde_json 构造,避免 format! 拼接产生非法 JSON(error_code 虽为静态串,
    // 统一走安全路径以便未来扩展)。
    let payload = serde_json::json!({
        "code": error_code,
        "message": "Agent execution error, please retry.",
    })
    .to_string();
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "error".to_string(),
        payload,
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// gRPC 连接彻底失败(重试耗尽;seq=0 合成终态事件)。
///
/// 不再把 transport 原文塞进 SSE 事件 —— 原文对用户无意义(transport error 多半不是根因),
/// 且已在调用处 `warn!` 入日志供排查。有 [`DiagCtx`] 时做一次**实时诊断**(OOM/CrashLoop/
/// 容器缺失/启动中),给精准根因文案(与 chat 路径共用 [`root_cause_message`],两路一致);
/// 无 DiagCtx(测试 / 无 runtime)→ 通用"Compute environment temporarily unavailable"。
/// 错误码统一 [`ERR_AGENT_CONTAINER_UNAVAILABLE`](前端可据码退避重试)。
async fn make_terminal_error_event(diag: Option<&Arc<DiagCtx>>, locale: &str) -> ProgressEvent {
    let code = shared_types::error_codes::ERR_AGENT_CONTAINER_UNAVAILABLE;
    let message = match diag {
        // 实时诊断根因 → 精准文案。诊断本身失败不阻断:diagnose() 内部已兜底默认诊断
        // (→ root_cause_message 的通用分支),不会让错误路径二次失败。
        Some(ctx) => {
            let d = diagnose(&ctx.runtime, &ctx.identifier, ctx.service_type.clone()).await;
            root_cause_message(&d, locale)
        }
        None => shared_types::error_codes::get_error_message(code, locale),
    };
    // serde_json 构造,避免 format! 拼 JSON 产生非法 JSON。
    let payload = serde_json::json!({
        "code": code,
        "message": message,
    })
    .to_string();
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "error".to_string(),
        payload,
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// epoch 变化时的 cursor-reset 哨兵(seq=0):告知客户端重置去重游标(client_last_seq=0),
/// 让新 epoch 的低 seq 事件不被静默丢弃(#15)。非终态(message_type≠SessionPromptEnd,不关流)。
fn make_cursor_reset_event() -> ProgressEvent {
    ProgressEvent {
        message_type: "StreamReset".to_string(),
        sub_type: "epoch_changed".to_string(),
        payload: serde_json::json!({
            "reason": "EpochChanged",
            "description": "Agent stream epoch changed; reset your dedup cursor"
        })
        .to_string(),
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

fn map_tonic_code(code: Code) -> &'static str {
    match code {
        Code::Unavailable => "GRPC_SERVICE_UNAVAILABLE",
        Code::Cancelled => "GRPC_CANCELLED",
        Code::DeadlineExceeded => "GRPC_TIMEOUT",
        Code::Unknown => "GRPC_UNKNOWN_ERROR",
        _ => "GRPC_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Consumer, Producer};

    #[test]
    fn replay_since_filters_by_seq() {
        // 构造一个 SharedStream 的 ring 直接测试 replay_since 逻辑
        let mut ring: HeapRb<(u64, SharedEvent)> = HeapRb::new(10);
        for seq in 1..=5 {
            let ev = Arc::new(ProgressEvent {
                message_type: "AgentSessionUpdate".into(),
                sub_type: "test".into(),
                payload: "{}".into(),
                request_id: None,
                seq,
                timestamp: seq as i64,
            });
            drop(ring.try_push((seq, ev)));
        }
        let ring = Mutex::new(ring);
        let got: Vec<u64> = ring
            .lock()
            .iter()
            .filter(|(s, _)| *s > 3)
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(got, vec![4, 5]);
    }

    #[test]
    fn replay_since_is_non_destructive() {
        let mut ring: HeapRb<(u64, SharedEvent)> = HeapRb::new(10);
        for seq in 1..=3 {
            drop(ring.try_push((
                seq,
                Arc::new(ProgressEvent {
                    message_type: "X".into(),
                    sub_type: "y".into(),
                    payload: "{}".into(),
                    request_id: None,
                    seq,
                    timestamp: 0,
                }),
            )));
        }
        let ring = Mutex::new(ring);
        let first: Vec<u64> = ring.lock().iter().map(|(s, _)| *s).collect();
        let second: Vec<u64> = ring.lock().iter().map(|(s, _)| *s).collect();
        assert_eq!(first, second, "iter must be non-destructive");
        assert_eq!(first, vec![1, 2, 3]);
    }

    #[test]
    fn registry_default_is_empty() {
        let r = SessionStreamRegistry::default();
        assert!(r.is_empty());
    }

    #[test]
    fn create_lock_is_removed_only_without_active_holders() {
        let registry = SessionStreamRegistry::default();
        let held = registry
            .create_locks
            .entry("session-a".to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();

        registry.remove_unused_create_lock("session-a");
        assert!(registry.create_locks.contains_key("session-a"));

        drop(held);
        registry.remove_unused_create_lock("session-a");
        assert!(!registry.create_locks.contains_key("session-a"));
    }

    #[tokio::test]
    async fn removing_matching_stream_cancels_its_backend_task() {
        let registry = SessionStreamRegistry::default();
        let shared = SharedStream::new(
            "session-a".into(),
            "127.0.0.1:1".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;
        registry
            .streams
            .insert("session-a".into(), Arc::clone(&shared));

        assert!(!shared.cancel_token.is_cancelled());
        assert!(registry.remove_and_shutdown("session-a", &shared));
        assert!(shared.cancel_token.is_cancelled());
        assert!(!registry.streams.contains_key("session-a"));
    }

    fn arc_event(seq: u64, sub: &str) -> Arc<ProgressEvent> {
        Arc::new(ProgressEvent {
            message_type: "AgentSessionUpdate".into(),
            sub_type: sub.into(),
            payload: "{}".into(),
            request_id: None,
            seq,
            timestamp: 0,
        })
    }

    /// SharedStream::new 会 spawn 后台 gRPC task（连 127.0.0.1:1 必失败，但不影响 dispatch_event
    /// —— 该方法只操作 ring/last_seq/broadcast，不依赖 gRPC）。测试结束 runtime drop 会 cancel task。
    #[tokio::test]
    async fn dispatch_event_buffers_real_events_skips_synthetic() {
        let shared = SharedStream::new(
            "s1".into(),
            "127.0.0.1:1".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;

        shared.dispatch_event(arc_event(1, "m1"));
        shared.dispatch_event(arc_event(2, "m2"));
        shared.dispatch_event(arc_event(0, "synthetic")); // seq=0 合成消息：不入 ring、不更新 last_seq

        assert_eq!(shared.last_seq(), 2, "seq=0 must not update last_seq");
        let got: Vec<u64> = shared
            .replay_since(0)
            .into_iter()
            .map(|ev| ev.seq)
            .collect();
        assert_eq!(got, vec![1, 2], "only seq>0 events enter ring");
    }

    #[tokio::test]
    async fn client_guard_increments_and_decrements_ref_count() {
        let registry = Arc::new(SessionStreamRegistry::default());
        let shared = SharedStream::new(
            "s1".into(),
            "127.0.0.1:1".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;

        assert_eq!(shared.ref_count.load(Ordering::Acquire), 0);
        let guard1 = shared.acquire_client(Arc::clone(&registry));
        let guard2 = shared.acquire_client(Arc::clone(&registry));
        assert_eq!(shared.ref_count.load(Ordering::Acquire), 2);

        drop(guard1);
        assert_eq!(
            shared.ref_count.load(Ordering::Acquire),
            1,
            "one guard dropped, one remains"
        );

        drop(guard2);
        assert_eq!(
            shared.ref_count.load(Ordering::Acquire),
            0,
            "all guards dropped → ref_count back to 0"
        );
        // 最后一个 guard drop 会 spawn 30s 延迟清理；测试结束 runtime drop 会 cancel 它。
    }

    #[tokio::test]
    async fn shutdown_streams_by_addr_closes_only_matching_streams() {
        let registry = SessionStreamRegistry::default();
        let matched_a = SharedStream::new(
            "session-a".into(),
            "10.0.0.1:50051".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;
        let matched_b = SharedStream::new(
            "session-b".into(),
            "10.0.0.1:50051".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;
        let unmatched = SharedStream::new(
            "session-c".into(),
            "10.0.0.2:50051".into(),
            Arc::new(GrpcChannelPool::new()),
            "en",
            Arc::new(|_| {}),
            None,
        )
        .await;
        registry
            .streams
            .insert("session-a".into(), Arc::clone(&matched_a));
        registry
            .streams
            .insert("session-b".into(), Arc::clone(&matched_b));
        registry
            .streams
            .insert("session-c".into(), Arc::clone(&unmatched));

        // 只关闭匹配地址的流
        let closed = registry.shutdown_streams_by_addr("10.0.0.1:50051");
        assert_eq!(closed, 2, "两条匹配地址的流都应被关闭");
        assert!(matched_a.cancel_token.is_cancelled());
        assert!(matched_b.cancel_token.is_cancelled());
        assert!(!registry.streams.contains_key("session-a"));
        assert!(!registry.streams.contains_key("session-b"));

        // 不匹配的流保留且未 cancel
        assert!(!unmatched.cancel_token.is_cancelled());
        assert!(registry.streams.contains_key("session-c"));
        assert_eq!(registry.len(), 1);

        // 幂等：重复关闭同一地址返回 0
        assert_eq!(registry.shutdown_streams_by_addr("10.0.0.1:50051"), 0);
    }

    #[test]
    fn shutdown_streams_by_addr_returns_zero_for_unknown_addr() {
        let registry = SessionStreamRegistry::default();
        assert_eq!(registry.shutdown_streams_by_addr("1.2.3.4:50051"), 0);
    }

    #[tokio::test]
    async fn terminal_error_event_payload_is_valid_json() {
        // 无 DiagCtx → 通用文案;payload 必须是合法 JSON(serde_json 构造,非 format! 拼接)。
        let ev = make_terminal_error_event(None, "en-US").await;
        let payload: serde_json::Value =
            serde_json::from_str(&ev.payload).expect("payload must be valid JSON");
        assert_eq!(
            payload["code"],
            shared_types::error_codes::ERR_AGENT_CONTAINER_UNAVAILABLE
        );
        assert!(
            payload["message"].is_string(),
            "message must be a JSON string"
        );
        assert_eq!(ev.message_type, "SessionPromptEnd");
        assert_eq!(ev.sub_type, "error");
        assert_eq!(ev.seq, 0, "synthetic terminal event uses seq=0");
    }

    #[test]
    fn stream_error_payload_is_valid_json() {
        let ev = make_stream_error_event(Code::Unavailable, "irrelevant");
        let payload: serde_json::Value =
            serde_json::from_str(&ev.payload).expect("payload must be valid JSON");
        assert_eq!(payload["code"], "GRPC_SERVICE_UNAVAILABLE");
        assert_eq!(payload["message"], "Agent execution error, please retry.");
    }
}
