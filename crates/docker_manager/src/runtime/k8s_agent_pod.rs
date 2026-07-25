//! agent-runner 变更操作（从 k8s_agent_pod.rs 拆分后本文件仅保留变更类；
//! create 见 k8s_agent_create.rs，query 见 k8s_agent_query.rs）。
//!
//! - `stop_container_by_identifier_inner`：销毁 STS + ClusterIP/headless svc，保留 PVC（硬约束）。
//! - `restart_agent_container_inplace`：原地重启（exec `kill -TERM 1` → kubelet restartPolicy
//!   原地重启容器，卷不 unstage，避免 CephFS re-stage ~60s）。
//! - `sync_states_inner`：缓存对账（按 STS 存在判活，不查瞬态 pod 404）。
//! - `cleanup_all_inner`：全量回收（svc→STS→pod 顺序删，等终止，保留 PVC）。

use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, RemovedContainerInfo, RuntimeContainerInfo,
};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::api::{Api, DeleteParams, ListParams};
use shared_types::ServiceType;
use tracing::{debug, info, warn};

use super::k8s_pod::K8sPodOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::{KubernetesRuntime, RUNTIME_MANAGED_LABEL};

impl KubernetesRuntime {


    pub(crate) async fn stop_container_by_identifier_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type)?;

        // STS 实际 pod 名（等待终止用）；pod_name 即 sts_name。
        let agent_pod = self.agent_pod_name(identifier, service_type)?;

        info!(
            "[K8S] Destroying agent StatefulSet {} (identifier={}, service_type={})",
            pod_name, identifier, service_type
        );

        // Step 0: 删除 ClusterIP Service（先摘流量 / 移除 DNS，再销毁 pod）
        if let Err(e) = self.delete_agent_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete ClusterIP Service for {}: {} (continuing)", identifier, e);
        }

        // Step 1: 删除 StatefulSet（Foreground cascade → pod 随之终止）。回收 = 彻底销毁 STS
        // （非 scale 0；scale 0 会留 STS 永不清理）。PVC 保留（数据复用，下次 ensure 重建挂回）。
        if let Err(e) = self.delete_agent_statefulset(identifier, service_type).await {
            warn!("[K8S] Failed to delete StatefulSet {}: {} (continuing)", pod_name, e);
        }

        // Step 2: 等 pod {sts}-0 完全终止（Foreground cascade 异步；等其 404 再继续，
        // 避免与立即重建的新 pod 抢 RWO PVC）。
        if let Err(e) = self.wait_for_pod_terminated(&agent_pod).await {
            warn!("[K8S] wait_for_pod_terminated for {} failed: {} (continuing)", agent_pod, e);
        }

        // Step 3: 删除 headless Service（与 STS/ClusterIP 一并彻底回收）
        if let Err(e) = self.delete_agent_headless_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete headless Service for {}: {} (continuing)", identifier, e);
        }

        self.pod_cache.write().await.remove(identifier);

        info!(
            "[K8S] agent {} destroyed (STS + ClusterIP/headless svc deleted; PVC preserved for reuse), total time: {:.1}s",
            pod_name,
            total_start.elapsed().as_secs_f64()
        );

        Ok(())
    }


    /// 原地重启 agent 容器：exec 进 agent 容器 `kill -TERM 1` → agent_runner SIGTERM handler
    /// 优雅退出 → kubelet `restartPolicy=Always` **原地重启容器**（卷不 unstage，避免 CephFS
    /// `NodeStageVolume` re-stage ~60s）。对比 destroy+recreate（删 STS+等 pod 404+重建，慢）。
    ///
    /// 轮询 agent 容器 `restartCount` 自增确认 kubelet 已原地重启；30s 超时 → Err（调用方
    /// `pod_restart` 回落 destroy+recreate，处理 agent 卡死/PID 1 不接 SIGTERM 等异常）。
    /// agent 容器名固定 "agent"；PID 1 = agent_runner（实测 ComputerAgentRunner
    /// `/usr/local/bin/agent_runner -p 8086`），SIGTERM 直达其 shutdown handler。
    pub(crate) async fn restart_agent_container_inplace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        use kube::api::AttachParams;
        use tokio::io::AsyncReadExt;

        const AGENT_CONTAINER: &str = "agent";
        const RESTART_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let pod_name = self.agent_pod_name(identifier, service_type)?;

        // 1. 基线 restartCount（agent 容器；缺失视作 0）。pod 取失败 → Err（fail-fast）。
        let pod = self
            .pods()
            .get(&pod_name)
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get pod for restart baseline: {e}"))
            })?;
        let baseline = container_status(&pod, AGENT_CONTAINER)
            .map(|(rc, _)| rc)
            .unwrap_or(0);

        // 2. exec kill -TERM 1（agent 容器 PID 1 = agent_runner → SIGTERM → 优雅退出 → kubelet 原地重启）
        let ap = AttachParams::default()
            .container(AGENT_CONTAINER)
            .stdout(true)
            .stderr(true)
            .stdin(false)
            .tty(false);
        let mut attached = self
            .pods()
            .exec(
                &pod_name,
                vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "kill -TERM 1".to_string(),
                ],
                &ap,
            )
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("exec kill: {e}")))?;
        // drain stdout/stderr（kill 通常无输出；读空释放 buffer，避免 join 死锁；reader 出作用域 drop 后再 join）
        if let Some(mut r) = attached.stdout() {
            let mut buf = String::new();
            let _ = r.read_to_string(&mut buf).await;
        }
        if let Some(mut r) = attached.stderr() {
            let mut buf = String::new();
            let _ = r.read_to_string(&mut buf).await;
        }
        if let Err(e) = attached.join().await {
            debug!("[K8S] restart exec join (kill -TERM 1): {e} (non-fatal, SIGTERM 已发)");
        }

        // 3. 轮询原地重启完成：restartCount 自增（kubelet 原地重启）+ ready。超时 → Err（回落）。
        let deadline = std::time::Instant::now() + RESTART_TIMEOUT;
        loop {
            if std::time::Instant::now() > deadline {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "in-place restart timeout: agent restartCount did not increment within {RESTART_TIMEOUT:?} (pod={pod_name})"
                )));
            }
            tokio::time::sleep(RESTART_POLL_INTERVAL).await;
            let restarted = self
                .pods()
                .get(&pod_name)
                .await
                .ok()
                .and_then(|p| container_status(&p, AGENT_CONTAINER));
            if let Some((rc, ready)) = restarted
                && rc > baseline
                && ready
            {
                info!(
                    "[K8S] agent restarted in-place: {} (restartCount {}→{}, ready, volume 未 unstage)",
                    pod_name, baseline, rc
                );
                return Ok(());
            }
            // restartCount 未自增 / ready 未就绪 / pod get 短暂失败（重启中）→ 继续轮询
        }
    }


    pub(crate) async fn sync_states_inner(
        &self,
    ) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        let mut removed = Vec::new();

        // 获取缓存快照 (identifier, RuntimeContainerInfo)
        let cache_snapshot: Vec<(String, RuntimeContainerInfo)> = self
            .pod_cache
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let checked_count = cache_snapshot.len() as u32;

        for (identifier, container_info) in cache_snapshot {
            // container_name 已是 sts_name（get_container_info 源头剥过 -0）。判"真没了"看 STS
            // 是否存在，不看 pod 404（STS replicas>0 时 pod 被 evict/重建会瞬时空缺，误判清缓存
            // 会中断重建）。
            let sts_name = &container_info.container_name;
            match self.statefulsets().get(sts_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // STS 已删 → 真没了；从缓存移除 + 收集（消费方 container_sync 只用 container_ip 清 gRPC 池）
                    self.pod_cache.write().await.remove(&identifier);
                    removed.push(RemovedContainerInfo {
                        container_name: container_info.container_name.clone(),
                        container_ip: container_info.container_ip.clone(),
                        identifier: identifier.clone(),
                        // FIXME: RuntimeContainerInfo 不带 service_type；消费方未用，暂占位。
                        service_type: ServiceType::WebAgentRunner,
                    });
                    info!(
                        "[K8S_SYNC] StatefulSet gone, removed from cache: {} (identifier={})",
                        sts_name, identifier
                    );
                }
                Ok(_) => {
                    // STS 存在（replicas>0 pod 运行/重建中；replicas=0 已停）→ 不动缓存
                }
                Err(e) => {
                    warn!("[K8S_SYNC] Failed to check StatefulSet {}: {}", sts_name, e);
                }
            }
        }

        Ok((checked_count, removed))
    }


    pub(crate) async fn cleanup_all_inner(&self) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        info!("[K8S_CLEANUP] Starting cleanup_all — sequential Service → Pod → PVC deletion");

        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);

        // ── Step 0: 批量删除 K8s Service ──
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        match services
            .delete_collection(&DeleteParams::default(), &lp)
            .await
        {
            Ok(_) => info!("[K8S_CLEANUP] Service delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Service delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 1: 获取所有 managed Pod 名称（用于后续等待终止）──
        let pods_to_wait: Vec<String> = self
            .pods()
            .list(&lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!(
                    "Failed to list pods for cleanup: {}",
                    e
                ))
            })?
            .items
            .iter()
            .filter_map(|pod| pod.metadata.name.clone())
            .collect();

        info!(
            "[K8S_CLEANUP] Found {} managed pods to clean",
            pods_to_wait.len()
        );

        // ── Step 2: 批量删除 Pod（graceful, Foreground 传播）──
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
            grace_period_seconds: Some(15),
            ..Default::default()
        };

        // 先删 StatefulSet（cascade 删其 pod，且阻止 STS 控制器重建 pod）。
        // agent-runner 现走 STS，若直接删 pod 而 STS 仍在，控制器会立即重建 pod → 永远删不掉。
        match self.statefulsets().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S CLEANUP] StatefulSet delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S CLEANUP] StatefulSet delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // 再删 Pod（兜底：清理历史遗留的游离裸 pod，或 STS cascade 未覆盖的残留）
        match self.pods().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S_CLEANUP] Pod delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Pod delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 3: 等待所有 Pod 完全终止 ──
        // 关键：必须在删除 PVC 之前完成，确保 FUSE 卷已卸载
        let wait_futures: Vec<_> = pods_to_wait
            .iter()
            .map(|pod_name| self.wait_for_pod_terminated(pod_name))
            .collect();

        let wait_results = futures_util::future::join_all(wait_futures).await;
        for (pod_name, result) in pods_to_wait.iter().zip(wait_results.iter()) {
            if let Err(e) = result {
                tracing::warn!(
                    "[K8S_CLEANUP] Pod {} termination wait failed: {}",
                    pod_name,
                    e
                );
            }
        }

        // ── Step 4: PVC 清理策略 ──
        //
        // 不在 cleanup_all 中主动删除 PVC，原因：
        // 1. Pod 删除时 K8s PropagationPolicy::Foreground 会级联清理关联的 PVC
        // 2. 主动删除正在被 pod 使用的 PVC 会导致 PVC 卡在 Terminating 状态（pvc-protection finalizer）
        // 3. 多副本部署时，cleanup_all 会误删其他 rcoder 实例正在使用的 PVC
        // 4. Terminating PVC 会导致后续 create_container 失败（409 重试循环）
        info!(
            "[K8S_CLEANUP] PVC cleanup skipped — PVCs are cleaned up via K8s cascading deletion when pods are removed"
        );

        // 清理缓存 (含 subvolume_path_cache — 跨重启 PVC 可能被运维删除重建,
        // 陈旧 cache 导致 resolve 命中旧 subvolPath → rcoder 读老 subvol 而 pod 挂新 PVC → 数据面分裂)
        self.pod_cache.write().await.clear();
        self.subvolume_path_cache.write().await.clear();

        info!(
            "[K8S_CLEANUP] cleanup_all completed in {:.1}s",
            total_start.elapsed().as_secs_f64()
        );
        Ok(())
    }
}

/// 取 pod 内指定容器的 (restartCount, ready)。找不到容器/无状态 → None。
/// `restart_agent_container_inplace` 的 baseline 与 poll 共用（避免重复 pod→status→find 展开）。
fn container_status(pod: &Pod, container_name: &str) -> Option<(i32, bool)> {
    let cs = pod.status.as_ref()?.container_statuses.as_ref()?;
    let c = cs.iter().find(|c| c.name == container_name)?;
    Some((c.restart_count, c.ready))
}
