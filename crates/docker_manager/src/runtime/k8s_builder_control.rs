//! UID/version-fenced builder compute controls; PVC and Service are untouched.
use super::{k8s_pod::K8sPodOps, kubernetes_runtime::KubernetesRuntime};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use k8s_openapi::api::{apps::v1::StatefulSet, core::v1::Pod};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams, Preconditions},
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderPodIdentity,
    ContainerBasicInfo, ServiceType, UserAppExecutionContext,
};
use std::time::Duration;

impl KubernetesRuntime {
    pub(super) async fn exec_bound_builder(
        &self,
        target: &BuilderControlTarget,
        command: Vec<String>,
    ) -> Result<container_runtime_api::ExecResult> {
        target.validate().map_err(Error::Conflict)?;
        let expected = target
            .pod
            .as_ref()
            .ok_or_else(|| Error::Conflict("Captured builder Pod is absent".into()))?;
        if command.is_empty() {
            return Err(Error::ConfigurationError(
                "Builder exec command is empty".into(),
            ));
        }
        let actual = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        if actual.workload.as_ref().map(|v| (&v.uid, &v.name))
            != target.workload.as_ref().map(|v| (&v.uid, &v.name))
            || actual.pod.as_ref().map(|v| (&v.uid, &v.name))
                != Some((&expected.uid, &expected.name))
        {
            return Err(Error::Conflict(
                "Captured builder exec identity changed".into(),
            ));
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = pods
            .get(&expected.name)
            .await
            .map_err(|error| api_error("Inspect builder exec Pod", error))?;
        if pod.metadata.uid.as_deref() != Some(expected.uid.as_str())
            || pod.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict("Builder Pod changed before exec".into()));
        }
        let container = pod
            .spec
            .as_ref()
            .and_then(|spec| spec.containers.iter().find(|v| v.name == "agent"))
            .ok_or_else(|| Error::Conflict("Builder agent container is missing".into()))?;
        let identities: Vec<_> = container
            .env
            .iter()
            .flatten()
            .filter(|v| v.name == "RCODER_PHYSICAL_POD_UID")
            .collect();
        if identities.len() != 1
            || identities[0].value.is_some()
            || !identities[0]
                .value_from
                .as_ref()
                .and_then(|v| v.field_ref.as_ref())
                .is_some_and(|v| v.field_path == "metadata.uid")
        {
            return Err(Error::Conflict(
                "Builder lacks its physical Pod identity; recreate before management writes".into(),
            ));
        }
        self.exec_pod_container(
            &expected.name,
            "agent",
            builder_exec_guard(&expected.uid, command),
        )
        .await
    }

    pub(super) async fn resume_bound_builder(
        &self,
        params: &container_runtime_api::ContainerCreateParams,
    ) -> Result<ContainerBasicInfo> {
        let target = async {
            let context = params.execution_context.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires execution context".into())
            })?;
            let binding = params.resource_binding.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires durable resource proof".into())
            })?;
            let target = self
                .capture_builder_compute_with_binding(context, Some(binding), false)
                .await?;
            let workload = target.workload.as_ref().ok_or_else(|| {
                Error::Conflict(
                    "Bound builder disappeared; automatic replacement is forbidden".into(),
                )
            })?;
            let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
            let current = api
                .get(&workload.name)
                .await
                .map_err(|error| api_error("Inspect bound builder configuration", error))?;
            if workload_identity_with_binding(&current, context, Some(binding), false)? != *workload
            {
                return Err(Error::Conflict(
                    "Bound builder changed during configuration validation".into(),
                ));
            }
            let desired =
                self.build_agent_pod_spec(&context.app_id, &ServiceType::UserappBuilder, params)?;
            let actual = current
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .ok_or_else(|| {
                    Error::ConfigurationError("Bound builder Pod template missing".into())
                })?;
            if !configured_fields_match(
                &serde_json::to_value(desired)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
                &serde_json::to_value(actual)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
            ) {
                return Err(Error::Conflict(
                    "Bound builder Pod configuration changed".into(),
                ));
            }
            Ok::<_, Error>(target)
        }
        .await
        .map_err(|error| rejected_before_write(error.to_string()))?;
        self.apply_builder_compute_mode(&target, true, true)
            .await?
            .ok_or_else(|| Error::Conflict("Bound builder did not return a running Pod".into()))
    }

    pub(super) async fn capture_builder_compute(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, None, false)
            .await
    }

    pub(super) async fn capture_builder_compute_with_binding(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
        adoption: bool,
    ) -> Result<BuilderControlTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::Conflict)?;
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let sts = match api.get(&name).await {
            Ok(sts) => sts,
            Err(kube::Error::Api(error)) if error.code == 404 => {
                return Ok(BuilderControlTarget {
                    resource_binding: None,
                    context: context.clone(),
                    workload: None,
                    pod: None,
                });
            }
            Err(error) => return Err(api_error("Capture builder workload", error)),
        };
        let workload = workload_identity_with_binding(&sts, context, binding, adoption)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_name = self.agent_pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let pod = match pods.get(&pod_name).await {
            Ok(pod) => Some(pod_identity(&pod, &workload)?),
            Err(kube::Error::Api(error)) if error.code == 404 => None,
            Err(error) => return Err(api_error("Capture builder pod", error)),
        };
        Ok(BuilderControlTarget {
            resource_binding: binding.cloned(),
            context: context.clone(),
            workload: Some(workload),
            pod,
        })
    }

    pub(super) async fn apply_builder_compute(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, restart, false)
            .await
    }

    /// 控制编排 + 关键阶段诊断事件（批次 C）：成功事件在真实结果确认后
    /// 发布；失败区分明确拒绝（RequestRejected/Conflict）与不可确认
    /// （Timeout/传输错误，对齐 RecoveryRequired 语义）。非阻塞 fire-and-
    /// forget，发布失败绝不改变控制结果。
    async fn apply_builder_compute_mode(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        let outcome = self
            .apply_builder_compute_mode_inner(target, restart, only_start)
            .await;
        self.report_control_outcome(target, restart, only_start, &outcome);
        outcome
    }

    fn report_control_outcome(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
        outcome: &Result<Option<ContainerBasicInfo>>,
    ) {
        let Some(workload) = &target.workload else {
            return;
        };
        let action = match (restart, only_start) {
            (_, true) => "WakeCompute",
            (true, false) => "RestartCompute",
            (false, false) => "StopCompute",
        };
        let (type_, reason) = match outcome {
            Ok(None) => (
                super::k8s_event_publisher::DiagnosticEventType::Normal,
                "ComputeStopped",
            ),
            Ok(Some(_)) => (
                super::k8s_event_publisher::DiagnosticEventType::Normal,
                "ComputeStarted",
            ),
            Err(Error::RequestRejected(_) | Error::Conflict(_)) => (
                super::k8s_event_publisher::DiagnosticEventType::Warning,
                "ControlRejected",
            ),
            Err(_) => (
                super::k8s_event_publisher::DiagnosticEventType::Warning,
                "ControlUncertain",
            ),
        };
        let detail = match outcome {
            Ok(None) => "stopped".to_string(),
            Ok(Some(info)) => format!("pod={}", info.container_id),
            Err(error) => format!("{error}"),
        };
        let note = format!(
            "app={} lifecycle={} operation={} {detail}",
            target.context.app_id, target.context.lifecycle_id, target.context.operation_id
        );
        self.event_publisher
            .publish(super::k8s_event_publisher::DiagnosticEvent::new(
                type_,
                reason,
                action,
                note,
                k8s_openapi::api::core::v1::ObjectReference {
                    api_version: Some("apps/v1".into()),
                    kind: Some("StatefulSet".into()),
                    name: Some(workload.name.clone()),
                    namespace: Some(self.namespace.clone()),
                    uid: Some(workload.uid.clone()),
                    ..Default::default()
                },
            ));
    }

    async fn apply_builder_compute_mode_inner(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(rejected_before_write)?;
        let Some(workload) = &target.workload else {
            return Ok(None);
        };
        if workload.kind != AppResourceKind::StatefulSet {
            return Err(rejected_before_write(
                "Non-Kubernetes builder control target".into(),
            ));
        }
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = sts_api
            .get(&workload.name)
            .await
            .map_err(|error| rejected_before_write(format!("Verify builder workload: {error}")))?;
        if workload_identity_with_binding(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )
        .map_err(|error| rejected_before_write(error.to_string()))?
            != *workload
        {
            return Err(rejected_before_write(
                "Builder workload changed after capture".into(),
            ));
        }
        if restart && only_start {
            let replicas = current
                .spec
                .as_ref()
                .and_then(|spec| spec.replicas)
                .unwrap_or(1);
            if !matches!(replicas, 0 | 1) {
                return Err(rejected_before_write(
                    "Bound builder has unexpected replicas".into(),
                ));
            }
            if replicas == 0 {
                sts_api.patch(&workload.name, &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({
                        "metadata": {"uid": workload.uid, "resourceVersion": workload.resource_version},
                        "spec": {"replicas": 1}
                    }))).await.map_err(|error| super::builder_completion::k8s_error(
                        format!("Wake captured builder workload: {error}"), error))?;
            }
        } else if restart {
            let pod = target.pod.as_ref().ok_or_else(|| {
                rejected_before_write("Builder restart requires a captured pod".into())
            })?;
            let actual = pods
                .get(&pod.name)
                .await
                .map_err(|error| rejected_before_write(format!("Verify restart pod: {error}")))?;
            if pod_identity(&actual, workload)
                .map_err(|error| rejected_before_write(error.to_string()))?
                != *pod
            {
                return Err(rejected_before_write(
                    "Builder pod changed after capture".into(),
                ));
            }
            pods.delete(&pod.name, &pod_delete_params(pod))
                .await
                .map_err(|error| {
                    super::builder_completion::k8s_error(
                        format!("Restart captured builder pod: {error}"),
                        error,
                    )
                })?;
        } else {
            sts_api
                .patch(
                    &workload.name,
                    &PatchParams::default(),
                    &Patch::Merge(stop_patch(workload)),
                )
                .await
                .map_err(|error| {
                    super::builder_completion::k8s_error(
                        format!("Stop captured builder workload: {error}"),
                        error,
                    )
                })?;
        }
        // Only observation is time bounded. A timeout never authorizes lease
        // release; the caller records an uncertain operation for reconciliation.
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let pod_name = self.agent_pod_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
        // 批次 B（plan §3）：STS + Pod 双资源 watch 观察——两条流共享总
        // deadline/退避边界；身份/replicas 冲突快速失败；完成候选由下方
        // 最后 GET 复核（观察不提供跨对象事务，也不授权任何写或租约释放，
        // KR06）。同一 Pod 删除后重现（同 UID）回到观察，不放大预算。
        loop {
            let observed = {
                let workload = workload.clone();
                let sts_name = workload.name.clone();
                let context = target.context.clone();
                let binding = target.resource_binding.clone();
                let captured_pod = target.pod.clone();
                match super::k8s_observation::await_builder_verdict(
                    &sts_api,
                    &sts_name,
                    &pods,
                    &pod_name,
                    deadline,
                    tokio_util::sync::CancellationToken::new(),
                    move |event| {
                        builder_verdict(
                            event,
                            &workload,
                            &context,
                            binding.as_ref(),
                            captured_pod.as_ref(),
                            restart,
                            only_start,
                        )
                    },
                )
                .await
                {
                    Ok(super::k8s_observation::Verdict::Complete(outcome)) => outcome,
                    Ok(super::k8s_observation::Verdict::Rejected(reason)) => {
                        return Err(Error::Conflict(reason));
                    }
                    Ok(super::k8s_observation::Verdict::Pending) => {
                        return Err(Error::K8sError(
                            "Builder observation ended while pending".into(),
                        ));
                    }
                    Err(error) => return Err(observation_error(error)),
                }
            };
            match observed {
                BuilderObservation::Stopped => {
                    // K04：最终权威复核共用观察的绝对 deadline——接近截止时
                    // 出现的候选不得在复核阶段无界等待（GET hang/慢响应由
                    // 剩余预算截断，超时按未知结果交上层保护，不当作缺席/稳定）。
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::K8sError(
                            "Builder stop verification budget exhausted".into(),
                        ));
                    }
                    // 复核①：Pod 确已消失。同 UID 重现回到观察（STS 控制器
                    // 仍在终止窗口），异 UID 视为替换冲突。
                    match pods.get(&pod_name).await {
                        Err(kube::Error::Api(error)) if error.code == 404 => {}
                        Err(error) => {
                            return Err(api_error("Verify stopped builder pod", error));
                        }
                        Ok(pod) => {
                            let seen = pod_identity(&pod, workload)
                                .map_err(|error| Error::Conflict(error.to_string()))?;
                            if target.pod.as_ref().is_some_and(|old| old.uid == seen.uid) {
                                continue;
                            }
                            return Err(Error::Conflict(
                                "A replacement builder pod appeared while stopping".into(),
                            ));
                        }
                    }
                    // 复核②：STS 身份未替换且 replicas 保持 0。STS 整体消失
                    // （被带外删除）显式归类为冲突（K01）——捕获身份已不存在，
                    // 不当作成功也不混入通用后端错误。K04：同样受绝对 deadline
                    // 约束（复核①耗尽剩余预算时不再进入无界 GET）。
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::K8sError(
                            "Builder stop verification budget exhausted".into(),
                        ));
                    }
                    let current = match sts_api.get(&workload.name).await {
                        Ok(current) => current,
                        Err(kube::Error::Api(error)) if error.code == 404 => {
                            return Err(Error::Conflict(
                                "Builder workload vanished while stopping".into(),
                            ));
                        }
                        Err(error) => {
                            return Err(api_error("Verify stopped builder workload", error));
                        }
                    };
                    verify_workload_stable(
                        &current,
                        &target.context,
                        target.resource_binding.as_ref(),
                        workload,
                        Some(0),
                    )
                    // K02：写后复核失败不得归类为写前拒绝——patch 已落盘，
                    // RequestRejected 会让上层释放 mutating 并记 Failed，
                    // 丢失未知结果保护。复核冲突 = 不确定 → Conflict（上层
                    // 保留保护，操作转 RecoveryRequired）
                    .map_err(Error::Conflict)?;
                    return Ok(None);
                }
                BuilderObservation::Ready(boxed) => {
                    let (info, captured) = *boxed;
                    // K04：Ready 复核共用绝对 deadline（同上，不无界等待）
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::K8sError(
                            "Builder readiness verification budget exhausted".into(),
                        ));
                    }
                    // 复核①：完成候选 Pod 仍是同一物理对象（UID 核验）。
                    let pod = pods
                        .get(&pod_name)
                        .await
                        .map_err(|error| api_error("Verify ready builder pod", error))?;
                    let seen = pod_identity(&pod, workload)
                        .map_err(|error| Error::Conflict(error.to_string()))?;
                    if seen.uid != captured.uid {
                        return Err(Error::Conflict(
                            "Builder pod changed between observation and verification".into(),
                        ));
                    }
                    // 复核②：STS 身份未替换（wake 还须 replicas 保持 1）。
                    // K04：预算耗尽时不进入无界 GET。
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::K8sError(
                            "Builder readiness verification budget exhausted".into(),
                        ));
                    }
                    let current = sts_api
                        .get(&workload.name)
                        .await
                        .map_err(|error| api_error("Verify ready builder workload", error))?;
                    verify_workload_stable(
                        &current,
                        &target.context,
                        target.resource_binding.as_ref(),
                        workload,
                        only_start.then_some(1),
                    )
                    // K02：同上——写后（wake/restart 的 scale 写入已发生）复核
                    // 冲突保持未知结果保护，不当作写前拒绝
                    .map_err(Error::Conflict)?;
                    return Ok(Some(info));
                }
            }
        }
    }
}

