//! dial9 spawner 门面：业务 spawn 点与 dial9 插桩的唯一接缝。
//!
//! 三个公开 API（[`spawn`] / [`spawn_in`] / [`spawn_in_join_set`]）**始终导出**：
//! - `dial9` feature 关闭：直通原 Tokio API（`tokio::spawn` / `Handle::spawn` /
//!   `JoinSet::spawn`），零插桩、行为与替换前逐字一致——业务 crate 不需要在
//!   每个 spawn 点写 cfg 分支，裸调 `dial9::JoinSetExt` 在关闭组合下编不过的
//!   问题也由此消除。
//! - `dial9` feature 开启：经 dial9 记录 wake 因果（ready→实际 poll 的调度
//!   延迟）与 task 生命周期；返回句柄类型与 Tokio 完全一致
//!   （`JoinHandle` / `AbortHandle`），收集与关停逻辑零改造。
//!
//! # 边界与退化行为（接入/排障前必读）
//!
//! - **TLS 未挂线时静默退化**：feature 开启但当前线程 TLS 未标记 dial9 或未持有
//!   已连接 recorder 时，`spawn` / `spawn_in_join_set` 直通原 Tokio——任务正常
//!   执行，trace 无 instrumented 标记。是否编入 feature、runtime 是否 attach、
//!   运行 env 是否启用（DIAL9_ENABLED）是三个独立轴，须分别核对，不能按进程
//!   名一刀切（详见 specs/observability-hotpath-dial9-upgrade 挂线矩阵）。
//! - **同线程切换 runtime 的 TLS 限制**：dial9 0.5.2 在 attach 时给构建线程设置
//!   TLS 标记，解析句柄时不核验当前 runtime ID。该线程随后进入未 attach 的
//!   runtime，仍可能使用先前 recorder 包装任务并记录 wake；TLS 不能保证
//!   recorder 与执行 runtime 的身份一致。
//! - **`spawn_in` 惰性解析**：包装 future 在目标 runtime 的工作线程首次 poll
//!   时才解析插桩句柄，调用线程无需在 dial9 runtime 上下文内。
//! - **根 future 无插桩**：runtime `block_on` 的根 future 在任务外执行，没有
//!   task ID、不产生 wake/dump 事件——主服务根 future 不经本门面。
//! - **执行 runtime 归属**：任务在 spawn 所选的 runtime（`spawn_in` 显式指定）
//!   上执行；观测 recorder 的归属还需核对前述 TLS 限制。
//! - **无 traced 变体的 API 维持原样**：`spawn_blocking` / `spawn_local*` /
//!   `build_task` dial9 0.5.2 未提供 traced 变体，业务调用点不动。
//! - **task-local 不自动继承**：Tokio task-local 本就不跨 spawn 自动继承，
//!   本门面不引入隐式传播；取消（`abort`）与 panic 语义（`JoinError` 传播）
//!   与原生一致。

use std::future::Future;

/// 在当前 runtime 上 spawn 一个任务（dial9 feature 开启时带插桩）。
///
/// 等价 `tokio::spawn`；feature 关闭时即 `tokio::spawn`。
/// 保留业务调用位置（`#[track_caller]`），panic 语义与原生一致。
#[track_caller]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "dial9")]
    {
        dial9::spawn(future)
    }
    #[cfg(not(feature = "dial9"))]
    {
        tokio::spawn(future)
    }
}

/// 在指定 runtime 上 spawn 一个任务（从任何线程调用均可）。
///
/// 等价 `tokio::runtime::Handle::spawn`；feature 关闭时即 `rt.spawn(future)`。
/// 开启时插桩在目标 runtime 的工作线程首次 poll 时惰性解析（见模块文档）。
#[track_caller]
pub fn spawn_in<F>(
    runtime: &tokio::runtime::Handle,
    future: F,
) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "dial9")]
    {
        dial9::spawn_in(runtime, future)
    }
    #[cfg(not(feature = "dial9"))]
    {
        runtime.spawn(future)
    }
}

