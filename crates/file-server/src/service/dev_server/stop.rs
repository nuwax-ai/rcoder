//! dev server 终止: stop_dev / shutdown_all / Drop 兜底。

use std::collections::HashSet;

use super::log;
use super::process;
use super::support::lock;
use super::types::{CleanupStatus, DevServerManager, StoppedDev};
use crate::error::{AppError, AppResult};
use crate::models::KilledPid;

impl DevServerManager {
    /// stop-dev (对齐 nuwax stopDevServerByProjectId; 系统级 pid 扫描 + 杀整组 +
    /// 释放端口 + 清 temp 日志)。候选 pid = 内存 Map pid ∪ `ps` 扫描 pid (去重)。
    pub async fn stop_dev(&self, project_id: &str) -> AppResult<StoppedDev> {
        let proc = lock(&self.processes)?.remove(project_id);
        // P3-02：外部 owner——经运行 API 提交 Stop（不经进程信号、不扫 ps、
        // 不杀我们从未 spawn 的进程树）。
        if let Some(external) = proc.as_ref().and_then(|p| p.external_owner.as_ref()) {
            return self.stop_external_owner(project_id, external).await;
        }
        // P1-03：监督句柄同步摘除（进程组终止后句柄的 wait worker 自行收割
        // 退出；此处不等待——停止确认语义在 dev 任务层经 wait_exit 处理）。
        let supervised = lock(&self.supervised)?.remove(project_id);
        // 候选 pid: 内存 Map + 系统扫描 (去重)
        let mut candidates: Vec<u32> = Vec::new();
        if let Some(p) = &proc {
            candidates.push(p.pid);
        }
        candidates.extend(process::find_pids_by_project_id(project_id).await);
        candidates.sort_unstable();
        candidates.dedup();

        let candidates: Vec<(u32, Option<u32>)> = candidates
            .into_iter()
            .map(|pid| (pid, process::process_group_id(pid)))
            .collect();
        let mut stopped_groups = HashSet::new();
        let mut killed: Vec<KilledPid> = Vec::new();
        for (pid, pgid) in candidates {
            // 第一个成员已通过 kill(-pgid) 停止整组，其余成员不应
            // 因无法再次发送信号而被误报为 false。
            if pgid.is_some_and(|group| stopped_groups.contains(&group)) {
                killed.push(KilledPid { pid, killed: true });
                continue;
            }
            let k = self.terminate_pid_group(pid).await;
            if k && let Some(group) = pgid {
                stopped_groups.insert(group);
            }
            killed.push(KilledPid { pid, killed: k });
        }
        if let Some(p) = proc {
            self.port_pool.release(project_id);
            log::cleanup_temp_logs(&p.log_dir).await;
        }
        // stdout 管道有界排空（进程组已停；后代持有写端时窗口到期放弃）。
        if let Some(supervised) = supervised {
            let drain_timeout = std::time::Duration::from_secs(
                self.config.dev_stop_max_attempts as u64 * self.config.dev_stop_check_interval_ms
                    / 1000,
            );
            supervised.drain_stdout(drain_timeout).await;
            // P1-05：记录 cleanup_status。即使进程已退出，仍可能有子进程继承 stdout
            // 导致 EOF 未到达——在 wait_exit 结束前标记 Cleaning，完成后再标 Cleaned。
            {
                let mut cleanup = lock(&self.cleanup_state)?;
                cleanup.insert(project_id.to_string(), CleanupStatus::Cleaning);
            }
            let cleanup_project_id = project_id.to_string();
            let cleanup_map = self.cleanup_state.clone();
            tokio::spawn(async move {
                if supervised.wait_exit(drain_timeout).await.is_some()
                    && let Ok(mut cleanup) = cleanup_map.lock()
                {
                    cleanup.insert(cleanup_project_id, CleanupStatus::Cleaned);
                }
            });
        }
        Ok(StoppedDev {
            killed_pids: killed,
        })
    }

    /// 外部 owner 的停止：提交 Stop 操作并等待终态。管理面保持运行
    /// （owner 语义），业务停止以操作 Succeeded 为证。
    async fn stop_external_owner(
        &self,
        project_id: &str,
        external: &crate::models::ExternalOwner,
    ) -> AppResult<StoppedDev> {
        let workspace_id = project_id
            .rsplit(':')
            .next()
            .unwrap_or(project_id)
            .to_string();
        let route = async {
            let client = super::owner_client::OwnerClient::new(&external.address, &external.token)?;
            let status = client.status().await?;
            let operation_id = format!("fs-stop-{}", uuid::Uuid::new_v4().simple());
            client
                .submit_stop(
                    &operation_id,
                    &workspace_id,
                    status.revision,
                    &external.runtime_instance_id,
                )
                .await?;
            client
                .wait_terminal(&operation_id, std::time::Duration::from_secs(120))
                .await
        };
        let view = route
            .await
            .map_err(|error| AppError::business(format!("stop external owner: {error:#}")))?;
        if !matches!(
            view.state,
            shared_types::RuntimeOperationState::Succeeded
                | shared_types::RuntimeOperationState::Cancelled
        ) {
            return Err(AppError::business(format!(
                "external owner stop failed: {:?} ({})",
                view.state,
                view.error_message.as_deref().unwrap_or("no detail"),
            )));
        }
        Ok(StoppedDev {
            killed_pids: Vec::new(),
        })
    }