/// 双流观察的业务判定产物（分类闭包的 T）。
#[derive(Clone)]
enum BuilderObservation {
    /// stop 完成：Pod 消失。
    Stopped,
    /// Pod 就绪：基本信息 + 观察到它时的物理身份（供最后复核比对）。
    Ready(Box<(ContainerBasicInfo, BuilderPodIdentity)>),
}

/// 就绪 Pod → 运行信息（Ready 条件已由调用方核验）。
fn ready_builder_info(
    pod: &Pod,
    workload: &AppResourceIdentity,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> std::result::Result<BuilderObservation, String> {
    let endpoint = super::k8s_builder_deletion::workspace_endpoint_from_bound_pod(
        pod, workload, context, binding,
    )
    .map_err(|error| error.to_string())?;
    let identity = pod_identity(pod, workload).map_err(|error| error.to_string())?;
    let created_at = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .ok_or_else(|| "Builder pod creation time is missing".to_string())?
        .0;
    let created_at = chrono::DateTime::from_timestamp(
        created_at.as_second(),
        created_at.subsec_nanosecond() as u32,
    )
    .ok_or_else(|| "Builder pod creation time is out of range".to_string())?;
    Ok(BuilderObservation::Ready(Box::new((
        ContainerBasicInfo {
            container_id: endpoint.container_id,
            container_name: identity.name.clone(),
            container_ip: endpoint.address.to_string(),
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            external_port: 0,
            project_id: context.app_id.clone(),
            status: "Running".into(),
            created_at,
            service_url: format!(
                "http://{}:{}",
                endpoint.address,
                shared_types::GRPC_DEFAULT_PORT
            ),
        },
        identity,
    ))))
}

/// 双流分类闭包：Err = 身份/配置冲突（Fatal 快速失败）；Ok(Pending) 继续
/// 观察；Ok(Complete) 给出完成候选（仍需调用方 GET 复核）。
fn builder_verdict(
    event: super::k8s_observation::BuilderWatchEvent<'_>,
    workload: &AppResourceIdentity,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    captured_pod: Option<&BuilderPodIdentity>,
    restart: bool,
    only_start: bool,
) -> std::result::Result<super::k8s_observation::Verdict<BuilderObservation>, String> {
    match event {
        super::k8s_observation::BuilderWatchEvent::Sts(current) => {
            let identity = workload_identity_with_binding(current, context, binding, false)
                .map_err(|error| error.to_string())?;
            if identity.uid != workload.uid {
                return Err("Builder workload replaced during control".into());
            }
            if only_start
                && current
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.replicas)
                    .unwrap_or(1)
                    != 1
            {
                return Err("Builder replicas changed while waking".into());
            }
            if !restart && current.spec.as_ref().and_then(|spec| spec.replicas) != Some(0) {
                return Err("Builder replicas changed while stopping".into());
            }
            Ok(super::k8s_observation::Verdict::Pending)
        }
        super::k8s_observation::BuilderWatchEvent::PodAbsent => {
            if restart {
                // 旧 Pod 删除是 restart 的前置，等待新 Pod 就绪。
                Ok(super::k8s_observation::Verdict::Pending)
            } else {
                Ok(super::k8s_observation::Verdict::Complete(
                    BuilderObservation::Stopped,
                ))
            }
        }
        super::k8s_observation::BuilderWatchEvent::Pod(pod) => {
            let observed = pod_identity(pod, workload).map_err(|error| error.to_string())?;
            if restart {
                if !only_start && captured_pod.is_some_and(|old| old.uid == observed.uid) {
                    // 旧 Pod 仍在终止窗口——等新 Pod。
                    return Ok(super::k8s_observation::Verdict::Pending);
                }
                if pod.metadata.deletion_timestamp.is_none()
                    && pod
                        .status
                        .as_ref()
                        .and_then(|status| status.conditions.as_ref())
                        .is_some_and(|conditions| {
                            conditions.iter().any(|condition| {
                                condition.type_ == "Ready" && condition.status == "True"
                            })
                        })
                {
                    return ready_builder_info(pod, workload, context, binding)
                        .map(super::k8s_observation::Verdict::Complete);
                }
                Ok(super::k8s_observation::Verdict::Pending)
            } else if captured_pod.is_some_and(|old| old.uid != observed.uid) {
                Err("A replacement builder pod appeared while stopping".into())
            } else {
                Ok(super::k8s_observation::Verdict::Pending)
            }
        }
    }
}

