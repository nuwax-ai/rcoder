//! Docker inventory and exact-CID retirement. Automatic callers must prove
//! deployment ownership, persistent workspace coverage and current idleness.
use super::DockerRuntime;
use container_runtime_api::{
    AgentResourceIdentity, AgentResourceKind, AgentStopOutcome, AgentStopSnapshot,
    ComputeDataProtection, ContainerRuntimeError as Error, ContainerRuntimeResult as Result,
};
use shared_types::ServiceType;
use std::collections::HashMap;
use tracing::debug;

#[cfg(test)]
mod legacy_adoption_tests;
#[cfg(test)]
mod tests;

impl DockerRuntime {
    pub(super) fn agent_retirement_namespace(&self) -> String {
        let address = self
            .inner
            .config
            .docker_host
            .clone()
            .or_else(|| std::env::var("DOCKER_SOCKET_PATH").ok())
            .or_else(|| std::env::var("DOCKER_HOST").ok())
            .unwrap_or_else(|| "local".into());
        format!("docker:{address}")
    }
    pub(super) fn agent_retirement_name(
        &self,
        identifier: &str,
        family: &ServiceType,
    ) -> Result<String> {
        let prefix = self
            .inner
            .config
            .multi_image_config
            .get_service_config(family)
            .map(|config| config.container_prefix())
            .unwrap_or_else(|| family.container_prefix());
        crate::utils::DockerUtils::generate_container_name(prefix, identifier)
            .map_err(Error::ConfigurationError)
    }

    pub(super) async fn docker_agent_inventory(&self) -> Result<Vec<AgentStopSnapshot>> {
        let containers = self
            .inner
            .get_docker_client()
            .list_containers(Some(bollard::query_parameters::ListContainersOptions {
                all: true,
                filters: Some(HashMap::from([(
                    "label".into(),
                    vec!["identifier".into(), "service-type".into()],
                )])),
                ..Default::default()
            }))
            .await
            .map_err(|error| {
                Error::DockerError(format!("List managed Agent containers: {error}"))
            })?;
        use futures_util::{StreamExt, TryStreamExt, stream};
        let mut snapshots: Vec<AgentStopSnapshot> = stream::iter(containers)
            .map(|container| self.inspect_agent_candidate(container))
            .buffer_unordered(8)
            .try_filter_map(|snapshot| async move { Ok(snapshot) })
            .try_collect()
            .await?;
        // 存量无标签容器纳管：旧版本创建的 agent 容器缺管理标签，对标签
        // 过滤不可见。名字反解纳管只让既有裁决链看见候选；删除仍需完整
        // 证据链（未登记/探活闲置/观察窗口/数据保护/精确 CID）。
        self.append_legacy_web_agent_candidates(&mut snapshots)
            .await?;
        Ok(snapshots)
    }

    /// 部署配置的 Web 前缀（与创建侧同源；空前缀不纳管）。
    fn configured_web_agent_prefix(&self) -> String {
        self.inner
            .config
            .multi_image_config
            .get_service_config(&ServiceType::WebAgentRunner)
            .map(|config| config.container_prefix())
            .unwrap_or_else(|| ServiceType::WebAgentRunner.container_prefix())
            .to_string()
    }

    fn configured_computer_agent_prefix(&self) -> String {
        self.inner
            .config
            .multi_image_config
            .get_service_config(&ServiceType::ComputerAgentRunner)
            .map(|config| config.container_prefix())
            .unwrap_or_else(|| ServiceType::ComputerAgentRunner.container_prefix())
            .to_string()
    }

