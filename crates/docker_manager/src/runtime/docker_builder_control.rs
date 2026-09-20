//! Compute-only operations against immutable Docker IDs. No storage deletion.
use super::docker_runtime::DockerRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{
    BuilderControlTarget, ContainerBasicInfo, ServiceType, UserAppExecutionContext,
};

impl DockerRuntime {
    pub(super) async fn reconcile_builder_start(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<ContainerBasicInfo>> {
        if !super::docker_compute_receipt::matches_start(target).await? {
            return Ok(None);
        }
        let Some(resource) = &target.workload else {
            return Ok(None);
        };
        let info = self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| {
                Error::DockerError(format!("Observe acknowledged builder start: {error}"))
            })?;
        if control_identity_with_binding(
            &info,
            &resource.name,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )? != *resource
        {
            return Err(Error::Conflict(
                "Acknowledged builder identity changed".into(),
            ));
        }
        if !info
            .state
            .as_ref()
            .is_some_and(|state| state.running == Some(true) && state.restarting != Some(true))
        {
            return Ok(None);
        }
        running_builder_info(&info, target)
    }
    pub(super) async fn reconcile_builder_stop(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<bool> {
        if !super::docker_compute_receipt::matches_stop(target).await? {
            return Ok(false);
        }
        let Some(resource) = &target.workload else {
            return Ok(false);
        };
        if resource.kind != shared_types::AppResourceKind::Container {
            return Ok(false);
        }
        match self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
        {
            Ok(info) => Ok(control_identity_with_binding(
                &info,
                &resource.name,
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )? == *resource
                && info.state.as_ref().and_then(|state| state.running) == Some(false)),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(true),
            Err(error) => Err(Error::DockerError(format!(
                "Reconcile Docker builder stop: {error}"
            ))),
        }
    }
    pub(super) async fn exec_bound_builder(
        &self,
        target: &BuilderControlTarget,
        command: Vec<String>,
    ) -> Result<container_runtime_api::ExecResult> {
        target.validate().map_err(Error::Conflict)?;
        let resource = target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Captured builder is absent".into()))?;
        if resource.kind != shared_types::AppResourceKind::Container || command.is_empty() {
            return Err(Error::ConfigurationError(
                "Builder exec requires a container and command".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let before = client
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| Error::DockerError(format!("Inspect builder exec target: {error}")))?;
        let actual = control_identity_with_binding(
            &before,
            &resource.name,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )?;
        if actual != *resource {
            return Err(Error::Conflict(
                "Captured builder exec identity changed".into(),
            ));
        }
        super::docker_app_runtime::execute_container_command(client, &resource.uid, command).await
    }

    /// Resume the captured adopted container; Docker labels/config are immutable.
    /// This never runs create, removes storage, or restarts an already-running app.
    pub(super) async fn resume_bound_builder(
        &self,
        params: &container_runtime_api::ContainerCreateParams,
        expected_image: &str,
    ) -> Result<ContainerBasicInfo> {
        let check_cancelled = || {
            if params
                .creation_cancelled
                .load(std::sync::atomic::Ordering::Acquire)
            {
                Err(Error::CreationCancelled)
            } else {
                Ok(())
            }
        };
        check_cancelled()?;
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
            let resource = target.workload.as_ref().ok_or_else(|| {
                Error::Conflict(
                    "Bound builder disappeared; automatic replacement is forbidden".into(),
                )
            })?;
            let info = self
                .inner
                .get_docker_client()
                .inspect_container(&resource.uid, None)
                .await
                .map_err(|error| {
                    Error::DockerError(format!("Inspect bound builder configuration: {error}"))
                })?;
            if info
                .config
                .as_ref()
                .and_then(|config| config.image.as_deref())
                != Some(expected_image)
            {
                return Err(Error::Conflict(
                    "Bound builder image configuration changed".into(),
                ));
            }
            Ok::<_, Error>(target)
        }
        .await
        .map_err(|error| rejected_before_write(error.to_string()))?;
        check_cancelled()?;
        // Await the start and its receipt before acknowledging cancellation.
        // Dropping a Docker mutation future does not cancel the daemon's write.
        let result = self.apply_builder_compute_mode(&target, true, true).await?;
        check_cancelled()?;
        result.ok_or_else(|| {
            Error::Conflict("Bound builder did not return a running container".into())
        })
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
        let name = crate::utils::DockerUtils::generate_container_name(
            ServiceType::UserappBuilder.container_prefix(),
            &context.app_id,
        )
        .map_err(Error::ConfigurationError)?;
        let workload = match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(info) => Some(control_identity_with_binding(
                &info, &name, context, binding, adoption,
            )?),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => None,
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect builder control target: {error}"
                )));
            }
        };
        Ok(BuilderControlTarget {
            resource_binding: binding.cloned(),
            context: context.clone(),
            workload,
            pod: None,
        })
    }

    /// Continue a compute restart after its stop boundary. Starting an already
    /// running captured container is idempotent; it must not restart it again.
    pub(super) async fn start_builder_compute(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, true, true).await
    }

    pub(super) async fn apply_builder_compute(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, restart, false)
            .await
    }

    async fn apply_builder_compute_mode(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(rejected_before_write)?;
        let Some(resource) = &target.workload else {
            return Ok(None);
        };
        if resource.kind != shared_types::AppResourceKind::Container {
            return Err(rejected_before_write(
                "Non-Docker builder control target".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let before = match client.inspect_container(&resource.uid, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if !restart => return Ok(None),
            Err(error) => {
                return Err(rejected_before_write(format!(
                    "Inspect captured builder before mutation: {error}"
                )));
            }
        };
        if control_identity_with_binding(
            &before,
            &resource.name,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )
        .map_err(|error| rejected_before_write(error.to_string()))?
            != *resource
        {
            return Err(rejected_before_write(
                "Builder control identity changed".into(),
            ));
        }
        if restart && only_start {
            match client
                .start_container(
                    &resource.uid,
                    None::<bollard::query_parameters::StartContainerOptions>,
                )
                .await
            {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 304, ..
                }) => {}
                Err(error) => return Err(first_write_error(error)),
            }
        } else if restart {
            client
                .restart_container(
                    &resource.uid,
                    Some(bollard::query_parameters::RestartContainerOptions {
                        t: Some(30),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(first_write_error)?;
        } else {
            match client
                .stop_container(
                    &resource.uid,
                    Some(bollard::query_parameters::StopContainerOptions {
                        t: Some(30),
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 304 | 404,
                    ..
                }) => {}
                Err(error) => {
                    return Err(first_write_error(error));
                }
            }
        }
        let after = match client.inspect_container(&resource.uid, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) if !restart => {
                // Auto-remove already deleted the stopped builder. The Docker
                // stop acknowledgement is still the causal evidence recovery
                // observers match on; persist it before reporting success.
                super::docker_compute_receipt::save_stop(target).await?;
                return Ok(None);
            }
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Confirm builder control: {error}"
                )));
            }
        };
        if control_identity_with_binding(
            &after,
            &resource.name,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )? != *resource
        {
            return Err(Error::Conflict(
                "Builder control identity changed after mutation".into(),
            ));
        }
        let running = after
            .state
            .as_ref()
            .and_then(|state| state.running)
            .ok_or_else(|| Error::Conflict("Builder running state is unavailable".into()))?;
        if running != restart {
            return Err(Error::Conflict(
                "Builder control result does not match requested state".into(),
            ));
        }
        if !restart {
            super::docker_compute_receipt::save_stop(target).await?;
            return Ok(None);
        }
        if only_start {
            super::docker_compute_receipt::save_start(target).await?;
        }
        running_builder_info(&after, target)
    }
}

