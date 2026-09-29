//! DEV-1 §3.4：本地监督目标的停止协调。
//!
//! 事故锚点（app 211）：stop 只按平台环境根解析、对旧实例残留记录完成
//! "带证据的停止"，活编排器（registry 段根）毫发无损；且 stop 成功后本地
//! 登记不收束，下一次 start 撞守卫失败。本模块把本地停止改为：
//! 只读发现 → 逐候选同一请求停止（超时可续查，不自撞 Busy）→ 全部收束
//! 后按捕获 launch 身份条件收束登记（含自己 spawn 的编排器进程清理）。

use std::path::Path;
use std::time::Duration;

use runtime_supervisor::Phase;

use super::discovery;
use super::support::lock;
use super::types::{DevServerManager, StoppedDev};
use crate::error::{AppError, AppResult};
use crate::models::KilledPid;

/// 单个候选目标的监督停止预算（沿用既有 stop_supervised_owner 的 90s：
/// 覆盖守护树排空与离线收据；超时不放大，attempt 保留续行）。
const SUPERVISION_STOP_BUDGET: Duration = Duration::from_secs(90);

impl DevServerManager {
    /// 外部 owner 链路未接手时的本地目标收束（DEV-1 §3.4）。
    pub(super) async fn stop_local_targets(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> AppResult<StoppedDev> {
        // 捕获本次停止开始时的 launch 身份：收束时 compare-and-remove，
        // 迟到的停止结果不得删除停止期间新启动实例的登记。
        let captured = lock(&self.launches)?.get(project_id).cloned();
        let captured_launch = captured.as_ref().map(|l| l.launch_id.clone());
        if let Some(root) = captured.as_ref().and_then(|l| l.state_root.as_ref()) {
            tracing::debug!(project_id, root = %root.display(), "stopping local launch");
        }

        let mut stopped_live = false;
        let mut pending: Vec<String> = Vec::new();
        for target in discovery::discover_targets(project_path) {
            // 已 Stopped 的历史根 = cleaned_stale（该根已收束），不作为项目
            // 级停止证据，也不阻挡其他候选的停止。
            if target.snapshot.phase == Phase::Stopped {
                continue;
            }
            let key = format!("{project_id}|{}", target.state_root.display());
            // §3.3 同一目标只保留一个可续查 attempt：在途即续行，不新建身份。
            // 锁内只做读存，不持 guard 跨 prepare 的 await（并行竞态下两个
            // attempt 只留一个在表内，落选方的请求 Busy 后由在表 attempt 续行）。
            let attempt = {
                let existing = lock(&self.local_stops)?.get(&key).cloned();
                match existing {
                    Some(attempt) => attempt,
                    None => {
                        let prepared = runtime_supervisor::prepare_stop_work(
                            &target.state_root,
                            target.binding(),
                        )
                        .await
                        .map_err(|error| {
                            AppError::owner_error("prepare local supervision stop", error)
                        })?;
                        lock(&self.local_stops)?.insert(key.clone(), prepared.clone());
                        prepared
                    }
                }
            };
            self.persist_local_stop(&key, &attempt);
            let mut attempt = attempt;
            match runtime_supervisor::continue_stop_work(&mut attempt, SUPERVISION_STOP_BUDGET)
                .await
            {
                Ok(_) => {
                    stopped_live = true;
                    lock(&self.local_stops)?.remove(&key);
                    self.forget_persisted_local_stop(&key);
                }
                Err(error) => {
                    if error
                        .downcast_ref::<tokio::time::error::Elapsed>()
                        .is_some()
                    {
                        // 超时/丢回复：attempt（含晚绑定的捕获身份）保留持久化，
                        // 下次重试同一请求续行；不报成功、不新建 Stop。
                        lock(&self.local_stops)?.insert(key.clone(), attempt.clone());
                        self.persist_local_stop(&key, &attempt);
                        pending.push(target.state_root.display().to_string());
                    } else {
                        // 明确拒绝（Busy/身份/协议）：清掉本 attempt，新的用户
                        // 重试可产生新身份；错误向上传播。
                        lock(&self.local_stops)?.remove(&key);
                        self.forget_persisted_local_stop(&key);
                        return Err(AppError::owner_error(
                            "local supervision stop refused",
                            error,
                        ));
                    }
                }
            }
        }
        if !pending.is_empty() {
            return Err(AppError::business(format!(
                "local stop is still in progress for {}; retry resumes the same request",
                pending.join(", ")
            )));
        }
        // 遗留本地登记（无 launch 身份、无监督目标）：登记 pid 路径 + 退出
        // 确认保护（R05——不假成功）。新式 spawn（有 launch 记录）走条件收束。
        let has_launch = lock(&self.launches)?.contains_key(project_id);
        let has_entry = lock(&self.processes)?.contains_key(project_id);
        if !has_launch && has_entry {
            return self.stop_registered_only(project_id).await;
        }
        // 全部候选已收束（或本就无候选）→ 条件收束本地登记。
        self.close_local_registration(project_id, captured_launch.as_deref())
            .await;
        Ok(StoppedDev {
            owner_stopped: stopped_live,
            killed_pids: Vec::new(),
        })
    }

    /// 条件收束（§3.4）：仅当当前 launch 仍是本次停止捕获的那次（或双方都
    /// 无 launch 记录的遗留登记）时移除 processes/supervised/launches；期间
    /// 被新 spawn 替换则保留新登记。自己 spawn 的编排器在业务停止确认后
    /// 终止其进程组（新启动需要 3010；管理进程保留语义只适用于 serve 型
    /// 外部 owner，不适用于本管理器拥有的子进程）。
    async fn close_local_registration(&self, project_id: &str, captured_launch: Option<&str>) {
        let current_launch = lock(&self.launches)
            .ok()
            .and_then(|launches| launches.get(project_id).map(|l| l.launch_id.clone()));
        let unchanged = match (captured_launch, current_launch.as_deref()) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            tracing::info!(
                project_id,
                "local registration replaced by a newer launch during stop; keeping the new entry"
            );
            return;
        }
        let proc = lock(&self.processes)
            .ok()
            .and_then(|mut m| m.remove(project_id));
        let supervised = lock(&self.supervised)
            .ok()
            .and_then(|mut m| m.remove(project_id));
        if let Ok(mut launches) = lock(&self.launches) {
            launches.remove(project_id);
        }
        let mut killed: Vec<KilledPid> = Vec::new();
        if let Some(supervised) = supervised {
            // 终止自己 spawn 的编排器进程组（业务已确认停止；此处清理管理面
            // ——旧进程持有 3010，不清理则新启动必然撞端口）。
            let killed_all = self.terminate_pid_group(supervised.pid()).await;
            killed.push(KilledPid {
                pid: supervised.pid(),
                killed: killed_all,
            });
            let drain_timeout = Duration::from_secs(
                self.config.dev_stop_max_attempts as u64 * self.config.dev_stop_check_interval_ms
                    / 1000,
            );
            supervised.drain_stdout(drain_timeout).await;
            let cleanup_map = self.cleanup_state.clone();
            let cleanup_project_id = project_id.to_string();
            tokio::spawn(async move {
                if supervised.wait_exit(drain_timeout).await.is_some()
                    && let Ok(mut cleanup) = cleanup_map.lock()
                {
                    cleanup.insert(cleanup_project_id, super::types::CleanupStatus::Cleaned);
                }
            });
        }
        if let Some(p) = proc.as_ref() {
            if p.external_owner.is_none() {
                self.port_pool.release(project_id).ok();
                super::log::cleanup_temp_logs(&p.log_dir).await;
            } else {
                // 外部 owner 登记不该出现在本地收束路径；保持原样并告警。
                tracing::warn!(
                    project_id,
                    "external-owner entry reached local registration closure"
                );
                if let Ok(mut processes) = lock(&self.processes) {
                    processes.insert(project_id.to_string(), p.clone());
                }
            }
        }
    }

    /// 持久化可续查停止（§3.3：file-server 重启后按原请求身份续行）。
    fn persist_local_stop(&self, key: &str, attempt: &runtime_supervisor::StopWorkAttempt) {
        let record = super::types::LocalStopRecord::from_attempt(attempt);
        if let Err(error) = self.external_transaction(|state| {
            state.local_stops.insert(key.to_string(), record.clone());
            anyhow::Ok(())
        }) {
            tracing::warn!(%key, %error, "persist local stop record failed")
        }
    }

    fn forget_persisted_local_stop(&self, key: &str) {
        if let Err(error) = self.external_transaction(|state| {
            state.local_stops.remove(key);
            anyhow::Ok(())
        }) {
            tracing::warn!(%key, %error, "forget local stop record failed")
        }
    }
}
