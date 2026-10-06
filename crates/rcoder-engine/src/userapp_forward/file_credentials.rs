//! File-peer credentials from existing platform and actual container inputs.
use crate::app_state::AppState;
use shared_types::{
    FileServerCredentialsProvider, FileServerRequestCredentials, UserappStage, WakeFailure,
};
use std::sync::Weak;

pub(crate) struct ConfiguredFileCredentials(Weak<AppState>);
impl ConfiguredFileCredentials {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self(state)
    }
}
#[async_trait::async_trait]
impl FileServerCredentialsProvider for ConfiguredFileCredentials {
    async fn for_target(
        &self,
        stage: UserappStage,
        app_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<FileServerRequestCredentials, WakeFailure> {
        let state = self.0.upgrade().ok_or_else(|| {
            WakeFailure::new(
                shared_types::ERR_RUNTIME_UNAVAILABLE,
                "internal_http_configuration",
                "Internal HTTP host state is unavailable",
            )
        })?;
        credentials(&state, stage, app_id, deadline).await
    }
}

pub async fn credentials(
    state: &AppState,
    stage: UserappStage,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<FileServerRequestCredentials, WakeFailure> {
    let configured = state
        .config
        .file_server_proxy
        .as_ref()
        .and_then(|config| config.auth_token.clone());
    let mut token = file_server_proxy::FileServerProxyConfig::resolve_auth_token(
        configured,
        std::env::var_os("FILE_SERVER_PROXY_TOKEN"),
    )
    .map_err(|message| {
        WakeFailure::new(
            shared_types::ERR_RUNTIME_CONFIGURATION,
            "internal_http_configuration",
            message,
        )
    })?;
    let environment = match stage {
        UserappStage::Dev => {
            if shared_types::is_kubernetes_runtime() {
                state
                    .config
                    .kubernetes_config
                    .as_ref()
                    .and_then(|config| {
                        config.get_service_config(&shared_types::ServiceType::UserappBuilder)
                    })
                    .map(|config| config.environment.clone())
            } else {
                state.config.docker_config.as_ref().and_then(|config| {
                    config
                        .get_multi_image_config()
                        .get_service_config(&shared_types::ServiceType::UserappBuilder)
                        .map(|config| config.environment.clone())
                })
            }
        }
        UserappStage::Prod => {
            production_environment(state.runtime().as_ref(), app_id, deadline).await?
        }
    };
    if let Some(environment) = environment
        && let Some(value) = environment.get("FILE_SERVER_PROXY_TOKEN")
    {
        token = Some(value.trim().to_owned()).filter(|value| !value.is_empty());
    }
    Ok(FileServerRequestCredentials { proxy_token: token })
}

async fn production_environment(
    runtime: &dyn container_runtime_api::UserAppDeploymentRuntime,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<std::collections::HashMap<String, String>>, WakeFailure> {
    let spec = tokio::time::timeout_at(deadline, runtime.get_app_container_spec(app_id))
        .await
        .map_err(|_| WakeFailure::timeout("file_credentials_configuration", None, true))?
        .map_err(|error| {
            let (code, summary) = container_runtime_api::runtime_error_code(&error);
            let mut failure = WakeFailure::new(code, "file_credentials_configuration", summary);
            failure.retryable = matches!(
                code,
                shared_types::ERR_RUNTIME_UNAVAILABLE | shared_types::ERR_RUNTIME_TIMEOUT
            );
            let mut origin = &error;
            while let container_runtime_api::ContainerRuntimeError::CreationAborted {
                source, ..
            } = origin
            {
                origin = source;
            }
            if let container_runtime_api::ContainerRuntimeError::OperationInProgress(operation) =
                origin
            {
                failure.operation_id = operation.operation_id.clone();
            }
            failure
        })?;
    Ok(spec.env)
}

#[cfg(test)]
mod prod_configuration_tests {
    use super::*;
    use container_runtime_api::{
        ContainerRuntimeError, ContainerRuntimeResult, ContainerSpecSnapshot,
        UserAppDeploymentRuntime,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct SpecRuntime {
        result: std::sync::Mutex<Option<ContainerRuntimeResult<ContainerSpecSnapshot>>>,
        calls: AtomicUsize,
        delayed: bool,
    }
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for SpecRuntime {
        async fn get_app_container_spec(
            &self,
            app_id: &str,
        ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
            assert_eq!(app_id, "fixtureapp");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delayed {
                std::future::pending::<()>().await;
            }
            self.result
                .lock()
                .expect("fixture mutex")
                .take()
                .expect("exactly one runtime read")
        }
    }
    #[tokio::test]
    async fn production_credentials_read_actual_container_environment_and_do_not_ignore_query_failures()
     {
        let runtime = SpecRuntime {
            result: std::sync::Mutex::new(Some(Ok(ContainerSpecSnapshot {
                env: Some(std::collections::HashMap::from([(
                    "FILE_SERVER_PROXY_TOKEN".into(),
                    "existing-container-token".into(),
                )])),
                ..Default::default()
            }))),
            calls: AtomicUsize::new(0),
            delayed: false,
        };
        let env = production_environment(
            &runtime,
            "fixtureapp",
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .expect("actual spec")
        .expect("configured env");
        assert_eq!(env["FILE_SERVER_PROXY_TOKEN"], "existing-container-token");
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        for (error, expected) in [
            (
                ContainerRuntimeError::Timeout("query timed out".into()),
                shared_types::ERR_RUNTIME_TIMEOUT,
            ),
            (
                ContainerRuntimeError::ConnectionError("runtime unavailable".into()),
                shared_types::ERR_RUNTIME_UNAVAILABLE,
            ),
            (
                ContainerRuntimeError::ContainerNotFound("actual resource absent".into()),
                shared_types::ERR_CONTAINER_NOT_FOUND,
            ),
        ] {
            let runtime = SpecRuntime {
                result: std::sync::Mutex::new(Some(Err(error))),
                calls: AtomicUsize::new(0),
                delayed: false,
            };
            let failure = production_environment(
                &runtime,
                "fixtureapp",
                tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .await
            .expect_err("query failure cannot become empty credentials");
            assert_eq!(failure.code.as_ref(), expected);
            assert_eq!(failure.stage.as_ref(), "file_credentials_configuration");
            assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        }
    }
    #[tokio::test]
    async fn production_credential_query_uses_parent_deadline() {
        let runtime = SpecRuntime {
            result: std::sync::Mutex::new(None),
            calls: AtomicUsize::new(0),
            delayed: true,
        };
        let failure = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            production_environment(
                &runtime,
                "fixtureapp",
                tokio::time::Instant::now() + std::time::Duration::from_millis(20),
            ),
        )
        .await
        .expect("bounded parent query")
        .expect_err("deadline elapsed");
        assert_eq!(failure.code.as_ref(), shared_types::ERR_RUNTIME_TIMEOUT);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}
