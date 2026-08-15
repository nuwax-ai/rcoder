//! UserApp 活动追踪与流量唤醒接口（trait）
//!
//! 支撑「闲置自动回收 + 流量唤醒」特性：
//! - [`AppAccessTracker`]（同步）由 Pingora 代理热路径调用，记录每个 UserApp 的最近 HTTP 访问时间，
//!   作为闲置回收的唯一信号源。镜像 [`crate::ContainerLookup`] 的同步 trait 风格（DashMap 读写，无 runtime 依赖）。
//! - [`AppWakeControl`]（异步）由 Pingora 在请求过滤阶段调用：当目标 app 处于 stopped（scale0）时，
//!   hold-and-wait 拉起（scale→1）并轮询 Ready，超时返回 [`WakeOutcome::Timeout`]。
//!
//! 两个 trait 仅暴露 Pingora 代理层（跨 crate 消费者）需要的方法（ISP：接口最小化）。
//! 其余同 crate 调用者（AppService / 回收扫描器）持具体 `AppActivityRegistry` 类型，直接用其 pub 方法
//! （`last_accessed_at` / `mark_running` / `mark_stopped` / `is_waking` / `seed_accessed`）。

/// UserApp HTTP 访问追踪（同步，无 runtime 依赖）
///
/// 由 Pingora `request_filter` 对 `/proxy/apps/{app_id}/{port}/...` 路由调用。
/// `touch` 内部应做节流（实现自行决定粒度），避免高 QPS 下的 DashMap 锁竞争。
pub trait AppAccessTracker: Send + Sync {
    /// 记录 app 的最近一次真实 HTTP 访问（实现内部节流）。
    fn touch(&self, app_id: &str);
}

/// 唤醒结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// 已就绪（本次唤醒成功，或本就 Running）
    Ready,
    /// 调用前已是 Running（无需唤醒）
    AlreadyRunning,
    /// 唤醒超时（app 仍在后台启动；客户端应 503 + Retry-After）
    Timeout,
    /// 唤醒失败（scale 失败、runtime 未就绪、app 进入 Error 相等）
    Failed(String),
}

/// UserApp 流量唤醒控制（异步）
///
/// 由 Pingora `request_filter` 在检测到目标 app stopped 时调用 [`AppWakeControl::ensure_running`]，
/// hold-and-wait 拉起容器（上限由实现配置，默认 60s）。并发请求由实现内部合流为一次 scale-up。
#[async_trait::async_trait]
pub trait AppWakeControl: Send + Sync {
    /// app 是否处于 stopped（scale replicas==0）。读内存表，O(1)，供 Pingora 快速短路。
    fn is_stopped(&self, app_id: &str) -> bool;

    /// 确保 app Running：
    /// - stopped → 唤醒（scale→1 + 轮询 Ready，hold-and-wait ≤ wake_timeout），并发请求合流；
    /// - running → 立即返回 [`WakeOutcome::AlreadyRunning`]。
    async fn ensure_running(&self, app_id: &str) -> WakeOutcome;
}

/// UserApp 活动状态的持久化行（AppActivityRegistry ↔ 存储后端的数据载体）
#[derive(Debug, Clone)]
pub struct ActivityRow {
    /// UserApp 应用 ID
    pub app_id: String,
    /// 最近真实 HTTP 访问时间（wall-clock；None=从未访问）
    pub last_accessed: Option<chrono::DateTime<chrono::Utc>>,
    /// 已 scale-to-zero，可被流量唤醒
    pub stopped: bool,
    /// 用户主动停止/发布切换中，禁止流量自动唤醒
    pub wake_blocked: bool,
}

/// AppActivityRegistry 的影子持久化契约（跨 crate：app_manager 产出/消费，rcoder-storage 实现）
///
/// registry 本体保持内存单例语义（wake single-flight/RecycleTransition 是进程内协调机制），
/// 本契约只负责数据的跨重启持久化：flusher 周期批量落库、启动时全量加载回内存。
/// 实现须保证幂等（upsert）。
#[async_trait::async_trait]
pub trait ActivityPersistence: Send + Sync {
    /// 批量 upsert 活动状态行（flusher 每 ~5s 调用）
    async fn flush_batch(&self, rows: Vec<ActivityRow>) -> anyhow::Result<()>;

    /// 全量加载（启动时调用；空表返回空 Vec）
    async fn load_all(&self) -> anyhow::Result<Vec<ActivityRow>>;

    /// 删除单行（forget_app/delete_app 后调用）
    async fn delete(&self, app_id: &str) -> anyhow::Result<()>;
}