/// 完成候选后的 STS 复核：身份未替换；`require_replicas` 给出动作方向
/// （stop=0 / wake=1；restart 不约束 replicas）。
fn verify_workload_stable(
    current: &StatefulSet,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    workload: &AppResourceIdentity,
    require_replicas: Option<i32>,
) -> std::result::Result<(), String> {
    let identity = workload_identity_with_binding(current, context, binding, false)
        .map_err(|error| error.to_string())?;
    if identity.uid != workload.uid {
        return Err("Builder workload replaced during control".into());
    }
    if let Some(required) = require_replicas
        && current.spec.as_ref().and_then(|spec| spec.replicas) != Some(required)
    {
        return Err("Builder replicas changed after control".into());
    }
    Ok(())
}

/// 观察层结构化错误 → 运行时错误（超时不授权租约释放，仅报告不可确认）。
fn observation_error(error: super::k8s_observation::ObservationError) -> Error {
    match error {
        super::k8s_observation::ObservationError::Deadline { .. } => {
            Error::Timeout("Builder compute confirmation timed out; reconciliation required".into())
        }
        super::k8s_observation::ObservationError::Cancelled => {
            Error::Timeout("Builder compute observation cancelled".into())
        }
        super::k8s_observation::ObservationError::Fatal { code, message } => {
            Error::K8sError(format!("Builder observation failed ({code:?}): {message}"))
        }
        super::k8s_observation::ObservationError::StreamEnded { message } => {
            Error::K8sError(format!("Builder observation stream ended: {message}"))
        }
    }
}

