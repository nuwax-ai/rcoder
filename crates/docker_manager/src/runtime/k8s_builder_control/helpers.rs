use super::*;

/// 双流观察的业务判定产物（分类闭包的 T）。
#[derive(Clone)]
pub(super) enum BuilderObservation {
    /// stop 完成：Pod 消失。
    Stopped,
    /// Pod 就绪：基本信息 + 观察到它时的物理身份（供最后复核比对）。
    Ready(Box<(ContainerBasicInfo, BuilderPodIdentity)>),
}

/// 就绪 Pod → 运行信息（Ready 条件已由调用方核验）。
pub(super) fn ready_builder_info(
    pod: &Pod,
    workload: &AppResourceIdentity,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    require_ready: bool,
) -> std::result::Result<BuilderObservation, String> {
    let endpoint =
        crate::runtime::k8s_builder_deletion::workspace_endpoint_from_bound_pod_with_readiness(
            pod,
            workload,
            context,
            binding,
            require_ready,
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
            // 契约一：container_name ≡ 稳定 workload 名（寻址基名）；物理
            // Pod 名保留在 BuilderPodIdentity，不再泄漏进注册表名。
            container_name: workload.name.clone(),
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
            // §1.1：workload UID（STS metadata.uid）与 Pod UID（container_id）
            // 一同捕获——契约二代次守卫与契约三对账按此判"同 workload"。
            workload_uid: Some(workload.uid.clone()),
        },
        identity,
    ))))
}

/// 双流分类闭包：Err = 身份/配置冲突（Fatal 快速失败）；Ok(Pending) 继续
/// 观察；Ok(Complete) 给出完成候选（仍需调用方 GET 复核）。
#[derive(Clone, Copy)]
pub(super) struct BuilderVerdictMode {
    pub(super) restart: bool,
    pub(super) only_start: bool,
    pub(super) require_ready: bool,
}

pub(super) fn builder_verdict(
    event: crate::runtime::k8s_observation::BuilderWatchEvent<'_>,
    workload: &AppResourceIdentity,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    captured_pod: Option<&BuilderPodIdentity>,
    expected_image: Option<&str>,
    mode: BuilderVerdictMode,
) -> std::result::Result<crate::runtime::k8s_observation::Verdict<BuilderObservation>, String> {
    match event {
        crate::runtime::k8s_observation::BuilderWatchEvent::Sts(current) => {
            let identity = workload_identity_with_binding(current, context, binding, false)
                .map_err(|error| error.to_string())?;
            if identity.uid != workload.uid {
                return Err("Builder workload replaced during control".into());
            }
            if mode.only_start
                && current
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.replicas)
                    .unwrap_or(1)
                    != 1
            {
                return Err("Builder replicas changed while waking".into());
            }
            if !mode.restart && current.spec.as_ref().and_then(|spec| spec.replicas) != Some(0) {
                return Err("Builder replicas changed while stopping".into());
            }
            Ok(crate::runtime::k8s_observation::Verdict::Pending)
        }
        crate::runtime::k8s_observation::BuilderWatchEvent::PodAbsent => {
            if mode.restart {
                // 旧 Pod 删除是 restart 的前置，等待新 Pod 就绪。
                Ok(crate::runtime::k8s_observation::Verdict::Pending)
            } else {
                Ok(crate::runtime::k8s_observation::Verdict::Complete(
                    BuilderObservation::Stopped,
                ))
            }
        }
        crate::runtime::k8s_observation::BuilderWatchEvent::Pod(pod) => {
            let observed = pod_identity(pod, workload).map_err(|error| error.to_string())?;
            if mode.restart {
                if !mode.only_start && captured_pod.is_some_and(|old| old.uid == observed.uid) {
                    // 旧 Pod 仍在终止窗口——等新 Pod。
                    return Ok(crate::runtime::k8s_observation::Verdict::Pending);
                }
                if pod.metadata.deletion_timestamp.is_none()
                    && pod_agent_image_matches(pod, expected_image)
                    && (if mode.require_ready {
                        pod.status
                            .as_ref()
                            .and_then(|status| status.conditions.as_ref())
                            .is_some_and(|conditions| {
                                conditions.iter().any(|condition| {
                                    condition.type_ == "Ready" && condition.status == "True"
                                })
                            })
                    } else {
                        builder_agent_running(pod)
                    })
                {
                    return ready_builder_info(pod, workload, context, binding, mode.require_ready)
                        .map(crate::runtime::k8s_observation::Verdict::Complete);
                }
                Ok(crate::runtime::k8s_observation::Verdict::Pending)
            } else if captured_pod.is_some_and(|old| old.uid != observed.uid) {
                Err("A replacement builder pod appeared while stopping".into())
            } else {
                Ok(crate::runtime::k8s_observation::Verdict::Pending)
            }
        }
    }
}

