//! Dev server 进程管理器 (对齐 nuwax `processManager.js` + `startDevUtils.js` 等)。
//!
//! 替换 nuwax 的裸 `child_process.spawn` (detached) + 内存 Map。
//! 用 `tokio::process::Command` (process_group + 丢弃 Child 句柄) + `Mutex<HashMap>`。
//!
//! 拆分: [`types`] (类型定义 + 管理器本体) / [`start`] (启动 + 就绪轮询 + 守卫) /
//! [`stop`] (终止 + shutdown_all + Drop)。本 mod.rs 保留组合方法 (restart/keep-alive/查询)。
//!
//! 注意 (对齐 nuwax 已知宽松行为):
//! - 纯内存状态, 无持久化 (重启即丢)
//! - 存活轮询超时后仍返回成功 (nuwax 行为)
//! - keep-alive 被动探活, 无空闲自动停止

pub mod error_classify;
pub mod log;
pub mod port_pool;
pub mod process;
mod start;
mod stop;
mod support;
mod types;

pub use error_classify::{STDERR_RING_CAP, StderrRing, ViteStartupError};
pub use log::read_dev_log;
pub use port_pool::{PortPool, PortPoolStatus};
pub use process::{is_process_running, is_project_alive, kill_process_group, now_ms};
pub use types::KeepAliveResult;
pub use types::{DevServerManager, StartedDev, StoppedDev};

use crate::models::{DevProcess, ReadDevLogResult};

use std::path::Path;

use crate::error::AppResult;
use support::lock;

impl DevServerManager {
    /// restart-dev = stop + start。
    pub async fn restart_dev(
        &self,
        project_id: &str,
        project_path: &Path,
        base_path: Option<&str>,
    ) -> AppResult<StartedDev> {
        self.stop_dev(project_id).await?;
        self.start_dev(project_id, project_path, base_path).await
    }

    /// keep-alive (对齐 nuwax: 探活, 不存活则重启)。
    pub async fn keep_alive(
        &self,
        project_id: &str,
        _pid: u32,
        port: u16,
        base_path: Option<&str>,
        project_path: &Path,
    ) -> AppResult<KeepAliveResult> {
        let alive = is_project_alive(port, base_path, self.config.dev_alive_check_timeout_ms).await;
        if alive {
            return Ok(KeepAliveResult {
                alive: true,
                action: None,
                pid: None,
                port: None,
            });
        }
        // 不存活 → 重启, 返回新 pid/port (对齐 nuwax 透传 startDevServer 返回值)
        self.stop_dev(project_id).await?;
        let started = self.start_dev(project_id, project_path, base_path).await?;
        Ok(KeepAliveResult {
            alive: true,
            action: Some("start".into()),
            pid: Some(started.pid),
            port: Some(started.port),
        })
    }

    /// list-dev: 内存 Map 快照。
    pub fn list_dev(&self) -> AppResult<Vec<DevProcess>> {
        Ok(lock(&self.processes)?.values().cloned().collect())
    }

    /// port-pool-status。
    pub fn port_pool_status(&self) -> AppResult<PortPoolStatus> {
        self.port_pool.status()
    }

