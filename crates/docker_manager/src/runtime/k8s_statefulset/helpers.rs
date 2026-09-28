use super::*;

/// 取 PodSpec 里 name=workspace 卷的 PVC claim 名（漂移检测用）。
pub(super) fn workspace_claim_name(spec: &PodSpec) -> Option<String> {
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
/// Builder 模板复用校验结论。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BuilderTemplateCheck {
    /// 记录指纹与期望一致（或内容已由下方内容校验独立证明等价），可直接复用。
    Current,
    /// launch 内容与期望等价，但记录指纹是旧算法写入的存量值（跨算法不可比）。
    /// 调用方以纯 metadata patch 重写注解（heal_builder_template_hash）后按
    /// Current 对待——不愈合则存量 STS 的每次全量校验必然 mismatch，落成
    /// 围栏/收束循环（哈希算法演进的一次性迁移语义）。
    StaleHash,
}

/// 哈希指纹与内容等价判定的共同 env 口径：剔除随请求/创建抖动的键
/// （per-request 槽位 + 每创建随机的部署凭据）后按 name 排序。两侧口径
/// 必须同源——任一侧独立漂移会造成"指纹相等但内容不等"或反向的假信号。
pub(super) fn canonical_env(env: &[EnvVar]) -> Vec<EnvVar> {
    const VOLATILE_ENV_KEYS: [&str; 4] = [
        "TENANT_ID",
        "SPACE_ID",
        "ISOLATION_TYPE",
        "APP_CLI_DEPLOY_TOKEN",
    ];
    let mut entries: Vec<EnvVar> = env
        .iter()
        .filter(|entry| !VOLATILE_ENV_KEYS.contains(&entry.name.as_str()))
        .map(|entry| {
            let mut entry = entry.clone();
            // 空值归一：apiserver 对 env value="" 的存储不可往返——发送
            // Some("") 的条目存回读出即无 value 字段（builder 的 USER_ID 恒空
            // 即 2026-09-22 app-155 全量 ensure/restart 恒 Conflict 的根源，
            // "drifted keys: ~USER_ID"）。Some("") 与 None 的区别在 API 层
            // 不可表达，归一后比较才与真实漂移对齐，非放宽校验。
            if entry.value.as_deref() == Some("") {
                entry.value = None;
            }
            entry
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// 内容优先的复用校验：launch 投影（镜像/command/args/归一化 env/容器集合/
/// workspace claim）逐一比对，真实漂移在此以精确文案拒绝；指纹注解最后判
/// 定——mismatch 且内容等价 = 旧算法存量注解，返回 StaleHash 交由调用方
/// 自愈。注意：指纹原本覆盖的投影外字段（probe/port 等 API 默认化字段，
/// 既有内容校验明确忽略）不再触发拒绝，与 computer-agent 路径的 advisory
/// 哲学一致——此类残余漂移由 cleaner 的空闲换代路径滚动，不再围栏。
pub(super) fn validate_builder_statefulset(
    existing: &StatefulSet,
    desired: &StatefulSet,
    context: &shared_types::UserAppExecutionContext,
) -> ContainerRuntimeResult<BuilderTemplateCheck> {
    let conflict = |message: &str| ContainerRuntimeError::Conflict(message.into());
    let annotations = existing
        .metadata
        .annotations
        .as_ref()
        .ok_or_else(|| conflict("Builder StatefulSet requires identity adoption"))?;
    context
        .validate_resource_metadata(annotations)
        .map_err(ContainerRuntimeError::Conflict)?;
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
    if existing_pod.containers.len() != desired_pod.containers.len() {
        return Err(conflict("Builder container set changed"));
    }
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
        // env 是指纹原本覆盖而 launch 字段比对未覆盖的部分——等价判定必须
        // 收口在这里，否则内容校验会放过 env 漂移（旧实现由指纹门兜底）。
        // 差异 key 清单进文案：发版窗口（runtime image digest 随版本变）的
        // 漂移源一眼可辨，不再需要二进制对比。
        let actual_env = canonical_env(actual.env.as_deref().unwrap_or(&[]));
        let desired_env = canonical_env(desired_container.env.as_deref().unwrap_or(&[]));
        if actual_env != desired_env {
            let actual_keys: std::collections::BTreeSet<&str> =
                actual_env.iter().map(|entry| entry.name.as_str()).collect();
            let desired_keys: std::collections::BTreeSet<&str> = desired_env
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            let mut drift: Vec<String> = desired_keys
                .difference(&actual_keys)
                .map(|key| format!("+{key}"))
                .chain(
                    actual_keys
                        .difference(&desired_keys)
                        .map(|key| format!("-{key}")),
                )
                .collect();
            for entry in &desired_env {
                if let Some(current) = actual_env.iter().find(|e| e.name == entry.name)
                    && current.value != entry.value
                {
                    drift.push(format!("~{}", entry.name));
                }
            }
            return Err(conflict(&format!(
                "Builder container environment changed (drifted keys: {})",
                drift.join(",")
            )));
        }
    }
    let desired_hash = desired
        .metadata
        .annotations
        .as_ref()
        .and_then(|values| values.get(TEMPLATE_HASH_ANNOTATION));
    let desired_hash =
        desired_hash.ok_or_else(|| conflict("Builder desired template hash is missing"))?;
    if annotations.get(TEMPLATE_HASH_ANNOTATION) == Some(desired_hash) {
        return Ok(BuilderTemplateCheck::Current);
    }
    // 走到这里意味着 launch 投影全部等价，注解 mismatch 只能是算法演进的
    // 存量值——授权自愈而非围栏。
    Ok(BuilderTemplateCheck::StaleHash)
}

pub(super) fn conditional_scale_patch(
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

pub(super) fn agent_template_hash(pod_spec: &PodSpec) -> String {
    let mut spec = pod_spec.clone();
    for container in &mut spec.containers {
        container.resources = None;
        if let Some(env) = &mut container.env {
            // Excluded from the fingerprint: per-request slot values, and the
            // per-creation deploy credential (a fresh random token on every
            // build — hashing it made every re-computation differ and turned
            // the drift check into a coin flip; test-env app 151/155 incident).
            // Canonical ordering: env lists are assembled from HashMaps whose
            // iteration order is per-process random. Without sorting, two
            // replicas computing the hash for the identical desired spec
            // produce different values and the reuse check rejects a perfectly
            // matching controller ("configuration changed" false positive).
            // 口径与 validate_builder_statefulset 的内容等价判定同源（canonical_env）。
            *env = canonical_env(env);
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

/// agent STS create-409 winner 校验（step-D 写面 fencing，对齐 builder STS
/// winner-validate）：身份标签/注解 + 启动内容（image/command/args/容器集合）
/// 等价才可复用；漂移 → Conflict。template-hash 注解按既有口径只作记账
/// （内容校验才是复用门），不参与比对。
pub(super) fn validate_agent_statefulset(
    existing: &StatefulSet,
    desired: &StatefulSet,
) -> ContainerRuntimeResult<()> {
    let existing_meta = &existing.metadata;
    let desired_meta = &desired.metadata;
    for (key, value) in desired_meta.labels.iter().flatten() {
        if existing_meta.labels.as_ref().and_then(|l| l.get(key)) != Some(value) {
            return Err(ContainerRuntimeError::Conflict(format!(
                "Agent StatefulSet identity label '{key}' differs from desired"
            )));
        }
    }
    for (key, value) in desired_meta.annotations.iter().flatten() {
        if key == TEMPLATE_HASH_ANNOTATION {
            continue;
        }
        if existing_meta.annotations.as_ref().and_then(|a| a.get(key)) != Some(value) {
            return Err(ContainerRuntimeError::Conflict(format!(
                "Agent StatefulSet identity annotation '{key}' differs from desired"
            )));
        }
    }
    let existing_pod = existing
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref());
    let desired_pod = desired.spec.as_ref().and_then(|s| s.template.spec.as_ref());
    let (Some(existing_pod), Some(desired_pod)) = (existing_pod, desired_pod) else {
        return Err(ContainerRuntimeError::Conflict(
            "Agent StatefulSet pod template is missing".into(),
        ));
    };
    if existing_pod.containers.len() != desired_pod.containers.len() {
        return Err(ContainerRuntimeError::Conflict(
            "Agent StatefulSet container set differs from desired".into(),
        ));
    }
    for desired_container in &desired_pod.containers {
        let Some(existing_container) = existing_pod
            .containers
            .iter()
            .find(|c| c.name == desired_container.name)
        else {
            return Err(ContainerRuntimeError::Conflict(format!(
                "Agent StatefulSet container '{}' missing in existing",
                desired_container.name
            )));
        };
        if existing_container.image != desired_container.image
            || existing_container.command != desired_container.command
            || existing_container.args != desired_container.args
        {
            return Err(ContainerRuntimeError::Conflict(format!(
                "Agent StatefulSet container '{}' launch content differs from desired",
                desired_container.name
            )));
        }
    }
    Ok(())
}