    /// 名字兜底不能覆盖已有的归属证据；三个入口共用同一守卫。
    fn legacy_web_agent_identifier<'a>(
        &self,
        name: &'a str,
        labels: Option<&HashMap<String, String>>,
    ) -> Option<&'a str> {
        let web_prefix = self.configured_web_agent_prefix();
        if web_prefix.is_empty() {
            return None;
        }
        let identifier = name.strip_prefix(&format!("{web_prefix}-"))?;
        if crate::utils::DockerUtils::generate_container_name(&web_prefix, identifier)
            != Ok(name.to_string())
            || shared_types::container_identity_from_name(
                name,
                &web_prefix,
                &self.configured_computer_agent_prefix(),
            ) != Some((identifier, ServiceType::WebAgentRunner))
        {
            return None;
        }
        if labels.is_some_and(|labels| {
            labels.get("identifier").is_some_and(|id| id != identifier)
                || labels.get("service-type").is_some_and(|value| {
                    value.parse::<ServiceType>().ok() != Some(ServiceType::WebAgentRunner)
                })
                || labels.contains_key("managed-by")
                || labels.contains_key(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
                || shared_types::USERAPP_RESOURCE_IDENTITY_KEYS
                    .iter()
                    .any(|key| labels.contains_key(*key))
        }) {
            return None;
        }
        Some(identifier)
    }

    /// 名字反解纳管存量无标签 Web agent 容器（Docker 模式专属路径）。
    ///
    /// 守卫：名字在配置 Web 前缀下**精确回构**、最长前缀反解族 == Web（防
    /// 吞 Userapp/Builder 名）、不在带标签结果集（去重）、缺管理双标签、
    /// Running（无探活端点的容器不纳管，保守保持不可见）。
    async fn append_legacy_web_agent_candidates(
        &self,
        snapshots: &mut Vec<AgentStopSnapshot>,
    ) -> Result<()> {
        let web_prefix = self.configured_web_agent_prefix();
        if web_prefix.is_empty() {
            return Ok(());
        }
        let legacy = self
            .inner
            .get_docker_client()
            .list_containers(Some(bollard::query_parameters::ListContainersOptions {
                all: true,
                filters: Some(HashMap::from([(
                    "name".into(),
                    vec![format!("^{web_prefix}-")],
                )])),
                ..Default::default()
            }))
            .await
            .map_err(|error| {
                Error::DockerError(format!("List legacy Agent containers: {error}"))
            })?;
        let labeled_uids: std::collections::HashSet<String> = snapshots
            .iter()
            .filter_map(|snapshot| snapshot.container.as_ref().map(|c| c.uid.clone()))
            .collect();
        for container in legacy {
            let Some(uid) = container
                .id
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            if labeled_uids.contains(&uid) {
                continue;
            }
            let Some(name) = container
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|value| value.trim_start_matches('/'))
            else {
                continue;
            };
            // 名字精确回构、族解析及已有标签必须一致。
            let Some(identifier) =
                self.legacy_web_agent_identifier(name, container.labels.as_ref())
            else {
                continue;
            };
            // 有管理双标签者属标签路径域（列表过滤按键存在不校验值；
            // 值错乱由标签路径的 Conflict 语义处置）。
            if container.labels.as_ref().is_some_and(|labels| {
                labels.contains_key("identifier") && labels.contains_key("service-type")
            }) {
                continue;
            }
            match self
                .inspect_agent_snapshot(
                    &container,
                    ServiceType::WebAgentRunner,
                    identifier,
                    name,
                    true,
                )
                .await
            {
                Ok(Some(snapshot)) => snapshots.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    debug!(uid = %uid, %error,
                        "Legacy Agent candidate inspection failed; skipping this round");
                }
            }
        }
        Ok(())
    }

    async fn inspect_agent_candidate(
        &self,
        container: bollard::models::ContainerSummary,
    ) -> Result<Option<AgentStopSnapshot>> {
        let Some(labels) = container.labels.as_ref() else {
            return Ok(None);
        };
        let Some(family) = labels
            .get("service-type")
            .and_then(|value| value.parse::<ServiceType>().ok())
        else {
            return Ok(None);
        };
        match family {
            ServiceType::Userapp => return Ok(None),
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::UserappBuilder => {}
        }
        let Some(identifier) = labels.get("identifier").filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let Some(name) = container
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|value| value.trim_start_matches('/'))
        else {
            return Ok(None);
        };
        self.inspect_agent_snapshot(&container, family, identifier, name, false)
            .await
    }

    /// 由 list 摘要 + inspect 构造退休快照。`legacy` 为名字反解纳管候选：
    /// 无标签即其形态，不做标签等值校验，但要求 Running 且名字未在 list
    /// 与 inspect 间变化；标签路径（`legacy=false`）保持原 Conflict 语义。
    async fn inspect_agent_snapshot(
        &self,
        container: &bollard::models::ContainerSummary,
        family: ServiceType,
        identifier: &str,
        name: &str,
        legacy: bool,
    ) -> Result<Option<AgentStopSnapshot>> {
        let uid = container
            .id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::DockerError("Managed Agent has no full container ID".into()))?
            .to_string();
        let name = name.to_string();
        let identifier = identifier.to_string();
        let family = family.family_representative();
        // Live evidence needs one inspect per candidate: StartedAt resets
        // observation on same-CID restarts, and mounts decide whether managed
        // data survives container removal. An unreadable candidate is absent
        // this round, so observation continuity never accrues on stale data.
        let inspected = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.inner.get_docker_client().inspect_container(&uid, None),
        )
        .await
        {
            Ok(Ok(inspected)) => inspected,
            Ok(Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            })) => return Ok(None),
            outcome => {
                debug!(uid = %uid, ?outcome, "Managed Agent inspect is unavailable; skipping this round");
                return Ok(None);
            }
        };
        let started_at = inspected
            .state
            .as_ref()
            .and_then(|state| state.started_at.as_deref())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if legacy
            && !inspected
                .state
                .as_ref()
                .is_some_and(|state| state.running == Some(true))
        {
            // 名字反解候选要求 Running：无网络端点即无法逐轮探活，保守
            // 不纳管（保持既有不可见状态，不引入"无端点即闲置"语义）。
            debug!(uid = %uid, name = %name,
                "Legacy Agent candidate is not running; not adopted this round");
            return Ok(None);
        }
        let persistent_mounts = self
            .persistent_agent_workspace(&inspected, family, &identifier)
            .await;
        let auto_remove = inspected
            .host_config
            .as_ref()
            .and_then(|config| config.auto_remove)
            .unwrap_or(false);
        // Inspect is the single identity/network truth, not the older
        // list response or an arbitrary HashMap network belonging to peers.
        let observed_labels = inspected.config.as_ref().and_then(|c| c.labels.as_ref());
        if inspected.id.as_deref() != Some(uid.as_str()) {
            return Err(Error::Conflict(
                "Docker Agent identity changed during inventory".into(),
            ));
        }
        if legacy {
            if self.legacy_web_agent_identifier(&name, observed_labels) != Some(identifier.as_str())
                || inspected
                    .name
                    .as_deref()
                    .map(|value| value.trim_start_matches('/'))
                    != Some(name.as_str())
            {
                debug!(uid = %uid, name = %name,
                    "Legacy Agent candidate was renamed during inventory; skipping this round");
                return Ok(None);
            }
        } else if observed_labels.and_then(|l| l.get("identifier")) != Some(&identifier)
            || observed_labels
                .and_then(|l| l.get("service-type"))
                .and_then(|s| s.parse::<ServiceType>().ok())
                != Some(family)
        {
            return Err(Error::Conflict(
                "Docker Agent identity changed during inventory".into(),
            ));
        }
        let network = self.inner.get_main_network_name().await;
        let endpoint = inspected
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
            .and_then(|n| n.get(&network))
            .and_then(|n| n.ip_address.as_deref())
            .filter(|ip| !ip.is_empty())
            .map(|ip| format!("http://{ip}:{}", shared_types::GRPC_DEFAULT_PORT));
        let identity = AgentResourceIdentity {
            kind: AgentResourceKind::Container,
            name,
            uid,
            resource_version: started_at,
            owner_uid: None,
            created_at: container
                .created
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
        };
        Ok(Some(AgentStopSnapshot {
            app_id_hint: None,
            namespace: Some(self.agent_retirement_namespace()),
            identifier,
            service_type: family,
            controller: None,
            pods: Vec::new(),
            container: Some(identity),
            services: Vec::new(),
            replicas: None,
            endpoint,
            compute_data_protection: Some(ComputeDataProtection {
                persistent_mounts,
                auto_remove,
            }),
        }))
    }

    pub(super) async fn capture_docker_agent_retirement(
        &self,
        identifier: &str,
        family: &ServiceType,
    ) -> Result<AgentStopSnapshot> {
        let mut matches = self
            .docker_agent_inventory()
            .await?
            .into_iter()
            .filter(|snapshot| {
                snapshot.identifier == identifier
                    && snapshot.service_type.same_container_family(*family)
            });
        let captured = matches.next();
        if matches.next().is_some() {
            return Err(Error::Conflict(
                "Multiple Docker Agent containers share this scope".into(),
            ));
        }
        if captured.is_none() {
            let name = self.agent_retirement_name(identifier, family)?;
            match self
                .inner
                .get_docker_client()
                .inspect_container(&name, None)
                .await
            {
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Ok(_) => {
                    return Err(Error::Conflict(
                        "Unclassified Docker Agent exists at its canonical name".into(),
                    ));
                }
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "Observe unclassified Agent: {error}"
                    )));
                }
            }
        }
        Ok(captured.unwrap_or_else(|| AgentStopSnapshot {
            app_id_hint: None,
            namespace: Some(self.agent_retirement_namespace()),
            identifier: identifier.into(),
            service_type: family.family_representative(),
            controller: None,
            pods: Vec::new(),
            container: None,
            services: Vec::new(),
            replicas: None,
            endpoint: None,
            compute_data_protection: None,
        }))
    }

    pub(super) async fn retire_docker_agent_snapshot(
        &self,
        snapshot: &AgentStopSnapshot,
        automatic: bool,
    ) -> Result<AgentStopOutcome> {
        if snapshot.namespace.as_deref() != Some(self.agent_retirement_namespace().as_str())
            || snapshot.controller.is_some()
            || !snapshot.pods.is_empty()
            || !snapshot.services.is_empty()
            || snapshot.identifier.is_empty()
        {
            return Err(Error::Conflict(
                "Docker Agent retirement scope differs".into(),
            ));
        }
        match snapshot.service_type {
            ServiceType::Userapp => {
                return Err(Error::Conflict(
                    "Production UserApp is not an Agent retirement target".into(),
                ));
            }
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::UserappBuilder => {}
        }
        let Some(target) = &snapshot.container else {
            return Ok(AgentStopOutcome::Complete);
        };
        if target.kind != AgentResourceKind::Container || target.uid.is_empty() {
            return Err(Error::Conflict(
                "Docker Agent physical identity is missing".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let observed = match client.inspect_container(&target.uid, None).await {
            Ok(observed) => observed,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(AgentStopOutcome::Complete),
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect captured Agent: {error}"
                )));
            }
        };
        let labels = observed
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        if observed.id.as_deref() != Some(target.uid.as_str()) {
            return Err(Error::Conflict(
                "Captured Docker Agent ownership changed".into(),
            ));
        }
        let managed_labels = labels.filter(|labels| {
            labels.contains_key("identifier") && labels.contains_key("service-type")
        });
        match managed_labels {
            Some(labels) => {
                if labels.get("identifier").map(String::as_str)
                    != Some(snapshot.identifier.as_str())
                    || labels
                        .get("service-type")
                        .and_then(|value| value.parse::<ServiceType>().ok())
                        .is_none_or(|family| !family.same_container_family(snapshot.service_type))
                {
                    return Err(Error::Conflict(
                        "Captured Docker Agent ownership changed".into(),
                    ));
                }
            }
            // 无管理标签 = 名字反解纳管的存量容器：以精确 CID + 观测名 +
            // 名字反解身份复核归属（标签不可变，出现标签即视为身份变化）。
            None => {
                let observed_name = observed
                    .name
                    .as_deref()
                    .map(|value| value.trim_start_matches('/'))
                    .unwrap_or_default();
                if snapshot.service_type != ServiceType::WebAgentRunner
                    || observed_name != target.name
                    || self.legacy_web_agent_identifier(observed_name, labels)
                        != Some(snapshot.identifier.as_str())
                {
                    return Err(Error::Conflict(
                        "Captured legacy Agent ownership changed".into(),
                    ));
                }
            }
        }
        if automatic {
            validate_automatic_evidence(snapshot, &observed)?;
            if !self
                .persistent_agent_workspace(&observed, snapshot.service_type, &snapshot.identifier)
                .await
            {
                return Err(Error::Conflict(
                    "Docker workspace no longer has deployment-owned persistent coverage".into(),
                ));
            }
        }
        // Never remove volumes or resolve this captured CID again by name.
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.remove_container(
                &target.uid,
                Some(bollard::query_parameters::RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            ),
        )
        .await
        .map_err(|_| Error::Timeout("Captured Agent removal outcome is unknown".into()))?
        .or_else(|error| match error {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => Ok(()),
            error => Err(error),
        })
        .map_err(|error| Error::DockerError(format!("Remove captured Agent: {error}")))?;
        match client.inspect_container(&target.uid, None).await {
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.inner
                    .retire_container_cache(&target.uid)
                    .await
                    .map_err(|error| Error::DockerError(error.to_string()))?;
                Ok(AgentStopOutcome::Complete)
            }
            Ok(_) => Err(Error::Conflict(
                "Captured Agent remains present after removal".into(),
            )),
            Err(error) => Err(Error::DockerError(format!(
                "Confirm captured Agent absence: {error}"
            ))),
        }
    }
}

