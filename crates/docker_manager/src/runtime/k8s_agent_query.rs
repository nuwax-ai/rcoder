//! agent-runner 查询/读取（从 k8s_agent_pod.rs 拆出）：cache + K8s API 按 label 查 + 列举。
//!
//! - `get_container_info_inner` / `get_container_info_by_identifier_inner`：按 identifier 查（后者带 svc self-heal）。
//! - `find_container_inner`：cache → pod 名 → 标准 label → 旧 label 三级查。
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
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        // .cloned() 让 cached 成为 owned,读守卫在条件求值结束即释放 —— 否则守卫跨下面
        // build_container_basic_info().await 持续占读锁,卡住写者(stop/cleanup)。
        // 读守卫物化到独立块（guard 跨 await 地雷同 k8s_agent_create.rs 修正注释）。
        let entry = {
            let guard = self.pod_cache.read().await;
            guard.get(identifier).cloned()
        };
        if let Some(entry) = entry
            && entry.cached_at.elapsed() < POD_CACHE_TTL
            && entry.info.status == ContainerRuntimeStatus::Running
        {
            return Ok(Some(
                self.build_container_basic_info(identifier, &entry.info)
                    .await?,
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
                    self.pod_cache.write().await.insert(
                        identifier.to_string(),
                        CachedPod {
                            info: pod_info.clone(),
                            cached_at: std::time::Instant::now(),
                        },
                    );
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
        // Check cache first（TTL 未过期才命中，避免外部删除后返旧）。
        // 读守卫物化（同上——guard 不跨下方 pods().get() 的网络 await）。
        let cached = {
            let guard = self.pod_cache.read().await;
            guard
                .get(identifier)
                .filter(|entry| entry.cached_at.elapsed() < POD_CACHE_TTL)
                .map(|entry| entry.info.clone())
        };
        if let Some(info) = cached {
            return Ok(Some(info));
        }

        // 1) Query by concrete pod name
        let pod_name = self.pod_name(identifier, service_type)?;
        match self.pods().get(&pod_name).await {
            Ok(pod) => {
                let info = Self::runtime_info_from_pod(&pod);
                self.maybe_cache_running_pod(identifier, &info).await;
                return Ok(Some(info));
            }
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
            let info = Self::runtime_info_from_pod(&pod);
            self.maybe_cache_running_pod(identifier, &info).await;
            return Ok(Some(info));
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
                let info = Self::runtime_info_from_pod(&pod);
                self.maybe_cache_running_pod(identifier, &info).await;
                return Ok(Some(info));
            }
        }

        Ok(None)
    }

    /// find_container_inner 查询成功且 Running 时回填缓存。
    /// 避免 TTL 过期后每次 find_container 都打 K8s API（status checker 等
    /// 高频调用方）；与 get_container_info_inner 的写入语义一致（仅缓存 Running）。
    async fn maybe_cache_running_pod(&self, identifier: &str, info: &RuntimeContainerInfo) {
        if info.status == ContainerRuntimeStatus::Running {
            self.pod_cache.write().await.insert(
                identifier.to_string(),
                CachedPod {
                    info: info.clone(),
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

    /// 诊断 agent pod 容器状态(gRPC 连接失败时定位真实根因)。
    ///
    /// 取 STS pod `{prefix}-{identifier}-0` 的 "agent" 容器 ContainerStatus,解析:
    /// restart_count / ready / last_terminate_reason(OOMKilled)/ last_exit_code / waiting_reason
    /// (CrashLoopBackOff)/ 可读 detail(复用 [`super::k8s_app_query::container_error_message`])。
    /// pod 不存在(404)→ exists=false;其他 K8s API 错误 → 向上传播 Err(调用方兜底为"未知")。
    pub(crate) async fn diagnose_agent_pod_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<AgentPodDiagnostic> {
        let pod_name = self.agent_pod_name(identifier, service_type)?;
        let pod = match self.pods().get(&pod_name).await {
            Ok(pod) => pod,
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // pod 不存在:本身就是根因(默认诊断 exists=true,这里显式置 false)
                return Ok(AgentPodDiagnostic {
                    exists: false,
                    ..Default::default()
                });
            }
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "diagnose_agent_pod: get pod {pod_name} failed: {e}"
                )));
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
