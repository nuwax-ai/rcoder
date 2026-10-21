//! 协调票据模式：DevServerManager 作为预览协调器的本地执行器。
//!
//! 与 legacy 路径的差异（不变量来源 spec.md 执行边界）：
//! - registry 键 = preview_key（不是 project_id），登记携带 instance_id/base_path；
//! - 端口来自协调器全局分配（不走本地 PortPool，端口记账在权威存储）；
//! - stop 只按**记录 pid** 组杀，绝不 ps 扫描、绝不误杀他实例；
//! - 身份不匹配（迟到旧操作打到新登记）返回 IdentityMismatch 而非执行。

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use shared_types::{
    ExecutorLogChunk, ExecutorStartTicket, ExecutorStopOutcome, ExecutorVerifyReport,
    PreviewExecutor, PreviewExecutorError,
};

use super::process;
use super::start::StartingGuard;
use super::support::lock;
use super::types::{DevServerManager, StartedDev};
use crate::error::{AppError, AppResult};

#[cfg(all(test, unix))]
#[path = "io_probe_tests.rs"]
mod io_probe_tests;

impl DevServerManager {
    /// 票据启动（协调器签发端口与实例身份）。
    pub async fn start_coordinated(&self, ticket: &ExecutorStartTicket) -> AppResult<StartedDev> {
        let project_path = Path::new(&ticket.project_path);
        // workspace.manifest.toml = userapp 域（app-cli 引擎），不进 Custom Page 协调。
        let manifest = project_path.join("workspace.manifest.toml");
        if tokio::fs::try_exists(&manifest).await? {
            return Err(AppError::business(
                "workspace manifest projects are userapp domain, not coordinated previews",
            ));
        }
        // 启动锁（键=preview_key；受理互斥已由权威存储保证，此处防同进程重复执行）
        {
            let mut starting = lock(&self.starting)?;
            if starting.contains(&ticket.preview_key) {
                return Err(AppError::business(
                    "preview instance already starting on this host",
                ));
            }
            starting.insert(ticket.preview_key.clone());
        }
        let _guard = StartingGuard {
            starting: &self.starting,
            project_id: ticket.preview_key.clone(),
        };
        let matching_instance = lock(&self.processes)?
            .get(&ticket.preview_key)
            .is_none_or(|record| record.instance_id.as_deref() == Some(&ticket.instance_id));
        if matching_instance {
            self.resume_pending_registered_stop(&ticket.preview_key)
                .await?;
        }
        // 幂等：同实例重放返回现有登记；异实例残留为异常态，fail-fast。
        if let Some(p) = lock(&self.processes)?.get(&ticket.preview_key).cloned() {
            if p.instance_id.as_deref() == Some(&ticket.instance_id) {
                return Ok(StartedDev {
                    pid: p.pid,
                    port: p.port,
                });
            }
            return Err(AppError::business(format!(
                "local registration for preview key exists with a different instance; refusing to start (recorded instance: {:?})",
                p.instance_id
            )));
        }

        self.spawn_and_register(
            &ticket.preview_key,
            &ticket.log_key,
            project_path,
            ticket.port,
            ticket.base_path.as_deref(),
            Some(&ticket.instance_id),
        )
        .await
    }

