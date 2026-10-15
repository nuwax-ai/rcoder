//! Full builder configuration equality after explicit Kubernetes defaults.
//! Normalized values are comparison-only: never send them back to the API.
use super::*;
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec, Probe, ResourceRequirements};
use std::collections::BTreeSet;

pub(crate) fn retained_builder_credential(spec: &PodSpec) -> Result<Option<EnvVar>> {
    let agent = spec
        .containers
        .iter()
        .find(|container| container.name == "agent")
        .ok_or_else(|| Error::Conflict("Bound builder agent container missing".into()))?;
    let credentials: Vec<_> = agent
        .env
        .iter()
        .flatten()
        .filter(|entry| entry.name == "APP_CLI_DEPLOY_TOKEN")
        .collect();
    if credentials.len() > 1 {
        tracing::warn!(
            "legacy builder has duplicate deploy credential entries; retaining effective last entry"
        );
    }
    // Kubelet resolves duplicate env names using the last entry. A historical
    // shape is not authentication failure and must not block a live owner.
    Ok(credentials.last().map(|entry| (*entry).clone()))
}

/// Reuse concerns the actual application data views, not today's platform
/// template. Missing newer management injection is a repair observation.
pub(super) fn verify_existing_builder_views(app_id: &str, spec: &PodSpec) -> Result<()> {
    let agent = spec
        .containers
        .iter()
        .find(|container| container.name == "agent")
        .ok_or_else(|| Error::Conflict("Bound builder agent container missing".into()))?;
    let mut volumes = spec
        .volumes
        .iter()
        .flatten()
        .filter(|volume| volume.name == "workspace");
    let workspace = volumes
        .next()
        .and_then(|volume| volume.persistent_volume_claim.as_ref())
        .ok_or_else(|| Error::Conflict("Bound builder workspace PVC missing".into()))?;
    if volumes.next().is_some()
        || workspace.claim_name.is_empty()
        || workspace.read_only == Some(true)
    {
        return Err(Error::Conflict(
            "Bound builder workspace PVC binding is invalid".into(),
        ));
    }
    let expected = [
        (
            format!("{}/{app_id}", shared_types::paths::USERAPP_DEV_HOME),
            app_id,
        ),
        (shared_types::paths::USERAPP_DEV_DATA.into(), "data"),
        (shared_types::paths::USERAPP_DEV_LOGS.into(), "logs"),
        (
            shared_types::paths::USERAPP_DEV_AGENT_STORE.into(),
            "agent-store",
        ),
    ];
    let mounts: Vec<_> = agent
        .volume_mounts
        .iter()
        .flatten()
        .filter(|mount| mount.name == "workspace")
        .collect();
    if mounts.len() != expected.len()
        || expected.iter().any(|(path, sub)| {
            mounts
                .iter()
                .filter(|mount| {
                    mount.mount_path == *path
                        && mount.sub_path.as_deref() == Some(*sub)
                        && mount.sub_path_expr.is_none()
                        && mount.read_only != Some(true)
                })
                .count()
                != 1
        })
    {
        return Err(Error::Conflict(
            "Bound builder workspace views point outside the application".into(),
        ));
    }
    let domain = agent
        .env
        .iter()
        .flatten()
        .find(|entry| entry.name == runtime_supervisor::domain::DOMAIN_ENV)
        .and_then(|entry| entry.value.as_deref());
    if let Some(domain) = domain {
        // The API endpoint/CA can evolve while this same physical owner lives.
        // Compare its own data volume, never substitute the current authority.
        match serde_json::from_str::<serde_json::Value>(domain) {
            Ok(value) => {
                let authority = value
                    .get("authority")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let expected_domain = super::super::k8s_native_domain::builder_domain_env(
                    authority,
                    &workspace.claim_name,
                    agent.volume_mounts.as_deref().unwrap_or_default(),
                );
                let expected_domain: serde_json::Value = serde_json::from_str(&expected_domain)
                    .map_err(|error| {
                        Error::ConfigurationError(format!(
                            "Encode builder volume identity: {error}"
                        ))
                    })?;
                if let Some(volume) = value.get("volume").and_then(serde_json::Value::as_str)
                    && Some(volume) != expected_domain["volume"].as_str()
                {
                    return Err(Error::Conflict(
                        "Bound builder execution domain points to another data volume".into(),
                    ));
                }
            }
            Err(error) => tracing::warn!(%error, app = app_id,
                "legacy builder execution domain needs management-plane reconciliation"),
        }
    } else {
        tracing::info!(
            app = app_id,
            "legacy builder lacks execution-domain injection; retaining owned compute"
        );
    }
    Ok(())
}

