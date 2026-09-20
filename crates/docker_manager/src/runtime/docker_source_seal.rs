use super::{docker_runtime::DockerRuntime, source_seal as seal};
use bollard::{
    models::{ContainerCreateBody, HostConfig},
    query_parameters::{
        CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    },
};
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, OfflineSourceSealTarget,
};
use futures_util::StreamExt;
use shared_types::{RuntimeGenerationHandoff, RuntimeGenerationSourceSeal, UserAppMutationTarget};

impl DockerRuntime {
    async fn stopped_seal_source(
        &self,
        source: &UserAppMutationTarget,
    ) -> Result<bollard::models::ContainerInspectResponse> {
        let current = self.capture_stop_target(&source.context).await?;
        if current.resource.uid != source.resource.uid
            || current.resource.name != source.resource.name
        {
            return Err(seal::invalid("Offline source container changed"));
        }
        let info = self
            .inner
            .get_docker_client()
            .inspect_container(&source.resource.uid, None)
            .await
            .map_err(|_| seal::invalid("Cannot inspect original stopped container"))?;
        let state = info
            .state
            .as_ref()
            .ok_or_else(|| seal::invalid("Stopped container state is missing"))?;
        if state.running != Some(false)
            || state.restarting == Some(true)
            || state.paused == Some(true)
        {
            return Err(seal::invalid("Offline source container is not stopped"));
        }
        Ok(info)
    }
    pub(super) async fn prepare_offline_seal(
        &self,
        source: &UserAppMutationTarget,
        auth: &RuntimeGenerationHandoff,
    ) -> Result<OfflineSourceSealTarget> {
        seal::validate(source, auth)?;
        let info = self.stopped_seal_source(source).await?;
        let config = info
            .config
            .as_ref()
            .ok_or_else(|| seal::invalid("Source configuration missing"))?;
        let env = config
            .env
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(|value| value.split_once('=').map(|(k, v)| (k.into(), v.into())))
            .collect();
        let (workspace, selected_env) = seal::environment(&env, auth)?;
        let state_root = selected_env
            .iter()
            .find_map(|entry| entry.strip_prefix("APP_CLI_STATE_ROOT="))
            .ok_or_else(|| seal::invalid("Source state root missing"))?;
        if !info.mounts.as_ref().is_some_and(|mounts| {
            mounts.iter().any(|m| {
                m.destination.as_ref().is_some_and(|path| {
                    state_root == path || state_root.starts_with(&format!("{path}/"))
                })
            })
        }) {
            return Err(seal::invalid(
                "Source state root is outside captured volumes",
            ));
        }
        if !info.mounts.as_ref().is_some_and(|mounts| {
            mounts.iter().any(|m| {
                m.destination.as_ref().is_some_and(|path| {
                    workspace == *path || workspace.starts_with(&format!("{path}/"))
                })
            })
        }) {
            return Err(seal::invalid(
                "Source workspace is not on a captured volume",
            ));
        }
        let command = seal::command(&workspace, auth)?;
        let image = info
            .image
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| seal::invalid("Source immutable image is missing"))?;
        let image_info = self
            .inner
            .get_docker_client()
            .inspect_image(&image)
            .await
            .map_err(|_| seal::invalid("Cannot inspect source immutable image"))?;
        let mut merged_env: std::collections::BTreeMap<String, String> = image_info
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.split_once('=').map(|(k, v)| (k.into(), v.into())))
            .collect();
        for entry in selected_env {
            if let Some((key, value)) = entry.split_once('=') {
                merged_env.insert(key.into(), value.into());
            }
        }
        let env: Vec<String> = merged_env
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let mounts = mount_identity(info.mounts.as_deref().unwrap_or_default());
        let name = seal::name(auth)?;
        let specification = serde_json::json!({"image":image,"env":env,"entrypoint":command,"volumes_from":[format!("{}:rw",source.resource.uid)],"network_mode":"none","mounts":mounts,"cmd":[]});
        let client = self.inner.get_docker_client();
        match client.inspect_container(&name, None).await {
            Ok(_) => {}
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                let body = ContainerCreateBody {
                    image: Some(image),
                    env: Some(env),
                    entrypoint: Some(command),
                    cmd: Some(vec![]),
                    host_config: Some(HostConfig {
                        volumes_from: Some(vec![format!("{}:rw", source.resource.uid)]),
                        network_mode: Some("none".into()),
                        ..Default::default()
                    }),
                    labels: Some([("rcoder.offline-seal".into(), name.clone())].into()),
                    ..Default::default()
                };
                client
                    .create_container(
                        Some(CreateContainerOptions {
                            name: Some(name.clone()),
                            platform: String::new(),
                        }),
                        body,
                    )
                    .await
                    .map_err(|_| {
                        seal::invalid(
                            "Offline helper creation result is unknown; recover original helper",
                        )
                    })?;
            }
            Err(_) => return Err(seal::invalid("Cannot resolve original offline helper")),
        }
        let helper = client
            .inspect_container(&name, None)
            .await
            .map_err(|_| seal::invalid("Cannot inspect original offline helper"))?;
        let target = OfflineSourceSealTarget {
            source: source.clone(),
            helper_name: name,
            helper_uid: helper
                .id
                .clone()
                .ok_or_else(|| seal::invalid("Offline helper UID missing"))?,
            authorization: auth.clone(),
            specification,
        };
        validate_helper(&helper, &target)?;
        Ok(target)
    }
    pub(super) async fn run_offline_seal(
        &self,
        target: &OfflineSourceSealTarget,
    ) -> Result<RuntimeGenerationSourceSeal> {
        seal::validate(&target.source, &target.authorization)?;
        self.stopped_seal_source(&target.source).await?;
        let client = self.inner.get_docker_client();
        loop {
            let helper = client
                .inspect_container(&target.helper_uid, None)
                .await
                .map_err(|_| seal::invalid("Original offline helper is unavailable"))?;
            validate_helper(&helper, target)?;
            let state = helper
                .state
                .as_ref()
                .ok_or_else(|| seal::invalid("Offline helper state missing"))?;
            let status = state
                .status
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            if status == "created" {
                client
                    .start_container(&target.helper_uid, None::<StartContainerOptions>)
                    .await
                    .map_err(|_| seal::invalid("Offline helper start outcome unknown"))?;
            } else if status == "exited" {
                if state.exit_code != Some(0) {
                    return Err(seal::invalid(
                        "Offline helper failed; original operation remains protected",
                    ));
                }
                let mut logs = client.logs(
                    &target.helper_uid,
                    Some(LogsOptions {
                        stdout: true,
                        stderr: false,
                        ..Default::default()
                    }),
                );
                let mut text = String::new();
                while let Some(log) = logs.next().await {
                    text.push_str(
                        &log.map_err(|_| seal::invalid("Offline helper receipt unavailable"))?
                            .to_string(),
                    );
                    if text.len() > 65536 {
                        return Err(seal::invalid("Offline helper receipt is oversized"));
                    }
                }
                return seal::receipt(&text, &target.authorization);
            } else if status != "running" {
                return Err(seal::invalid("Offline helper has an unexpected state"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    pub(super) async fn cleanup_offline_seal(
        &self,
        target: &OfflineSourceSealTarget,
    ) -> Result<()> {
        let client = self.inner.get_docker_client();
        let info = match client.inspect_container(&target.helper_uid, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(()),
            Err(_) => return Err(seal::invalid("Cannot inspect helper for cleanup")),
        };
        validate_helper(&info, target)?;
        if info.state.as_ref().and_then(|state| state.running) != Some(false) {
            return Err(seal::invalid("Cannot clean a running offline helper"));
        }
        client
            .remove_container(
                &target.helper_uid,
                Some(RemoveContainerOptions {
                    force: false,
                    v: false,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| seal::invalid("Offline helper cleanup outcome unknown"))
    }
}
fn validate_helper(
    info: &bollard::models::ContainerInspectResponse,
    target: &OfflineSourceSealTarget,
) -> Result<()> {
    let cfg = info
        .config
        .as_ref()
        .ok_or_else(|| seal::invalid("Helper configuration missing"))?;
    let host = info
        .host_config
        .as_ref()
        .ok_or_else(|| seal::invalid("Helper host configuration missing"))?;
    let mut env = cfg.env.clone().unwrap_or_default();
    env.sort();
    let actual = serde_json::json!({"image":info.image,"env":env,"entrypoint":cfg.entrypoint,"volumes_from":host.volumes_from,"network_mode":host.network_mode,"mounts":mount_identity(info.mounts.as_deref().unwrap_or_default()),"cmd":cfg.cmd.clone().unwrap_or_default()});
    if host.privileged == Some(true)
        || host.pid_mode.as_ref().is_some_and(|s| !s.is_empty())
        || host.binds.as_ref().is_some_and(|v| !v.is_empty())
        || host.devices.as_ref().is_some_and(|v| !v.is_empty())
        || host.mounts.as_ref().is_some_and(|v| !v.is_empty())
    {
        return Err(seal::invalid(
            "Offline helper gained unapproved host access",
        ));
    }
    if info.id.as_deref() != Some(&target.helper_uid)
        || actual != target.specification
        || cfg
            .labels
            .as_ref()
            .and_then(|l| l.get("rcoder.offline-seal"))
            != Some(&target.helper_name)
    {
        return Err(Error::Conflict(
            "Offline helper immutable identity/specification mismatch".into(),
        ));
    }
    Ok(())
}

fn mount_identity(mounts: &[bollard::models::MountPoint]) -> Vec<serde_json::Value> {
    let mut values:Vec<_>=mounts.iter().map(|m|serde_json::json!({"type":m.typ,"source":m.source,"destination":m.destination,"name":m.name,"rw":m.rw})).collect();
    values.sort_by_key(|value| value.to_string());
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_container_verifier_rejects_command_and_host_access_changes() {
        let value = serde_json::json!({"Id":"helperuid","Image":"sha256:image","Config":{"Image":"sha256:image","Env":["PROJECT_ID=sealapp"],"Entrypoint":["app-cli","seal-source"],"Cmd":[],"Labels":{"rcoder.offline-seal":"helper"}},"HostConfig":{"VolumesFrom":["sourceuid:rw"],"NetworkMode":"none"},"Mounts":[]});
        let target = seal::test_target(
            serde_json::json!({"image":"sha256:image","env":["PROJECT_ID=sealapp"],"entrypoint":["app-cli","seal-source"],"cmd":[],"volumes_from":["sourceuid:rw"],"network_mode":"none","mounts":[]}),
        );
        assert!(validate_helper(&serde_json::from_value(value.clone()).unwrap(), &target).is_ok());
        for mutation in ["cmd", "pid", "privileged", "mount", "env", "uid"] {
            let mut changed = value.clone();
            match mutation {
                "cmd" => changed["Config"]["Cmd"] = serde_json::json!(["serve"]),
                "pid" => changed["HostConfig"]["PidMode"] = "host".into(),
                "privileged" => changed["HostConfig"]["Privileged"] = true.into(),
                "mount" => changed["HostConfig"]["Binds"] = serde_json::json!(["/:/host"]),
                "env" => changed["Config"]["Env"] = serde_json::json!(["PROJECT_ID=other"]),
                "uid" => changed["Id"] = "otheruid".into(),
                _ => unreachable!(),
            }
            assert!(
                validate_helper(&serde_json::from_value(changed).unwrap(), &target).is_err(),
                "{mutation}"
            );
        }
    }
}
