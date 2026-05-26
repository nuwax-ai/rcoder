//! Kubernetes Pod 生命周期管理
//!
//! 提供 Pod 状态提取、就绪等待、终止等待等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 Pod 操作方法。

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use chrono::Utc;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, RuntimeContainerInfo};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::Pod;
#[cfg(feature = "kubernetes")]
use kube::api::DeleteParams;
#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

/// Pod 生命周期管理操作的 trait extension
///
/// 为 `KubernetesRuntime` 添加 Pod 相关方法：
/// - Pod 命名 (`pod_name`)
/// - 状态提取 (`extract_pod_status`, `runtime_info_from_pod`, `detect_container_failure`)
/// - 就绪等待 (`wait_for_pod_ready`)
/// - 终止等待 (`wait_for_pod_terminated`)
#[cfg(feature = "kubernetes")]
#[async_trait]
pub(crate) trait K8sPodOps {
    /// 生成 Pod 名称
    ///
    /// K8s Pod 名称必须符合 RFC 1123：小写字母数字 + '-'，必须以字母数字开头/结尾。
    /// 将下划线替换为连字符以确保兼容性。
    fn pod_name(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String>;

    /// 从 Pod 对象提取运行状态
    fn extract_pod_status(pod: &Pod) -> ContainerRuntimeStatus;

    /// 从 Pod 对象构建 RuntimeContainerInfo
    fn runtime_info_from_pod(pod: &Pod) -> RuntimeContainerInfo;

    /// 检测容器失败原因
    ///
    /// 从 container_statuses 中提取 waiting 状态的 reason，
    /// 用于诊断 CrashLoopBackOff、ImagePullBackOff 等问题。
    fn detect_container_failure(pod: &Pod) -> Option<String>;

    /// 等待 Pod 就绪
    ///
    /// 使用 readinessProbe 检查 Pod 是否真正就绪，而非仅检查 Running 状态。
    /// 超时时间由 `config.pod_ttl_seconds` 配置（默认 120s）。
    async fn wait_for_pod_ready(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;

    /// 等待 Pod 完全终止（从 API Server 消失，返回 404）
    ///
    /// Pod 的 `termination_grace_period_seconds = 60s`，
    /// 设置总超时 75s（grace period + 15s 缓冲）。超时后执行 force-delete（`gracePeriodSeconds=0`）
    /// 强制杀死容器，确保不会无限卡死。
    async fn wait_for_pod_terminated(&self, pod_name: &str) -> ContainerRuntimeResult<()>;
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl K8sPodOps for KubernetesRuntime {
    fn pod_name(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let prefix =
            KubernetesRuntime::sanitize_k8s_name_part(&self.service_container_prefix(service_type)?);
        let sanitized_id = project_id.replace('_', "-");
        Ok(format!("{}-{}", prefix, sanitized_id))
    }

    fn extract_pod_status(pod: &Pod) -> ContainerRuntimeStatus {
        match &pod.status {
            Some(status) => match status.phase.as_deref() {
                Some("Running") => ContainerRuntimeStatus::Running,
                Some("Succeeded") => ContainerRuntimeStatus::Succeeded,
                Some("Failed") => ContainerRuntimeStatus::Failed,
                Some("Pending") => ContainerRuntimeStatus::Pending,
                Some(phase) => ContainerRuntimeStatus::Unknown(phase.to_string()),
                None => ContainerRuntimeStatus::Unknown("No phase".to_string()),
            },
            None => ContainerRuntimeStatus::Pending,
        }
    }

    fn runtime_info_from_pod(pod: &Pod) -> RuntimeContainerInfo {
        let status = Self::extract_pod_status(pod);
        let metadata = &pod.metadata;
        RuntimeContainerInfo {
            container_id: metadata.uid.clone().unwrap_or_default(),
            container_name: metadata.name.clone().unwrap_or_default(),
            container_ip: pod
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.clone())
                .unwrap_or_default(),
            status,
            created_at: metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0)
                .unwrap_or_else(Utc::now),
        }
    }

    fn detect_container_failure(pod: &Pod) -> Option<String> {
        let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
        for cs in statuses {
            if let Some(state) = &cs.state
                && let Some(waiting) = &state.waiting
                && let Some(reason) = &waiting.reason
            {
                return Some(reason.clone());
            }
        }
        None
    }

    async fn wait_for_pod_ready(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // Pod wait timeout: configurable from config, default 120s
        let timeout = std::time::Duration::from_secs(self.config.pod_ttl_seconds.unwrap_or(120));
        let start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type)?;

        while start.elapsed() < timeout {
            match self.pods().get(&pod_name).await {
                Ok(pod) => {
                    // 检查 Pod phase，提前识别永久失败状态
                    if let Some(phase) = pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
                        match phase {
                            "Failed" => {
                                // Pod 实际失败：容器异常退出/OOM/镜像拉失败 等
                                let reason = Self::detect_container_failure(&pod);
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} entered terminal state: Failed, reason: {:?}",
                                    pod_name, reason
                                )));
                            }
                            "Succeeded" => {
                                // 容器跑完正常退出（run-to-completion 场景，例如一次性任务型 Pod）。
                                // 这不是失败：pod 已成功执行到结束。对调用方而言"ready" 的语义是
                                // "容器至少跑起来过"，Succeeded 满足这个语义。
                                info!(
                                    "[K8S] Pod {} completed with phase=Succeeded (run-to-completion)",
                                    pod_name
                                );
                                return Ok(());
                            }
                            "Running" => {
                                // Pod 运行中，检查 Ready condition
                            }
                            _ => {
                                // Pending 等其他状态，继续等待
                            }
                        }
                    }

                    // 检测 CrashLoopBackOff 和 ImagePullBackOff
                    if let Some(reason) = Self::detect_container_failure(&pod) {
                        match reason.as_str() {
                            "CrashLoopBackOff" => {
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} is in CrashLoopBackOff state",
                                    pod_name
                                )));
                            }
                            "ImagePullBackOff" => {
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} failed to pull image: {:?}",
                                    pod_name,
                                    pod.status
                                        .as_ref()
                                        .and_then(|s| s.container_statuses.as_ref())
                                )));
                            }
                            _ => {
                                // 其他 waiting 状态，继续等待
                                debug!("[K8S] Pod {} waiting: {}", pod_name, reason);
                            }
                        }
                    }

                    // 检查 Pod 是否 Ready (需要 readinessProbe 返回成功)
                    if let Some(status) = &pod.status
                        && let Some(conditions) = &status.conditions
                    {
                        let all_ready = conditions
                            .iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True");
                        if all_ready {
                            info!("[K8S] Pod {} is Ready", pod_name);
                            return Ok(());
                        }
                        // 调试用：打印当前状态
                        let ready_status = conditions
                            .iter()
                            .map(|c| format!("{}={}", c.type_, c.status))
                            .collect::<Vec<_>>()
                            .join(", ");
                        debug!("[K8S] Pod {} conditions: {}", pod_name, ready_status);
                    }
                }
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // Pod 还没创建，继续等待
                }
                Err(e) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "Failed to get pod '{}': {}",
                        pod_name, e
                    )));
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ContainerRuntimeError::Timeout(
            "Pod did not become ready in time".to_string(),
        ))
    }

    async fn wait_for_pod_terminated(&self, pod_name: &str) -> ContainerRuntimeResult<()> {
        let timeout = std::time::Duration::from_secs(75);
        let poll_interval = std::time::Duration::from_secs(1);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            match self.pods().get(pod_name).await {
                Ok(_) => {
                    debug!(
                        "[K8S] Pod {} still terminating ({:.0}s elapsed)...",
                        pod_name,
                        start.elapsed().as_secs_f64()
                    );
                }
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    info!(
                        "[K8S] Pod {} fully terminated (took {:.1}s)",
                        pod_name,
                        start.elapsed().as_secs_f64()
                    );
                    return Ok(());
                }
                Err(e) => {
                    // 409 Conflict 等情况：Pod 正在被修改，下次轮询重试
                    debug!(
                        "[K8S] Poll pod {} returned {} (will retry)",
                        pod_name, e
                    );
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        // 超时：force-delete（gracePeriodSeconds=0 立即杀死容器）
        warn!(
            "[K8S] Pod {} did not terminate within 75s, issuing force-delete",
            pod_name
        );
        let force_dp = DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        };
        match self.pods().delete(pod_name, &force_dp).await {
            Ok(_) => info!(
                "[K8S] Pod {} force-delete requested (gracePeriod=0)",
                pod_name
            ),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                info!("[K8S] Pod {} already gone after timeout", pod_name);
            }
            Err(e) => {
                warn!(
                    "[K8S] Force-delete pod {} failed: {}. \
                     Pod may still be running; subsequent PVC cleanup may encounter unmount issues.",
                    pod_name, e
                );
                // 不返回错误，让调用方继续：
                // - K8s 最终会回收 Pod（grace period 到期后 kubelet 强杀）
                // - PVC 清理有独立的超时和容错机制
            }
        }
        Ok(())
    }
}