/// Ordinary platform configuration is not ownership. Keep the already bound
/// compute usable while checking the fixed storage and execution-domain views.
pub(crate) fn verify_bound_builder_storage(desired: &PodSpec, actual: &PodSpec) -> Result<()> {
    let project = |spec: &PodSpec| -> Result<PodSpec> {
        let agent = spec
            .containers
            .iter()
            .find(|container| container.name == "agent")
            .ok_or_else(|| Error::Conflict("Bound builder agent container missing".into()))?;
        let mut fixed = Container {
            name: "agent".into(),
            ..Default::default()
        };
        fixed.volume_mounts = Some(
            agent
                .volume_mounts
                .iter()
                .flatten()
                .filter(|mount| {
                    mount.name == "workspace" || mount.name == "rcoder-platform-binding"
                })
                .cloned()
                .collect(),
        );
        fixed.env = Some(
            agent
                .env
                .iter()
                .flatten()
                .filter(|entry| {
                    entry.name == runtime_supervisor::domain::DOMAIN_ENV
                        || entry.name == "RCODER_PHYSICAL_POD_UID"
                })
                .cloned()
                .collect(),
        );
        let fixed = PodSpec {
            containers: vec![fixed],
            volumes: Some(
                spec.volumes
                    .iter()
                    .flatten()
                    .filter(|volume| {
                        volume.name == "workspace" || volume.name == "rcoder-platform-binding"
                    })
                    .cloned()
                    .collect(),
            ),
            ..Default::default()
        };
        normalized_builder_pod_spec(&fixed)
    };
    if project(desired)? != project(actual)? {
        return Err(Error::Conflict(
            "Bound builder workspace or execution-domain binding changed".into(),
        ));
    }
    Ok(())
}

pub(crate) fn normalized_builder_pod_spec(spec: &PodSpec) -> Result<PodSpec> {
    let mut spec = spec.clone();
    empty_list(&mut spec.image_pull_secrets);
    empty_list(&mut spec.init_containers);
    empty_list(&mut spec.ephemeral_containers);
    empty_list(&mut spec.volumes);
    empty_list(&mut spec.host_aliases);
    empty_list(&mut spec.tolerations);
    empty_list(&mut spec.topology_spread_constraints);
    empty_list(&mut spec.readiness_gates);
    empty_list(&mut spec.resource_claims);
    empty_list(&mut spec.scheduling_gates);
    spec.dns_policy.get_or_insert_with(|| "ClusterFirst".into());
    spec.enable_service_links.get_or_insert(true);
    spec.host_ipc.get_or_insert(false);
    spec.host_network.get_or_insert(false);
    spec.host_pid.get_or_insert(false);
    spec.host_users.get_or_insert(true);
    spec.restart_policy.get_or_insert_with(|| "Always".into());
    spec.scheduler_name
        .get_or_insert_with(|| "default-scheduler".into());
    spec.set_hostname_as_fqdn.get_or_insert(false);
    spec.share_process_namespace.get_or_insert(false);
    spec.termination_grace_period_seconds.get_or_insert(30);
    // The deprecated alias is equivalent only when it names the same account.
    if spec.service_account == spec.service_account_name {
        spec.service_account = None;
    }
    if spec.security_context.as_ref().is_some_and(|context| {
        context == &k8s_openapi::api::core::v1::PodSecurityContext::default()
    }) {
        spec.security_context = None;
    }
    let mut names = BTreeSet::new();
    for container in &mut spec.containers {
        if !names.insert(container.name.clone()) {
            return Err(Error::ConfigurationError(
                "Builder has duplicate container names".into(),
            ));
        }
        normalize_container(container)?;
    }
    // Normal containers have map semantics; init container order is meaningful.
    spec.containers
        .sort_by(|left, right| left.name.cmp(&right.name));
    for container in spec.init_containers.iter_mut().flatten() {
        if !names.insert(container.name.clone()) {
            return Err(Error::ConfigurationError(
                "Builder has duplicate container names".into(),
            ));
        }
        normalize_container(container)?;
    }
    if let Some(volumes) = &mut spec.volumes {
        let mut names = BTreeSet::new();
        for volume in &mut *volumes {
            if !names.insert(volume.name.clone()) {
                return Err(Error::ConfigurationError(
                    "Builder has duplicate volume names".into(),
                ));
            }
            if let Some(claim) = &mut volume.persistent_volume_claim {
                claim.read_only.get_or_insert(false);
            }
            if let Some(downward) = &mut volume.downward_api {
                downward.default_mode.get_or_insert(420);
                for item in downward.items.iter_mut().flatten() {
                    if let Some(field) = &mut item.field_ref {
                        field.api_version.get_or_insert_with(|| "v1".into());
                    }
                }
            }
            if let Some(config) = &mut volume.config_map {
                config.default_mode.get_or_insert(420);
                config.optional.get_or_insert(false);
            }
            if let Some(secret) = &mut volume.secret {
                secret.default_mode.get_or_insert(420);
                secret.optional.get_or_insert(false);
            }
            if let Some(projected) = &mut volume.projected {
                projected.default_mode.get_or_insert(420);
            }
        }
        volumes.sort_by(|left, right| left.name.cmp(&right.name));
    }
    normalize_resources(&mut spec.resources);
    Ok(spec)
}

