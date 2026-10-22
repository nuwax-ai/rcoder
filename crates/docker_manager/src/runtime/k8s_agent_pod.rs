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
use shared_types::{ContainerBasicInfo, ServiceType};
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
        match service_type {
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject => {
                let snapshot = self
                    .capture_agent_retirement(identifier, service_type)
                    .await?;
                match self.retire_agent_snapshot(&snapshot, false).await? {
                    container_runtime_api::AgentStopOutcome::Complete => Ok(()),
                    container_runtime_api::AgentStopOutcome::ComputeGoneServicesPending {
                        pending,
                    } => Err(ContainerRuntimeError::ContainerStopError(format!(
                        "Agent compute exited but captured Service cleanup remains pending: {pending:?}"
                    ))),
                }
            }
            ServiceType::UserappBuilder => {
                let workload = self.pod_name(identifier, service_type)?;
                let pod = self.agent_pod_name(identifier, service_type)?;
                self.stop_idle_builder(identifier, &workload, &pod, None, None)
                    .await
            }
            ServiceType::Userapp => Err(ContainerRuntimeError::ConfigurationError(
                "Production UserApp stop must use its Deployment lifecycle".into(),
            )),
        }
    }

    /// Idle retirement captures every deletion target before the bounded owner
    /// drain. A replacement created during drain is never another delete target.
    pub(crate) async fn retire_captured_idle_builder(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<()> {
        self.retire_captured_idle_builder_until(
            identifier,
            captured,
            tokio::time::Instant::now() + super::idle_builder_retirement::RETIREMENT_BUDGET,
        )
        .await
    }

    pub(crate) async fn retire_captured_idle_builder_until(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
        deadline: tokio::time::Instant,
    ) -> ContainerRuntimeResult<()> {
        let dispatched = std::sync::atomic::AtomicBool::new(false);
        tokio::time::timeout_at(
            deadline,
            self.retire_captured_idle_builder_inner(identifier, captured, &dispatched),
        )
        .await
        .map_err(|_| {
            super::idle_builder_retirement::deadline_error(
                dispatched.load(std::sync::atomic::Ordering::Acquire),
            )
        })?
    }

    async fn retire_captured_idle_builder_inner(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
        dispatched: &std::sync::atomic::AtomicBool,
    ) -> ContainerRuntimeResult<()> {
        let sts_name = &captured.container_name;
        if captured.project_id != identifier
            || identifier.is_empty()
            || sts_name.is_empty()
            || captured.container_id.is_empty()
            || captured.workload_uid.as_ref().is_none_or(String::is_empty)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Captured idle builder identity is incomplete".into(),
            ));
        }
        self.stop_idle_builder(
            identifier,
            sts_name,
            &format!("{sts_name}-0"),
            Some(captured),
            Some(dispatched),
        )
        .await
    }

    async fn stop_idle_builder(
        &self,
        identifier: &str,
        sts_name: &str,
        pod_name: &str,
        captured: Option<&ContainerBasicInfo>,
        dispatched: Option<&std::sync::atomic::AtomicBool>,
    ) -> ContainerRuntimeResult<()> {
        use super::k8s_runtime_helpers::conditioned_delete_params;
        use kube::api::PropagationPolicy;
        let map_error = |error| {
            super::builder_completion::k8s_error("Retire captured idle builder".into(), error)
        };
        let sts = self
            .statefulsets()
            .get_opt(sts_name)
            .await
            .map_err(map_error)?;
        let pod = self.pods().get_opt(pod_name).await.map_err(map_error)?;
        if let Some(captured) = captured {
            if sts
                .as_ref()
                .is_some_and(|sts| sts.metadata.uid.as_ref() != captured.workload_uid.as_ref())
                || pod.as_ref().is_some_and(|pod| {
                    pod.metadata.uid.as_deref() != Some(captured.container_id.as_str())
                })
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Idle builder physical target was replaced".into(),
                ));
            }
            // Absence of the original Pod is an idempotent completion, not
            // authority to delete a pending successor controller or its Services.
            if pod.is_none() {
                let mut cache = self.pod_cache.write().await;
                if cache.get(identifier).is_some_and(|entry| {
                    entry.service_type == ServiceType::UserappBuilder
                        && entry.info.container_id == captured.container_id
                }) {
                    cache.remove(identifier);
                }
                return Ok(());
            }
            if sts.as_ref().is_none_or(|sts| {
                let labels = sts.metadata.labels.as_ref();
                labels
                    .and_then(|labels| labels.get("rcoder.io/identifier"))
                    .map(String::as_str)
                    != Some(identifier)
                    || labels
                        .and_then(|labels| labels.get("rcoder.io/service-type"))
                        .map(String::as_str)
                        != Some(ServiceType::UserappBuilder.container_family_key())
            }) || pod.as_ref().is_none_or(|pod| {
                !pod.metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            owner.controller == Some(true)
                                && owner.kind == "StatefulSet"
                                && owner.name == sts_name
                                && Some(&owner.uid) == captured.workload_uid.as_ref()
                        })
                    })
            }) {
                return Err(ContainerRuntimeError::Conflict(
                    "Captured idle builder ownership mismatch".into(),
                ));
            }
        }
        let mut services = Vec::new();
        let service_names = if captured.is_some() {
            [format!("{sts_name}-svc"), format!("{sts_name}-headless")]
        } else {
            [
                self.agent_service_name(identifier, &ServiceType::UserappBuilder)?,
                self.agent_headless_svc_name(identifier, &ServiceType::UserappBuilder)?,
            ]
        };
        for (index, name) in service_names.into_iter().enumerate() {
            if let Some(service) = self
                .services_api()
                .get_opt(&name)
                .await
                .map_err(map_error)?
            {
                if captured.is_some() {
                    super::k8s_service::validate_builder_service(&service, identifier, index == 1)?;
                }
                services.push((name, conditioned_delete_params(&service.metadata, None)?));
            }
        }
        let deletion = sts
            .as_ref()
            .map(|sts| {
                conditioned_delete_params(&sts.metadata, Some(PropagationPolicy::Foreground))
            })
            .transpose()?;
        let pod_uid = pod.as_ref().and_then(|pod| pod.metadata.uid.as_deref());
        if let Some(uid) = pod_uid
            && pod.as_ref().is_some_and(|pod| {
                pod.metadata
                    .owner_references
                    .as_ref()
                    .is_some_and(|owners| {
                        owners.iter().any(|owner| {
                            owner.controller == Some(true)
                                && sts.as_ref().and_then(|sts| sts.metadata.uid.as_ref())
                                    == Some(&owner.uid)
                        })
                    })
            })
        {
            // Independent of business readiness. Failure must not prevent the
            // physical stop. The in-container UID check fences a replaced Pod.
            if let Some(dispatched) = dispatched {
                dispatched.store(true, std::sync::atomic::Ordering::Release);
            }
            let drain = self.exec_pod_container(pod_name, "agent", vec![
                "sh".into(), "-c".into(),
                "[ \"$RCODER_PHYSICAL_POD_UID\" = \"$1\" ] || exit 75; exec app-cli --app-cli-container-stop \"$2\"".into(),
                "--".into(), uid.into(), format!("/home/user/{identifier}"),
            ]);
            let result = tokio::time::timeout(std::time::Duration::from_secs(8), drain).await;
            if !matches!(&result, Ok(Ok(exit)) if exit.exit_code == 0) {
                warn!(%identifier, ?result, "Owner drain incomplete; continuing physical idle stop");
            }
        }
        if let Some(captured) = captured {
            match self.pods().get_opt(pod_name).await.map_err(map_error)? {
                Some(live)
                    if live.metadata.uid.as_deref() != Some(captured.container_id.as_str()) =>
                {
                    return Err(ContainerRuntimeError::Conflict(
                        "Builder Pod replaced during idle owner drain".into(),
                    ));
                }
                None => {
                    let mut cache = self.pod_cache.write().await;
                    if cache.get(identifier).is_some_and(|entry| {
                        entry.service_type == ServiceType::UserappBuilder
                            && entry.info.container_id == captured.container_id
                    }) {
                        cache.remove(identifier);
                    }
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        if let Some(params) = deletion {
            if let Some(dispatched) = dispatched {
                dispatched.store(true, std::sync::atomic::Ordering::Release);
            }
            match self.statefulsets().delete(sts_name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(response)) if response.code == 404 => {}
                Err(error) => return Err(map_error(error)),
            }
        } else if pod.is_some() {
            return Err(ContainerRuntimeError::Conflict(
                "Idle builder Pod has no captured controller; use physical control recovery".into(),
            ));
        }
        // Service cleanup is UID/version-bound too. It cannot delete resources
        // published by a concurrent new ensure. PVC is never a deletion target.
        for (name, params) in services {
            match self.services_api().delete(&name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(response)) if response.code == 404 => {}
                Err(error) => {
                    warn!(%identifier, %name, %error, "Captured idle Service cleanup incomplete");
                }
            }
        }
        if let Some(uid) = pod_uid {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    match self.pods().get_opt(pod_name).await.map_err(map_error)? {
                        None => break Ok(()),
                        Some(live) if live.metadata.uid.as_deref() != Some(uid) => {
                            break Err(ContainerRuntimeError::Conflict(
                                "Builder Pod replaced during idle retirement".into(),
                            ));
                        }
                        Some(_) => tokio::time::sleep(std::time::Duration::from_millis(300)).await,
                    }
                }
            })
            .await
            .map_err(|_| {
                ContainerRuntimeError::Timeout(
                    "Captured idle builder Pod remains terminating".into(),
                )
            })??;
            let mut cache = self.pod_cache.write().await;
            if cache.get(identifier).is_some_and(|entry| {
                entry.service_type == ServiceType::UserappBuilder && entry.info.container_id == uid
            }) {
                cache.remove(identifier);
            }
        }
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
        let pod = self.pods().get(&pod_name).await.map_err(|e| {
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
            if let Err(e) = r.read_to_string(&mut buf).await {
                warn!("[K8S] restart exec drain stdout (kill -TERM 1): {e} (non-fatal)");
            }
        }
        if let Some(mut r) = attached.stderr() {
            let mut buf = String::new();
            if let Err(e) = r.read_to_string(&mut buf).await {
                warn!("[K8S] restart exec drain stderr (kill -TERM 1): {e} (non-fatal)");
            }
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

        // 获取缓存快照 (identifier, RuntimeContainerInfo, service_type)
        let cache_snapshot: Vec<(String, RuntimeContainerInfo, ServiceType)> = self
            .pod_cache
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.info.clone(), v.service_type))
            .collect();

        let checked_count = cache_snapshot.len() as u32;

        for (identifier, container_info, service_type) in cache_snapshot {
            // container_name 已是 sts_name（get_container_info 源头剥过 -0）。判"真没了"看 STS
            // 是否存在，不看 pod 404（STS replicas>0 时 pod 被 evict/重建会瞬时空缺，误判清缓存
            // 会中断重建）。
            let workload_name = &container_info.container_name;
            match self.statefulsets().get(workload_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // 契约一：container_name ≡ workload 名。STS 不存在时先排除
                    // bare pod（它以 pod 名为 workload 身份，无控制器）——任一
                    // 候选 pod 名仍存活则保留；确认全消失才移除缓存。
                    let pods = self.pods();
                    let mut alive = false;
                    for candidate in [workload_name.clone(), format!("{workload_name}-0")] {
                        match pods.get_opt(&candidate).await {
                            Ok(Some(pod)) if pod.metadata.deletion_timestamp.is_none() => {
                                alive = true;
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("[K8S_SYNC] Pod existence probe {candidate} failed: {e}");
                                alive = true;
                                break;
                            }
                        }
                    }
                    if alive {
                        continue;
                    }
                    // 控制器与 bare pod 均确认消失 → 真没了；移除缓存（消费方
                    // container_sync 只用 container_ip 清 gRPC 池）。
                    self.pod_cache.write().await.remove(&identifier);
                    removed.push(RemovedContainerInfo {
                        container_name: container_info.container_name.clone(),
                        container_ip: container_info.container_ip.clone(),
                        identifier: identifier.clone(),
                        service_type,
                    });
                    info!(
                        "[K8S_SYNC] Workload gone (controller and pod), removed from cache: {} (identifier={})",
                        workload_name, identifier
                    );
                }
                Ok(_) => {
                    // STS 存在（replicas>0 pod 运行/重建中；replicas=0 已停）→ 不动缓存
                }
                Err(e) => {
                    warn!(
                        "[K8S_SYNC] Failed to check StatefulSet {}: {}",
                        workload_name, e
                    );
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
