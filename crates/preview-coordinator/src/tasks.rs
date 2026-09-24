//! 后台任务：心跳轮询 / activity 刷盘 / 空闲回收 / 启动对账 / starting 自愈。
//!
//! 只负责本 Pod 宿主的实例（list_by_host(mine)）；全部经权威存储 CAS 收口。
//! 空闲回收判定阈值含安全余量 = idle_secs + 2×刷盘间隔（消除"activity 还在
//! 别的副本内存里未刷盘"的边界竞态——宁可晚回收，不误回收）。
use std::sync::Arc;
use std::time::Duration;

use shared_types::{PreviewCoordination as _, PreviewInstanceState};

use crate::identity::reboot_reconcile_args;
use crate::service::PreviewCoordinator;

impl PreviewCoordinator {
    /// 启动对账（装配时调用一次）：同 Pod UID 旧 boot 代次的活跃行 → Stopped。
    pub async fn run_startup_reconcile(&self) {
        let (pod_uid, boot_id) = reboot_reconcile_args();
        match self.store.reconcile_host_reboot(&pod_uid, &boot_id).await {
            Ok(reconciled) if !reconciled.is_empty() => {
                tracing::info!(
                    count = reconciled.len(),
                    "preview startup reconcile: prior-boot instances marked stopped"
                );
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("preview startup reconcile failed: {error}"),
        }

        // A StatefulSet replacement has a new Pod UID, so the same-Pod reboot
        // reconciliation above cannot see rows owned by a deleted predecessor.
        // Query host existence once per distinct UID, then settle each exact
        // instance with generation-bound store writes.
        let rows = match self.store.list_active().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("preview orphan scan failed: {error}");
                return;
            }
        };
        let mut by_pod = std::collections::BTreeMap::<String, Vec<_>>::new();
        for row in rows {
            if row.host_id == self.host().host_id {
                continue;
            }
            by_pod
                .entry(crate::evidence::pod_uid_of(&row.host_id).to_string())
                .or_default()
                .push(row);
        }
        for (orphan_uid, rows) in by_pod {
            if self.host_pod_exists(&orphan_uid).await {
                continue;
            }
            let evidence = format!(
                "host pod {orphan_uid} no longer exists (verified during startup reconciliation)"
            );
            for row in rows {
                if let Err(error) = self.settle_orphaned_instance(&row, &evidence).await {
                    tracing::warn!(
                        preview_key = %row.preview_key,
                        host_id = %row.host_id,
                        "preview orphan reconciliation lost a race or failed: {error}"
                    );
                }
            }
        }
    }

    /// 一轮心跳扫描：本机实例探活上报；Starting 自愈（迟到的启动完成补发布）；
    /// 登记缺失/身份不符 → Unknown；确认死 → Failed。
    pub async fn run_heartbeat_sweep(&self) {
        let rows = match self.store.list_by_host(&self.host().host_id, true).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("preview heartbeat sweep read failed: {error}");
                return;
            }
        };
        for row in rows {
            let report = match self
                .executor
                .verify_local(&row.preview_key, &row.instance_id)
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    tracing::warn!(
                        preview_key = %row.preview_key,
                        "preview heartbeat verify error: {error}"
                    );
                    continue;
                }
            };
            // Starting 行归启动预算管（execute_start 超时自会收敛），心跳不碰：
            // 冷缓存 pnpm install 可达数分钟，期间执行器必然无本地登记——若在此
            // 判定 registration lost 会误转 Unknown，随后 publish_running CAS 失败
            // （E2E/K8s 冷装实测 state=unknown 根因）。已登记且存活的迟到完成
            // 在此自愈补发布。
            if row.state == PreviewInstanceState::Starting {
                if report.identity_match {
                    self.heal_late_start(&row, &report).await;
                }
                continue;
            }
            if !report.identity_match {
                ignore_store_result(
                    self.store
                        .mark_unknown(
                            &row.preview_key,
                            &row.instance_id,
                            "local registration lost or mismatched",
                        )
                        .await,
                    "mark_unknown",
                );
                continue;
            }
            if !report.alive {
                ignore_store_result(
                    self.store
                        .mark_failed(
                            &row.preview_key,
                            &row.instance_id,
                            row.revision,
                            "heartbeat: registered process not alive",
                        )
                        .await,
                    "mark_failed",
                );
                continue;
            }
            ignore_store_result(
                self.store
                    .refresh_heartbeat(&row.preview_key, &row.instance_id)
                    .await,
                "refresh_heartbeat",
            );
        }
    }

    /// Starting 迟到完成自愈：预算超时后进程才就绪 → 补发布为 Ready。
    async fn heal_late_start(
        &self,
        row: &shared_types::PreviewInstanceRecord,
        report: &shared_types::ExecutorVerifyReport,
    ) {
        if !report.alive {
            return; // 已登记但进程死：留给启动预算/下一轮处理，不在此判死
        }
        let (Some(pid), Some(port)) = (report.pid, report.port) else {
            return;
        };
        match self
            .store
            .publish_running(
                &row.preview_key,
                &row.operation_id,
                row.revision,
                pid,
                port,
                row.base_path.as_deref(),
            )
            .await
        {
            Ok(_) => tracing::info!(
                preview_key = %row.preview_key,
                "preview heartbeat self-healed a late start into ready"
            ),
            Err(shared_types::PreviewStoreError::Conflict(_)) => {}
            Err(error) => {
                tracing::warn!(preview_key = %row.preview_key, "self-heal publish: {error}")
            }
        }
    }

    /// 一轮 activity 刷盘（GREATEST 合并；失败丢弃，下一轮 keep-alive 会再攒）。
    pub async fn run_activity_flush(&self) {
        let entries = self.activity().drain();
        if entries.is_empty() {
            return;
        }
        match self.store.flush_activity(&entries).await {
            Ok(updated) => tracing::debug!(updated, "preview activity flushed"),
            Err(error) => tracing::warn!(
                dropped = entries.len(),
                "preview activity flush failed (entries dropped; next keep-alive re-accumulates): {error}"
            ),
        }
    }

    /// 一轮空闲回收（仅本机实例；阈值含 2×刷盘间隔安全余量）。
    pub async fn run_idle_recycle(&self) {
        let idle_secs = self.config().idle_recycle_secs;
        if idle_secs == 0 {
            return;
        }
        let margin = 2 * self.config().activity_flush_interval_secs;
        let threshold = Duration::from_secs(idle_secs + margin);
        let rows = match self.store.list_by_host(&self.host().host_id, true).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("preview idle recycle read failed: {error}");
                return;
            }
        };
        for row in rows {
            if row.state != PreviewInstanceState::Ready {
                continue;
            }
            let idle = chrono::Utc::now()
                .signed_duration_since(row.last_activity_at)
                .to_std()
                .unwrap_or_default();
            if idle < threshold {
                continue;
            }
            tracing::info!(
                preview_key = %row.preview_key,
                idle_secs = idle.as_secs(),
                "preview idle recycle: admitting stop"
            );
            let envelope = self
                .stop_dev(&shared_types::PreviewStopRequest {
                    project_id: row.project_id.clone(),
                    pid: row.pid,
                })
                .await;
            if let Err(error) = envelope {
                tracing::warn!(preview_key = %row.preview_key, "idle recycle stop: {error}");
            }
        }
    }

    /// 启动全部后台循环（rcoder 装配调用；shutdown 广播后各循环退出，
    /// flusher 退出前补一轮刷盘）。项目停机信号惯例=broadcast::Sender<()>。
    pub fn spawn_background_tasks(self: &Arc<Self>, shutdown: tokio::sync::broadcast::Sender<()>) {
        let heartbeat = Arc::clone(self);
        let mut shutdown_heartbeat = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                heartbeat.config().heartbeat_interval_secs.max(1),
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => heartbeat.run_heartbeat_sweep().await,
                    _ = shutdown_heartbeat.recv() => break,
                }
            }
        });
        let flusher = Arc::clone(self);
        let mut shutdown_flusher = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                flusher.config().activity_flush_interval_secs.max(1),
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => flusher.run_activity_flush().await,
                    _ = shutdown_flusher.recv() => {
                        flusher.run_activity_flush().await;
                        break;
                    }
                }
            }
        });
        let recycler = Arc::clone(self);
        let mut shutdown_recycler = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => recycler.run_idle_recycle().await,
                    _ = shutdown_recycler.recv() => break,
                }
            }
        });
    }
}

/// 后台路径的存储写回失败只记日志（正确性由 CAS 与下一轮扫描收敛）。
fn ignore_store_result(result: Result<impl Send, shared_types::PreviewStoreError>, context: &str) {
    if let Err(error) = result {
        tracing::debug!(context, "background preview store write skipped: {error}");
    }
}
