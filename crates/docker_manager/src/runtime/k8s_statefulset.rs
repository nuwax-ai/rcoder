//! agent-runner StatefulSet 操作（K8s 原生 pod 级自愈）。
//!
//! agent-runner 由裸 Pod 改为 per-identifier StatefulSet（replicas 1）：
//! - pod 被 evict/删除/节点挂 → StatefulSet 控制器自动重建同名 pod（挂回同 PVC，数据不丢）；
//! - 容器级 OOM 仍由 restartPolicy=Always 原地重启（pod 模板继承）；
//! - stop/destroy = 删 STS + ClusterIP/headless svc（保留 PVC；下次 ensure 重建 STS 挂回同 PVC）。
//!
//! 仅 ComputerAgentRunner / WebAgentRunner 走此路径；UserApp 仍用 Deployment（create_deployment）。

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{PodSpec, PodTemplateSpec, Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use tracing::{debug, info, warn};

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use shared_types::ServiceType;

use crate::runtime::k8s_pod::K8sPodOps;
use crate::runtime::k8s_service::build_standard_labels;

use super::KubernetesRuntime;

/// rcoder.io/service-type label key（与 build_standard_labels 写入的一致，用于 STS 重名时类型校验）
const SERVICE_TYPE_LABEL: &str = "rcoder.io/service-type";

impl KubernetesRuntime {
    /// StatefulSet API 访问器（与 pods()/pvcs() 对齐）。
    pub(crate) fn statefulsets(&self) -> Api<StatefulSet> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// headless Service 名（STS serviceName 指向它，供稳定 DNS/身份）。
    pub(crate) fn agent_headless_svc_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        Ok(format!(
            "{}-headless",
            self.pod_name(identifier, service_type)?
        ))
    }

    /// STS 实际 Pod 名（StatefulSet 稳定命名：`{sts_name}-0`）。
    pub(crate) fn agent_pod_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        Ok(format!("{}-0", self.pod_name(identifier, service_type)?))
    }

    /// STS pod 名(`{sts_name}-0`)→ sts_name。从缓存的 container_name(pod 名)反推 STS 名
    /// (查 STS 存在性 / 拼 Service FQDN 用)。agent-runner 单副本,ordinal 恒为 0,
    /// 故剥末尾 "-0" 安全(不与业务 id 冲突:id 末位为 0 时 sts 名自身不含 -0 后缀)。
    pub(crate) fn sts_name_from_pod_name(pod_name: &str) -> &str {
        pod_name.strip_suffix("-0").unwrap_or(pod_name)
    }

    /// 确保 headless Service 存在（STS 必需，clusterIP=None）。selector 与 pod labels 一致。
    pub(crate) async fn ensure_agent_headless_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let svc_name = self.agent_headless_svc_name(identifier, service_type)?;
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        if services.get(&svc_name).await.is_ok() {
            return Ok(()); // 已存在
        }
        let labels = build_standard_labels(identifier, service_type);
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(svc_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                cluster_ip: Some("None".to_string()), // headless（STS 身份必需）
                selector: Some(labels),
                ports: Some(vec![ServicePort {
                    name: Some("grpc".to_string()),
                    port: shared_types::GRPC_DEFAULT_PORT as i32,
                    target_port: Some(IntOrString::Int(shared_types::GRPC_DEFAULT_PORT as i32)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };
        services
            .create(&PostParams::default(), &svc)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("create headless svc: {e}")))?;
        debug!("[K8S-STS] headless Service created: {}", svc_name);
        Ok(())
    }

    /// 删除 headless Service（destroy/回收时与 STS、ClusterIP svc 一起清；ensure 幂等，
    /// 残留也无害，但彻底回收应一并删）。404 视作已删。
    pub(crate) async fn delete_agent_headless_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let svc_name = self.agent_headless_svc_name(identifier, service_type)?;
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        match services.delete(&svc_name, &DeleteParams::default()).await {
            Ok(_) => {
                debug!("[K8S-STS] headless Service deleted: {}", svc_name);
                Ok(())
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::K8sError(format!(
                "delete headless svc {}: {}",
                svc_name, e
            ))),
        }
    }

    /// 构造 StatefulSet（replicas + pod 模板 = 现有 PodSpec；serviceName 指向 headless svc）。
    fn build_agent_statefulset(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        pod_spec: PodSpec,
        replicas: i32,
    ) -> ContainerRuntimeResult<StatefulSet> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let headless = self.agent_headless_svc_name(identifier, service_type)?;
        let labels = build_standard_labels(identifier, service_type);
        Ok(StatefulSet {
            metadata: ObjectMeta {
                name: Some(sts_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                service_name: Some(headless),
                replicas: Some(replicas),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                // 单副本 STS，OrderedReady/Parallel 无差别，省略用默认
                ..Default::default()
            }),
            status: None,
        })
    }

    /// 确保 StatefulSet 存在且 replicas=期望值(幂等)。
    /// - 不存在 → 创建(replicas=期望);
    /// - 存在但 service_type 不匹配(历史重名) → 删旧重建;
    /// - 存在且匹配 → patch replicas 到期望(纠正漂移,如被外部 scale 过;正常 1→1 为 no-op)。
    pub(crate) async fn ensure_agent_statefulset(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        pod_spec: PodSpec,
        replicas: i32,
    ) -> ContainerRuntimeResult<()> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let sts_api = self.statefulsets();
        match sts_api.get(&sts_name).await {
            Ok(existing) => {
                let existing_st = existing
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(SERVICE_TYPE_LABEL));
                if existing_st != Some(&service_type.to_string()) {
                    warn!(
                        "[K8S-STS] {} exists but service_type mismatch (existing={:?}, requested={:?}); recreating",
                        sts_name, existing_st, service_type
                    );
                    self.delete_agent_statefulset(identifier, service_type)
                        .await?;
                    let sts =
                        self.build_agent_statefulset(identifier, service_type, pod_spec, replicas)?;
                    sts_api
                        .create(&PostParams::default(), &sts)
                        .await
                        .map_err(|e| {
                            ContainerRuntimeError::K8sError(format!("recreate sts: {e}"))
                        })?;
                    info!("[K8S-STS] recreated {} (type={:?})", sts_name, service_type);
                } else {
                    // 类型匹配：scale 到期望 replicas（幂等）
                    self.scale_agent_statefulset(identifier, service_type, replicas)
                        .await?;
                }
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                let sts =
                    self.build_agent_statefulset(identifier, service_type, pod_spec, replicas)?;
                sts_api
                    .create(&PostParams::default(), &sts)
                    .await
                    .map_err(|e| ContainerRuntimeError::K8sError(format!("create sts: {e}")))?;
                info!(
                    "[K8S-STS] StatefulSet created: {} (replicas={}, type={:?})",
                    sts_name, replicas, service_type
                );
            }
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get sts {}: {}",
                    sts_name, e
                )));
            }
        }
        Ok(())
    }

    /// scale StatefulSet 到指定 replicas（patch spec.replicas）。
    pub(crate) async fn scale_agent_statefulset(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        replicas: i32,
    ) -> ContainerRuntimeResult<()> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let sts_api = self.statefulsets();
        // SSA patch：只改 replicas（field manager 独立，不误伤其他字段）
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        sts_api
            .patch(
                &sts_name,
                &PatchParams::default().force(),
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("scale sts {sts_name}: {e}")))?;
        debug!("[K8S-STS] scaled {} to replicas={}", sts_name, replicas);
        Ok(())
    }

    /// 删除 StatefulSet（cascade：pod 随之删除）。purge / service_type 重名重建 / cleanup_all 用。
    pub(crate) async fn delete_agent_statefulset(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let sts_api = self.statefulsets();
        match sts_api
            .delete(
                &sts_name,
                &DeleteParams {
                    propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {
                info!("[K8S-STS] StatefulSet deleted: {}", sts_name);
                Ok(())
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!("[K8S-STS] StatefulSet {} not found, skip delete", sts_name);
                Ok(())
            }
            Err(e) => Err(ContainerRuntimeError::K8sError(format!(
                "delete sts {}: {}",
                sts_name, e
            ))),
        }
    }
}
