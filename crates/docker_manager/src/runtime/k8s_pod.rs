//! Kubernetes Pod 生命周期管理
//!
//! 提供 Pod 状态提取、就绪等待、终止等待等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 Pod 操作方法。

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use chrono::Utc;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::Pod;
#[cfg(feature = "kubernetes")]
use kube::api::DeleteParams;
#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;
#[cfg(feature = "kubernetes")]
use tracing::{debug, error, info, warn};

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

// Pod 生命周期超时常量
#[cfg(feature = "kubernetes")]
#[allow(dead_code)] // wait_for_pod_ready 当前用 config.pod_ttl_seconds 兜底；保留作 ready 超时语义占位
const POD_READY_TIMEOUT_SECS: u64 = 300;
#[cfg(feature = "kubernetes")]
const POD_TERMINATION_TIMEOUT_SECS: u64 = 30;
#[cfg(feature = "kubernetes")]
const FORCE_DELETE_CLEANUP_TIMEOUT_SECS: u64 = 15;

/// Pod 生命周期管理操作的 trait extension
///
/// 为 `KubernetesRuntime` 添加 Pod 相关方法：
/// - Pod 命名 (`pod_name`)
/// - 状态提取 (`extract_pod_status`, `runtime_info_from_pod`)
/// - 就绪等待 (`wait_for_pod_ready`，watch 观察——批次 A)
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
        let prefix = KubernetesRuntime::sanitize_k8s_name_part(
            &self.service_container_prefix(service_type)?,
        );
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

        // 从 Pod 的 labels 中提取环境变量信息 + 结构化身份（标签直读，消费方
        // 不再从容器名反解）
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

        RuntimeContainerInfo {
            container_id: metadata.uid.clone().unwrap_or_default(),
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
            service_type,
            project_id: slots.project_id,
            user_id: slots.user_id,
            pod_id: slots.pod_id,
            app_id: slots.app_id,
        }
    }

    async fn wait_for_pod_ready(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // Pod wait timeout: configurable from config, default 120s
        let timeout = std::time::Duration::from_secs(self.config.pod_ttl_seconds.unwrap_or(120));
        // agent-runner 走 StatefulSet，pod 稳定名为 {sts_name}-0（非裸 pod 的 {sts_name}）。
        let pod_name = self.agent_pod_name(identifier, service_type)?;
        // kube-runtime 批次 A：轮询改单对象 watch（总 deadline 覆盖初始 LIST/
        // 重连/复核——KR03）；判定语义逐字保留（分类器 = 原 phase/reason/Ready
        // 三段判据，KR01）。调用方取消 = drop 本 future（select 分支随流一并
        // 释放，KR04——观察不持后台任务）。
        let deadline = std::time::Instant::now() + timeout;
        let started = std::time::Instant::now();
        let verdict = super::k8s_observation::await_pod_verdict(
            &self.pods(),
            &pod_name,
            deadline,
            tokio_util::sync::CancellationToken::new(),
            super::k8s_observation::classify_pod_readiness,
        )
        .await;
        info!(
            "[K8S] Pod {} readiness observation finished in {:.1}s",
            pod_name,
            started.elapsed().as_secs_f64()
        );
        match verdict {
            Ok(super::k8s_observation::Verdict::Complete(())) => {
                info!("[K8S] Pod {} is Ready", pod_name);
                Ok(())
            }
            Ok(super::k8s_observation::Verdict::Rejected(reason)) => Err(
                ContainerRuntimeError::K8sError(format!("Pod {pod_name}: {reason}")),
            ),
            Ok(super::k8s_observation::Verdict::Pending) => Err(ContainerRuntimeError::K8sError(
                format!("Pod {pod_name}: observation ended while pending"),
            )),
            Err(super::k8s_observation::ObservationError::Deadline { last_transient }) => {
                Err(ContainerRuntimeError::Timeout(format!(
                    "Pod did not become ready in time (last observation: {})",
                    last_transient.as_deref().unwrap_or("none")
                )))
            }
            Err(error) => Err(ContainerRuntimeError::K8sError(format!(
                "Failed to get pod '{pod_name}': {error}"
            ))),
        }
    }

    async fn wait_for_pod_terminated(&self, pod_name: &str) -> ContainerRuntimeResult<()> {
        let timeout = std::time::Duration::from_secs(POD_TERMINATION_TIMEOUT_SECS);
        // 细化轮询: 容器秒退后 pod 对象仍要 ~2-3s 才从 API 消失(kubelet 清理)。
        // 1s 轮询会多等最多 1s; 300ms 既及时察觉删除, 又不给 API server 压力(最多 ~100 次廉价 GET)。
        let poll_interval = std::time::Duration::from_millis(300);
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
                    debug!("[K8S] Poll pod {} returned {} (will retry)", pod_name, e);
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        // 超时：force-delete（gracePeriodSeconds=0 立即杀死容器）
        warn!(
            "[K8S] Pod {} did not terminate within 30s, issuing force-delete",
            pod_name
        );
        let force_dp = DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        };
        match self.pods().delete(pod_name, &force_dp).await {
            Ok(_) => {
                info!(
                    "[K8S] Pod {} force-delete requested (gracePeriod=0)",
                    pod_name
                );
                // force-delete 成功后仍需等待 Pod 真正消失（404）
                // 因为 force-delete 只是发起请求，kubelet 还需要时间清理
                let cleanup_start = std::time::Instant::now();
                let cleanup_timeout =
                    std::time::Duration::from_secs(FORCE_DELETE_CLEANUP_TIMEOUT_SECS);
                while cleanup_start.elapsed() < cleanup_timeout {
                    match self.pods().get(pod_name).await {
                        Ok(_) => {
                            debug!(
                                "[K8S] Pod {} still cleaning up after force-delete ({:.0}s elapsed)...",
                                pod_name,
                                cleanup_start.elapsed().as_secs_f64()
                            );
                        }
                        Err(kube::Error::Api(ae)) if ae.code == 404 => {
                            info!(
                                "[K8S] Pod {} fully terminated after force-delete (took {:.1}s)",
                                pod_name,
                                start.elapsed().as_secs_f64()
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            debug!(
                                "[K8S] Poll pod {} after force-delete returned {} (will retry)",
                                pod_name, e
                            );
                        }
                    }
                    tokio::time::sleep(poll_interval).await;
                }
                // 超时后 Pod 仍未终止：返回错误而非静默继续
                // 调用方需据此判断是否继续 PVC 操作（避免 RWO 数据竞争）
                error!(
                    "[K8S] Pod {} still not 404 after force-delete + 15s wait, returning error",
                    pod_name
                );
                Err(ContainerRuntimeError::Timeout(format!(
                    "Pod {} still not terminated after force-delete + 15s cleanup wait",
                    pod_name
                )))
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                info!("[K8S] Pod {} already gone after timeout", pod_name);
                Ok(())
            }
            Err(e) => {
                // force-delete 失败：Pod 仍在运行，返回错误让调用方知道
                // 这会影响后续的 PVC 清理决策
                error!(
                    "[K8S] Force-delete pod {} failed: {}. Pod is still running.",
                    pod_name, e
                );
                Err(ContainerRuntimeError::ContainerStopError(format!(
                    "force-delete pod {} failed after 75s timeout: {}",
                    pod_name, e
                )))
            }
        }
    }
}
