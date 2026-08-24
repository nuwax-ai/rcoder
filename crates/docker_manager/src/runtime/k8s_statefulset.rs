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

/// rcoder.io/template-hash 注解 key：创建时记录期望 PodSpec 的指纹，
/// ensure 时对比感知模板漂移（镜像/env/command/sidecar/资源等全部内容）。
pub(crate) const TEMPLATE_HASH_ANNOTATION: &str = "rcoder.io/template-hash";

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
    /// 顶层注解记录模板指纹——ensure 时对比感知漂移（镜像/env/command 等升级
    /// 后，存量 STS 不会自动更新模板，指纹差异是唯一可见信号）。
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
        let template_hash = agent_template_hash(&pod_spec);
        Ok(StatefulSet {
            metadata: ObjectMeta {
                name: Some(sts_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                annotations: Some(
                    [(TEMPLATE_HASH_ANNOTATION.to_string(), template_hash)]
                        .into_iter()
                        .collect(),
                ),
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
                let desired_ws_claim = workspace_claim_name(&pod_spec);
                let existing_ws_claim = existing
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.template.spec.as_ref())
                    .and_then(workspace_claim_name);
                if existing_st != Some(&service_type.to_string()) {
                    warn!(
                        "[K8S-STS] {} exists but service_type mismatch (existing={:?}, requested={:?}); recreating",
                        sts_name, existing_st, service_type
                    );
                    self.recreate_agent_statefulset(identifier, service_type, pod_spec, replicas)
                        .await?;
                } else if desired_ws_claim.is_some() && desired_ws_claim != existing_ws_claim {
                    // workspace 卷漂移（如 builder per-app PVC → 开发共享卷的拓扑变更）:
                    // STS template 不滚动更新, 不重建会与新代码定位分裂数据面（build 读不到源码）。
                    warn!(
                        "[K8S-STS] {} workspace PVC drift (existing={:?}, desired={:?}); recreating",
                        sts_name, existing_ws_claim, desired_ws_claim
                    );
                    self.recreate_agent_statefulset(identifier, service_type, pod_spec, replicas)
                        .await?;
                } else {
                    // 模板漂移可见化（第三维）：镜像/env/command 等模板内容升级后，
                    // 存量 STS 不会自动更新（STS 模板仅在创建时固化）——对比指纹
                    // 不一致时 warn 留痕，不主动重建（chat 路径 = 活跃会话，滚动
                    // 由 cleaner 的空闲换代路径负责）。存量无注解（功能上线前创建）
                    // 视为未知，不告警（避免升级后全量误报）。
                    let existing_hash = existing
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(TEMPLATE_HASH_ANNOTATION));
                    let desired_hash = agent_template_hash(&pod_spec);
                    if let Some(existing_hash) = existing_hash
                        && existing_hash != &desired_hash
                    {
                        warn!(
                            "[K8S-STS] {} template drift detected (existing_hash={}, desired_hash={}); keeping running pod — idle recycle will roll it",
                            sts_name, existing_hash, desired_hash
                        );
                    }
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

    /// 删旧重建 StatefulSet（service_type/卷漂移共用路径; PVC 数据不动）。
    async fn recreate_agent_statefulset(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        pod_spec: PodSpec,
        replicas: i32,
    ) -> ContainerRuntimeResult<()> {
        let sts_name = self.pod_name(identifier, service_type)?;
        self.delete_agent_statefulset(identifier, service_type)
            .await?;
        let sts = self.build_agent_statefulset(identifier, service_type, pod_spec, replicas)?;
        // Foreground 删除的对象要等 pod 全部终止（agent pod grace 15s + preStop）
        // 才真正消失，紧随的 create 会撞 409 AlreadyExists——按 PVC 同款模式
        // 限时重试（ensure_workspace_pvc 先例）
        let create_start = std::time::Instant::now();
        let max_create_wait = std::time::Duration::from_secs(60);
        loop {
            match self
                .statefulsets()
                .create(&PostParams::default(), &sts)
                .await
            {
                Ok(_) => break,
                Err(kube::Error::Api(ae)) if ae.code == 409 => {
                    if create_start.elapsed() > max_create_wait {
                        return Err(ContainerRuntimeError::K8sError(format!(
                            "recreate sts {sts_name}: still exists after {:.1}s of retries",
                            create_start.elapsed().as_secs_f64()
                        )));
                    }
                    warn!(
                        "[K8S-STS] {} still being deleted (409), retrying in 2s... (elapsed {:.1}s)",
                        sts_name,
                        create_start.elapsed().as_secs_f64()
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "recreate sts: {e}"
                    )));
                }
            }
        }
        info!("[K8S-STS] recreated {} (type={:?})", sts_name, service_type);
        Ok(())
    }

    /// 存量 agent STS 的容器镜像是否落后于当前进程期望（空闲滚动升级判据）。
    ///
    /// STS 模板仅在创建时固化，rcoder 升版后存量 agent 继续跑旧镜像；本方法
    /// 实读 STS 模板里 agent 容器的 image 与 [`Self::select_image`]（现读 env，
    /// 升版后自然携带新 tag）对比。404 视为无漂移（无 STS 即无换代需求）。
    pub(crate) async fn is_agent_image_drifted(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let sts = match self.statefulsets().get(&sts_name).await {
            Ok(sts) => sts,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(false),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "get sts {sts_name}: {e}"
                )));
            }
        };
        let existing_image = sts
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| spec.containers.first())
            .and_then(|c| c.image.clone());
        let desired_image = self.select_image(service_type);
        let drifted = existing_image
            .as_deref()
            .map(|img| img != desired_image)
            .unwrap_or(false);
        if drifted {
            info!(
                "[K8S-STS] {} image drifted: existing={:?}, desired={}",
                sts_name, existing_image, desired_image
            );
        }
        Ok(drifted)
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