/// The creation-side workspace mount is also our deployment boundary. A socket,
/// config mount or another deployment's directory never authorizes removal.
impl DockerRuntime {
    async fn persistent_agent_workspace(
        &self,
        inspected: &bollard::models::ContainerInspectResponse,
        family: ServiceType,
        identifier: &str,
    ) -> bool {
        let Some(config) = self
            .inner
            .config
            .multi_image_config
            .get_service_config(&family)
        else {
            return false;
        };
        let resolution = config.effective_workspace_resolution_path();
        let Ok(host_root) =
            crate::path::resolve_container_path_to_host(std::path::Path::new(&resolution)).await
        else {
            return false;
        };
        persistent_agent_workspace_in_scope(inspected, family, identifier, config, &host_root)
    }
}

fn persistent_agent_workspace_in_scope(
    inspected: &bollard::models::ContainerInspectResponse,
    family: ServiceType,
    identifier: &str,
    config: &shared_types::ServiceImageConfig,
    host_root: &std::path::Path,
) -> bool {
    // 无标签容器（名字反解纳管的存量 agent）没有可核对的标记——无标记即
    // 非外来资源；外来标记检查只在标签存在时生效，env + 挂载证据照常裁决。
    let labels = inspected.config.as_ref().and_then(|c| c.labels.as_ref());
    if labels.is_some_and(|labels| {
        labels.contains_key("managed-by")
            || labels.contains_key(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
            || shared_types::USERAPP_RESOURCE_IDENTITY_KEYS
                .iter()
                .any(|key| labels.contains_key(*key))
    }) {
        return false;
    }
    let Some(env) = inspected.config.as_ref().and_then(|c| c.env.as_ref()) else {
        return false;
    };
    let value = |key: &str| env.iter().find_map(|s| s.strip_prefix(&format!("{key}=")));
    let component = |key: &str| {
        value(key).filter(|v| {
            !v.is_empty()
                && std::path::Path::new(v).components().count() == 1
                && matches!(
                    std::path::Path::new(v).components().next(),
                    Some(std::path::Component::Normal(_))
                )
        })
    };
    let target_root = config.workspace_container_path();
    if value("PROJECT_WORKSPACE_BASE").is_some_and(|root| root != target_root) {
        return false;
    }
    let relative = match family {
        ServiceType::WebAgentRunner => {
            let Some(project) = component("PROJECT_ID") else {
                return false;
            };
            if project == identifier {
                std::path::PathBuf::from(project)
            } else {
                // Shared Web workloads use the isolation layout from the
                // original request, never identifier-as-project guessing.
                let Some(tenant) = component("TENANT_ID") else {
                    return false;
                };
                match value("ISOLATION_TYPE") {
                    Some("tenant") => std::path::PathBuf::from(tenant),
                    Some("space") => {
                        let Some(space) = component("SPACE_ID") else {
                            return false;
                        };
                        std::path::Path::new(tenant).join(space)
                    }
                    Some("project") => {
                        let Some(space) = component("SPACE_ID") else {
                            return false;
                        };
                        std::path::Path::new(tenant).join(space).join(project)
                    }
                    _ => return false,
                }
            }
        }
        ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
            let Some(user) = component("USER_ID").filter(|user| *user == identifier) else {
                return false;
            };
            return workspace_is_persistent(
                inspected,
                &host_root.join(user).to_string_lossy(),
                &target_root,
            );
        }
        ServiceType::Userapp | ServiceType::UserappBuilder => return false,
    };
    workspace_is_persistent(
        inspected,
        &host_root.join(&relative).to_string_lossy(),
        &std::path::Path::new(&target_root)
            .join(relative)
            .to_string_lossy(),
    )
}