fn normalize_container(container: &mut Container) -> Result<()> {
    empty_list(&mut container.env);
    empty_list(&mut container.env_from);
    empty_list(&mut container.command);
    empty_list(&mut container.args);
    empty_list(&mut container.ports);
    empty_list(&mut container.volume_mounts);
    empty_list(&mut container.volume_devices);
    empty_list(&mut container.resize_policy);
    container.stdin.get_or_insert(false);
    container.stdin_once.get_or_insert(false);
    container.tty.get_or_insert(false);
    container
        .termination_message_path
        .get_or_insert_with(|| "/dev/termination-log".into());
    container
        .termination_message_policy
        .get_or_insert_with(|| "File".into());
    if container.image_pull_policy.is_none() {
        let image = container.image.as_deref().unwrap_or_default();
        let tail = image.rsplit('/').next().unwrap_or_default();
        let always = !image.contains('@') && (!tail.contains(':') || tail.ends_with(":latest"));
        container.image_pull_policy = Some(if always { "Always" } else { "IfNotPresent" }.into());
    }
    if let Some(env) = &mut container.env {
        let mut names = BTreeSet::new();
        let mut ordered_expansion = false;
        for entry in &mut *env {
            if !names.insert(entry.name.clone()) {
                return Err(Error::ConfigurationError(format!(
                    "Builder container {} has duplicate environment key {}",
                    container.name, entry.name
                )));
            }
            ordered_expansion |= entry
                .value
                .as_deref()
                .is_some_and(|value| value.contains("$("));
            if entry.value.as_deref() == Some("") {
                entry.value = None;
            }
            if let Some(source) = &mut entry.value_from {
                if let Some(field) = &mut source.field_ref {
                    field.api_version.get_or_insert_with(|| "v1".into());
                }
                if let Some(config) = &mut source.config_map_key_ref {
                    config.optional.get_or_insert(false);
                }
                if let Some(secret) = &mut source.secret_key_ref {
                    secret.optional.get_or_insert(false);
                }
            }
        }
        // References expand against earlier env entries. Never erase that order.
        if !ordered_expansion {
            env.sort_by(|left, right| left.name.cmp(&right.name));
        }
    }
    for port in container.ports.iter_mut().flatten() {
        port.protocol.get_or_insert_with(|| "TCP".into());
    }
    if let Some(mounts) = &mut container.volume_mounts {
        let mut paths = BTreeSet::new();
        for mount in &mut *mounts {
            if !paths.insert(mount.mount_path.clone()) {
                return Err(Error::ConfigurationError(format!(
                    "Builder container {} has duplicate volume mount paths",
                    container.name
                )));
            }
            mount.read_only.get_or_insert(false);
        }
        mounts.sort_by(|left, right| left.mount_path.cmp(&right.mount_path));
    }
    for probe in [
        &mut container.liveness_probe,
        &mut container.readiness_probe,
        &mut container.startup_probe,
    ]
    .into_iter()
    .flatten()
    {
        normalize_probe(probe);
    }
    normalize_resources(&mut container.resources);
    Ok(())
}

