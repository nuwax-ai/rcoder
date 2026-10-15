//! Fresh Docker observations for both UserApp environments. No registry writes,
//! leases, ensure calls or name-based exec fallbacks.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use bollard::models::{ContainerInspectResponse, ContainerStateStatusEnum as Status};
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, ExecResult,
    USERAPP_DBX_READINESS_COMMAND, USERAPP_READINESS_COMMAND, UserAppReadinessInstance,
    UserAppReadinessTarget, UserAppRuntimeReadiness,
};
use shared_types::{ServiceType, UserAppNoComputeState as State, UserappStage};

use super::docker_runtime::{DockerRuntime, extract_container_ip};

impl DockerRuntime {
    pub(super) async fn inspect_readiness(
        &self,
        app_id: &str,
        stage: UserappStage,
    ) -> Result<UserAppRuntimeReadiness> {
        let service_type = match stage {
            UserappStage::Dev => ServiceType::UserappBuilder,
            UserappStage::Prod => ServiceType::Userapp,
        };
        let name = crate::utils::DockerUtils::generate_container_name(
            service_type.container_prefix(),
            app_id,
        )
        .map_err(Error::ConfigurationError)?;
        let info = match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return Ok(UserAppRuntimeReadiness::NotRunning(State::Missing));
            }
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect readiness target: {error}"
                )));
            }
        };
        Ok(from_inspect(app_id, stage, &info))
    }

    pub(super) async fn exec_readiness(
        &self,
        target: &UserAppReadinessTarget,
    ) -> Result<Option<ExecResult>> {
        self.exec_readiness_command(target, &USERAPP_READINESS_COMMAND)
            .await
    }

    pub(super) async fn exec_dbx_readiness(
        &self,
        target: &UserAppReadinessTarget,
    ) -> Result<Option<ExecResult>> {
        self.exec_readiness_command(target, &USERAPP_DBX_READINESS_COMMAND)
            .await
    }

    async fn exec_readiness_command(
        &self,
        target: &UserAppReadinessTarget,
        command: &[&str],
    ) -> Result<Option<ExecResult>> {
        let UserAppReadinessInstance::Docker { container_id, .. } = &target.instance else {
            return Err(Error::ConfigurationError(
                "Expected Docker readiness target".into(),
            ));
        };
        if self.inspect_readiness(&target.app_id, target.stage).await?
            != UserAppRuntimeReadiness::Running(Box::new(target.clone()))
        {
            return Ok(None);
        }
        let result = super::docker_app_runtime::execute_container_command(
            self.inner.get_docker_client(),
            container_id,
            command.iter().map(|arg| (*arg).to_owned()).collect(),
        )
        .await;
        // A concurrent stop/removal is an observation change, not a query-system
        // failure. Never repeat the command in a replacement chosen by name.
        if self.inspect_readiness(&target.app_id, target.stage).await?
            != UserAppRuntimeReadiness::Running(Box::new(target.clone()))
        {
            return Ok(None);
        }
        result.map(Some)
    }
}

fn from_inspect(
    app_id: &str,
    stage: UserappStage,
    info: &ContainerInspectResponse,
) -> UserAppRuntimeReadiness {
    let not_running = UserAppRuntimeReadiness::NotRunning;
    let Some(labels) = info
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
    else {
        return not_running(State::Unknown);
    };
    let (service_type, id_label) = match stage {
        UserappStage::Dev => (ServiceType::UserappBuilder, "identifier"),
        UserappStage::Prod => (
            ServiceType::Userapp,
            shared_types::USERAPP_DOCKER_APP_ID_LABEL,
        ),
    };
    if labels.get("service-type").map(String::as_str) != Some(service_type.to_string().as_str())
        || labels.get(id_label).map(String::as_str) != Some(app_id)
    {
        return not_running(State::Unknown);
    }
    let Some(state) = info.state.as_ref() else {
        return not_running(State::Unknown);
    };
    match state.status {
        Some(Status::CREATED | Status::RESTARTING) => return not_running(State::Starting),
        Some(Status::EXITED) => {
            return not_running(
                if state.oom_killed == Some(true) || state.exit_code.is_some_and(|code| code != 0) {
                    State::Failed
                } else {
                    State::Stopped
                },
            );
        }
        Some(Status::DEAD) => return not_running(State::Failed),
        Some(Status::RUNNING) if state.running == Some(true) => {}
        _ => return not_running(State::Unknown),
    }
    let Some(id) = info.id.as_ref().filter(|id| !id.is_empty()) else {
        return not_running(State::Unknown);
    };
    let preferred = info
        .host_config
        .as_ref()
        .and_then(|config| config.network_mode.as_deref());
    let address = extract_container_ip(info, preferred)
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| SocketAddr::new(ip, shared_types::APP_CLI_ADMIN_PORT));
    UserAppRuntimeReadiness::Running(Box::new(UserAppReadinessTarget {
        app_id: app_id.into(),
        stage,
        instance: UserAppReadinessInstance::Docker {
            container_id: id.clone(),
            started_at: state.started_at.clone(),
        },
        address,
        published_address: published_address(info, shared_types::APP_CLI_ADMIN_PORT),
        dbx_published_address: published_address(info, shared_types::DBX_PORT),
    }))
}