    /// get-dev-log。
    pub async fn read_dev_log(
        &self,
        project_id: &str,
        start_index: usize,
        log_type: &str,
    ) -> AppResult<ReadDevLogResult> {
        let dir = log::log_dir(&self.config, project_id);
        read_dev_log(&dir, start_index, log_type, self.config.log_read_max_bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn build_args_vite_with_base() {
        let d = build_args_test("vite", 4001, Some("foo")).unwrap();
        assert_eq!(d.program, "npx");
        assert!(d.args.iter().any(|a| a == "vite"));
        assert!(d.args.iter().any(|a| a == "4001"));
        assert!(d.args.iter().any(|a| a == "--strictPort"));
        assert!(d.args.iter().any(|a| a == "0.0.0.0"));
        // --clearScreen false 抑制 ANSI 清屏转义污染日志
        assert!(d.args.iter().any(|a| a == "--clearScreen"));
        assert!(
            d.args
                .windows(2)
                .any(|w| w[0] == "--clearScreen" && w[1] == "false")
        );
        assert!(d.args.iter().any(|a| a == "--base"));
        assert!(d.args.iter().any(|a| a == "/foo/"));
        assert!(d.env_extra.is_empty());
    }

    #[test]
    fn build_args_next_sets_base_env() {
        let d = build_args_test("next dev", 4002, Some("bar")).unwrap();
        assert_eq!(d.program, "npx");
        assert!(d.args.iter().any(|a| a == "next"));
        assert!(d.args.iter().any(|a| a == "4002"));
        assert!(
            d.env_extra
                .iter()
                .any(|(k, v)| k == "BASE_PATH" && v == "/bar/")
        );
    }

    #[test]
    fn build_args_rejects_unsupported() {
        assert!(build_args_test("webpack serve", 4003, None).is_err());
    }

    #[test]
    fn build_args_default_base_is_full_proxy_path() {
        // base 为空时默认 /proxy/{port}/ (HMR 依赖: base 必须是完整代理路径)
        let d = build_args_test("vite", 4005, None).unwrap();
        assert!(d.args.iter().any(|a| a == "--base"));
        assert!(d.args.iter().any(|a| a == "/proxy/4005/"));
    }

    fn build_args_test(script: &str, port: u16, base: Option<&str>) -> AppResult<process::DevArgs> {
        process::build_dev_args(script, port, base)
    }

    #[tokio::test]
    async fn shutdown_all_empty_is_noop() {
        // 空实例表 → 立即返回, 不 panic
        let mgr = DevServerManager::new(std::sync::Arc::new(
            Config::from_env().expect("test config"),
        ));
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn drop_with_stale_entry_does_not_panic() {
        // 塞一个不可能存活的 pid: Drop 对其 SIGKILL 返回 false 但不 panic, 仍还端口 + 清表
        let mgr = DevServerManager::new(std::sync::Arc::new(
            Config::from_env().expect("test config"),
        ));
        {
            let mut procs = mgr.processes.lock().unwrap();
            procs.insert(
                "ghost".to_string(),
                DevProcess {
                    pid: 999_999,
                    port: 0,
                    project_id: "ghost".to_string(),
                    started_at: 0,
                    log_dir: std::path::PathBuf::from("/tmp/nonexistent-fs-test"),
                    temp_log_name: "ghost.log".to_string(),
                },
            );
        }
        drop(mgr); // 不 panic 即通过
    }

    #[tokio::test]
    async fn poll_alive_returns_promptly_when_server_ready() {
        // 探活用即时返回 true 的 stub：绕开 reqwest Client 构建 / 系统 CA 加载的固有延迟
        // （macOS keychain 首加载数百 ms，会让真实探活的计时断言 flaky）。焦点纯粹在 poll_alive
        // 的循环逻辑——server ready 时首轮探活即返回，而非固定盲等 sleep。
        let mut config = Config::from_env().expect("test config");
        config.dev_alive_poll_interval_ms = 10;
        let mgr = DevServerManager::new(std::sync::Arc::new(config));
        // pid 用当前进程 → is_process_running 恒 true
        let pid = std::process::id();
        let ring: std::sync::Arc<StderrRing> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let start = std::time::Instant::now();
        let res = mgr
            .poll_alive(pid, 0, None, &ring, &|_port, _base, _timeout| {
                Box::pin(async { true })
            })
            .await;
        let elapsed = start.elapsed();
        assert!(res.is_ok(), "ready 时 poll_alive 应成功");
        // 即时探活 + 10ms 间隔，首轮即返回，应远 < 200ms（原固定 1s sleep > 1s）。
        assert!(
            elapsed.as_millis() < 200,
            "poll_alive 耗时 {elapsed:?}, 期望 < 200ms (即时探活 stub, 首轮即返回)"
        );
    }
}
