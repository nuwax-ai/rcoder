//! agent-runner 查询/读取（从 k8s_agent_pod.rs 拆出）：cache + K8s API 按 label 查 + 列举。
//!
//! - `get_container_info_inner` / `get_container_info_by_identifier_inner`：按 identifier 查（后者带 svc self-heal）。
//! - `find_container_inner`：cache → pod 名 → 标准 label → 旧 label 三级查。
//! - `list_containers_inner`：列举 rcoder-runtime managed pods。
//!
//! 与 k8s_agent_create.rs（创建）、k8s_agent_pod.rs（变更）正交。

use chrono::Utc;
use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, RuntimeContainerInfo,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use shared_types::{ContainerBasicInfo, ServiceType};
use tracing::warn;

use super::k8s_pod::K8sPodOps;
use super::k8s_service::K8sServiceOps;
use super::kubernetes_runtime::{KubernetesRuntime, RUNTIME_MANAGED_LABEL};

impl KubernetesRuntime {
    pub(crate) async fn get_container_info_inner(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        // .cloned() 让 cached 成为 owned,读守卫在条件求值结束即释放 —— 否则守卫跨下面
        // build_container_basic_info().await 持续占读锁,卡住写者(stop/cleanup)。
        if let Some(cached) = self.pod_cache.read().await.get(identifier).cloned()
            && cached.status == ContainerRuntimeStatus::Running
        {
            return Ok(Some(
                self.build_container_basic_info(identifier, &cached).await?,
            ));
        }

        // Query K8s API - 使用标准 K8s 标签查询（与 build_standard_labels 一致）
        let search_queries = vec![
            format!("app.kubernetes.io/instance={}", identifier),
            format!("rcoder.io/identifier={}", identifier),
        ];

        for query in search_queries {
            let lp = ListParams::default().labels(&query);
            if let Ok(pods) = self.pods().list(&lp).await
                && let Some(pod) = pods.items.into_iter().next()
            {
                let status = Self::extract_pod_status(&pod);
                let metadata = &pod.metadata;
                let uid = metadata.uid.clone().unwrap_or_default();
                let name = metadata.name.clone().unwrap_or_default();
                let pod_ip = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.pod_ip.clone())
                    .unwrap_or_default();
                let created_at = metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now);

                let pod_info = RuntimeContainerInfo {
                    container_id: uid,
                    // agent-runner 走 STS：pod 名 = {sts_name}-0，但 container_name 用作寻址基名
                    // （Service FQDN/grpc_addr/backend_addr 都从它派生 `{name}-svc`），故剥 -0 还原
                    // sts_name，否则所有 gRPC/VNC 地址会指向不存在的 {...}-0-svc。bare-pod 残留无
                    // -0 后缀，strip 安全（identity）。实际 pod 名由 agent_pod_name() 按需取。
                    container_name: Self::sts_name_from_pod_name(&name).to_string(),
                    container_ip: pod_ip,
                    status,
                    created_at,
                    env_vars: None,
                };

                // Update cache if running
                if pod_info.status == ContainerRuntimeStatus::Running {
                    self.pod_cache
                        .write()
                        .await
                        .insert(identifier.to_string(), pod_info.clone());
                }

                return Ok(Some(
                    self.build_container_basic_info(identifier, &pod_info)
                        .await?,
                ));
            }
        }

        Ok(None)
    }

    pub(crate) async fn get_container_info_by_identifier_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let info = self.get_container_info_inner(identifier).await?;
        if info.is_some() {
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
        // Check cache first
        if let Some(cached) = self.pod_cache.read().await.get(identifier) {
            return Ok(Some(cached.clone()));
        }

        // 1) Query by concrete pod name
        let pod_name = self.pod_name(identifier, service_type)?;
        match self.pods().get(&pod_name).await {
            Ok(pod) => return Ok(Some(Self::runtime_info_from_pod(&pod))),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to get pod by name '{}': {}",
                    pod_name, e
                )));
            }
        }

        // 2) Query by labels (使用新的标准标签)
        let selector = format!("app.kubernetes.io/instance={}", identifier);
        let pods = self
            .pods()
            .list(&ListParams::default().labels(&selector).limit(1))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "Failed to list pods with selector '{}': {}",
                    selector, e
                ))
            })?;

        if let Some(pod) = pods.items.into_iter().next() {
            return Ok(Some(Self::runtime_info_from_pod(&pod)));
        }

        // 3) 兼容旧标签查询（平滑迁移）
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
                return Ok(Some(Self::runtime_info_from_pod(&pod)));
            }
        }

        Ok(None)
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

            // 从 Pod 的 labels 中提取环境变量信息
            let mut env_vars = std::collections::HashMap::new();
            if let Some(labels) = &metadata.labels {
                if let Some(project_id) = labels.get("project_id") {
                    env_vars.insert("PROJECT_ID".to_string(), project_id.clone());
                }
                if let Some(user_id) = labels.get("user_id") {
                    env_vars.insert("USER_ID".to_string(), user_id.clone());
                }
            }

            let pod_info = RuntimeContainerInfo {
                container_id: metadata.uid.clone().unwrap_or_default(),
                // 同 get 路径：剥 STS ordinal -0，container_name 作寻址基名（见上方注释）。
                container_name: Self::sts_name_from_pod_name(
                    &metadata.name.clone().unwrap_or_default(),
                )
                .to_string(),
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
            };
            result.push(pod_info);
        }

        Ok(result)
    }
}