pub(super) fn builder_agent_running(pod: &Pod) -> bool {
    pod.status.as_ref().is_some_and(|status| {
        status.phase.as_deref() == Some("Running")
            && status
                .container_statuses
                .as_ref()
                .is_some_and(|containers| {
                    containers.iter().any(|container| {
                        container.name == "agent"
                            && container
                                .state
                                .as_ref()
                                .and_then(|state| state.running.as_ref())
                                .is_some()
                    })
                })
    })
}

/// 完成候选后的 STS 复核：身份未替换；`require_replicas` 给出动作方向
/// （stop=0 / wake=1；restart 不约束 replicas）。
pub(super) fn verify_workload_stable(
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
pub(super) fn observation_error(error: crate::runtime::k8s_observation::ObservationError) -> Error {
    match error {
        crate::runtime::k8s_observation::ObservationError::Deadline { .. } => {
            Error::Timeout("Builder compute confirmation timed out; reconciliation required".into())
        }
        crate::runtime::k8s_observation::ObservationError::Cancelled => {
            Error::Timeout("Builder compute observation cancelled".into())
        }
        crate::runtime::k8s_observation::ObservationError::Fatal { code, message } => {
            Error::K8sError(format!("Builder observation failed ({code:?}): {message}"))
        }
        crate::runtime::k8s_observation::ObservationError::StreamEnded { message } => {
            Error::K8sError(format!("Builder observation stream ended: {message}"))
        }
    }
}

/// Kubernetes may populate omitted defaults, but every explicitly requested
/// field and ordered list entry must match. Sidecars/config changes are rejected.
pub(super) fn configured_fields_match(
    desired: &serde_json::Value,
    actual: &serde_json::Value,
) -> bool {
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

pub(super) fn rejected_before_write(message: String) -> Error {
    Error::RequestRejected(shared_types::RuntimeRequestRejection {
        status: 409,
        message,
    })
}

pub(super) fn stop_patch(workload: &AppResourceIdentity) -> serde_json::Value {
    serde_json::json!({"metadata":{"uid":workload.uid,"resourceVersion":workload.resource_version},"spec":{"replicas":0}})
}

/// The old Pod is already gone when this write runs. Updating the template and
/// scaling from zero in one conditional write avoids an intermediate rollout
/// and keeps the StatefulSet/PVC identities unchanged.
pub(super) fn builder_compute_start_patch(
    workload: &AppResourceIdentity,
    receipt: &str,
    image: Option<&str>,
) -> serde_json::Value {
    let mut patch = serde_json::json!({
        "metadata": {
            "uid": workload.uid,
            "resourceVersion": workload.resource_version,
            "annotations": {"rcoder.io/compute-start-receipt": receipt}
        },
        "spec": {"replicas": 1}
    });
    if let Some(image) = image {
        patch["spec"]["template"] = serde_json::json!({
            "spec": {"containers": [{"name": "agent", "image": image}]}
        });
    }
    patch
}

pub(super) fn statefulset_agent_image_matches(sts: &StatefulSet, expected: Option<&str>) -> bool {
    expected.is_none_or(|image| {
        sts.spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|container| container.name == "agent")
            })
            .and_then(|container| container.image.as_deref())
            == Some(image)
    })
}

pub(super) fn pod_agent_image_matches(pod: &Pod, expected: Option<&str>) -> bool {
    expected.is_none_or(|image| {
        pod.spec
            .as_ref()
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|container| container.name == "agent")
            })
            .and_then(|container| container.image.as_deref())
            == Some(image)
    })
}
pub(super) fn pod_delete_params(pod: &BuilderPodIdentity) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(pod.uid.clone()),
            resource_version: Some(pod.resource_version.clone()),
        }),
        ..Default::default()
    }
}
pub(super) fn required(value: Option<&str>, field: &str) -> Result<String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::ConfigurationError(format!("Builder {field} is missing")))
}

pub(super) fn workload_identity_with_binding(
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
pub(super) fn pod_identity(
    pod: &Pod,
    workload: &AppResourceIdentity,
) -> Result<BuilderPodIdentity> {
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
pub(super) fn api_error(context: &str, error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 409) {
        Error::Conflict(format!("{context}: {error}"))
    } else {
        Error::K8sError(format!("{context}: {error}"))
    }
}

pub(super) fn builder_exec_guard(uid: &str, command: Vec<String>) -> Vec<String> {
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
