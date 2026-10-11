//! File-peer credentials. Existing builders use their effective process inputs,
//! so a platform template or Secret rotation cannot change a live peer's token.
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
    if stage == UserappStage::Dev {
        return development_credentials(state, app_id, deadline).await;
    }
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
    let environment = production_environment(state.runtime().as_ref(), app_id, deadline).await?;
    if let Some(environment) = environment
        && let Some(value) = environment.get("FILE_SERVER_PROXY_TOKEN")
    {
        token = Some(value.trim().to_owned()).filter(|value| !value.is_empty());
    }
    Ok(FileServerRequestCredentials { proxy_token: token })
}

async fn development_credentials(
    state: &AppState,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<FileServerRequestCredentials, WakeFailure> {
    let read = async {
        let app = state
            .userapp_store
            .get_application(app_id)
            .await
            .map_err(store_configuration_failure)?
            .ok_or_else(|| {
                WakeFailure::new(
                    shared_types::ERR_APP_NOT_FOUND,
                    "file_credentials_configuration",
                    "Application identity is unavailable for development file credentials",
                )
            })?;
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.to_owned(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: "file-credentials-observation".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        let target = crate::userapp_builder::adoption::capture_bound_target(state, &context)
            .await
            .map_err(|error| {
                if let Some(runtime) =
                    error.downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                {
                    configuration_runtime_failure(runtime)
                } else {
                    WakeFailure::new(
                        shared_types::ERR_BACKEND_ERROR,
                        "file_credentials_configuration",
                        shared_types::sanitize_error_text(&format!(
                            "Capture development file peer: {error:#}"
                        )),
                    )
                }
            })?;
        let credentials =
            development_credentials_for_target(state.runtime().as_ref(), &target, deadline).await?;
        let current = state
            .userapp_store
            .get_application(app_id)
            .await
            .map_err(store_configuration_failure)?;
        if !current.is_some_and(|current| {
            current.lifecycle_id == app.lifecycle_id && current.state == app.state
        }) {
            return Err(WakeFailure::new(
                shared_types::ERR_CONFLICT,
                "file_credentials_configuration",
                "Application lifecycle changed during development file credential observation",
            ));
        }
        Ok(credentials)
    };
    tokio::time::timeout_at(deadline, read)
        .await
        .map_err(|_| WakeFailure::timeout("file_credentials_configuration", None, true))?
}

fn store_configuration_failure(error: shared_types::UserAppStoreError) -> WakeFailure {
    let error = shared_types::AppError::from(app_manager::AppOperationError::from(error));
    match error {
        shared_types::AppError::Structured(detail) => WakeFailure::new(
            detail.code,
            "file_credentials_configuration",
            detail.internal_message.unwrap_or_else(|| {
                "Development file credential identity observation failed".into()
            }),
        ),
        error => WakeFailure::new(
            shared_types::ERR_BACKEND_ERROR,
            "file_credentials_configuration",
            shared_types::sanitize_error_text(&error.to_string()),
        ),
    }
}

async fn development_credentials_for_target(
    runtime: &dyn container_runtime_api::AgentContainerRuntime,
    target: &shared_types::BuilderControlTarget,
    deadline: tokio::time::Instant,
) -> Result<FileServerRequestCredentials, WakeFailure> {
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(WakeFailure::timeout(
                "file_credentials_configuration",
                None,
                true,
            ));
        }
        let result =
            tokio::time::timeout_at(deadline, runtime.get_bound_builder_file_credentials(target))
                .await
                .map_err(|_| WakeFailure::timeout("file_credentials_configuration", None, true))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(WakeFailure::timeout(
                "file_credentials_configuration",
                None,
                true,
            ));
        }
        match result {
            Err(container_runtime_api::ContainerRuntimeError::ManagementNotRunning) => {
                // Only a known startup window is retryable. The runtime checks
                // the same captured Pod/controller each time; replacement or
                // permission errors never fall back to platform credentials.
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + std::time::Duration::from_millis(100))
                        .min(deadline),
                )
                .await;
            }
            Ok(credentials) => return Ok(credentials),
            Err(error) => return Err(configuration_runtime_failure(&error)),
        }
    }
}

fn configuration_runtime_failure(
    error: &container_runtime_api::ContainerRuntimeError,
) -> WakeFailure {
    let (code, summary) = container_runtime_api::runtime_error_code(error);
    let mut failure = WakeFailure::new(code, "file_credentials_configuration", summary);
    failure.retryable = matches!(
        code,
        shared_types::ERR_RUNTIME_UNAVAILABLE | shared_types::ERR_RUNTIME_TIMEOUT
    );
    let mut origin = error;
    while let container_runtime_api::ContainerRuntimeError::CreationAborted { source, .. } = origin
    {
        origin = source;
    }
    if let container_runtime_api::ContainerRuntimeError::OperationInProgress(operation) = origin {
        failure.operation_id = operation.operation_id.clone();
    }
    failure
}

async fn production_environment(
    runtime: &dyn container_runtime_api::UserAppDeploymentRuntime,
    app_id: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<std::collections::HashMap<String, String>>, WakeFailure> {
    let spec = tokio::time::timeout_at(deadline, runtime.get_app_container_spec(app_id))
        .await
        .map_err(|_| WakeFailure::timeout("file_credentials_configuration", None, true))?
        .map_err(|error| configuration_runtime_failure(&error))?;
    Ok(spec.env)
}

#[cfg(test)]
#[path = "file_credentials_tests.rs"]
mod development_tests;

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
