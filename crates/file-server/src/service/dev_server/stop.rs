//! dev server 终止: stop_dev / shutdown_all / Drop 兜底。

use std::collections::HashSet;
use std::path::Path;

use super::log;
use super::process;
use super::support::lock;
use super::types::{CleanupStatus, DevServerManager, StoppedDev};
use crate::error::{AppError, AppResult};
use crate::models::KilledPid;

impl DevServerManager {
    /// stop-dev (对齐 nuwax stopDevServerByProjectId; 系统级 pid 扫描 + 杀整组 +
    /// 释放端口 + 清 temp 日志)。候选 pid = 内存 Map pid ∪ `ps` 扫描 pid (去重)。
    ///
    /// R05：external owner 停止未确认终态前**不移除登记**（先取快照；
    /// Succeeded 确认后由 [`Self::stop_external_owner`] 摘除）。
    pub async fn stop_dev(&self, project_id: &str) -> AppResult<StoppedDev> {
        self.check_external_store().map_err(|error| {
            AppError::business(format!("external owner recovery required: {error:#}"))
        })?;
        let snapshot = lock(&self.processes)?.get(project_id).cloned();
        if let Some(external) = snapshot.as_ref().and_then(|p| p.external_owner.as_ref()) {
            return self.stop_external_owner(project_id, external).await;
        }
        if self
            .read_external_state()
            .map_err(|error| AppError::business(format!("external recovery required: {error:#}")))?
            .owners
            .contains_key(project_id)
        {
            return Err(AppError::business(
                "persisted owner registration is not loaded; refusing process-name cleanup",
            ));
        }
        let proc = lock(&self.processes)?.remove(project_id);
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

    /// UserApp manifest 域的停止（R05）：**禁止 ps 扫描兜底**——managed 模式
    /// 下登记缺失不授权 legacy 清理。
    /// - 有 external 登记 → 经运行 API 停止（[`Self::stop_external_owner`]，
    ///   幂等按原 operation_id 恢复）；
    /// - 无登记但 3010 有 owner 应答 → 拒绝（诊断明确：owner 仍持有运行态，
    ///   不得按进程名/端口抢杀）；
    /// - 无登记且无应答 → 幂等成功（无运行态）。
    pub async fn stop_userapp_dev(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> AppResult<StoppedDev> {
        self.check_external_store().map_err(|error| {
            AppError::business(format!("external owner recovery required: {error:#}"))
        })?;
        let snapshot = lock(&self.processes)?.get(project_id).cloned();
        if let Some(mut external) = snapshot
            .as_ref()
            .and_then(|p| p.external_owner.as_ref().cloned())
        {
            // 恢复的登记 token 为空哨兵（凭据不落盘）——从 owner 状态根重读；
            // 读不到时保持空值（owner 拒绝认证，错误可诊断）
            if external.token.is_empty() {
                let app_id = std::env::var("PROJECT_ID")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "unknown-app".to_string());
                if let Some((_root, token)) =
                    super::owner_client::find_owner_token(project_path, &app_id)
                {
                    external.token = token;
                    if let Ok(mut processes) = lock(&self.processes)
                        && let Some(record) = processes.get_mut(project_id)
                        && let Some(owner) = record.external_owner.as_mut()
                    {
                        owner.token = external.token.clone();
                    }
                }
            }
            return self.stop_external_owner(project_id, &external).await;
        }
        let owner_addr = self.config.app_cli_admin_probe_addr.clone();
        if super::owner_client::probe_owner(&owner_addr)
            .await
            .is_some()
        {
            return Err(AppError::business(format!(
                "admin port {owner_addr} is held by a running app-cli owner but this \
                 server has no registration for it (restarted?); stop must be routed \
                 through the runtime owner API — refusing process-name based cleanup"
            )));
        }
        // 无 owner 应答：本地 spawn 登记（supervised）或无运行态都收束为无 external
        // 停止；本地登记存在时仍走 stop_dev 的登记 pid 路径（ps 扫描跳过——
        // managed 域不杀我们未 spawn 的进程）。
        if snapshot.is_some() {
            self.stop_registered_only(project_id).await
        } else {
            Ok(StoppedDev {
                killed_pids: Vec::new(),
            })
        }
    }

    /// 只停**登记的**本地 pid（managed 域：无 ps 扫描、无端口猜杀）。
    async fn stop_registered_only(&self, project_id: &str) -> AppResult<StoppedDev> {
        let Some(proc) = lock(&self.processes)?.remove(project_id) else {
            return Ok(StoppedDev {
                killed_pids: Vec::new(),
            });
        };
        let supervised = lock(&self.supervised)?.remove(project_id);
        let mut killed = Vec::new();
        if proc.pid > 0 {
            let ok = self.terminate_pid_group(proc.pid).await;
            killed.push(KilledPid {
                pid: proc.pid,
                killed: ok,
            });
        }
        self.port_pool.release(project_id);
        log::cleanup_temp_logs(&proc.log_dir).await;
        if let Some(supervised) = supervised {
            let drain_timeout = std::time::Duration::from_secs(
                self.config.dev_stop_max_attempts as u64 * self.config.dev_stop_check_interval_ms
                    / 1000,
            );
            supervised.drain_stdout(drain_timeout).await;
            if supervised.wait_exit(drain_timeout).await.is_none() {
                // 本地编排进程未确认退出：登记保留保护（R05——不假成功）
                lock(&self.processes)?.insert(project_id.to_string(), proc);
                return Err(AppError::business(
                    "local orchestrator exit not confirmed; registration kept, retry stop",
                ));
            }
        }
        Ok(StoppedDev {
            killed_pids: killed,
        })
    }

    /// 外部 owner 的停止：提交 Stop 操作并等待终态。管理面保持运行
    /// （owner 语义），业务停止以操作 Succeeded 为证。
    ///
    /// R04/R05：
    /// - **Cancelled 不是停止成功**——取消既不证明业务已停也无清理证据；
    /// - 登记与在途操作记录在确认 Succeeded 前全部保留（含跨 file-server
    ///   重启的持久化恢复）；重试按**原 operation_id** 查询（响应丢失/
    ///   轮询断连不产生新 ID、不退回 ps 扫描）。
    async fn stop_external_owner(
        &self,
        project_id: &str,
        external: &crate::models::ExternalOwner,
    ) -> AppResult<StoppedDev> {
        // R05 幂等恢复：在途停止操作按原 ID 续查，不产生新 ID
        let pending = lock(&self.external_stops)?.get(project_id).cloned();
        let route = async {
            let identity = super::owner_client::probe_owner(&external.address)
                .await
                .ok_or_else(|| anyhow::anyhow!("external owner identity unavailable"))?;
            anyhow::ensure!(
                super::owner_client::protocol_compatible(&identity)
                    && identity.runtime_instance_id == external.runtime_instance_id
                    && !identity.workspace_id.trim().is_empty(),
                "external owner identity changed; refusing to transmit credentials"
            );
            let workspace_id = identity.workspace_id.clone();
            if let Some(record) = &pending {
                anyhow::ensure!(
                    record.workspace_id == workspace_id,
                    "pending stop workspace does not match the registered owner"
                );
            }

            let token = if external.token.is_empty() {
                let source = Path::new(&identity.source_root);
                anyhow::ensure!(
                    source.is_absolute() && !identity.source_root.trim().is_empty(),
                    "registered owner source root is invalid"
                );
                super::owner_client::find_owner_token(source, &identity.application_id)
                    .map(|(_, token)| token)
                    .ok_or_else(|| anyhow::anyhow!("registered owner credentials unavailable"))?
            } else {
                external.token.clone()
            };
            let client = super::owner_client::OwnerClient::new(&external.address, &token)?;
            if let Some(record) = pending {
                // Legacy files only have an ID: never invent an original revision for replay.
                let view = client
                    .wait_terminal(&record.operation_id, std::time::Duration::from_secs(120))
                    .await?;
                anyhow::ensure!(
                    view.operation_id == record.operation_id
                        && view.runtime_instance_id == external.runtime_instance_id
                        && view.kind == shared_types::RuntimeOperationKind::Stop,
                    "legacy stop identity mismatch"
                );
                if matches!(
                    view.state,
                    shared_types::RuntimeOperationState::Succeeded
                        | shared_types::RuntimeOperationState::Failed
                        | shared_types::RuntimeOperationState::Cancelled
                ) {
                    self.external_transaction(|state| {
                        let original = state
                            .stops
                            .get(project_id)
                            .ok_or_else(|| anyhow::anyhow!("legacy stop record missing"))?;
                        anyhow::ensure!(
                            original.operation_id == record.operation_id,
                            "legacy stop changed"
                        );
                        state.stops.remove(project_id);
                        if view.state == shared_types::RuntimeOperationState::Succeeded {
                            state.owners.remove(project_id);
                        }
                        Ok(())
                    })?;
                    lock(&self.external_stops)
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                        .remove(project_id);
                }
                return Ok::<_, anyhow::Error>(view);
            }
            let status = client.status().await?;
            let request = shared_types::RuntimeOperationRequest {
                operation_id: format!("fs-stop-{}", uuid::Uuid::new_v4().simple()),
                expected_runtime_instance_id: external.runtime_instance_id.clone(),
                expected_revision: status.revision,
                workspace_id: workspace_id.clone(),
                kind: shared_types::RuntimeOperationKind::Stop,
                profile: shared_types::RunProfileInput::Source { workspace_id },
                run_config: None,
                request_context: None,
            };
            let intent = self.prepare_external_intent(
                project_id,
                Path::new(&identity.source_root),
                external,
                &request,
            )?;
            self.resume_external_intent(project_id, &client, &intent, &request)
                .await?;
            let view = client
                .wait_terminal(
                    &intent.request.operation_id,
                    std::time::Duration::from_secs(120),
                )
                .await?;
            super::external_store::verify_view(&view, &intent.request)?;
            if matches!(
                view.state,
                shared_types::RuntimeOperationState::Succeeded
                    | shared_types::RuntimeOperationState::Failed
                    | shared_types::RuntimeOperationState::Cancelled
            ) {
                self.finish_external_intent(
                    project_id,
                    &intent.request,
                    view.state == shared_types::RuntimeOperationState::Succeeded,
                )?;
            }
            Ok(view)
        };
        let view = route
            .await
            .map_err(|error| AppError::business(format!("stop external owner: {error:#}")))?;
        match view.state {
            shared_types::RuntimeOperationState::Succeeded => {
                // 确认终态后才移除登记与在途记录（R05）
                lock(&self.processes)?.remove(project_id);
                lock(&self.external_stops)?.remove(project_id);

                Ok(StoppedDev {
                    killed_pids: Vec::new(),
                })
            }
            shared_types::RuntimeOperationState::Cancelled => Err(AppError::business(format!(
                "external owner stop was cancelled (operation {}); registration kept — \
                 a new explicit stop may be requested",
                view.operation_id
            ))),
            other => Err(AppError::business(format!(
                "external owner stop not confirmed: {other:?} ({}) — registration kept",
                view.error_message.as_deref().unwrap_or("no detail"),
            ))),
        }
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

#[cfg(test)]
mod external_stop_tests {
    use super::*;
    use crate::models::{DevProcess, ExternalOwner};
    use shared_types::{RuntimeOperationState, RuntimeOperationView};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// 可编程 mock owner（独立端口——不经 3010，避免跨测试竞争）：
    /// - `submitted`：受理过的 operation_id（断言"不产生新 ID"）；
    /// - `states`：op id → 终态（未登记 = Running 非终态）。
    #[derive(Clone, Default)]
    struct StopMockState {
        submitted: Arc<Mutex<Vec<String>>>,
        states: Arc<Mutex<HashMap<String, RuntimeOperationState>>>,
    }

    fn envelope<T: serde::Serialize>(data: &T) -> serde_json::Value {
        serde_json::json!({"success": true, "code": "OK", "data": data, "message": "ok"})
    }

    fn view(id: &str, state: RuntimeOperationState) -> RuntimeOperationView {
        RuntimeOperationView {
            operation_id: id.to_string(),
            kind: shared_types::RuntimeOperationKind::Stop,
            state,
            request_digest: "digest".to_string(),
            revision: 1,
            runtime_instance_id: "instance-test".to_string(),
            error_code: None,
            error_message: None,
            failure_detail: None,
        }
    }

    async fn serve_stop_mock(state: StopMockState) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("addr").to_string();
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(|| async {
                    axum::Json(envelope(&shared_types::RuntimeIdentityView {
                        application_id: "app-r05".into(),
                        service_family: "userapp-dev".into(),
                        workspace_id: "ws-stable-hash".into(),
                        source_root: "/workspace".into(),
                        runtime_instance_id: "instance-test".into(),
                        deployment_generation_id: "gen-test".into(),
                        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                        capabilities: vec![],
                    }))
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(|| async {
                    let status = shared_types::RuntimeStatusView {
                        desired: shared_types::DesiredState::Stopped,
                        observed: shared_types::ObservedHealth::Stopped,
                        active_target: None,
                        revision: 1,
                        active_operation_id: None,
                        recovery_protection: false,
                        runtime_instance_id: "instance-test".to_string(),
                    };
                    axum::Json(envelope(&status))
                }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(
                    |axum::extract::State(state): axum::extract::State<StopMockState>,
                     axum::Json(req): axum::Json<serde_json::Value>| {
                        async move {
                            let id = req
                                .get("operation_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("op")
                                .to_string();
                            assert_eq!(req["workspace_id"], "ws-stable-hash");
                            state.submitted.lock().unwrap().push(id.clone());
                            (
                                axum::http::StatusCode::ACCEPTED,
                                axum::Json(envelope(&shared_types::RuntimeOperationAccepted {
                                    operation_id: id.clone(),
                                    state: RuntimeOperationState::Accepted,
                                    poll: format!("/v1/runtime/operations/{id}"),
                                })),
                            )
                        }
                    },
                ),
            )
            .route(
                "/v1/runtime/operations/{id}",
                axum::routing::get(
                    |axum::extract::State(state): axum::extract::State<StopMockState>,
                     axum::extract::Path(id): axum::extract::Path<String>| {
                        async move {
                            if !state.submitted.lock().unwrap().contains(&id) {
                                return (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({"message": "not found"})),
                                );
                            }
                            // 精确 id 优先，"*" 通配（测试预置所有操作的目标终态）
                            let view_state = {
                                let states = state.states.lock().unwrap();
                                states
                                    .get(&id)
                                    .or_else(|| states.get("*"))
                                    .cloned()
                                    .unwrap_or(RuntimeOperationState::Stopping)
                            };
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(envelope(&view(&id, view_state))),
                            )
                        }
                    },
                ),
            )
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });
        (addr, task)
    }

    fn manager_with_external(logs: &Path, address: &str) -> DevServerManager {
        let mut config = crate::Config::from_env().expect("test config");
        config.log_base_dir = logs.to_path_buf();
        let manager = DevServerManager::new(Arc::new(config));
        lock(&manager.processes).unwrap().insert(
            "userapp:app-r05".to_string(),
            DevProcess {
                pid: 0,
                port: shared_types::APP_ENTRY_PORT,
                project_id: "userapp:app-r05".to_string(),
                instance_id: None,
                base_path: None,
                started_at: 0,
                log_dir: logs.to_path_buf(),
                temp_log_name: String::new(),
                external_owner: Some(ExternalOwner {
                    address: address.to_string(),
                    token: "test-token".to_string(),
                    runtime_instance_id: "instance-test".to_string(),
                }),
            },
        );
        manager
    }

    /// R05 反例：Stop 未确认（Failed 终态）→ 登记保留；重试按**原
    /// operation_id** 查询（受理数不变）直至 Succeeded 才移除登记。
    #[tokio::test]
    async fn confirmed_failed_stop_keeps_owner_and_next_explicit_request_gets_new_id() {
        let logs = tempfile::tempdir().expect("logs");
        let state = StopMockState::default();
        // 预置：所有操作受理后即 Failed 终态（owner 侧失败）
        state
            .states
            .lock()
            .unwrap()
            .insert("*".to_string(), RuntimeOperationState::Failed);
        let (addr, task) = serve_stop_mock(state.clone()).await;
        let manager = manager_with_external(logs.path(), &addr);

        // 第一次 stop：owner 返回 Failed → 未确认，登记保留
        let error = manager
            .stop_dev("userapp:app-r05")
            .await
            .expect_err("failed stop must not report success");
        let message = error.to_string();
        assert!(message.contains("not confirmed"), "got: {message}");
        assert!(
            lock(&manager.processes)
                .unwrap()
                .contains_key("userapp:app-r05"),
            "registration must survive unconfirmed stop"
        );
        let first_op = state.submitted.lock().unwrap()[0].clone();

        // Confirmed terminal failure is immutable. A new explicit Stop executes a new operation.
        state
            .states
            .lock()
            .unwrap()
            .insert("*".to_string(), RuntimeOperationState::Succeeded);
        manager
            .stop_dev("userapp:app-r05")
            .await
            .expect("retry stop must succeed once owner confirms");
        assert_eq!(
            state.submitted.lock().unwrap().len(),
            2,
            "explicit retry after confirmed terminal failure must submit a new operation, got {:?}",
            state.submitted.lock().unwrap()
        );
        assert_ne!(state.submitted.lock().unwrap()[1], first_op);
        assert!(
            !lock(&manager.processes)
                .unwrap()
                .contains_key("userapp:app-r05"),
            "registration removed only after Succeeded"
        );
        task.abort();
    }

    #[tokio::test]
    async fn confirmed_legacy_failure_clears_disk_and_cache_only_after_commit() {
        let logs = tempfile::tempdir().unwrap();
        let state = StopMockState::default();
        state.submitted.lock().unwrap().push("legacy-stop".into());
        state
            .states
            .lock()
            .unwrap()
            .insert("legacy-stop".into(), RuntimeOperationState::Failed);
        let (addr, task) = serve_stop_mock(state.clone()).await;
        let manager = manager_with_external(logs.path(), &addr);
        manager.persist_external_state().unwrap();
        let record = super::super::types::ExternalStopRecord {
            operation_id: "legacy-stop".into(),
            workspace_id: "ws-stable-hash".into(),
            submitted_at_ms: 0,
        };
        manager
            .external_transaction(|disk| {
                disk.stops.insert("userapp:app-r05".into(), record.clone());
                Ok(())
            })
            .unwrap();
        lock(&manager.external_stops)
            .unwrap()
            .insert("userapp:app-r05".into(), record);
        assert!(manager.stop_dev("userapp:app-r05").await.is_err());
        assert!(lock(&manager.external_stops).unwrap().is_empty());
        let disk = manager.read_external_state().unwrap();
        assert!(disk.stops.is_empty());
        assert!(disk.owners.contains_key("userapp:app-r05"));
        state
            .states
            .lock()
            .unwrap()
            .insert("*".into(), RuntimeOperationState::Succeeded);
        manager.stop_dev("userapp:app-r05").await.unwrap();
        assert_eq!(state.submitted.lock().unwrap().len(), 2);
        assert_ne!(state.submitted.lock().unwrap()[1], "legacy-stop");
        task.abort();
    }

    #[tokio::test]
    async fn broken_durable_state_stops_before_any_http_submission() {
        let logs = tempfile::tempdir().unwrap();
        let state = StopMockState::default();
        let (addr, task) = serve_stop_mock(state.clone()).await;
        let manager = manager_with_external(logs.path(), &addr);
        std::fs::create_dir(manager.external_state_path()).unwrap();
        assert!(manager.stop_dev("userapp:app-r05").await.is_err());
        assert!(state.submitted.lock().unwrap().is_empty());
        assert!(
            lock(&manager.processes)
                .unwrap()
                .contains_key("userapp:app-r05")
        );
        task.abort();
    }

    /// R04 反例：Stop 终态 Cancelled ≠ 停止成功——报错并保留登记
    /// （取消既不证明业务已停也无清理证据）。
    #[tokio::test]
    async fn cancelled_stop_is_not_success_and_keeps_registration() {
        let logs = tempfile::tempdir().expect("logs");
        let state = StopMockState::default();
        // 预置：所有操作受理后即 Cancelled 终态（取消竞争）
        state
            .states
            .lock()
            .unwrap()
            .insert("*".to_string(), RuntimeOperationState::Cancelled);
        let (addr, task) = serve_stop_mock(state.clone()).await;
        let manager = manager_with_external(logs.path(), &addr);

        let error = manager
            .stop_dev("userapp:app-r05")
            .await
            .expect_err("cancelled stop must not report success");
        let message = error.to_string();
        assert!(message.contains("cancelled"), "got: {message}");
        assert!(
            lock(&manager.processes)
                .unwrap()
                .contains_key("userapp:app-r05"),
            "registration must survive cancelled stop"
        );
        task.abort();
    }

    /// R05 反例：file-server 重启（新 manager 实例）后 external 控制关系
    /// 从持久化文件恢复；token 不落盘（空哨兵 + 状态根重读）。
    #[tokio::test]
    async fn external_registration_survives_manager_restart_without_token_on_disk() {
        let logs = tempfile::tempdir().expect("logs");
        let state = StopMockState::default();
        let (addr, task) = serve_stop_mock(state.clone()).await;
        {
            let manager = manager_with_external(logs.path(), &addr);
            manager
                .persist_external_state()
                .expect("persist external registration");
        }
        // token 不得出现在状态文件里
        let content =
            std::fs::read_to_string(logs.path().join("dev-server-external.json")).expect("state");
        assert!(
            !content.contains("test-token"),
            "token must not persist: {content}"
        );
        // 新实例恢复登记（token 为空哨兵）
        let mut config = crate::Config::from_env().expect("test config");
        config.log_base_dir = logs.path().to_path_buf();
        let restored = DevServerManager::new(Arc::new(config));
        let entry = lock(&restored.processes)
            .unwrap()
            .get("userapp:app-r05")
            .cloned()
            .expect("restored registration");
        assert_eq!(entry.external_owner.as_ref().expect("owner").address, addr);
        assert_eq!(
            entry.external_owner.as_ref().expect("owner").token,
            "",
            "restored token is the empty sentinel"
        );
        task.abort();
    }

    /// R05 反例：managed 域登记缺失 + 3010 有 owner 应答 → 拒绝（不 ps 扫杀）；
    /// 无应答 → 幂等成功。
    #[tokio::test]
    async fn userapp_stop_without_registration_refuses_when_owner_listens() {
        let logs = tempfile::tempdir().expect("logs");
        let mut config = crate::Config::from_env().expect("test config");
        config.log_base_dir = logs.path().to_path_buf();

        // 有 owner 应答（identity 端点）：拒绝
        let identity = shared_types::RuntimeIdentityView {
            application_id: "unknown-app".to_string(),
            service_family: "userapp-dev".to_string(),
            workspace_id: "ws-x".to_string(),
            source_root: "/ws".to_string(),
            runtime_instance_id: "instance-x".to_string(),
            deployment_generation_id: "gen".to_string(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let app = axum::Router::new().route(
            "/v1/runtime/identity",
            axum::routing::get(move || {
                let identity = identity.clone();
                async move { axum::Json(envelope(&identity)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        config.app_cli_admin_probe_addr = listener.local_addr().expect("addr").to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("identity mock");
        });
        let manager = DevServerManager::new(Arc::new(config));
        let ws = tempfile::tempdir().expect("ws");
        let error = manager
            .stop_userapp_dev("userapp:ghost", ws.path())
            .await
            .expect_err("owner listening without registration must be refused");
        assert!(
            error.to_string().contains("no registration"),
            "diagnostic should explain refusal: {error}"
        );
        task.abort();

        // 无应答：幂等成功（无运行态）
        let mut config = crate::Config::from_env().expect("test config");
        config.log_base_dir = logs.path().to_path_buf();
        config.app_cli_admin_probe_addr = "127.0.0.1:1".to_string(); // 无监听
        let manager = DevServerManager::new(Arc::new(config));
        let stopped = manager
            .stop_userapp_dev("userapp:ghost", ws.path())
            .await
            .expect("no owner → idempotent success");
        assert!(stopped.killed_pids.is_empty());
    }
}
