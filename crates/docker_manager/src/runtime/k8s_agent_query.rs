//! agent-runner 查询/读取（从 k8s_agent_pod.rs 拆出）：cache + 分层解析器查询。
//!
//! - `get_container_info_inner` / `get_container_info_by_identifier_inner`：按 identifier 查
//!   （契约四：经 `k8s_resolution` 分层解析，冲突/未知如实上报；仅普通 agent 保留 svc self-heal）。
//! - `find_container_inner`：cache → 分层解析 → 旧标签兼容三级。
//! - `list_containers_inner`：列举 rcoder-runtime managed pods。
//!
//! 与 k8s_agent_create.rs（创建）、k8s_agent_pod.rs（变更）正交。

use chrono::Utc;
use container_runtime_api::{
    AgentPodDiagnostic, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use shared_types::{ContainerBasicInfo, ServiceType};
use tracing::warn;

use super::k8s_pod::K8sPodOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::{
    CachedPod, KubernetesRuntime, POD_CACHE_TTL, RUNTIME_MANAGED_LABEL,
};

impl KubernetesRuntime {
    pub(crate) async fn get_container_info_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        // .cloned() 让 cached 成为 owned,读守卫在条件求值结束即释放 —— 否则守卫跨下面
        // build_container_basic_info().await 持续占读锁,卡住写者(stop/cleanup)。
        // 读守卫物化到独立块（guard 跨 await 地雷同 k8s_agent_create.rs 修正注释）。
        // 类型校验：同 identifier 下 STS 族与生产 UserApp 可并存（如 builder 与
        // 生产 Deployment 同 app_id），异族条目视为 miss，防止互串。
        let entry = {
            let guard = self.pod_cache.read().await;
            guard.get(identifier).cloned()
        };
        if let Some(entry) = entry
            && entry.cached_at.elapsed() < POD_CACHE_TTL
            && entry.info.status == ContainerRuntimeStatus::Running
            // 家族归一：Computer 族共享容器，缓存/请求两侧归一后比较
            && entry.service_type.family_representative() == service_type.family_representative()
        {
            return Ok(Some(
                self.build_container_basic_info(identifier, &entry.info)
                    .await?,
            ));
        }

        // Query K8s API —— 契约四分层解析：label 发现 + 身份核验（selector
        // 单一事实源在 k8s_resolution）；冲突/未知如实上报，不盲取首条。
        match self.resolve_pod(identifier, service_type).await {
            super::k8s_resolution::PodResolution::Present { info: boxed, .. } => {
                let mut info = *boxed;
                // 标签直读身份缺槽（bare-pod 等历史形态）按查询类型回填
                let fallback_slots =
                    container_runtime_api::slots_from_identifier(service_type, identifier);
                info.project_id = info.project_id.or(fallback_slots.project_id);
                info.user_id = info.user_id.or(fallback_slots.user_id);
                info.pod_id = info.pod_id.or(fallback_slots.pod_id);
                info.app_id = info.app_id.or(fallback_slots.app_id);
                info.service_type = info.service_type.or(Some(*service_type));

                // Update cache if running
                if info.status == ContainerRuntimeStatus::Running {
                    self.pod_cache.write().await.insert(
                        identifier.to_string(),
                        CachedPod {
                            info: info.clone(),
                            service_type: service_type.family_representative(),
                            cached_at: std::time::Instant::now(),
                        },
                    );
                }

                return Ok(Some(
                    self.build_container_basic_info(identifier, &info).await?,
                ));
            }
            super::k8s_resolution::PodResolution::Conflict { reason } => {
                return Err(ContainerRuntimeError::Conflict(reason));
            }
            super::k8s_resolution::PodResolution::Unknown { reason } => {
                return Err(ContainerRuntimeError::K8sError(reason));
            }
            // Absent / WorkloadWithoutPod：无容器可报（workload 在而 pod 暂缺
            // 对 info 查询同样返回 None——不判停，业务策略由调用方应用）。
            super::k8s_resolution::PodResolution::Absent { .. }
            | super::k8s_resolution::PodResolution::WorkloadWithoutPod { .. } => {}
        }

        Ok(None)
    }

    pub(crate) async fn get_container_info_by_identifier_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let info = self
            .get_container_info_inner(identifier, service_type)
            .await?;
        if info.is_some() && *service_type == ServiceType::UserappBuilder {
            let name = self.agent_service_name(identifier, service_type)?;
            let api: kube::Api<k8s_openapi::api::core::v1::Service> =
                kube::Api::namespaced(self.client.clone(), &self.namespace);
            let Some(service) = api.get_opt(&name).await.map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Inspect builder Service: {error}"))
            })?
            else {
                // Incomplete routing is not a missing Pod. The admitted ensure
                // path will reuse its bound workload and repair the Service.
                return Ok(None);
            };
            super::k8s_service::validate_builder_service(&service, identifier, false)?;
        }
        // Managed UserApp reads must not issue writes outside the operation
        // lease. Service repair belongs to admitted creation/resume, where its
        // acknowledged result is persisted before releasing the writer.
        if info.is_some()
            && !matches!(
                service_type,
                ServiceType::UserappBuilder | ServiceType::Userapp
            )
        {
            // Self-heal：异常创建（如 OrbStack sandbox 超时）可能留下"pod 在、svc 丢"
            // 的不一致状态——pod 重试后起来了，但 create_agent_service 那步没跑完。
            // 后续 Chat 走 svc FQDN `{pod}-svc:50051` 会 transport error → GRPC_ERROR。
            // create_agent_service 幂等（先 get，存在即返回，缺失才建），此处补建，避免人工删 pod 介入。
            // 失败仅 warn（get 是读操作，自愈失败不应阻塞读）。
            if let Err(e) = self.create_agent_service(identifier, service_type).await {
                warn!(
                    "[K8S] self-heal: 补建 agent service 失败 identifier={}, service_type={:?} (non-fatal): {}",
                    identifier, service_type, e
                );
            }
        }
        Ok(info)
    }

    pub(crate) async fn find_container_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // Check cache first（TTL 未过期才命中，避免外部删除后返旧）。
        // 读守卫物化（同上——guard 不跨下方 pods().get() 的网络 await）。
        // 类型校验：同 identifier 下 STS 族与生产 UserApp 可并存，异族条目视为 miss。
        let cached = {
            let guard = self.pod_cache.read().await;
            guard
                .get(identifier)
                .filter(|entry| {
                    entry.cached_at.elapsed() < POD_CACHE_TTL
                        && entry.service_type.family_representative()
                            == service_type.family_representative()
                })
                .map(|entry| entry.info.clone())
        };
        if let Some(info) = cached {
            return Ok(Some(info));
        }

        // 契约四分层解析：label 发现 + 身份核验 + STS 规范名兜底 + 缺席证据
        //（细节见 k8s_resolution）。冲突/未知如实上报；仍无候选时保留旧标签
        // 兼容查询（平滑迁移，生产 UserApp Deployment 无这些标签，无撞车面）。
        match self.resolve_pod(identifier, service_type).await {
            super::k8s_resolution::PodResolution::Present { info, .. } => {
                let info = *info;
                self.maybe_cache_running_pod(identifier, service_type, &info)
                    .await;
                return Ok(Some(info));
            }
            super::k8s_resolution::PodResolution::Conflict { reason } => {
                return Err(ContainerRuntimeError::Conflict(reason));
            }
            super::k8s_resolution::PodResolution::Unknown { reason } => {
                return Err(ContainerRuntimeError::K8sError(reason));
            }
            // 无候选：走旧标签兼容查询。Absent 的证据随日志留痕（对账/排障）。
            super::k8s_resolution::PodResolution::Absent { evidence } => {
                tracing::debug!(%evidence, %identifier, "pod resolution: absent");
            }
            super::k8s_resolution::PodResolution::WorkloadWithoutPod { .. } => {}
        }

        for old_selector in [
            format!("pod_id={}", identifier),
            format!("user_id={}", identifier),
            format!("project_id={}", identifier),
        ] {
            let pods = self
                .pods()
                .list(&ListParams::default().labels(&old_selector).limit(1))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "Failed to list pods with selector '{}': {}",
                        old_selector, e
                    ))
                })?;

            if let Some(pod) = pods.items.into_iter().next() {
                let info = Self::runtime_info_from_pod(&pod);
                self.maybe_cache_running_pod(identifier, service_type, &info)
                    .await;
                return Ok(Some(info));
            }
        }

        Ok(None)
    }

    /// find_container_inner 查询成功且 Running 时回填缓存。
    /// 避免 TTL 过期后每次 find_container 都打 K8s API（status checker 等
    /// 高频调用方）；与 get_container_info_inner 的写入语义一致（仅缓存 Running）。
    async fn maybe_cache_running_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        info: &RuntimeContainerInfo,
    ) {
        if info.status == ContainerRuntimeStatus::Running {
            self.pod_cache.write().await.insert(
                identifier.to_string(),
                CachedPod {
                    info: info.clone(),
                    service_type: service_type.family_representative(),
                    cached_at: std::time::Instant::now(),
                },
            );
        }
    }

    pub(crate) async fn list_containers_inner(
        &self,
    ) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let pods =
            self.pods().list(&lp).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Failed to list pods: {}", e))
            })?;

        let mut result = Vec::new();
        for p in pods.items {
            let pod: Pod = p;
            let status = Self::extract_pod_status(&pod);
            let metadata = &pod.metadata;

            // 从 Pod 的 labels 中提取环境变量信息 + 结构化身份（标签直读）
            let empty_labels = std::collections::BTreeMap::new();
            let labels = metadata.labels.as_ref().unwrap_or(&empty_labels);
            let mut env_vars = std::collections::HashMap::new();
            if let Some(project_id) = labels.get("project_id") {
                env_vars.insert("PROJECT_ID".to_string(), project_id.clone());
            }
            if let Some(user_id) = labels.get("user_id") {
                env_vars.insert("USER_ID".to_string(), user_id.clone());
            }
            let (service_type, slots) = super::k8s_service::container_identity_from_labels(labels);

            let pod_info = RuntimeContainerInfo {
                container_id: metadata.uid.clone().unwrap_or_default(),
                // 同 get 路径：ownerReference 权威派生 workload 名（契约一）。
                container_name: Self::workload_name_from_pod(metadata),
                container_ip: pod
                    .status
                    .as_ref()
                    .and_then(|s| s.pod_ip.clone())
                    .unwrap_or_default(),
                status,
                created_at: metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now),
                env_vars: Some(env_vars),
                service_type,
                project_id: slots.project_id,
                user_id: slots.user_id,
                pod_id: slots.pod_id,
                app_id: slots.app_id,
                workload_uid: Self::workload_uid_from_pod_owner(metadata),
            };
            result.push(pod_info);
        }

        Ok(result)
    }

    /// 诊断 agent pod 容器状态(gRPC 连接失败时定位真实根因)。
    ///
    /// 契约四：诊断类消费者必须能返回 Pending/CrashLoopBackOff/未 Ready——
    /// 经分层解析器发现候选（label 优先 + STS 规范名兜底），不只按派生名
    /// GET，标签漂移/换代窗口内同样可诊断。
    /// 取 "agent" 容器 ContainerStatus,解析: restart_count / ready /
    /// last_terminate_reason(OOMKilled)/ last_exit_code / waiting_reason
    /// (CrashLoopBackOff)/ 可读 detail(复用 [`super::k8s_app_query::container_error_message`])。
    /// pod 不存在 → exists=false（workload 在而 pod 暂缺同样 exists=false，
    /// detail 注明，不判停）；冲突/观察不完整 → 向上传播 Err。
    pub(crate) async fn diagnose_agent_pod_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<AgentPodDiagnostic> {
        let pod = match self.resolve_pod(identifier, service_type).await {
            super::k8s_resolution::PodResolution::Present { pod, .. } => pod,
            super::k8s_resolution::PodResolution::Absent { .. } => {
                return Ok(AgentPodDiagnostic {
                    exists: false,
                    ..Default::default()
                });
            }
            super::k8s_resolution::PodResolution::WorkloadWithoutPod { workload_name } => {
                return Ok(AgentPodDiagnostic {
                    exists: false,
                    detail: Some(format!(
                        "workload {workload_name} exists but no pod is scheduled (recreating?)"
                    )),
                    ..Default::default()
                });
            }
            super::k8s_resolution::PodResolution::Conflict { reason } => {
                return Err(ContainerRuntimeError::Conflict(reason));
            }
            super::k8s_resolution::PodResolution::Unknown { reason } => {
                return Err(ContainerRuntimeError::K8sError(reason));
            }
        };

        // agent-runner STS 主容器名(与 k8s_agent_create.rs 创建处、k8s_agent_pod.rs 的 AGENT_CONTAINER 保持一致)
        const AGENT_CONTAINER: &str = "agent";
        let Some(cs) = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|list| list.iter().find(|c| c.name == AGENT_CONTAINER))
        else {
            // pod 存在但 agent 容器状态尚未上报(刚创建 / ContainerCreating)
            return Ok(AgentPodDiagnostic {
                exists: true,
                ready: false,
                detail: Some("agent container status not available yet".to_string()),
                ..Default::default()
            });
        };

        let last_terminated = cs.last_state.as_ref().and_then(|ls| ls.terminated.as_ref());
        let waiting_reason = cs
            .state
            .as_ref()
            .and_then(|s| s.waiting.as_ref())
            .and_then(|w| w.reason.clone());

        Ok(AgentPodDiagnostic {
            exists: true,
            ready: cs.ready,
            restart_count: u32::try_from(cs.restart_count).unwrap_or(0),
            last_terminate_reason: last_terminated.and_then(|t| t.reason.clone()),
            last_exit_code: last_terminated.map(|t| t.exit_code),
            waiting_reason,
            detail: super::k8s_app_query::container_error_message(cs),
        })
    }
}
