//! 启动/观察链的短暂读缺失窗口（收据可见性，2026-09-28 A/B 事故后引入）。
//!
//! 背景：监督收据以 temp+rename 原子发布后，消费进程立即回读；在共享挂载
//! （macOS dev bind mount 等）上跨进程可见性存在短暂窗口，确定性触发
//! `read receipt ...: ENOENT`。本模块把"启动/观察期间的短缺失"收敛为
//! 有界、可诊断的程序内消化，语义约束见
//! `specs/native-cli-supervision/receipt-visibility-plan-2026-09-28.md` §3.1：
//!
//! - 单一绝对预算；成功路径零等待（不新增固定启动延迟）；
//! - 仅 `io::ErrorKind::NotFound` 与准入锁短暂 `WouldBlock` 视为瞬态
//!   Pending；JSON 损坏、权限失败、身份不符等立即失败；
//! - 每轮是一次完整尝试（闭包内重建/释放全部句柄），不得拼接两轮观察放行；
//! - 错误分类走错误链 downcast，不匹配文案；
//! - 超时返回原错误链 + 阶段/尝试次数/耗时，不写死文件系统根因；
//! - 真实发生重试/超时时输出聚合诊断（warn）。
//!
//! 与 `guardian::observe_receipt` 的非重置窗口同族：缺失不代表退出/失败，
//! 预算耗尽才如实上报。本模块只约束重试与异步等待；同步文件系统调用自身
//! 卡死不在保证范围。

use std::future::Future;
use std::time::Duration;

use anyhow::Error;

/// 轮询间隔（与既有 PID 轮询 / Gate::acquire 同量级）。
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// 错误链中是否含 `io::ErrorKind::NotFound`（文件/目录尚未可见）。
pub fn is_not_found(error: &Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// 错误链中是否含准入/所有权锁的短暂 `WouldBlock`（持有方即将释放）。
pub fn is_transiently_busy(error: &Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::fs::TryLockError>())
        .any(|error| matches!(error, std::fs::TryLockError::WouldBlock))
}

fn is_transient(error: &Error) -> bool {
    is_not_found(error) || is_transiently_busy(error)
}

/// 观察结果：`Ok(Some)` = 本轮就绪；`Ok(None)` = 领域级未就绪（如 PID 尚未
/// 登记）——与 NotFound/busy 同属可等待瞬态，由本函数统一消化。
pub type Attempt<T> = anyhow::Result<Option<T>>;

/// 有界观察一个启动/探测阶段：`attempt` 每轮做一次完整尝试。
///
/// 预算耗尽时：最后一次是错误 → 原错误链附加阶段/次数/耗时上下文返回；
/// 最后一次是 `Ok(None)` → 合成"未在预算内就绪"错误。两种情况都不携带
/// 文件系统根因猜测。
pub async fn observe<T, F, Fut>(
    stage: &'static str,
    budget: Duration,
    mut attempt: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt<T>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let started = std::time::Instant::now();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match attempt().await {
            Ok(Some(value)) => {
                if attempts > 1 {
                    tracing::warn!(
                        stage,
                        attempts,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "startup observation ready after transient miss"
                    );
                }
                return Ok(value);
            }
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "{stage} not ready within {:?} (attempts: {attempts})",
                        budget
                    ));
                }
            }
            Err(error) => {
                if !is_transient(&error) || tokio::time::Instant::now() >= deadline {
                    if is_transient(&error) {
                        return Err(error.context(format!(
                            "{stage} not visible within {:?} (attempts: {attempts})",
                            budget
                        )));
                    }
                    return Err(error);
                }
                tracing::warn!(
                    stage,
                    attempt = attempts,
                    error = %error,
                    "transient miss during startup observation, retrying"
                );
            }
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::path::PathBuf;

    fn missing_file(path: &std::path::Path) -> anyhow::Result<String> {
        std::fs::read_to_string(path).context("read fixture")
    }

    #[tokio::test]
    async fn immediate_success_does_not_wait() {
        let started = std::time::Instant::now();
        let value = observe("immediate", Duration::from_secs(3), || async {
            Ok(Some(7u32))
        })
        .await
        .expect("ready attempt succeeds");
        assert_eq!(value, 7);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "成功路径不得新增固定等待"
        );
    }

    #[tokio::test]
    async fn transient_not_found_is_retried_until_visible() {
        // 真实 fs 注入：延迟 120ms 后另一线程落文件（barrier 语义：断言重试确实发生）
        let dir = tempfile::tempdir().expect("tempdir");
        let path: PathBuf = dir.path().join("receipt.json");
        let writer = path.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            std::fs::write(&writer, "ok").expect("write fixture late");
        });
        let value = observe("delayed-fixture", Duration::from_secs(3), || async {
            match missing_file(&path) {
                Ok(content) => Ok(Some(content)),
                Err(error) if is_not_found(&error) => Err(error),
                Err(error) => Err(error),
            }
        })
        .await
        .expect("延迟出现的文件应在预算内观察到");
        handle.join().expect("writer thread");
        assert_eq!(value, "ok");
    }

    #[tokio::test]
    async fn deadline_expiry_preserves_original_error_chain() {
        let error = observe("never-ready", Duration::from_millis(80), || async {
            missing_file(std::path::Path::new(
                "/nonexistent-dir-observe-test/receipt.json",
            ))
            .map(|content| -> Option<String> {
                drop(content);
                None
            })
        })
        .await
        .expect_err("持续缺失必须按预算失败");
        assert!(
            is_not_found(&error),
            "超时错误必须保留原 NotFound 链: {error:#}"
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("attempts"),
            "超时上下文须带尝试次数: {rendered}"
        );
    }

    #[tokio::test]
    async fn domain_pending_ok_none_expires_with_synthetic_error() {
        let error = observe("pending-forever", Duration::from_millis(60), || async {
            let pending: Option<u8> = None;
            Ok(pending)
        })
        .await
        .expect_err("领域级未就绪耗尽预算必须失败");
        assert!(
            format!("{error:#}").contains("not ready within"),
            "未就绪超时错误: {error:#}"
        );
    }

    #[tokio::test]
    async fn non_transient_error_fails_immediately() {
        let started = std::time::Instant::now();
        let error = observe::<(), _, _>("hard-fail", Duration::from_secs(3), || async {
            Err(anyhow::anyhow!("identity mismatch"))
        })
        .await
        .expect_err("非瞬态错误立即失败");
        assert!(format!("{error:#}").contains("identity mismatch"));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "非瞬态错误不得消耗预算"
        );
    }
}