fn normalize_probe(probe: &mut Probe) {
    probe.initial_delay_seconds.get_or_insert(0);
    probe.timeout_seconds.get_or_insert(1);
    probe.period_seconds.get_or_insert(10);
    probe.success_threshold.get_or_insert(1);
    probe.failure_threshold.get_or_insert(3);
    if let Some(http) = &mut probe.http_get {
        http.scheme.get_or_insert_with(|| "HTTP".into());
        http.path.get_or_insert_with(|| "/".into());
        if http.host.as_deref() == Some("") {
            http.host = None;
        }
        empty_list(&mut http.http_headers);
    }
}

fn empty_list<T>(list: &mut Option<Vec<T>>) {
    if list.as_ref().is_some_and(Vec::is_empty) {
        *list = None;
    }
}

fn normalize_resources(resources: &mut Option<ResourceRequirements>) {
    if let Some(resources) = resources {
        for quantity in resources
            .limits
            .iter_mut()
            .chain(resources.requests.iter_mut())
            .flat_map(|map| map.values_mut())
        {
            if let Some(canonical) = exact_quantity(&quantity.0) {
                quantity.0 = canonical;
            }
        }
    }
    if resources
        .as_ref()
        .is_some_and(|value| value == &ResourceRequirements::default())
    {
        *resources = None;
    }
}

// Exact arithmetic avoids treating distinct large/fractional quantities as equal
// through floating point rounding. Unsupported/overflowing inputs retain their
// original spelling and therefore remain conservatively different.
fn exact_quantity(raw: &str) -> Option<String> {
    let suffixes = [
        ("Ki", 1024_i128, 1_i128),
        ("Mi", 1024_i128.pow(2), 1),
        ("Gi", 1024_i128.pow(3), 1),
        ("Ti", 1024_i128.pow(4), 1),
        ("Pi", 1024_i128.pow(5), 1),
        ("Ei", 1024_i128.pow(6), 1),
        ("n", 1, 1_000_000_000),
        ("u", 1, 1_000_000),
        ("m", 1, 1000),
        ("k", 1000, 1),
        ("M", 1_000_000, 1),
        ("G", 1_000_000_000, 1),
        ("T", 1_000_000_000_000, 1),
        ("P", 1_000_000_000_000_000, 1),
        ("E", 1_000_000_000_000_000_000, 1),
    ];
    let (number, multiplier, divisor) = suffixes
        .iter()
        .find_map(|(suffix, multiplier, divisor)| {
            raw.strip_suffix(suffix)
                .map(|number| (number, *multiplier, *divisor))
        })
        .unwrap_or((raw, 1, 1));
    let (number, exponent) = match number.find(['e', 'E']) {
        Some(at) => (&number[..at], number[at + 1..].parse::<i32>().ok()?),
        None => (number, 0),
    };
    let number = number.strip_prefix('+').unwrap_or(number);
    let (whole, fraction) = match number.split_once('.') {
        Some(parts) => parts,
        None => (number, ""),
    };
    if whole.starts_with('-')
        || whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let mut numerator = digits.parse::<i128>().ok()?.checked_mul(multiplier)?;
    let mut denominator = 10_i128
        .checked_pow(u32::try_from(fraction.len()).ok()?)?
        .checked_mul(divisor)?;
    if exponent >= 0 {
        numerator = numerator.checked_mul(10_i128.checked_pow(exponent.unsigned_abs())?)?;
    } else {
        denominator = denominator.checked_mul(10_i128.checked_pow(exponent.unsigned_abs())?)?;
    }
    let (mut left, mut right) = (numerator, denominator);
    while right != 0 {
        (left, right) = (right, left % right);
    }
    Some(format!("{}/{}", numerator / left, denominator / left))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_quantities_keep_precision_and_compare_equivalent_units() {
        assert_eq!(exact_quantity("6144Mi"), exact_quantity("6Gi"));
        assert_eq!(exact_quantity("0.5"), exact_quantity("500m"));
        assert_eq!(exact_quantity("1e3"), exact_quantity("1k"));
        assert_ne!(
            exact_quantity("9007199254740992"),
            exact_quantity("9007199254740993")
        );
        assert_ne!(exact_quantity("1.4"), exact_quantity("1.5"));
    }
}