/// Kubernetes may populate omitted defaults, but every explicitly requested
/// field and ordered list entry must match. Sidecars/config changes are rejected.
fn configured_fields_match(desired: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (desired, actual) {
        (serde_json::Value::Object(expected), serde_json::Value::Object(observed)) => {
            expected.iter().all(|(key, value)| {
                observed
                    .get(key)
                    .is_some_and(|actual| configured_fields_match(value, actual))
            })
        }
        (serde_json::Value::Array(expected), serde_json::Value::Array(observed)) => {
            expected.len() == observed.len()
                && expected
                    .iter()
                    .zip(observed)
                    .all(|(value, actual)| configured_fields_match(value, actual))
        }
        _ => desired == actual,
    }
}

fn rejected_before_write(message: String) -> Error {
    Error::RequestRejected(shared_types::RuntimeRequestRejection {
        status: 409,
        message,
    })
}

fn stop_patch(workload: &AppResourceIdentity) -> serde_json::Value {
    serde_json::json!({"metadata":{"uid":workload.uid,"resourceVersion":workload.resource_version},"spec":{"replicas":0}})
}
fn pod_delete_params(pod: &BuilderPodIdentity) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(pod.uid.clone()),
            resource_version: Some(pod.resource_version.clone()),
        }),
        ..Default::default()
    }
}
fn required(value: Option<&str>, field: &str) -> Result<String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::ConfigurationError(format!("Builder {field} is missing")))
}
#[cfg(test)]
fn workload_identity(
    sts: &StatefulSet,
    context: &UserAppExecutionContext,
) -> Result<AppResourceIdentity> {
    workload_identity_with_binding(sts, context, None, false)
}

