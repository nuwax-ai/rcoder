//! DEV-1 §3.4 / 复核 DEV-R4：本地监督目标的停止协调。
//!
//! 事故锚点（app 211）：stop 只按平台环境根解析、对旧实例残留记录完成
//! "带证据的停止"，活编排器（registry 段根）毫发无损；且 stop 成功后本地
//! 登记不收束，下一次 start 撞守卫失败。本模块把本地停止改为：
//! 同项目协调锁内——只读发现（含捕获根与持久停止根）→ 逐目标**占座式**
//! 同一请求停止（两个并发 Stop 共用胜者的 request_id；先持久化完整目标
//! 再发送；明确拒绝与未知结果分类处理）→ 全部收束后按捕获 launch 身份
//! 在真实退出确认后原子退休登记。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::discovery;
use super::supervise::ChildExit;
use super::support::lock;
use super::types::{DevServerManager, StoppedDev};
use crate::error::{AppError, AppResult};
use crate::models::KilledPid;

impl DevServerManager {
    /// Disk is authoritative across manager/process restarts. Do not let a
    /// stale in-memory snapshot omit a Stop captured by another caller.
    pub(super) fn pending_local_stops(
        &self,
        project_id: &str,
    ) -> AppResult<Vec<(String, runtime_supervisor::StopWorkAttempt)>> {
        let state = self
            .read_external_state()
            .map_err(|error| AppError::owner_error("read pending local stops", error))?;
        let mut attempts = Vec::new();
        for (key, record) in state.local_stops {
            if key.starts_with(&format!("{project_id}|")) {
                match record.to_attempt() {
                    Ok(attempt) => attempts.push((key, attempt)),
                    // A new explicit stop can quarantine an invalid cache in
                    // seat_stop_attempt after discovering the current target.
                    Err(error) => tracing::warn!(%key, %error, "invalid retained local stop"),
                }
            }
        }
        Ok(attempts)
    }

