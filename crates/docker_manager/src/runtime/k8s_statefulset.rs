//! agent-runner StatefulSet 操作（K8s 原生 pod 级自愈）。
//!
//! agent-runner 由裸 Pod 改为 per-identifier StatefulSet（replicas 1）：
//! - pod 被 evict/删除/节点挂 → StatefulSet 控制器自动重建同名 pod（挂回同 PVC，数据不丢）；
//! - 容器级 OOM 仍由 restartPolicy=Always 原地重启（pod 模板继承）；
//! - stop/destroy = 删 STS + ClusterIP/headless svc（保留 PVC；下次 ensure 重建 STS 挂回同 PVC）。
//!
//! 仅 ComputerAgentRunner / WebAgentRunner 走此路径；Userapp 仍用 Deployment（create_deployment）。

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
    /// Builder creation never repairs an ownership/configuration conflict by
    /// deleting a workload. A conflicting POST re-reads and validates the winner.
    pub(crate) async fn ensure_builder_statefulset(
        &self,
        context: &shared_types::UserAppExecutionContext,
        pod_spec: PodSpec,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let family = ServiceType::UserappBuilder;
        let mut desired = self.build_agent_statefulset(&context.app_id, &family, pod_spec, 1)?;
        desired
            .metadata
            .annotations
            .get_or_insert_default()
            .extend(context.resource_metadata());
        let desired_spec = desired.spec.as_mut().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("Builder StatefulSet spec missing".into())
        })?;
        desired_spec
            .template
            .metadata
            .get_or_insert_default()
            .annotations
            .get_or_insert_default()
            .extend(context.resource_metadata());
        let name = self.pod_name(&context.app_id, &family)?;
        let api = self.statefulsets();
        let existing = match api.get_opt(&name).await {
            Ok(Some(existing)) => existing,
            Ok(None) => match api.create(&PostParams::default(), &desired).await {
                Ok(_) => return Ok(()),
                Err(kube::Error::Api(status)) if status.code == 409 => {
                    api.get(&name).await.map_err(|error| {
                        super::builder_completion::k8s_error(
                            format!("Read competing builder StatefulSet: {error}"),
                            error,
                        )
                    })?
                }
                Err(error) => {
                    return Err(super::builder_completion::k8s_error(
                        format!("Create builder StatefulSet: {error}"),
                        error,
                    ));
                }
            },
            Err(error) => {
                return Err(super::builder_completion::k8s_error(
                    format!("Read builder StatefulSet: {error}"),
                    error,
                ));
            }
        };
        validate_builder_statefulset(&existing, &desired, context)?;
        self.scale_captured_statefulset(&existing, &family, 1).await
    }

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
            .map_err(|e| {
                crate::runtime::builder_completion::k8s_error(
                    format!("create headless svc: {e}"),
                    e,
                )
            })?;
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
            Err(e) => Err(crate::runtime::builder_completion::k8s_error(
                format!("delete headless svc {}: {}", svc_name, e),
                e,
            )),
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
                    self.scale_captured_statefulset(&existing, service_type, replicas)
                        .await?;
                }
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                let sts =
                    self.build_agent_statefulset(identifier, service_type, pod_spec, replicas)?;
                sts_api
                    .create(&PostParams::default(), &sts)
                    .await
                    .map_err(|e| {
                        crate::runtime::builder_completion::k8s_error(format!("create sts: {e}"), e)
                    })?;
                info!(
                    "[K8S-STS] StatefulSet created: {} (replicas={}, type={:?})",
                    sts_name, replicas, service_type
                );
            }
            Err(e) => {
                return Err(crate::runtime::builder_completion::k8s_error(
                    format!("get sts {}: {}", sts_name, e),
                    e,
                ));
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
                        return Err(crate::runtime::builder_completion::k8s_error(
                            format!("recreate sts {sts_name}: conflict retry budget exhausted"),
                            kube::Error::Api(ae),
                        ));
                    }
                    warn!(
                        "[K8S-STS] {} still being deleted (409), retrying in 2s... (elapsed {:.1}s)",
                        sts_name,
                        create_start.elapsed().as_secs_f64()
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    return Err(crate::runtime::builder_completion::k8s_error(
                        format!("recreate sts: {e}"),
                        e,
                    ));
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
    pub(crate) async fn is_agent_image_drifted_inner(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        let sts_name = self.pod_name(identifier, service_type)?;
        let sts = match self.statefulsets().get(&sts_name).await {
            Ok(sts) => sts,
            Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(false),
            Err(e) => {
                return Err(crate::runtime::builder_completion::k8s_error(
                    format!("get sts {sts_name}: {e}"),
                    e,
                ));
            }
        };
        // 按容器名定位（不依赖位次）：build 时 agent 主容器在首位，防御
        // sidecar 列表构造演进导致的位次变化
        let existing_image = sts
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|c| c.name == "agent")
                    .and_then(|c| c.image.clone())
            });
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

    async fn scale_captured_statefulset(
        &self,
        existing: &StatefulSet,
        service_type: &ServiceType,
        replicas: i32,
    ) -> ContainerRuntimeResult<()> {
        let patch = conditional_scale_patch(existing, service_type, replicas)?;
        let sts_name = existing
            .metadata
            .name
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("StatefulSet name missing".into())
            })?;
        if existing.spec.as_ref().and_then(|spec| spec.replicas) == Some(replicas) {
            return Ok(());
        }
        self.statefulsets()
            .patch(sts_name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| {
                crate::runtime::builder_completion::k8s_error(
                    format!("scale sts {sts_name}: {e}"),
                    e,
                )
            })?;
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
            Err(e) => Err(crate::runtime::builder_completion::k8s_error(
                format!("delete sts {}: {}", sts_name, e),
                e,
            )),
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