/// 语义契约服务器场景：object/ready_pod 为集群状态模板，标志位决定动作
/// 分支，`patched` 记录成功的 STS PATCH（此后单对象 GET 反映动作后 replicas）。
#[cfg(test)]
#[derive(Clone)]
struct ContractScenario {
    object: serde_json::Value,
    ready_pod: serde_json::Value,
    reject_patch: bool,
    restart: bool,
    wake: bool,
    replaced: bool,
    patched: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    recorder: std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
}

#[cfg(test)]
impl ContractScenario {
    fn post_action_replicas(&self) -> i64 {
        if self.wake { 1 } else { 0 }
    }
}

#[cfg(test)]
async fn handle_contract_connection(mut stream: tokio::net::TcpStream, scenario: ContractScenario) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut headers = String::new();
    let mut body = Vec::new();
    loop {
        let n = stream.read(&mut buffer).await.expect("read");
        if n == 0 {
            // 连接池探测/复用半关闭——无请求，丢弃该连接
            headers.clear();
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().expect("length"))
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                headers = head;
                body = bytes[end + 4..end + 4 + length].to_vec();
                break;
            }
        }
    }
    if headers.is_empty() {
        return;
    }
    let first = headers.lines().next().expect("request line");
    assert!(
        !first.contains("persistentvolumeclaims") && !first.contains("/services"),
        "storage/service mutation is fenced: {first}"
    );
    scenario.recorder.lock().expect("recorder").push((
        first.to_string(),
        headers.clone(),
        String::from_utf8_lossy(&body).to_string(),
    ));

    let is_watch = first.contains("watch=true");
    let (method, path_query) = {
        let mut parts = first.split_whitespace();
        (
            parts.next().expect("method").to_string(),
            parts.next().expect("path").to_string(),
        )
    };
    let path = path_query.split('?').next().expect("path").to_string();

    // WATCH 流：chunked 开头 + 按场景投递一个触发事件后保持连接
    if is_watch {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .expect("watch header");
        let event = if path.ends_with("/statefulsets") {
            // STS 流：投递当前身份（identity 闸门验证；replicas 已按场景推进）
            let mut observed = scenario.object.clone();
            observed["spec"]["replicas"] = serde_json::json!(scenario.post_action_replicas());
            Some(serde_json::json!({"type":"MODIFIED","object":observed}))
        } else if scenario.restart || scenario.wake {
            // restart/wake：新 ready Pod 上线（name=agent_pod_name 派生名——
            // watcher 按名单投递；uid ready-pod 与旧 pod-original 区分新旧）
            let mut fresh = scenario.ready_pod.clone();
            fresh["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            fresh["metadata"]["uid"] = serde_json::json!("ready-pod");
            Some(serde_json::json!({"type":"ADDED","object":fresh}))
        } else {
            // stop：Pod 消失完成（DELETED——kube-runtime ListWatch 只对
            // 已入册对象投递 Delete，LIST 必须先含旧 Pod）
            let mut gone = scenario.ready_pod.clone();
            gone["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            gone["metadata"]["uid"] = serde_json::json!("pod-original");
            Some(serde_json::json!({"type":"DELETED","object":gone}))
        };
        if let Some(event) = event {
            let payload = format!("{}\n", event);
            let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
            stream
                .write_all(chunk.as_bytes())
                .await
                .expect("watch event");
        }
        let mut drain = [0u8; 512];
        loop {
            if stream.read(&mut drain).await.unwrap_or(0) == 0 {
                break;
            }
        }
        return;
    }

    let single_sts = path.ends_with("/statefulsets/builder");
    let list_sts = path.ends_with("/statefulsets");
    let single_pod = {
        let tail = path.rsplit('/').next().unwrap_or("");
        path.contains("/pods/") && !tail.is_empty()
    };
    let list_pods = path.ends_with("/pods");
    // 动作后状态：成功的 STS PATCH 之后，单对象 GET 反映推进的 replicas
    let patched = scenario.patched.load(std::sync::atomic::Ordering::SeqCst) > 0;

    let (code, response): (u16, serde_json::Value) = if method == "DELETE" {
        if scenario.reject_patch {
            // 写操作整体被拒（restart 的 DELETE 同 PATCH 一道受拒——
            // 拒绝必须传播为 RequestRejected，绝不静默成功）
            (
                403,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"Delete denied","code":403}),
            )
        } else {
            (
                200,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
            )
        }
    } else if method == "PATCH" {
        if scenario.reject_patch {
            (
                403,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"Patch denied","code":403}),
            )
        } else {
            scenario
                .patched
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut observed = scenario.object.clone();
            observed["spec"]["replicas"] = serde_json::json!(scenario.post_action_replicas());
            (200, observed)
        }
    } else if scenario.replaced && single_sts && !list_sts {
        let mut replacement = scenario.object.clone();
        replacement["metadata"]["uid"] = serde_json::json!("replacement-sts");
        (200, replacement)
    } else if single_sts {
        let mut observed = scenario.object.clone();
        if patched {
            observed["spec"]["replicas"] = serde_json::json!(scenario.post_action_replicas());
        }
        (200, observed)
    } else if list_sts {
        let mut observed = scenario.object.clone();
        observed["spec"]["replicas"] = serde_json::json!(scenario.post_action_replicas());
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"StatefulSetList","metadata":{"resourceVersion":"8"},"items":[observed]}),
        )
    } else if single_pod && path.ends_with("/builder-0") {
        // 旧 Pod（restart 删除前置的身份复核对象：name/uid/rv 必须与捕获一致）
        let mut old = scenario.ready_pod.clone();
        old["metadata"]["name"] = serde_json::json!("builder-0");
        old["metadata"]["uid"] = serde_json::json!("pod-original");
        old["metadata"]["resourceVersion"] = serde_json::json!("9");
        (200, old)
    } else if single_pod {
        if scenario.restart || scenario.wake {
            (200, scenario.ready_pod.clone())
        } else {
            (
                404,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Pod absent","code":404}),
            )
        }
    } else if list_pods {
        let items = if scenario.restart || scenario.wake {
            // restart/wake（apply 侧 restart=true）：旧 Pod 已删——空集，
            // 新 ready Pod 由 watch ADDED 事件驱动（KR07：旧 Pod Ready 不能
            // 完成新 restart）
            serde_json::json!([])
        } else {
            // stop：旧 Pod 在册（DELETED 事件才能被 ListWatch 识别）
            let mut current = scenario.ready_pod.clone();
            current["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            current["metadata"]["uid"] = serde_json::json!("pod-original");
            serde_json::json!([current])
        };
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"10"},"items":items}),
        )
    } else {
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
        )
    };
    let body = response.to_string();
    stream
        .write_all(
            format!(
                "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("respond");
}

fn workload_identity_with_binding(
    sts: &StatefulSet,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    adoption: bool,
) -> Result<AppResourceIdentity> {
    if sts.metadata.deletion_timestamp.is_some() {
        return Err(Error::Conflict("Builder workload is deleting".into()));
    }
    let labels = sts
        .metadata
        .labels
        .as_ref()
        .ok_or_else(|| Error::Conflict("Builder workload labels missing".into()))?;
    if labels.get("rcoder.io/service-type").map(String::as_str)
        != Some(ServiceType::UserappBuilder.to_string().as_str())
        || labels.get("rcoder.io/identifier").map(String::as_str) != Some(context.app_id.as_str())
    {
        return Err(Error::Conflict(
            "Builder workload ownership mismatch".into(),
        ));
    }
    let metadata = sts.metadata.annotations.clone().unwrap_or_default();
    let uid = required(sts.metadata.uid.as_deref(), "workload UID")?;
    if !shared_types::builder_identity_is_bound(context, &metadata, &uid, binding)
        .map_err(Error::Conflict)?
        && !adoption
    {
        return Err(Error::Conflict(
            "Builder requires explicit physical resource adoption".into(),
        ));
    }
    Ok(AppResourceIdentity {
        kind: AppResourceKind::StatefulSet,
        name: required(sts.metadata.name.as_deref(), "workload name")?,
        uid: required(sts.metadata.uid.as_deref(), "workload UID")?,
        resource_version: Some(required(
            sts.metadata.resource_version.as_deref(),
            "workload version",
        )?),
    })
}
fn pod_identity(pod: &Pod, workload: &AppResourceIdentity) -> Result<BuilderPodIdentity> {
    if !pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners.iter().any(|owner| {
                owner.controller == Some(true)
                    && owner.api_version == "apps/v1"
                    && owner.kind == "StatefulSet"
                    && owner.name == workload.name
                    && owner.uid == workload.uid
            })
        })
    {
        return Err(Error::Conflict(
            "Builder pod belongs to another workload".into(),
        ));
    }
    Ok(BuilderPodIdentity {
        name: required(pod.metadata.name.as_deref(), "pod name")?,
        uid: required(pod.metadata.uid.as_deref(), "pod UID")?,
        resource_version: required(pod.metadata.resource_version.as_deref(), "pod version")?,
    })
}
fn api_error(context: &str, error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 409) {
        Error::Conflict(format!("{context}: {error}"))
    } else {
        Error::K8sError(format!("{context}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bound_wake_accepts_api_defaults_but_rejects_changed_container_or_storage() {
        let expected = serde_json::json!({
            "containers": [{"name":"builder", "image":"builder:verified", "volumeMounts":[{"name":"workspace","mountPath":"/workspace"}]}],
            "volumes": [{"name":"workspace", "persistentVolumeClaim":{"claimName":"original-pvc"}}]
        });
        let mut actual = expected.clone();
        actual["restartPolicy"] = serde_json::json!("Always");
        actual["containers"][0]["imagePullPolicy"] = serde_json::json!("IfNotPresent");
        assert!(configured_fields_match(&expected, &actual));
        for replacement in [
            serde_json::json!({"containers": [{"name":"builder", "image":"builder:other"}]}),
            serde_json::json!({"containers": []}),
        ] {
            assert!(!configured_fields_match(&expected, &replacement));
        }
        actual["volumes"][0]["persistentVolumeClaim"]["claimName"] =
            serde_json::json!("replacement-pvc");
        assert!(!configured_fields_match(&expected, &actual));
        actual = expected.clone();
        actual["containers"]
            .as_array_mut()
            .expect("containers")
            .push(serde_json::json!({"name":"injected-sidecar"}));
        assert!(!configured_fields_match(&expected, &actual));
    }

    #[tokio::test]
    async fn actual_stop_patch_is_fenced_and_never_touches_storage() {
        stop_api_contract(false, false, false, false).await;
        stop_api_contract(true, false, false, false).await;
        stop_api_contract(true, true, false, false).await;
    }

    #[tokio::test]
    async fn actual_bound_wake_fences_uid_and_version_and_preserves_storage() {
        stop_api_contract(false, false, true, false).await;
        stop_api_contract(true, false, true, false).await;
        stop_api_contract(false, false, true, true).await;
    }

    async fn stop_api_contract(reject_patch: bool, restart: bool, wake: bool, replaced: bool) {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "stop".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut object = serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"builder","uid":"sts-original","resourceVersion":"8","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"},"annotations":context.resource_metadata()},"spec":{"replicas":1,"serviceName":"builder","selector":{"matchLabels":{}},"template":{"metadata":{},"spec":{"containers":[]}}}});
        if wake {
            object["spec"]["replicas"] = serde_json::json!(0);
        }
        let mut ready_pod = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"rcoder-app-builder-app-0","uid":"ready-pod","resourceVersion":"10","creationTimestamp":"2026-01-01T00:00:00Z","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"},"annotations":context.resource_metadata(),"ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]},"status":{"phase":"Running","podIP":"10.0.0.9","conditions":[{"type":"Ready","status":"True"}]}});
        if wake {
            object["metadata"]["annotations"] = serde_json::json!({});
            object["spec"]["template"]["spec"]["containers"] =
                serde_json::json!([{"name":"agent","env":[{"name":"USER_ID","value":"owner"}]}]);
            ready_pod["metadata"]["annotations"] = serde_json::json!({});
            ready_pod["spec"] = serde_json::json!({"containers":[{"name":"agent","env":[{"name":"USER_ID","value":"owner"}]}]});
        }
        let workload = workload_identity_with_binding(
            &serde_json::from_value(object.clone()).expect("workload"),
            &context,
            None,
            wake,
        )
        .expect("identity");
        if replaced {
            object["metadata"]["uid"] = serde_json::json!("replacement-sts");
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let recorded =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String, String)>::new()));
        let scenario = ContractScenario {
            object: object.clone(),
            ready_pod: ready_pod.clone(),
            reject_patch,
            restart,
            wake,
            replaced,
            patched: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recorder: recorded.clone(),
        };
        // 批次 B：语义分类服务器——按 method/path/query 分类应答（单对象 GET /
        // 集合 LIST / watch 流 / PATCH / DELETE），watch 流按场景投递触发事件
        //（wake/restart=新 ready Pod ADDED、stop=Pod DELETED）。每连接独立
        // task——watch 长连接不能阻塞 accept；连接 task detach（语义断言在
        // 主任务对 recorder 的复核里，服务器由 abort 终止）。
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(_) => return,
                };
                tokio::spawn(handle_contract_connection(stream, scenario.clone()));
            }
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().expect("uri"),
        ))
        .expect("client");
        let runtime = KubernetesRuntime {
            client,
            namespace: "review-test".into(),
            config: super::super::kubernetes_runtime::KubernetesRuntimeConfig {
                namespace: "review-test".into(),
                cluster_domain: "cluster.local".into(),
                pod_ttl_seconds: None,
                image_pull_secret: None,
                service_account_name: "test".into(),
                nfs_server: "unused".into(),
                nfs_path: "/unused".into(),
                storage_class: "unused".into(),
                access_mode: "ReadWriteOnce".into(),
                docker_manager_config: Default::default(),
                kubernetes_config: Default::default(),
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
            event_publisher: Default::default(),
            event_counters: std::sync::Arc::new(
                crate::runtime::k8s_event_publisher::PublisherCounters::default(),
            ),
        };
        let target = BuilderControlTarget {
            resource_binding: wake.then(|| shared_types::UserAppResourceBinding {
                app_id: "app".into(),
                lifecycle_id: "life".into(),
                service_type: ServiceType::UserappBuilder,
                physical_uid: "sts-original".into(),
                adopted_by_operation: "adopt".into(),
            }),
            context,
            workload: Some(workload),
            pod: restart.then(|| BuilderPodIdentity {
                name: "builder-0".into(),
                uid: "pod-original".into(),
                resource_version: "9".into(),
            }),
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let outcome = runtime
                .apply_builder_compute_mode(&target, restart || wake, wake)
                .await;
            if reject_patch || replaced {
                assert!(
                    matches!(outcome, Err(Error::RequestRejected(_))),
                    "expected rejection, got: {outcome:?}"
                );
            } else if wake {
                assert_eq!(
                    outcome.expect("wake").expect("ready").container_id,
                    "ready-pod"
                );
            } else if restart {
                assert_eq!(
                    outcome.expect("restart").expect("ready").container_id,
                    "ready-pod"
                );
            } else {
                assert!(outcome.expect("stop").is_none());
            }
            // 语义复核：写操作必须按场景出现且携带物理前置；绝无存储/服务变更
            let requests = recorded.lock().expect("recorder").clone();
            assert!(!requests.is_empty(), "no requests were recorded");
            let mut sts_patch: Option<(String, String, String)> = None;
            let mut pod_delete: Option<(String, String, String)> = None;
            for request in &requests {
                let lower = request.0.to_ascii_lowercase();
                assert!(
                    !lower.contains("persistentvolumeclaims") && !lower.contains("/services"),
                    "storage/service mutation is fenced: {request:?}"
                );
                if lower.starts_with("patch ") && lower.contains("/statefulsets/builder") {
                    assert!(
                        sts_patch.is_none(),
                        "workload patched at most once: {requests:?}"
                    );
                    sts_patch = Some(request.clone());
                }
                if lower.starts_with("delete ") && lower.contains("/pods/") {
                    assert!(
                        pod_delete.is_none(),
                        "pod deleted at most once: {requests:?}"
                    );
                    pod_delete = Some(request.clone());
                }
            }
            if replaced {
                // 替换的 STS 在任何写之前被拒——绝无写操作
                assert!(
                    sts_patch.is_none() && pod_delete.is_none(),
                    "replacement must be fenced before any write: {requests:?}"
                );
            } else if wake || !restart {
                let (first, _, body) = sts_patch.expect("workload patch required").clone();
                assert!(first.contains("PATCH"), "recorded: {first}");
                assert!(
                    body.contains("\"uid\":\"sts-original\""),
                    "patch carries UID precondition: {body}"
                );
                assert!(
                    body.contains("\"resourceVersion\":\"8\""),
                    "patch carries version precondition: {body}"
                );
                assert!(
                    body.contains(&format!("\"replicas\":{}", if wake { 1 } else { 0 })),
                    "patch scales in the action direction: {body}"
                );
                assert!(
                    !body.contains("volumes") && !body.contains("persistentVolumeClaim"),
                    "patch must not touch storage: {body}"
                );
                assert!(
                    pod_delete.is_none(),
                    "stop/wake never deletes the pod: {requests:?}"
                );
            } else {
                let (first, _, body) = pod_delete.expect("pod delete required").clone();
                assert!(first.contains("DELETE"), "recorded: {first}");
                assert!(
                    first.contains("/pods/builder-0"),
                    "delete targets the captured pod: {first}"
                );
                let params: serde_json::Value =
                    serde_json::from_str(&body).expect("delete body json");
                assert_eq!(
                    params["preconditions"]["uid"], "pod-original",
                    "delete carries UID precondition: {body}"
                );
                assert_eq!(
                    params["preconditions"]["resourceVersion"], "9",
                    "delete carries version precondition: {body}"
                );
                assert!(
                    sts_patch.is_none(),
                    "restart never patches the workload: {requests:?}"
                );
            }
            // 服务器是常驻 accept 循环——显式中止并确认未 panic
            server.abort();
            match server.await {
                Err(join) if join.is_cancelled() => {}
                other => panic!("contract server task must end aborted: {other:?}"),
            }
        })
        .await
        .expect("total contract deadline");
    }

    #[test]
    fn workload_and_pod_identity_reject_replacement_ownership() {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "stop".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let sts: StatefulSet = serde_json::from_value(serde_json::json!({"metadata":{"name":"builder", "uid":"sts-original", "resourceVersion":"8", "labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"}, "annotations":context.resource_metadata()}})).expect("workload");
        let identity = workload_identity(&sts, &context).expect("identity");
        let mut replacement = context.clone();
        replacement.lifecycle_id = "replacement".into();
        assert!(workload_identity(&sts, &replacement).is_err());
        let mut pod: Pod = serde_json::from_value(serde_json::json!({"metadata":{"name":"builder-0","uid":"pod-original","resourceVersion":"9","ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]}})).expect("pod");
        assert!(pod_identity(&pod, &identity).is_ok());
        pod.metadata.owner_references.as_mut().expect("owners")[0].uid = "replacement-sts".into();
        assert!(pod_identity(&pod, &identity).is_err());
    }

    #[test]
    fn stop_and_restart_requests_carry_physical_preconditions() {
        let workload = AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "builder".into(),
            uid: "original-sts".into(),
            resource_version: Some("17".into()),
        };
        let patch = stop_patch(&workload);
        assert_eq!(patch["metadata"]["uid"], "original-sts");
        assert_eq!(patch["metadata"]["resourceVersion"], "17");
        assert_eq!(patch["spec"]["replicas"], 0);
        let params = pod_delete_params(&BuilderPodIdentity {
            name: "builder-0".into(),
            uid: "original-pod".into(),
            resource_version: "24".into(),
        });
        let preconditions = params.preconditions.expect("preconditions");
        assert_eq!(preconditions.uid.as_deref(), Some("original-pod"));
        assert_eq!(preconditions.resource_version.as_deref(), Some("24"));
    }
}

fn builder_exec_guard(uid: &str, command: Vec<String>) -> Vec<String> {
    let mut guarded = vec!["sh".into(), "-c".into(),
        "if [ \"${RCODER_PHYSICAL_POD_UID:-}\" != \"$1\" ]; then printf '%s\\n' 'Builder physical identity changed' >&2; exit 125; fi; shift; exec \"$@\"".into(),
        "rcoder-builder-exec".into(), uid.into()];
    guarded.extend(command);
    guarded
}

#[cfg(all(test, unix))]
mod database_exec_tests {
    use super::builder_exec_guard;
    use std::process::Command;

    #[test]
    fn builder_exec_guard_fences_replacement_and_preserves_arguments() {
        let literal = "spaces ' quote $HOME $(touch must_not_execute)";
        let args = builder_exec_guard(
            "original",
            vec!["printf".into(), "%s".into(), literal.into()],
        );
        for (uid, success) in [
            (Some("original"), true),
            (Some("replacement"), false),
            (None, false),
        ] {
            let mut command = Command::new(&args[0]);
            command
                .args(&args[1..])
                .env_remove("RCODER_PHYSICAL_POD_UID");
            if let Some(uid) = uid {
                command.env("RCODER_PHYSICAL_POD_UID", uid);
            }
            let result = command.output().unwrap();
            assert_eq!(result.status.success(), success);
            if success {
                assert_eq!(result.stdout, literal.as_bytes());
            } else {
                assert_eq!(result.status.code(), Some(125));
                assert!(result.stdout.is_empty());
            }
        }
    }
}