fn workspace_is_persistent(
    inspected: &bollard::models::ContainerInspectResponse,
    host: &str,
    target: &str,
) -> bool {
    use std::path::{Component, Path};
    let clean = |p: &str| {
        Path::new(p).is_absolute()
            && !Path::new(p)
                .components()
                .any(|c| matches!(c, Component::ParentDir))
    };
    if !clean(host) || !clean(target) {
        return false;
    }
    let mounts = inspected.mounts.as_deref().unwrap_or_default();
    mounts.iter().any(|mount| {
        let (Some(source), Some(destination)) =
            (mount.source.as_deref(), mount.destination.as_deref())
        else {
            return false;
        };
        // Exact creation-side source and destination; a parent mount is not
        // sufficient proof when a child may shadow the actual workspace.
        clean(source)
            && clean(destination)
            && source == host
            && destination == target
            && mount.rw == Some(true)
            && matches!(mount.typ.as_deref(), Some("bind") | Some("volume"))
            && !mounts.iter().any(|other| {
                other
                    .destination
                    .as_deref()
                    .is_some_and(|d| d != target && Path::new(d).starts_with(target))
            })
    })
}

fn validate_automatic_evidence(
    snapshot: &AgentStopSnapshot,
    current: &bollard::models::ContainerInspectResponse,
) -> Result<()> {
    match snapshot.service_type {
        ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject => {}
        ServiceType::Userapp | ServiceType::UserappBuilder => {
            return Err(Error::Conflict(
                "Application compute requires its lifecycle authority".into(),
            ));
        }
    }
    let expected = snapshot
        .container
        .as_ref()
        .and_then(|c| c.resource_version.as_deref());
    let current = current.state.as_ref().and_then(|s| s.started_at.as_deref());
    if expected.is_none_or(str::is_empty)
        || expected != current
        || !snapshot
            .compute_data_protection
            .as_ref()
            .is_some_and(|p| p.persistent_mounts)
    {
        return Err(Error::Conflict(
            "Docker execution or data protection changed before automatic retirement".into(),
        ));
    }
    Ok(())
}