/// agent PodSpec 的规范化指纹（模板漂移检测）：serde_json 序列化为规范文本
/// 后 DefaultHasher（与 config_hash_annotations 同款——跨进程确定、零新依赖）。
/// 确定性依据：结构体字段写出序固定（同版本二进制恒定；workspace 开
/// preserve_order 时 Value::Object 为 IndexMap 插入序=字段声明序，未开时为
/// BTreeMap 字典序——两种模式下同输入输出都稳定），k8s_openapi 的 map 字段
/// 本身是 BTreeMap 恒字典序。涵盖镜像/env/command/sidecar 等版本相关内容；
/// build_agent_pod_spec 无时间/随机成分，同参数构造恒等。
///
/// **per-request 字段剔除**：resources（用户可调资源限额）与 TENANT_ID/
/// SPACE_ID/ISOLATION_TYPE（请求携带时才注入）随请求抖动，混入指纹会让
/// 同版本的 ensure 对比误报 drift（参数噪声淹没版本信号）；这些字段的
/// 期望变更本来也不在滚动/重建语义内（ensure 恒不更新模板）。
fn validate_builder_statefulset(
    existing: &StatefulSet,
    desired: &StatefulSet,
    context: &shared_types::UserAppExecutionContext,
) -> ContainerRuntimeResult<()> {
    let conflict = |message: &str| ContainerRuntimeError::Conflict(message.into());
    let annotations = existing
        .metadata
        .annotations
        .as_ref()
        .ok_or_else(|| conflict("Builder StatefulSet requires identity adoption"))?;
    context
        .validate_resource_metadata(annotations)
        .map_err(ContainerRuntimeError::Conflict)?;
    let desired_hash = desired
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(TEMPLATE_HASH_ANNOTATION));
    if desired_hash.is_none() || annotations.get(TEMPLATE_HASH_ANNOTATION) != desired_hash {
        return Err(conflict("Builder StatefulSet configuration changed"));
    }
    let template = existing
        .spec
        .as_ref()
        .map(|spec| &spec.template)
        .ok_or_else(|| conflict("Builder StatefulSet template missing"))?;
    let identity = template
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.annotations.as_ref())
        .ok_or_else(|| conflict("Builder pod template identity missing"))?;
    context
        .validate_resource_metadata(identity)
        .map_err(ContainerRuntimeError::Conflict)?;
    let existing_pod = template
        .spec
        .as_ref()
        .ok_or_else(|| conflict("Builder pod spec missing"))?;
    let desired_pod = desired
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .ok_or_else(|| conflict("Desired builder pod spec missing"))?;
    if workspace_claim_name(existing_pod) != workspace_claim_name(desired_pod) {
        return Err(conflict("Builder workspace claim changed"));
    }
    // Inspect the actual launch fields too: an external patch can leave the
    // recorded template hash unchanged. Ignore API-defaulted probe/port fields.
    for desired_container in &desired_pod.containers {
        let actual = existing_pod
            .containers
            .iter()
            .find(|container| container.name == desired_container.name)
            .ok_or_else(|| conflict("Builder container missing"))?;
        if actual.image != desired_container.image
            || actual.command != desired_container.command
            || actual.args != desired_container.args
        {
            return Err(conflict("Builder container launch configuration changed"));
        }
    }
    Ok(())
}

fn conditional_scale_patch(
    existing: &StatefulSet,
    service_type: &ServiceType,
    replicas: i32,
) -> ContainerRuntimeResult<serde_json::Value> {
    if existing.metadata.deletion_timestamp.is_some()
        || existing
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(SERVICE_TYPE_LABEL))
            != Some(&service_type.to_string())
    {
        return Err(ContainerRuntimeError::Conflict(
            "StatefulSet ownership changed or deletion is in progress".into(),
        ));
    }
    let uid = existing
        .metadata
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("StatefulSet UID missing".into())
        })?;
    let version = existing
        .metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("StatefulSet resource version missing".into())
        })?;
    Ok(
        serde_json::json!({"metadata": {"uid": uid, "resourceVersion": version}, "spec": {"replicas": replicas}}),
    )
}