    /// 单 pid 的终止升级（SIGTERM 宽限 → SIGKILL）。供 legacy stop_dev（候选循环）
    /// 与协调票据 stop_coordinated（仅记录 pid）共用。
    pub(super) async fn terminate_pid_group(&self, pid: u32) -> bool {
        let ok = process::kill_process_group(pid);
        process::wait_for_stop(
            pid,
            self.config.dev_stop_check_interval_ms,
            self.config.dev_stop_max_attempts,
        )
        .await;
        let mut k = ok;
        if process::is_process_running(pid) {
            tracing::warn!("dev server (pid {pid}) 未在 SIGTERM 宽限期退出, 升级 SIGKILL");
            let force_sent = process::kill_process_group_force(pid);
            process::wait_for_stop(
                pid,
                self.config.dev_stop_check_interval_ms,
                self.config.dev_stop_max_attempts,
            )
            .await;
            // zombie 进程在父进程回收前 `kill(pid, 0)` 仍会返回存在，
            // 但 SIGKILL 已成功送达时业务上应视为 killed，对齐 nuwax killProcess。
            k = k || force_sent || !process::is_process_running(pid);
        }
        k
    }

    /// 全量优雅停止 (供 main.rs graceful shutdown 调用):
    /// 逐个项目走完整 `stop_dev` 流程 (SIGTERM → 等 → SIGKILL + ps 扫描 + 还端口 + 清日志)。
    /// 幂等: 进程已不在也安全返回; 单项失败记 warn 不中断其余。
    pub async fn shutdown_all(&self) {
        let snapshot: Vec<String> = lock(&self.processes)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if snapshot.is_empty() {
            return;
        }
        tracing::info!("shutdown_all: stopping {} dev server(s)", snapshot.len());
        for project_id in snapshot {
            if let Err(e) = self.stop_dev(&project_id).await {
                tracing::warn!(%project_id, "shutdown_all stop failed: {e}");
            }
        }
    }
}

/// Drop 兜底: 正常路径 (graceful shutdown) 已由 `shutdown_all` 清空实例表;
/// 此处只防 "panic / Arc 提前释放 / shutdown_all 未触发" 残留。
///
/// 约束: `Drop::drop` 不能 `.await`, 无法给 SIGTERM grace 宽限期 ——
/// 发 SIGTERM 后无等待地 SIGKILL 等价于直接 SIGKILL, 故此处省去无意义的 SIGTERM,
/// 直接对进程组 SIGKILL, 确保进程终止并还端口、清表。
///
/// ⚠️ 不覆盖场景: file-server 自身被 SIGKILL 强杀时进程直接终止, **Drop 不会执行**,
/// detached 的 dev server 仍会成孤儿 —— 这是 detached 模型的固有局限, 只能靠
/// 容器/编排层 (Pod 退出回收) 兜底。正常重启走 SIGTERM → `shutdown_all` 路径已覆盖。
impl Drop for DevServerManager {
    fn drop(&mut self) {
        let Ok(procs) = self.processes.get_mut() else {
            return;
        };
        if procs.is_empty() {
            return;
        }
        tracing::warn!(
            "DevServerManager dropped with {} live dev server(s) — best-effort SIGKILL",
            procs.len()
        );
        for (project_id, p) in procs.iter() {
            // 兜底硬杀: SIGKILL 进程组 (无法 await, 故不走 SIGTERM→等→SIGKILL 升级)
            if !process::kill_process_group_force(p.pid) {
                tracing::warn!(%project_id, pid = p.pid, "SIGKILL failed in Drop");
            }
            self.port_pool.release(project_id);
        }
        procs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::dev_server::{StderrRing, supervise::SupervisedChild};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn manager() -> DevServerManager {
        let mut config = crate::Config::from_env().expect("test config");
        config.dev_stop_check_interval_ms = 10;
        config.dev_stop_max_attempts = 20;
        DevServerManager::new(Arc::new(config))
    }

    fn spawn_child(cmd: &str) -> tokio::process::Child {
        tokio::process::Command::new("/bin/sh")
            .args(["-c", cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child")
    }

    fn adopt(child: tokio::process::Child) -> Arc<SupervisedChild> {
        let ring: Arc<StderrRing> = Arc::new(Mutex::new(VecDeque::new()));
        SupervisedChild::adopt(child, ring)
    }

    #[tokio::test]
    async fn stop_cleanup_state_transitions_to_cleaned_for_immediate_exit() {
        let mgr = manager();
        let project = "p1-05-clean";
        let child = spawn_child("true");
        let supervised = adopt(child);
        // immediate-exit child may already be gone; don't assert wait_exit state
        mgr.supervised
            .lock()
            .unwrap()
            .insert(project.to_string(), supervised);

        let _stopped = mgr.stop_dev(project).await.expect("stop");

        // short-lived child already exited -> cleanup should quickly flip to Cleaned.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!mgr.has_uncleaned_cleanup(project).unwrap());
    }

    #[tokio::test]
    async fn stop_cleanup_remains_cleaning_until_exit_observed() {
        let mgr = manager();
        let project = "p1-05-hang";
        let child = spawn_child("sleep 30");
        let supervised = adopt(child);
        mgr.supervised
            .lock()
            .unwrap()
            .insert(project.to_string(), supervised);

        let _stopped = mgr.stop_dev(project).await.expect("stop");

        // Immediately after stop_dev returns, cleanup watcher may still be pending.
        // With a long-lived child, has_uncleaned_cleanup should initially be true.
        assert!(mgr.has_uncleaned_cleanup(project).unwrap());
    }
}