    /// 同项目停止协调锁：串行化整个停止（发现→监督停止→登记退休），
    /// 第二个 Stop 不会在第一个仍在收束时看到半退休状态。
    fn userapp_stop_lock(&self, project_id: &str) -> AppResult<Arc<tokio::sync::Mutex<()>>> {
        let mut locks = lock(&self.coordinated_stop_locks)?;
        locks.retain(|_, entry| Arc::strong_count(entry) > 1);
        Ok(Arc::clone(
            locks
                .entry(project_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        ))
    }

    /// 外部 owner 链路未接手时的本地目标收束（DEV-1 §3.4）。
    pub(super) async fn stop_local_targets(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> AppResult<StoppedDev> {
        let stop_lock = self.userapp_stop_lock(project_id)?;
        let _serialized = stop_lock.lock().await;
        // 捕获本次停止开始时的 launch 身份：退休时 compare-and-remove，
        // 迟到的停止结果不得删除停止期间新启动实例的登记。
        let captured = lock(&self.launches)?.get(project_id).cloned();
        let captured_launch = captured.as_ref().map(|l| l.launch_id.clone());

        // 单个候选目标的监督停止预算（默认 90s：覆盖守护树排空与离线收据；
        // 超时不放大，attempt 保留续行）。
        let supervision_budget =
            Duration::from_secs(self.config.dev_supervision_stop_budget_secs.max(1));
        // DEV-R3：候选根 = 捕获 launch 的实际根 + 持久停止记录的目标根 +
        // 平台/registry 根——实际根已在内存捕获但不在重推导候选里时不得漏停。
        let mut extra_roots: Vec<PathBuf> = Vec::new();
        if let Some(root) = captured.as_ref().and_then(|l| l.state_root.as_ref()) {
            tracing::debug!(project_id, root = %root.display(), "stopping local launch");
            extra_roots.push(root.clone());
        }
        let pending = self.pending_local_stops(project_id)?;
        let persisted_roots: Vec<PathBuf> = pending
            .iter()
            .map(|(_, attempt)| attempt.root.clone())
            .collect();
        extra_roots.extend(persisted_roots);

        let report = discovery::discover_targets_with(project_path, &extra_roots);

        let mut stopped_live = false;
        // 持久 attempt（按 key 在座）与发现的活目标统一走同一续行入口；
        // DEV-R4：持久 attempt 本身也是发现入口——即使当前 snapshot 已
        // Stopped，也要按原请求核验收据后才能收束该记录。
        let mut seated_keys: Vec<String> = pending.iter().map(|(key, _)| key.clone()).collect();
        for target in &report.targets {
            let key = format!("{project_id}|{}", target.state_root.display());
            seated_keys.retain(|k| k != &key);
            let has_seated_attempt = pending.iter().any(|(pending_key, _)| pending_key == &key);
            // 已 Stopped 的历史根且无在途 attempt = cleaned_stale（该根已
            // 收束），不重复发停止请求，也不作为项目级停止证据。
            if target.snapshot.phase == runtime_supervisor::Phase::Stopped && !has_seated_attempt {
                continue;
            }
            self.stop_seated_target(
                &key,
                &target.state_root,
                target.binding(),
                supervision_budget,
            )
            .await?;
            stopped_live = true;
        }
        // 发现结果之外的在座 attempt（目标根暂不可见/registry 缺映射）：
        // 仍按原请求续行核验，不遗忘。
        for key in seated_keys {
            if let Some((_, attempt)) = pending.iter().find(|(pending_key, _)| pending_key == &key)
            {
                self.stop_seated_target(&key, &attempt.root, &attempt.binding, supervision_budget)
                    .await?;
                stopped_live = true;
            }
        }
        // DEV-R3：没有任何已核验目标、也没有本地登记，但存在观察失败——
        // 如实报观察受阻，不把"读不到"折叠成"无进程"。
        let has_launch = lock(&self.launches)?.contains_key(project_id);
        let has_entry = lock(&self.processes)?.contains_key(project_id);
        if !has_launch && !has_entry && report.targets.is_empty() && !report.unreadable.is_empty() {
            let unreadable = report
                .unreadable
                .iter()
                .map(|(root, error)| format!("{}: {error}", root.display()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::business(format!(
                "local supervision state is unreadable; stop is not confirmed ({unreadable})"
            )));
        }
        // 遗留本地登记（无 launch 身份、无监督目标）：登记 pid 路径 + 退出
        // 确认保护（R05——不假成功）。新式 spawn（有 launch 记录）走条件收束。
        if !has_launch && has_entry {
            return self.stop_registered_only(project_id).await;
        }
        // 全部候选已收束（或本就无候选）→ 退出确认后条件退休本地登记。
        let killed = self
            .close_local_registration(project_id, captured_launch.as_deref())
            .await?;
        Ok(StoppedDev {
            owner_stopped: stopped_live,
            killed_pids: killed,
        })
    }

    /// DEV-R4：同一目标只保留一个可续查 attempt——同步临界区内占座：
    /// 在途/持久化恢复的 attempt 直接复用（两个并发 Stop 发送**同一个**
    /// request_id），落座后先持久化完整目标，失败即不发送（fail fast）。
    fn seat_stop_attempt(
        &self,
        key: &str,
        root: &Path,
        binding: &runtime_supervisor::Binding,
    ) -> AppResult<runtime_supervisor::StopWorkAttempt> {
        let attempt = self.external_transaction(|state| {
            if let Some(record) = state.local_stops.get(key) {
                match record.to_attempt() {
                    Ok(existing) => {
                        anyhow::ensure!(existing.root == root && &existing.binding == binding,
                            "local stop slot belongs to another target");
                        return Ok(existing);
                    }
                    Err(error) => {
                        // Preserve malformed transport metadata; a new explicit
                        // stop still has to capture and verify the current owner.
                        state.retired.insert(format!("{key}|{}", uuid::Uuid::new_v4()),
                            serde_json::json!({"reason": "invalid_local_stop", "error": error.to_string(), "record": record}));
                    }
                }
            }
            let fresh = runtime_supervisor::StopWorkAttempt::allocate(root, binding);
            state.local_stops.insert(key.into(), super::types::LocalStopRecord::from_attempt(&fresh));
            Ok(fresh)
        }).map_err(|error| AppError::owner_error("reserve local stop request", error))?;
        lock(&self.local_stops)?.insert(key.into(), attempt.clone());
        Ok(attempt)
    }

    /// 按 attempt 身份移除（request_id 不匹配时保留表内记录——他人的
    /// 在途 attempt 不被迟到的清理误删）。
    fn retire_seated_stop(&self, key: &str, request_id: &str) {
        if let Ok(mut stops) = lock(&self.local_stops) {
            stops.retain(|k, attempt| !(k == key && attempt.request.request_id == request_id));
        }
    }

    /// 对一个目标执行（或续行）占座停止。返回 Err 仅用于**明确拒绝**
    ///（向上传播、attempt 退役）；超时/丢回复等未知结果保持 attempt 在座
    /// 并返回"仍在进行"的业务错误（可重试、同请求续行）。
    /// start 路径的清场（`ensure_no_local_execution`）对在途 attempt 也走
    /// 此入口续行——不新造身份。
    pub(super) async fn stop_seated_target(
        &self,
        key: &str,
        root: &Path,
        binding: &runtime_supervisor::Binding,
        budget: Duration,
    ) -> AppResult<()> {
        // Start can also continue a pending Stop. Serialize that continuation
        // with Stop itself without queuing a different user operation.
        let target_lock = self.userapp_stop_lock(&format!("local-stop:{key}"))?;
        let _continuation = target_lock.try_lock().map_err(|_| {
            AppError::business(
                "local stop is already being observed; retry resumes the same request",
            )
        })?;
        let mut attempt = self.seat_stop_attempt(key, root, binding)?;
        let outcome = runtime_supervisor::continue_stop_work_with_checkpoint(
            &mut attempt,
            budget,
            |captured| self.persist_captured_stop(key, captured),
        )
        .await;
        match outcome {
            Ok(_) => {
                self.forget_persisted_local_stop(key, &attempt)?;
                self.retire_seated_stop(key, &attempt.request.request_id);
                Ok(())
            }
            Err(error) => {
                if runtime_supervisor::is_stop_refused(&error) {
                    // 明确拒绝（Busy/身份/协议/RecoveryRequired）：清掉本
                    // attempt，新的用户重试可产生新身份；错误向上传播。
                    self.forget_persisted_local_stop(key, &attempt)?;
                    self.retire_seated_stop(key, &attempt.request.request_id);
                    return Err(AppError::owner_error(
                        "local supervision stop refused",
                        error,
                    ));
                }
                // 超时/丢回复/状态不可读 = 未知结果：attempt 保留，下次重试
                // 同一请求续行；不报成功、不新建 Stop。
                Err(AppError::owner_error(
                    &format!(
                        "local stop is not confirmed for {}; retry resumes the same request",
                        root.display()
                    ),
                    error,
                ))
            }
        }
    }

    /// 条件退休（§3.4 / DEV-R2）：仅当当前 launch 仍是本次停止捕获的那次
    ///（或双方都无 launch 记录的遗留登记）时，且**编排进程真实退出确认**
    /// 后，才在单一同步临界区退休三张表；期间被新 spawn 替换则保留新登记。
    /// WaitFailed/超时≠退出（R05）——登记保留、cleanup 标记 Cleaning、返回
    /// 可重试错误，绝不在后台补写 Cleaned。StopWork 已确认业务树收束，
    /// 这里清理管理面：旧进程持有 3010，不清理则新启动必然撞端口。
    pub(super) async fn close_local_registration(
        &self,
        project_id: &str,
        captured_launch: Option<&str>,
    ) -> AppResult<Vec<KilledPid>> {
        // Check the launch before capturing a child to signal, under the same
        // lock order as publication. A post-kill comparison protects the maps
        // but is too late to protect a replacement process.
        let (captured_process, supervised) = {
            let launches = lock(&self.launches)?;
            let current_launch = launches
                .get(project_id)
                .map(|launch| launch.launch_id.as_str());
            if current_launch != captured_launch {
                tracing::info!(
                    project_id,
                    "local launch replaced before stop cleanup; keeping the new process"
                );
                return Ok(Vec::new());
            }
            let processes = lock(&self.processes)?;
            let captured = processes.get(project_id).cloned();
            if captured
                .as_ref()
                .is_some_and(|process| process.external_owner.is_some())
            {
                return Err(AppError::business(
                    "runtime owner must be stopped through its control API",
                ));
            }
            (captured, lock(&self.supervised)?.get(project_id).cloned())
        };
        let mut killed: Vec<KilledPid> = Vec::new();
        if let Some(supervised) = supervised {
            let pid = supervised.pid();
            let drain_timeout = Duration::from_secs(
                self.config.dev_stop_max_attempts as u64 * self.config.dev_stop_check_interval_ms
                    / 1000,
            );
            let terminated = self.terminate_pid_group(pid).await;
            killed.push(KilledPid {
                pid,
                killed: terminated,
            });
            supervised.drain_stdout(drain_timeout).await;
            let exited = supervised.wait_exit(drain_timeout).await;
            let confirmed = matches!(exited, Some(ChildExit::Exited(_)));
            if !confirmed {
                let launches = lock(&self.launches)?;
                if launches
                    .get(project_id)
                    .map(|launch| launch.launch_id.as_str())
                    == captured_launch
                {
                    lock(&self.cleanup_state)?.insert(
                        project_id.to_string(),
                        super::types::CleanupStatus::Cleaning,
                    );
                }
                return Err(AppError::business(
                    "local orchestrator exit not confirmed; registration kept, retry stop",
                ));
            }
        } else if let Some(process) = &captured_process
            && process.pid > 0
        {
            let terminated = self.terminate_pid_group(process.pid).await;
            killed.push(KilledPid {
                pid: process.pid,
                killed: terminated,
            });
            if !terminated {
                return Err(AppError::business(
                    "registered process exit not confirmed; retry stop",
                ));
            }
        }
        // Phase 1：真实退出确认（或本无监督句柄）→ 单一同步临界区原子退休。
        let retired_proc = {
            let mut launches = lock(&self.launches)?;
            let current_launch = launches.get(project_id).map(|l| l.launch_id.clone());
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
                return Ok(killed);
            }
            let mut processes = lock(&self.processes)?;
            // Legacy entries may not have a launch ID. Keep their registration
            // until termination, and compare the captured process before removal.
            let identity =
                |p: &crate::models::DevProcess| (p.pid, p.started_at, p.instance_id.clone());
            if processes.get(project_id).map(identity) != captured_process.as_ref().map(identity) {
                return Ok(killed);
            }
            let mut supervised_map = lock(&self.supervised)?;
            self.port_pool.release(project_id)?;
            lock(&self.cleanup_state)?
                .insert(project_id.to_string(), super::types::CleanupStatus::Cleaned);
            launches.remove(project_id);
            let proc = processes.remove(project_id);
            supervised_map.remove(project_id);
            proc
        };
        // Remove only this launch's temporary log: a successor may already be
        // writing another log in the same directory after registration retirement.
        if let Some(p) = &retired_proc
            && !p.temp_log_name.is_empty()
            && let Err(error) = tokio::fs::remove_file(p.log_dir.join(&p.temp_log_name)).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, project_id, "remove stopped launch temporary log");
        }
        Ok(killed)
    }