fn agent_template_hash(pod_spec: &PodSpec) -> String {
    let mut spec = pod_spec.clone();
    for container in &mut spec.containers {
        container.resources = None;
        if let Some(env) = &mut container.env {
            env.retain(|e| !matches!(e.name.as_str(), "TENANT_ID" | "SPACE_ID" | "ISOLATION_TYPE"));
        }
    }
    let canonical = serde_json::to_value(&spec)
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

    #[test]
    fn builder_reuse_requires_lifecycle_configuration_and_template_identity() {
        let context = shared_types::UserAppExecutionContext {
            app_id: "app-one".into(),
            user_id: "owner".into(),
            lifecycle_id: "life-one".into(),
            operation_id: "operation-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "ab".repeat(32),
        };
        let mut annotations = context.resource_metadata();
        annotations.insert(TEMPLATE_HASH_ANNOTATION.into(), "template-one".into());
        let desired = StatefulSet {
            metadata: ObjectMeta {
                annotations: Some(annotations.clone()),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        annotations: Some(annotations),
                        ..Default::default()
                    }),
                    spec: Some(sample_pod_spec("builder:one")),
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_builder_statefulset(&desired, &desired, &context).is_ok());
        let mut next_operation = context.clone();
        next_operation.operation_id = "operation-two".into();
        assert!(validate_builder_statefulset(&desired, &desired, &next_operation).is_ok());
        next_operation.lifecycle_id = "life-two".into();
        assert!(validate_builder_statefulset(&desired, &desired, &next_operation).is_err());
        let mut foreign = desired.clone();
        foreign.metadata.annotations = None;
        assert!(validate_builder_statefulset(&foreign, &desired, &context).is_err());
        foreign = desired.clone();
        foreign.spec.as_mut().unwrap().template.metadata = None;
        assert!(validate_builder_statefulset(&foreign, &desired, &context).is_err());
        foreign = desired.clone();
        foreign
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(TEMPLATE_HASH_ANNOTATION.into(), "other-template".into());
        assert!(validate_builder_statefulset(&foreign, &desired, &context).is_err());
    }

    #[test]
    fn scale_patch_keeps_captured_identity_and_rejects_unowned_resources() {
        let mut existing = StatefulSet {
            metadata: ObjectMeta {
                name: Some("builder-one".into()),
                uid: Some("physical-original".into()),
                resource_version: Some("17".into()),
                labels: Some(
                    [(
                        SERVICE_TYPE_LABEL.into(),
                        ServiceType::UserappBuilder.to_string(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        let patch = conditional_scale_patch(&existing, &ServiceType::UserappBuilder, 1).unwrap();
        assert_eq!(patch["metadata"]["uid"], "physical-original");
        assert_eq!(patch["metadata"]["resourceVersion"], "17");
        assert_eq!(patch["spec"]["replicas"], 1);
        assert!(conditional_scale_patch(&existing, &ServiceType::Userapp, 1).is_err());
        existing.metadata.resource_version = None;
        assert!(conditional_scale_patch(&existing, &ServiceType::UserappBuilder, 1).is_err());
    }

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

    /// per-request 字段豁免：resources 与 TENANT_ID/SPACE_ID/ISOLATION_TYPE 随
    /// 请求抖动，不得进入指纹（否则同版本 ensure 对比误报 drift，参数噪声
    /// 淹没版本信号）。
    #[test]
    fn template_hash_ignores_per_request_fields() {
        let base = sample_pod_spec("repo/rcoder:0.1.230");
        // 调资源限额
        let mut with_resources = base.clone();
        with_resources.containers[0].resources =
            Some(k8s_openapi::api::core::v1::ResourceRequirements {
                limits: Some([("cpu".to_string(), quantity("2"))].into_iter().collect()),
                ..Default::default()
            });
        assert_eq!(
            agent_template_hash(&base),
            agent_template_hash(&with_resources),
            "resources change must not affect template hash"
        );
        // 注入隔离 env
        let mut with_isolation = base.clone();
        if let Some(env) = with_isolation.containers[0].env.as_mut() {
            env.push(k8s_openapi::api::core::v1::EnvVar {
                name: "TENANT_ID".to_string(),
                value: Some("t1".to_string()),
                ..Default::default()
            });
        }
        assert_eq!(
            agent_template_hash(&base),
            agent_template_hash(&with_isolation),
            "isolation env must not affect template hash"
        );
    }

    fn quantity(v: &str) -> k8s_openapi::apimachinery::pkg::api::resource::Quantity {
        k8s_openapi::apimachinery::pkg::api::resource::Quantity(v.to_string())
    }
}