/// 取 PodSpec 里 name=workspace 卷的 PVC claim 名（漂移检测用）。
fn workspace_claim_name(spec: &PodSpec) -> Option<String> {
    spec.volumes
        .as_ref()?
        .iter()
        .find(|v| v.name == "workspace")?
        .persistent_volume_claim
        .as_ref()
        .map(|p| p.claim_name.clone())
}

/// agent PodSpec 的规范化指纹（模板漂移检测）：serde_json 序列化经 Value 的
/// BTreeMap 字典序规范化（字段序/键序无关），再 DefaultHasher（与
/// config_hash_annotations 同款——跨进程确定、零新依赖）。涵盖镜像/env/
/// command/sidecar/资源等全部模板内容；build_agent_pod_spec 无时间/随机
/// 成分，同参数构造恒等。
fn agent_template_hash(pod_spec: &PodSpec) -> String {
    let canonical = serde_json::to_value(pod_spec)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hasher as _;
    hasher.write(canonical.as_bytes());
    format!("{:016x}", hasher.finish())
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;

    fn sample_pod_spec(image: &str) -> PodSpec {
        use k8s_openapi::api::core::v1::{Container, EnvVar};
        PodSpec {
            containers: vec![Container {
                name: "agent".to_string(),
                image: Some(image.to_string()),
                env: Some(vec![EnvVar {
                    name: "AGENT_MODE".to_string(),
                    value: Some("standard".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// 确定性：同参数两次构造（独立对象）指纹相等——跨副本/重启稳定是
    /// 漂移检测不误报的前提。
    #[test]
    fn template_hash_is_deterministic_for_same_input() {
        let a = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
        let b = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    /// 敏感性：镜像变更必须反映到指纹（升版检测的主场景）。
    #[test]
    fn template_hash_changes_when_image_changes() {
        let old = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.230"));
        let new = agent_template_hash(&sample_pod_spec("repo/rcoder:0.1.231"));
        assert_ne!(old, new);
    }

    /// 敏感性：非镜像字段（env）变更也必须反映（config 变更场景）。
    #[test]
    fn template_hash_changes_when_env_changes() {
        let mut spec = sample_pod_spec("repo/rcoder:0.1.230");
        let before = agent_template_hash(&spec);
        if let Some(env) = spec.containers[0].env.as_mut() {
            env[0].value = Some("advanced".to_string());
        }
        let after = agent_template_hash(&spec);
        assert_ne!(before, after);
    }
}