    /// 持久化可续查停止（§3.3：file-server 重启后按原请求身份续行）。
    /// DEV-R4：持久化失败向上传播——不能悄悄降级成"发送后再存"。
    fn persist_captured_stop(
        &self,
        key: &str,
        attempt: &runtime_supervisor::StopWorkAttempt,
    ) -> anyhow::Result<()> {
        let record = super::types::LocalStopRecord::from_attempt(attempt);
        self.external_transaction(|state| {
            let current = state
                .local_stops
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("local stop request was already retired"))?
                .to_attempt()?;
            anyhow::ensure!(
                current.request.request_id == attempt.request.request_id
                    && current.root == attempt.root
                    && current.binding == attempt.binding,
                "local stop request was replaced; reload its current identity"
            );
            anyhow::ensure!(
                current.mode == runtime_supervisor::StopMode::Unresolved || current == *attempt,
                "local stop target was captured concurrently; reload the retained request"
            );
            state.local_stops.insert(key.to_string(), record);
            anyhow::Ok(())
        })?;
        self.local_stops
            .lock()
            .map_err(|_| anyhow::anyhow!("local stop registry poisoned"))?
            .insert(key.into(), attempt.clone());
        Ok(())
    }

    fn forget_persisted_local_stop(
        &self,
        key: &str,
        attempt: &runtime_supervisor::StopWorkAttempt,
    ) -> AppResult<()> {
        self.external_transaction(|state| {
            if state
                .local_stops
                .get(key)
                .map(super::types::LocalStopRecord::to_attempt)
                .transpose()?
                .is_some_and(|current| current == *attempt)
            {
                state.local_stops.remove(key);
            }
            anyhow::Ok(())
        })
        .map_err(|error| AppError::owner_error("retire completed local stop request", error))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::supervise::SupervisedChild;
    use super::super::types::LocalLaunch;
    use super::*;

    #[test]
    fn independent_managers_share_capture_and_old_cleanup_keeps_new_request() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            log_base_dir: dir.path().join("logs"),
            ..Default::default()
        };
        let first = DevServerManager::new(Arc::new(config.clone()));
        let second = DevServerManager::new(Arc::new(config));
        let binding = runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: dir.path().join("project"),
        };
        let root = dir.path().join("state");
        let key = format!("project|{}", root.display());
        let mut captured = first.seat_stop_attempt(&key, &root, &binding).unwrap();
        let mut contender = second.seat_stop_attempt(&key, &root, &binding).unwrap();
        assert_eq!(captured, contender);
        captured.mode = runtime_supervisor::StopMode::Online {
            supervisor_id: "owner-a".into(),
        };
        first.persist_captured_stop(&key, &captured).unwrap();
        contender.mode = runtime_supervisor::StopMode::Online {
            supervisor_id: "owner-b".into(),
        };
        assert!(second.persist_captured_stop(&key, &contender).is_err());
        assert_eq!(
            second.pending_local_stops("project").unwrap()[0].1,
            captured
        );
        second
            .forget_persisted_local_stop(&key, &contender)
            .unwrap();
        assert_eq!(
            first.seat_stop_attempt(&key, &root, &binding).unwrap(),
            captured
        );
        first.forget_persisted_local_stop(&key, &captured).unwrap();
        let replacement = second.seat_stop_attempt(&key, &root, &binding).unwrap();
        assert_ne!(replacement.request.request_id, captured.request.request_id);
        first.forget_persisted_local_stop(&key, &captured).unwrap();
        assert_eq!(
            first.pending_local_stops("project").unwrap()[0].1,
            replacement
        );
    }

    #[tokio::test]
    async fn late_stop_does_not_terminate_a_replacement_launch() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = dir.path().to_path_buf();
        let manager = DevServerManager::new(Arc::new(config));
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let supervised = SupervisedChild::adopt(child, Arc::new(Default::default()));
        lock(&manager.launches).unwrap().insert(
            "project".into(),
            LocalLaunch {
                launch_id: "new-launch".into(),
                state_root: None,
            },
        );
        lock(&manager.supervised)
            .unwrap()
            .insert("project".into(), supervised.clone());
        lock(&manager.processes).unwrap().insert(
            "project".into(),
            crate::models::DevProcess {
                pid: supervised.pid(),
                port: 9080,
                project_id: "project".into(),
                instance_id: None,
                base_path: None,
                started_at: 0,
                log_dir: dir.path().to_path_buf(),
                temp_log_name: String::new(),
                external_owner: None,
            },
        );

        let result = manager
            .close_local_registration("project", Some("old-launch"))
            .await;
        let replacement_exited = supervised.wait_exit(Duration::from_millis(20)).await;
        // Reap the fixture even when the regression assertion will fail.
        manager.terminate_pid_group(supervised.pid()).await;
        supervised
            .wait_exit(Duration::from_secs(2))
            .await
            .expect("fixture exit");
        lock(&manager.processes).unwrap().remove("project");

        assert!(
            result.unwrap().is_empty(),
            "stale stop must not signal the new launch"
        );
        assert!(
            replacement_exited.is_none(),
            "new launch was terminated by the old stop"
        );
        assert_eq!(
            lock(&manager.launches).unwrap()["project"].launch_id,
            "new-launch"
        );
        assert!(Arc::ptr_eq(
            &lock(&manager.supervised).unwrap()["project"],
            &supervised
        ));
    }
}