/// 向 `JoinSet` spawn 一个任务，返回 `AbortHandle`。
///
/// 等价 `JoinSet::spawn`（原生本就返回 `AbortHandle`）；feature 关闭时即
/// `set.spawn(future)`，开启时经 `dial9::JoinSetExt::spawn_traced`
/// （内部仍 `set.spawn(TracedFuture)`，Output 类型与 join_next 收集不变，
/// 现有 `select! join_next()` 模式零改造）。
#[track_caller]
pub fn spawn_in_join_set<F, T>(
    set: &mut tokio::task::JoinSet<T>,
    future: F,
) -> tokio::task::AbortHandle
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    #[cfg(feature = "dial9")]
    {
        use dial9::JoinSetExt as _;
        set.spawn_traced(future)
    }
    #[cfg(not(feature = "dial9"))]
    {
        set.spawn(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 三态契约（默认未挂线 / feature 开启未挂线）在单测覆盖；attached 状态的
    // instrumented 标记与 wake 配对属真实 trace 验收（specs T3.4），不在单测冒称。

    #[tokio::test]
    async fn spawn_returns_output_and_preserves_panic_semantics() {
        let handle = spawn(async { 41 + 1 });
        assert_eq!(handle.await.expect("task completes"), 42);

        let handle: tokio::task::JoinHandle<()> = spawn(async {
            panic!("business panic must propagate as JoinError");
        });
        let err = handle.await.expect_err("panic surfaces via JoinError");
        assert!(err.is_panic());
    }

    #[tokio::test]
    async fn spawn_aborts_like_tokio() {
        let handle: tokio::task::JoinHandle<()> = spawn(async {
            std::future::pending::<()>().await;
        });
        handle.abort();
        let err = handle.await.expect_err("aborted task yields JoinError");
        assert!(
            err.is_cancelled(),
            "pending task aborted must report Cancelled"
        );
    }

    #[test]
    fn spawn_in_targets_given_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        assert!(tokio::runtime::Handle::try_current().is_err());
        // runtime 外先指定目标，再驱动 current-thread runtime 到任务完成。
        // 错误使用 tokio::spawn 的实现会在 runtime 外立即失败。
        let handle = spawn_in(rt.handle(), async {
            tokio::runtime::Handle::current().id()
        });
        let value = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), handle)
                .await
                .expect("target runtime drives task within deadline")
                .expect("task completes")
        });
        assert_eq!(value, rt.handle().id());
    }

    #[test]
    fn spawn_in_targets_another_runtime() {
        let caller_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("caller runtime");
        let target_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("target runtime");
        let handle = {
            let _caller_context = caller_rt.enter();
            spawn_in(target_rt.handle(), async {
                tokio::runtime::Handle::current().id()
            })
        };
        // 调用方不驱动；误派到 caller_rt 的任务会超时，不能挂住测试。
        let task_runtime = target_rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), handle)
                .await
                .expect("task must run on target runtime")
                .expect("task completes")
        });
        assert_eq!(task_runtime, target_rt.handle().id());
        assert_ne!(task_runtime, caller_rt.handle().id());
    }

    #[tokio::test]
    async fn spawn_in_join_set_collects_output_and_aborts() {
        // JoinSet 元素类型统一为 u64：先 abort 一个 pending 任务，再收集一个
        // 正常完成的任务，验证句柄（AbortHandle）与 join_next 输出契约。
        let mut set = tokio::task::JoinSet::new();
        let abort = spawn_in_join_set(&mut set, async { std::future::pending::<u64>().await });
        abort.abort();
        let first = set.join_next().await.expect("aborted task still joins");
        assert!(
            first
                .expect_err("aborted pending task yields JoinError")
                .is_cancelled(),
            "abort must surface as Cancelled through join_next"
        );

        spawn_in_join_set(&mut set, async { 8u64 });
        let next = set.join_next().await.expect("second task completes");
        assert_eq!(next.expect("no panic"), 8);
        assert!(set.join_next().await.is_none(), "set drained");
    }
}