fn published_address(info: &ContainerInspectResponse, container_port: u16) -> Option<SocketAddr> {
    info.network_settings
        .as_ref()
        .and_then(|network| network.ports.as_ref())
        .and_then(|ports| ports.get(&format!("{container_port}/tcp")))
        .and_then(Option::as_ref)
        .and_then(|bindings| {
            bindings.iter().find_map(|binding| {
                let port = binding
                    .host_port
                    .as_deref()?
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)?;
                let ip = binding.host_ip.as_deref()?.parse::<IpAddr>().ok()?;
                let ip = if ip.is_unspecified() {
                    if ip.is_ipv4() {
                        IpAddr::V4(Ipv4Addr::LOCALHOST)
                    } else {
                        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                    }
                } else {
                    ip
                };
                Some(SocketAddr::new(ip, port))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_readiness_distinguishes_stop_start_failure_and_actual_addresses() {
        let mut object = serde_json::json!({
            "Id":"builder-id", "Config":{"Labels":{"service-type":"userapp-builder","identifier":"194"}},
            "State":{"Status":"exited","Running":false,"ExitCode":0},
            "HostConfig":{"NetworkMode":"app-network"},
            "NetworkSettings":{"Networks":{"app-network":{"IPAddress":"172.21.0.5"}},
                "Ports":{"3010/tcp":[{"HostIp":"0.0.0.0","HostPort":"33010"}],
                    "4224/tcp":[{"HostIp":"::","HostPort":"34224"}]}}
        });
        // Use the actual service type wire name, not a duplicated test convention.
        object["Config"]["Labels"]["service-type"] = ServiceType::UserappBuilder.to_string().into();
        for (status, running, exit, expected) in [
            ("exited", false, 0, State::Stopped),
            ("exited", false, 137, State::Failed),
            ("created", false, 0, State::Starting),
            ("restarting", true, 1, State::Starting),
        ] {
            object["State"] =
                serde_json::json!({"Status":status,"Running":running,"ExitCode":exit});
            let info = serde_json::from_value(object.clone()).unwrap();
            assert_eq!(
                from_inspect("194", UserappStage::Dev, &info),
                UserAppRuntimeReadiness::NotRunning(expected)
            );
        }
        object["State"] = serde_json::json!({"Status":"running","Running":true,"StartedAt":"2026-09-28T00:00:00Z"});
        let info = serde_json::from_value(object).unwrap();
        let UserAppRuntimeReadiness::Running(target) =
            from_inspect("194", UserappStage::Dev, &info)
        else {
            panic!("expected builder target")
        };
        assert_eq!(target.instance.physical_id(), "builder-id");
        assert_eq!(target.address.unwrap().to_string(), "172.21.0.5:3010");
        assert_eq!(
            target.published_address.unwrap().to_string(),
            "127.0.0.1:33010"
        );
        assert_eq!(
            target.dbx_published_address.unwrap().to_string(),
            "[::1]:34224"
        );
        assert_eq!(
            from_inspect("194", UserappStage::Prod, &info),
            UserAppRuntimeReadiness::NotRunning(State::Unknown)
        );
    }
}