fn running_builder_info(
    after: &bollard::models::ContainerInspectResponse,
    target: &BuilderControlTarget,
) -> Result<Option<ContainerBasicInfo>> {
    let resource = target
        .workload
        .as_ref()
        .ok_or_else(|| Error::Conflict("Builder identity missing".into()))?;
    let preferred = after
        .host_config
        .as_ref()
        .and_then(|config| config.network_mode.as_deref());
    let address = super::docker_runtime::extract_container_ip(&after, preferred)
        .parse::<std::net::IpAddr>()
        .map_err(|error| Error::ConfigurationError(format!("Builder IP is invalid: {error}")))?;
    if address.is_unspecified() {
        return Err(Error::ConfigurationError(
            "Builder IP is unspecified".into(),
        ));
    }
    let created = after
        .created
        .as_deref()
        .ok_or_else(|| Error::ConfigurationError("Builder creation time is missing".into()))?;
    let created_at = chrono::DateTime::parse_from_rfc3339(created)
        .map_err(|error| {
            Error::ConfigurationError(format!("Builder creation time is invalid: {error}"))
        })?
        .with_timezone(&chrono::Utc);
    Ok(Some(ContainerBasicInfo {
        container_id: resource.uid.clone(),
        container_name: resource.name.clone(),
        container_ip: address.to_string(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: target.context.app_id.clone(),
        status: "Running".into(),
        created_at,
        service_url: format!("http://{address}:{}", shared_types::GRPC_DEFAULT_PORT),
    }))
}

fn rejected_before_write(message: String) -> Error {
    Error::RequestRejected(shared_types::RuntimeRequestRejection {
        status: 409,
        message,
    })
}
fn first_write_error(error: bollard::errors::Error) -> Error {
    super::builder_completion::docker_error(crate::DockerError::BollardError(error))
}

#[cfg(test)]
fn control_identity(
    info: &bollard::models::ContainerInspectResponse,
    name: &str,
    context: &UserAppExecutionContext,
) -> Result<shared_types::AppResourceIdentity> {
    control_identity_with_binding(info, name, context, None, false)
}

pub(super) fn control_identity_with_binding(
    info: &bollard::models::ContainerInspectResponse,
    name: &str,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    adoption: bool,
) -> Result<shared_types::AppResourceIdentity> {
    let identity =
        super::docker_builder_deletion::builder_identity(info.clone(), name, &context.app_id)?;
    let labels = info
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| Error::Conflict("Builder labels missing".into()))?;
    let metadata = labels
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !shared_types::builder_identity_is_bound(context, &metadata, &identity.uid, binding)
        .map_err(Error::Conflict)?
        && !adoption
    {
        return Err(Error::Conflict(
            "Builder requires explicit physical resource adoption".into(),
        ));
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bound_wake_starts_only_the_captured_id_and_accepts_already_running() {
        for start_status in [204, 304, 403] {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let context = UserAppExecutionContext {
                app_id: "app".into(),
                lifecycle_id: "life".into(),
                operation_id: "wake".into(),
                executor_id: "worker".into(),
                request_fingerprint: "a".repeat(64),
            };
            let mut labels: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            labels.insert(
                "service-type".into(),
                ServiceType::UserappBuilder.to_string(),
            );
            labels.insert("identifier".into(), "app".into());
            let object = serde_json::json!({"Id":"original-id","Created":"2026-01-01T00:00:00Z","Config":{"Labels":labels,"Env":["USER_ID=owner"]},"State":{"Running":true},"HostConfig":{"NetworkMode":"test"},"NetworkSettings":{"Networks":{"test":{"IPAddress":"172.20.0.4"}}}});
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                for index in 0..if start_status == 403 { 2 } else { 3 } {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 2048];
                    loop {
                        let n = stream.read(&mut buffer).await.expect("read");
                        assert!(n > 0);
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let headers = String::from_utf8_lossy(&bytes);
                    let request = headers.lines().next().expect("request");
                    assert!(
                        !request.contains("/restart")
                            && !request.contains("/create")
                            && !request.starts_with("DELETE ")
                    );
                    let (code, body) = if index == 1 {
                        assert!(
                            request.starts_with("POST ")
                                && request.contains("/containers/original-id/start")
                        );
                        (
                            start_status,
                            if start_status == 403 {
                                "{\"message\":\"denied\"}".to_owned()
                            } else {
                                String::new()
                            },
                        )
                    } else {
                        assert!(
                            request.starts_with("GET ")
                                && request.contains("/containers/original-id/json")
                        );
                        (200, object.to_string())
                    };
                    stream.write_all(format!("HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.expect("respond");
                }
            });
            let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
            let actor_task = tokio::spawn(actor.run());
            let manager = std::sync::Arc::new(crate::DockerManager {
                docker: bollard::Docker::connect_with_http(
                    &format!("http://{address}"),
                    1,
                    bollard::API_DEFAULT_VERSION,
                )
                .expect("client"),
                config: crate::DockerManagerConfig::default(),
                containers,
                main_network_name: std::sync::Arc::new(tokio::sync::RwLock::new("test".into())),
                api_cache: std::sync::Arc::new(crate::api_cache::DockerApiCache::new(
                    600, 600, 100,
                )),
            });
            let runtime = DockerRuntime::new(manager);
            let target = BuilderControlTarget {
                context,
                resource_binding: Some(shared_types::UserAppResourceBinding {
                    app_id: "app".into(),
                    lifecycle_id: "life".into(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: "original-id".into(),
                    adopted_by_operation: "adopt".into(),
                }),
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::Container,
                    name: "builder-app".into(),
                    uid: "original-id".into(),
                    resource_version: None,
                }),
                pod: None,
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let result = runtime
                    .apply_builder_compute_mode(&target, true, true)
                    .await;
                if start_status == 403 {
                    assert!(matches!(result, Err(Error::RequestRejected(_))));
                } else {
                    assert_eq!(
                        result.expect("wake").expect("container").container_id,
                        "original-id"
                    );
                }
                server.await.expect("server");
            })
            .await
            .expect("total contract deadline");
            actor_task.abort();
            if let Err(error) = actor_task.await {
                assert!(error.is_cancelled(), "actor failed before cleanup: {error}");
            }
        }
    }

    #[test]
    fn control_identity_rejects_owner_lifecycle_and_family_changes() {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "stop".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut labels = context.resource_metadata();
        labels.insert(
            "service-type".into(),
            ServiceType::UserappBuilder.to_string(),
        );
        labels.insert("identifier".into(), "app".into());
        let info = serde_json::from_value(
            serde_json::json!({"Id":"actual-id", "Config":{"Labels":labels}}),
        )
        .expect("inspect");
        assert_eq!(
            control_identity(&info, "builder", &context)
                .expect("identity")
                .uid,
            "actual-id"
        );
        let mut wrong = context.clone();
        wrong.lifecycle_id = "new-life".into();
        assert!(control_identity(&info, "builder", &wrong).is_err());
        wrong = context.clone();
        wrong.app_id = "other".into();
        assert!(control_identity(&info, "builder", &wrong).is_err());
        labels.insert("service-type".into(), ServiceType::Userapp.to_string());
        let info = serde_json::from_value(
            serde_json::json!({"Id":"actual-id", "Config":{"Labels":labels}}),
        )
        .expect("inspect");
        assert!(control_identity(&info, "builder", &context).is_err());
    }
}