    /// 票据停止：仅登记匹配时按记录 pid 组杀；登记缺失/身份不符不杀任何进程。
    pub async fn stop_coordinated(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> AppResult<ExecutorStopOutcome> {
        self.stop_coordinated_with(preview_key, instance_id, |pid| {
            self.terminate_pid_group(pid)
        })
        .await
    }

    async fn stop_coordinated_with<F, Fut>(
        &self,
        preview_key: &str,
        instance_id: &str,
        terminate: F,
    ) -> AppResult<ExecutorStopOutcome>
    where
        F: FnOnce(u32) -> Fut,
        Fut: Future<Output = bool>,
    {
        let stop_lock = {
            let mut locks = lock(&self.coordinated_stop_locks)?;
            locks.retain(|_, entry| Arc::strong_count(entry) > 1);
            Arc::clone(
                locks
                    .entry(preview_key.to_string())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _stop_guard = stop_lock.lock().await;

        // Capture under the same publication lock order. The shared closure
        // retains the exact launch/Child, confirms root and tree exit, and only
        // then retires the maps. A signal callback cannot authorize retirement.
        let (proc, launch) = {
            let launches = lock(&self.launches)?;
            let processes = lock(&self.processes)?;
            (
                processes.get(preview_key).cloned(),
                launches
                    .get(preview_key)
                    .map(|launch| launch.launch_id.clone()),
            )
        };
        let Some(p) = proc else {
            return Ok(ExecutorStopOutcome::NotRegistered);
        };
        if p.instance_id.as_deref() != Some(instance_id) {
            return Ok(ExecutorStopOutcome::IdentityMismatch);
        }
        // FnOnce is enough: the fallback has at most one registered PID. Owned
        // modern children use their original managed tree instead of callback.
        let terminate = tokio::sync::Mutex::new(Some(terminate));
        self.close_local_registration_with_termination(preview_key, launch.as_deref(), |pid| {
            let terminate = &terminate;
            async move {
                let call = terminate.lock().await.take();
                match call {
                    Some(call) => call(pid).await,
                    None => false,
                }
            }
        })
        .await?;
        if lock(&self.processes)?.contains_key(preview_key) {
            return Ok(ExecutorStopOutcome::IdentityMismatch);
        }
        Ok(ExecutorStopOutcome::Stopped)
    }

    /// 纯登记身份校验（无探活；转发放行热路径）。
    pub fn registration_matches_coordinated(&self, preview_key: &str, instance_id: &str) -> bool {
        lock(&self.processes)
            .map(|m| {
                m.get(preview_key)
                    .is_some_and(|p| p.instance_id.as_deref() == Some(instance_id))
            })
            .unwrap_or(false)
    }

    /// 票据校验：登记匹配 + 进程探活（心跳/恢复判定用）。
    pub async fn verify_coordinated(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> AppResult<ExecutorVerifyReport> {
        let entry = lock(&self.processes)?.get(preview_key).cloned();
        let Some(p) = entry else {
            return Ok(ExecutorVerifyReport {
                identity_match: false,
                alive: false,
                pid: None,
                port: None,
            });
        };
        if p.instance_id.as_deref() != Some(instance_id) {
            return Ok(ExecutorVerifyReport {
                identity_match: false,
                alive: false,
                pid: Some(i64::from(p.pid)),
                port: Some(p.port),
            });
        }
        let alive = process::is_project_alive(
            p.port,
            p.base_path.as_deref(),
            self.config.dev_alive_check_timeout_ms,
        )
        .await;
        Ok(ExecutorVerifyReport {
            identity_match: true,
            alive,
            pid: Some(i64::from(p.pid)),
            port: Some(p.port),
        })
    }
}

#[cfg(all(test, unix))]
mod signal_ack_regression {
    use super::*;
    use crate::models::DevProcess;
    use std::process::Stdio;

    /// The injected callback acknowledges dispatch but deliberately sends no
    /// signal. The real child remains alive; this is not an "unkillable sleep".
    /// Old stop_coordinated_with treats this bool as completion and goes RED.
    #[tokio::test]
    async fn signal_ack_without_observed_exit_keeps_original_registration() {
        let logs = tempfile::tempdir().unwrap();
        let manager = DevServerManager::new(Arc::new(crate::Config::default()));
        let key = format!("signal-ack-{}", uuid::Uuid::new_v4().simple());
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        lock(&manager.processes).unwrap().insert(
            key.clone(),
            DevProcess {
                pid,
                port: 19002,
                project_id: key.clone(),
                started_at: 1,
                instance_id: Some("original-instance".into()),
                base_path: None,
                log_dir: logs.path().to_path_buf(),
                temp_log_name: String::new(),
                external_owner: None,
            },
        );
        let outcome = manager
            .stop_coordinated_with(&key, "original-instance", |_pid| async { true })
            .await;
        let retained = lock(&manager.processes)
            .unwrap()
            .get(&key)
            .is_some_and(|row| row.pid == pid);
        let still_alive = child.try_wait().unwrap().is_none();
        // Own-handle cleanup always precedes assertions, including an old RED.
        lock(&manager.processes).unwrap().remove(&key);
        child.kill().await.unwrap();
        let _exit = child.wait().await.unwrap();
        assert!(still_alive, "the counterexample must still be executing");
        assert!(
            outcome.is_err(),
            "signal acknowledgement was misreported as stopped: {outcome:?}"
        );
        assert!(
            retained,
            "the exact registration was retired before exit observation"
        );
    }
}

/// [`shared_types::PreviewExecutor`] 的 DevServerManager 适配（rcoder 装配时注入协调器）。
pub struct DevServerExecutor {
    manager: Arc<DevServerManager>,
}

impl DevServerExecutor {
    pub fn new(manager: Arc<DevServerManager>) -> Self {
        Self { manager }
    }
}

fn executor_error(error: AppError) -> PreviewExecutorError {
    // R11：类型化分类——PortInUse 经 AppError::ProcessPortInUse 变体
    // 传递（修改中文提示文案不再破坏分类），其余按通用失败。
    match error {
        AppError::ProcessPortInUse { port, detail } => {
            PreviewExecutorError::PortInUse(format!("port {port} in use: {detail}"))
        }
        AppError::BusinessWithDetails(message, details)
            if details.get("code").and_then(serde_json::Value::as_str)
                == Some("TYPECHECK_FAILED") =>
        {
            PreviewExecutorError::Business {
                code: "TYPECHECK_FAILED".into(),
                message,
            }
        }
        other => PreviewExecutorError::Failed(other.to_string()),
    }
}

#[async_trait::async_trait]
impl PreviewExecutor for DevServerExecutor {
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        let started = self
            .manager
            .start_coordinated(ticket)
            .await
            .map_err(executor_error)?;
        Ok((i64::from(started.pid), started.port))
    }

    async fn stop_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        self.manager
            .stop_coordinated(preview_key, instance_id)
            .await
            .map_err(executor_error)
    }

    async fn verify_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        self.manager
            .verify_coordinated(preview_key, instance_id)
            .await
            .map_err(executor_error)
    }

    async fn registration_matches(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<bool, PreviewExecutorError> {
        Ok(self
            .manager
            .registration_matches_coordinated(preview_key, instance_id))
    }

    async fn read_log_local(
        &self,
        log_key: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        let result = self
            .manager
            .read_dev_log(log_key, start_index, log_type)
            .await
            .map_err(executor_error)?;
        Ok(ExecutorLogChunk {
            logs: result
                .logs
                .into_iter()
                .map(|l| shared_types::ExecutorLogLine {
                    line: l.line,
                    content: l.content,
                })
                .collect(),
            total_lines: result.total_lines,
            log_file_name: result.log_file_name,
        })
    }
}

#[cfg(test)]
mod r11_tests {
    use super::*;
    use crate::models::DevProcess;

    /// R11：PortInUse 分类经类型化变体——修改提示文案（含删除
    /// "--strictPort 不自动换端口" 标记句）不破坏分类。
    #[test]
    fn port_in_use_classification_survives_message_changes() {
        let err = AppError::ProcessPortInUse {
            port: 5173,
            // 文案完全不含旧标记字符串
            detail: "端口被别人占了，随便说点什么".to_string(),
        };
        match executor_error(err) {
            PreviewExecutorError::PortInUse(_) => {}
            other => panic!("expected PortInUse, got {other:?}"),
        }
        // 非 PortInUse 系统错误即使含旧标记文案也走 Failed（反例方向）
        match executor_error(AppError::system("boom --strictPort 不自动换端口")) {
            PreviewExecutorError::Failed(_) => {}
            other => panic!("system error with marker text must stay Failed, got {other:?}"),
        }
    }

    // 仅 POSIX：本用例用合成 PID + 无 supervised 记录，走「无受管子兄弟、靠 PID
    // 观测退出」的路径，验证第一个 stop 在退出观测确认前挂起、第二个 stop 等锁。
    // Windows 上 stop_exit_observed 是 cfg(windows) 的专用实现——脱离原始受管
    // 句柄无法观测进程树退出，直接返回 Err（"requires its original managed child
    // handle"），第一个 stop 会提前报错返回、根本进不到 terminate 挂起那一步，
    // 整个用例前提（release 前一直挂起）不成立。
    #[cfg(unix)]
    #[tokio::test]
    async fn overlapping_stops_wait_for_process_exit_before_reporting_not_registered() {
        let logs = tempfile::tempdir().expect("test logs");
        let manager = Arc::new(DevServerManager::new(Arc::new(crate::Config::default())));
        let key = "coordinated-stop-race";
        let instance_id = "instance-one";
        lock(&manager.processes).expect("process registry").insert(
            key.to_string(),
            DevProcess {
                pid: 2_147_483_646,
                port: 4001,
                project_id: key.to_string(),
                instance_id: Some(instance_id.to_string()),
                base_path: None,
                started_at: 0,
                log_dir: logs.path().to_path_buf(),
                temp_log_name: String::new(),
                external_owner: None,
            },
        );
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first_manager = Arc::clone(&manager);
        let first = tokio::spawn(async move {
            first_manager
                .stop_coordinated_with(key, instance_id, |_pid| async move {
                    entered_tx.send(()).expect("signal first stop entered");
                    release_rx.await.expect("release first stop");
                    true
                })
                .await
        });
        entered_rx.await.expect("first stop entered termination");
        let second_manager = Arc::clone(&manager);
        let mut second = tokio::spawn(async move {
            second_manager
                .stop_coordinated_with(key, instance_id, |_pid| async move {
                    panic!("second stop must not terminate the same process")
                })
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "second stop must wait for the first stop to finish"
        );
        assert!(
            lock(&manager.processes)
                .expect("process registry")
                .contains_key(key),
            "registration stays visible while process termination is pending"
        );
        release_tx.send(()).expect("release first stop");
        assert!(matches!(
            first.await.expect("first task").expect("first stop"),
            ExecutorStopOutcome::Stopped
        ));
        assert!(matches!(
            second.await.expect("second task").expect("second stop"),
            ExecutorStopOutcome::NotRegistered
        ));
    }
}
