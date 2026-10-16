//! Private current configuration for a retained Docker application's compute.
//!
//! This is one replaceable configuration, not a deployment command or an
//! operation journal. It lives in the controller's private persistent data,
//! outside the shared UserApp workspace and all application bind mounts. A
//! missing container can consequently reuse its applied environment and image
//! while app-cli restores the confirmed artifact from the application volume.

use super::{
    docker_app_create::validate_app_container_target,
    docker_app_mounts::prod_flat_container_paths,
    docker_runtime::{DockerRuntime, app_deployment_name},
    docker_workspace::userapp_workspace_root,
};
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError as Error, ContainerRuntimeResult as Result,
    ContainerSpecSnapshot, UserAppDeploymentRuntime,
};
use serde::{Deserialize, Serialize};
use shared_types::{ServiceType, USERAPP_RESOURCE_IDENTITY_KEYS};
use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

const CONFIGURATION_DIRECTORY: &str = ".runtime-config";
const PROD_DIRECTORY: &str = "prod";
const CONFIGURATION_VERSION: u8 = 1;
const CONFIGURATION_LIMIT: u64 = 4 * 1024 * 1024;
const DEPLOYMENT_ENVIRONMENT: [&str; 4] = [
    "APP_DEPLOY_URL",
    "APP_RELEASE_ID",
    "APP_DEPLOY_SHA256",
    shared_types::APP_DEPLOY_OPERATION_ID,
];

// Deliberately no Debug: environment values can contain credentials.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedConfiguration {
    version: u8,
    app_id: String,
    lifecycle_id: String,
    host_workspace_root: PathBuf,
    source_container_id: String,
    params: ContainerCreateParams,
}

fn fail(stage: &str, error: impl std::fmt::Display) -> Error {
    Error::DockerError(format!(
        "Docker applied application configuration {stage}: {error}"
    ))
}

fn conflict(message: &str) -> Error {
    Error::Conflict(format!(
        "Docker applied application configuration: {message}"
    ))
}

fn validate_identity(app_id: &str, lifecycle_id: &str) -> Result<()> {
    shared_types::validate_identifier(app_id, "app_id").map_err(Error::ConfigurationError)?;
    shared_types::validate_identifier(lifecycle_id, "lifecycle_id")
        .map_err(Error::ConfigurationError)
}

async fn host_workspace_root() -> Result<PathBuf> {
    crate::path::resolve_container_path_to_host(Path::new(
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
    ))
    .await
    .map_err(|error| fail("resolve workspace root", error))
}

async fn configuration_root() -> Result<PathBuf> {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return crate::path::resolve_container_path_to_host(Path::new(
            shared_types::paths::RCODER_DATA_ROOT,
        ))
        .await
        .map_err(|error| fail("resolve private data root", error));
    }
    Ok(PathBuf::from(shared_types::paths::RCODER_DATA_ROOT))
}

fn configuration_path(root: &Path, app_id: &str) -> PathBuf {
    root.join(CONFIGURATION_DIRECTORY)
        .join(PROD_DIRECTORY)
        .join(format!("{app_id}.json"))
}

fn is_immutable_image_id(image: &str) -> bool {
    image.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn verify_mounts(
    inspect: &bollard::models::ContainerInspectResponse,
    app_id: &str,
    host_root: &Path,
) -> Result<()> {
    for (relative, target) in shared_types::paths::userapp_prod_subpaths(app_id)
        .into_iter()
        .zip(prod_flat_container_paths(app_id))
    {
        let mut candidates = inspect
            .mounts
            .iter()
            .flatten()
            .filter(|mount| mount.destination.as_deref() == Some(target.as_str()));
        let mount = candidates
            .next()
            .ok_or_else(|| conflict("application data mount is absent"))?;
        if candidates.next().is_some()
            || mount.typ.as_deref() != Some("bind")
            || mount.source.as_deref().map(Path::new) != Some(host_root.join(relative).as_path())
        {
            return Err(conflict(
                "application data belongs to a different workspace root",
            ));
        }
    }
    Ok(())
}

fn capture_configuration(
    app_id: &str,
    lifecycle_id: &str,
    host_root: PathBuf,
    inspect: &bollard::models::ContainerInspectResponse,
    spec: ContainerSpecSnapshot,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> Result<AppliedConfiguration> {
    validate_identity(app_id, lifecycle_id)?;
    let source_container_id = validate_source(app_id, lifecycle_id, inspect, binding)?;
    let config = inspect
        .config
        .as_ref()
        .ok_or_else(|| conflict("container configuration is absent"))?;
    verify_mounts(inspect, app_id, &host_root)?;
    // Config.Image is the creation reference and may be a mutable latest tag.
    // Image is the daemon's actual content identity. Recovery must retain that
    // exact image rather than accidentally upgrading through a moved alias.
    let image = inspect
        .image
        .as_ref()
        .filter(|image| is_immutable_image_id(image))
        .ok_or_else(|| conflict("applied immutable image identity is absent or invalid"))?;
    let mut params = ContainerCreateParams::builder()
        .project_id(app_id)
        .service_type(ServiceType::Userapp)
        .image_override(image)
        .build();
    // The image remains the applied image, so its actual command is a valid
    // fallback for older containers that predate the explicit command label.
    params.command = spec.command.or_else(|| config.cmd.clone());
    params.env = spec.env;
    params.secrets = spec.secrets;
    params.ports = spec.ports;
    params.app_resources = spec.resources;
    params.health_check = spec.health_check;
    params.tenant_id = config
        .labels
        .as_ref()
        .and_then(|labels| labels.get("tenant").cloned());
    params.space_id = config
        .labels
        .as_ref()
        .and_then(|labels| labels.get("space").cloned());
    strip_deployment_intent(&mut params);
    Ok(AppliedConfiguration {
        version: CONFIGURATION_VERSION,
        app_id: app_id.into(),
        lifecycle_id: lifecycle_id.into(),
        host_workspace_root: host_root,
        source_container_id,
        params,
    })
}

fn validate_source(
    app_id: &str,
    lifecycle_id: &str,
    inspect: &bollard::models::ContainerInspectResponse,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> Result<String> {
    let uid = inspect
        .id
        .as_ref()
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| conflict("source physical container identity is absent"))?;
    if let Some(binding) = binding {
        if binding.app_id != app_id
            || binding.lifecycle_id != lifecycle_id
            || binding.service_type != ServiceType::Userapp
            || binding.physical_uid != *uid
            || shared_types::validate_identifier(
                &binding.adopted_by_operation,
                "adoption_operation_id",
            )
            .is_err()
        {
            return Err(conflict(
                "explicit resource binding differs from the applied container",
            ));
        }
        // The caller obtained this binding from the current lifecycle store.
        // Adoption is explicit authority for old immutable identity labels,
        // while the four physical mounts are verified independently below.
        return Ok(uid.clone());
    }
    validate_app_container_target(app_id, inspect)?;
    let labels = inspect
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| conflict("container ownership is absent"))?;
    let [application_key, lifecycle_key, _] = USERAPP_RESOURCE_IDENTITY_KEYS;
    if labels.get(application_key).map(String::as_str) != Some(app_id)
        || labels.get(lifecycle_key).map(String::as_str) != Some(lifecycle_id)
    {
        return Err(conflict("container application lifecycle differs"));
    }
    Ok(uid.clone())
}

fn strip_deployment_intent(params: &mut ContainerCreateParams) {
    params.execution_context = None;
    params.mutation_target = None;
    params.resource_binding = None;
    // APP_DEPLOY_GENERATION_ID is the stable container/deployment identity used
    // by app-cli restoration and subsequent hot deployments. Preserve it while
    // removing the former request's download and operation instructions.
    for environment in [&mut params.env, &mut params.secrets].into_iter().flatten() {
        for key in DEPLOYMENT_ENVIRONMENT {
            environment.remove(key);
        }
    }
}

impl AppliedConfiguration {
    fn into_params(
        mut self,
        app_id: &str,
        lifecycle_id: &str,
        host_root: &Path,
    ) -> Result<ContainerCreateParams> {
        if self.version != CONFIGURATION_VERSION
            || self.app_id != app_id
            || self.lifecycle_id != lifecycle_id
            || self.host_workspace_root != host_root
            || self.source_container_id.is_empty()
            || self.params.service_type != ServiceType::Userapp
            || self.params.project_id.as_deref() != Some(app_id)
            || self
                .params
                .image_override
                .as_ref()
                .is_none_or(|image| !is_immutable_image_id(image))
        {
            return Err(conflict("retained identity or workspace differs"));
        }
        strip_deployment_intent(&mut self.params);
        self.params.validate_execution_context()?;
        Ok(self.params)
    }
}

impl DockerRuntime {
    /// Capture only the currently applied physical configuration. The caller
    /// holds the application's operation lease; observation never adopts a
    /// same-named container from another installation or lifecycle.
    pub(super) async fn retain_app_configuration(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> Result<()> {
        validate_identity(app_id, lifecycle_id)?;
        let client = self.inner.get_docker_client();
        let name = app_deployment_name(app_id);
        let before = client
            .inspect_container(&name, None)
            .await
            .map_err(|error| fail("inspect applied container", error))?;
        let uid = validate_source(app_id, lifecycle_id, &before, binding)?;
        let spec = self.get_app_container_spec(app_id).await?;
        let after = client
            .inspect_container(&name, None)
            .await
            .map_err(|error| fail("confirm applied container", error))?;
        if validate_source(app_id, lifecycle_id, &after, binding)? != uid {
            return Err(conflict("container changed during configuration capture"));
        }
        let host_root = host_workspace_root().await?;
        let captured =
            capture_configuration(app_id, lifecycle_id, host_root, &after, spec, binding)?;
        let root = configuration_root().await?;
        let app_id = app_id.to_owned();
        tokio::task::spawn_blocking(move || write_configuration(&root, &app_id, &captured))
            .await
            .map_err(|error| fail("writer worker", error))?
    }

    /// Read the private applied configuration without executing its former
    /// deployment. Policy and the new operation identity are supplied by the
    /// coordinator; the existing app-cli state restores the retained artifact.
    pub(super) async fn load_retained_app_configuration(
        &self,
        app_id: &str,
        lifecycle_id: &str,
    ) -> Result<Option<ContainerCreateParams>> {
        validate_identity(app_id, lifecycle_id)?;
        let root = configuration_root().await?;
        let host_root = host_workspace_root().await?;
        let workspace_root = userapp_workspace_root().await?;
        let app_id = app_id.to_owned();
        let lifecycle_id = lifecycle_id.to_owned();
        tokio::task::spawn_blocking(move || {
            read_configuration(&root, &app_id)?
                .map(|configuration| {
                    verify_retained_storage(&workspace_root, &app_id)?;
                    configuration.into_params(&app_id, &lifecycle_id, &host_root)
                })
                .transpose()
        })
        .await
        .map_err(|error| fail("reader worker", error))?
    }
}

fn verify_retained_storage(root: &Path, app_id: &str) -> Result<()> {
    let [workspace, data, _, _] = shared_types::paths::userapp_prod_subpaths(app_id);
    for relative in [workspace, data] {
        let metadata = std::fs::symlink_metadata(root.join(relative))
            .map_err(|_| conflict("retained application workspace or data directory is absent"))?;
        if !metadata.is_dir() {
            return Err(conflict("retained application storage is not a directory"));
        }
    }
    // Logs and agent scratch state may be recreated normally. Missing business
    // data must never turn into an implicitly initialized empty database.
    Ok(())
}

fn private_directory(path: &Path, create: bool) -> Result<bool> {
    if create {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(fail("create private directory", error)),
        }
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(fail("inspect private directory", error)),
    };
    if !metadata.is_dir() {
        return Err(conflict(
            "private configuration directory is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if create {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| fail("protect private directory", error))?;
        } else if metadata.permissions().mode() & 0o077 != 0 {
            return Err(conflict(
                "private configuration directory permissions are not private",
            ));
        }
    }
    Ok(true)
}

fn write_configuration(root: &Path, app_id: &str, value: &AppliedConfiguration) -> Result<()> {
    let directory = root.join(CONFIGURATION_DIRECTORY);
    private_directory(&directory, true)?;
    let parent = directory.join(PROD_DIRECTORY);
    private_directory(&parent, true)?;
    let encoded = serde_json::to_vec(value)
        .map_err(|_| fail("encode", "configuration could not be encoded"))?;
    if encoded.len() as u64 > CONFIGURATION_LIMIT {
        return Err(conflict("private configuration exceeds size limit"));
    }
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| fail("open private temporary file", error))?;
        file.write_all(&encoded)
            .map_err(|error| fail("write", error))?;
        file.sync_all().map_err(|error| fail("sync file", error))?;
        // A new verified physical configuration replaces a damaged former file;
        // the old file's parseability never prevents explicit deployment repair.
        std::fs::rename(&temporary, configuration_path(root, app_id))
            .map_err(|error| fail("publish", error))?;
        #[cfg(unix)]
        {
            for path in [parent.as_path(), directory.as_path(), root] {
                std::fs::File::open(path)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|error| fail("sync directory", error))?;
            }
        }
        Ok(())
    })();
    if let Err(error) = std::fs::remove_file(&temporary)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "Could not remove applied configuration temporary file");
    }
    result
}

fn read_configuration(root: &Path, app_id: &str) -> Result<Option<AppliedConfiguration>> {
    let directory = root.join(CONFIGURATION_DIRECTORY);
    if !private_directory(&directory, false)?
        || !private_directory(&directory.join(PROD_DIRECTORY), false)?
    {
        return Ok(None);
    }
    let path = configuration_path(root, app_id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail("inspect private file", error)),
    };
    if !metadata.is_file() || metadata.len() > CONFIGURATION_LIMIT {
        return Err(conflict("invalid private configuration file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(conflict(
                "private configuration file permissions are not private",
            ));
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| fail("open", error))?;
    let mut bytes = Vec::new();
    file.take(CONFIGURATION_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| fail("read", error))?;
    if bytes.len() as u64 > CONFIGURATION_LIMIT {
        return Err(conflict("private configuration exceeds size limit"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| conflict("private configuration is damaged or unsupported"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::{AppPortSpec, AppResourceRequirements, ExposeType};
    use std::collections::HashMap;

    #[test]
    fn missing_business_storage_is_not_recreated_as_empty_data() {
        let root = tempfile::tempdir().unwrap();
        let [workspace, data, logs, _] = shared_types::paths::userapp_prod_subpaths("app1");
        assert!(verify_retained_storage(root.path(), "app1").is_err());
        assert!(!root.path().join(&workspace).exists());
        std::fs::create_dir_all(root.path().join(&workspace)).unwrap();
        assert!(verify_retained_storage(root.path(), "app1").is_err());
        assert!(!root.path().join(&data).exists());
        std::fs::create_dir_all(root.path().join(&data)).unwrap();
        verify_retained_storage(root.path(), "app1").unwrap();
        assert!(
            !root.path().join(logs).exists(),
            "logging history is not a recovery gate"
        );
    }

    fn fixture(
        host_root: &Path,
    ) -> (
        bollard::models::ContainerInspectResponse,
        ContainerSpecSnapshot,
    ) {
        let app_id = "app1";
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.into(),
            lifecycle_id: "life1".into(),
            operation_id: "create1".into(),
            executor_id: "executor1".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut labels = context.resource_metadata();
        labels.insert(
            shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
            app_id.into(),
        );
        labels.insert("managed-by".into(), "rcoder-app-manager".into());
        labels.insert("service-type".into(), ServiceType::Userapp.to_string());
        let mounts = shared_types::paths::userapp_prod_subpaths(app_id)
            .into_iter()
            .zip(prod_flat_container_paths(app_id))
            .map(|(relative, target)| {
                serde_json::json!({
                    "Type":"bind", "Source":host_root.join(relative), "Destination":target,
                })
            })
            .collect::<Vec<_>>();
        let inspect = serde_json::from_value(serde_json::json!({
            "Id":"physical1", "Image":format!("sha256:{}", "a".repeat(64)),
            "Config":{"Image":"runtime:latest", "Labels":labels},
            "Mounts":mounts,
        }))
        .unwrap();
        let mut env = HashMap::from([
            ("BUSINESS_TOKEN".into(), "secret=retained".into()),
            ("POSTGRES_PASSWORD".into(), "database-secret".into()),
            (
                shared_types::APP_DEPLOY_GENERATION_ID.into(),
                "stable-generation".into(),
            ),
        ]);
        for key in DEPLOYMENT_ENVIRONMENT {
            env.insert(key.into(), "old-deployment-value".into());
        }
        let spec = ContainerSpecSnapshot {
            env: Some(env),
            command: Some(vec!["custom-start".into()]),
            resources: Some(AppResourceRequirements {
                cpu: Some("0.5".into()),
                memory: Some("128Mi".into()),
                ..Default::default()
            }),
            ports: Some(vec![AppPortSpec {
                name: "entry".into(),
                port: 9080,
                expose_type: ExposeType::Http,
                strip_prefix: Some(false),
            }]),
            ..Default::default()
        };
        (inspect, spec)
    }

    fn captured(root: &Path) -> AppliedConfiguration {
        let (inspect, spec) = fixture(root);
        capture_configuration("app1", "life1", root.to_owned(), &inspect, spec, None).unwrap()
    }

    #[test]
    fn retains_applied_configuration_without_replaying_deployment_input() {
        let private_root = tempfile::tempdir().unwrap();
        let workspace_root = tempfile::tempdir().unwrap();
        write_configuration(
            private_root.path(),
            "app1",
            &captured(workspace_root.path()),
        )
        .unwrap();
        let params = read_configuration(private_root.path(), "app1")
            .unwrap()
            .unwrap()
            .into_params("app1", "life1", workspace_root.path())
            .unwrap();
        assert!(!workspace_root.path().join(CONFIGURATION_DIRECTORY).exists());
        assert!(
            !configuration_path(private_root.path(), "app1").starts_with(workspace_root.path())
        );
        // The private file location does not replace the application's physical
        // bind root: decoding under another workspace installation still fails.
        assert!(
            read_configuration(private_root.path(), "app1")
                .unwrap()
                .unwrap()
                .into_params("app1", "life1", private_root.path())
                .is_err()
        );
        assert_eq!(
            params.image_override,
            Some(format!("sha256:{}", "a".repeat(64)))
        );
        assert_eq!(params.command, Some(vec!["custom-start".into()]));
        let env = params.env.unwrap();
        assert_eq!(env["BUSINESS_TOKEN"], "secret=retained");
        assert_eq!(env["POSTGRES_PASSWORD"], "database-secret");
        assert_eq!(
            env[shared_types::APP_DEPLOY_GENERATION_ID],
            "stable-generation"
        );
        assert!(
            DEPLOYMENT_ENVIRONMENT
                .iter()
                .all(|key| !env.contains_key(*key))
        );
        assert!(params.execution_context.is_none());
        assert!(params.mutation_target.is_none());
        assert!(params.resource_binding.is_none());
        assert_eq!(
            params.app_resources.unwrap().memory.as_deref(),
            Some("128Mi")
        );
        assert_eq!(params.ports.unwrap()[0].strip_prefix, Some(false));
    }

    #[test]
    fn loading_keeps_stable_generation_without_replaying_an_old_operation() {
        let root = tempfile::tempdir().unwrap();
        let mut snapshot = captured(root.path());
        for key in DEPLOYMENT_ENVIRONMENT {
            snapshot
                .params
                .env
                .as_mut()
                .unwrap()
                .insert(key.into(), "former-request".into());
        }
        let mut secrets = HashMap::from([
            (
                shared_types::APP_DEPLOY_GENERATION_ID.into(),
                "stable-generation".into(),
            ),
            ("BUSINESS_SECRET".into(), "secret=preserved".into()),
        ]);
        for key in DEPLOYMENT_ENVIRONMENT {
            secrets.insert(key.into(), "former-request".into());
        }
        snapshot.params.secrets = Some(secrets);
        snapshot.params.execution_context = Some(shared_types::UserAppExecutionContext {
            app_id: "app1".into(),
            lifecycle_id: "life1".into(),
            operation_id: "old-request".into(),
            executor_id: "old-executor".into(),
            request_fingerprint: "a".repeat(64),
        });
        let restored = snapshot.into_params("app1", "life1", root.path()).unwrap();
        assert!(restored.execution_context.is_none());
        assert!(restored.mutation_target.is_none());
        assert!(restored.resource_binding.is_none());
        for environment in [
            restored.env.as_ref().unwrap(),
            restored.secrets.as_ref().unwrap(),
        ] {
            assert_eq!(
                environment[shared_types::APP_DEPLOY_GENERATION_ID],
                "stable-generation"
            );
            assert!(
                DEPLOYMENT_ENVIRONMENT
                    .iter()
                    .all(|key| !environment.contains_key(*key))
            );
        }
        assert_eq!(
            restored.secrets.as_ref().unwrap()["BUSINESS_SECRET"],
            "secret=preserved"
        );
    }

    #[test]
    fn immutable_image_identity_survives_changed_mutable_references() {
        let private_root = tempfile::tempdir().unwrap();
        let workspace_root = tempfile::tempdir().unwrap();
        let (mut inspect, spec) = fixture(workspace_root.path());
        let actual = inspect.image.clone().unwrap();
        let snapshot = capture_configuration(
            "app1",
            "life1",
            workspace_root.path().into(),
            &inspect,
            spec.clone(),
            None,
        )
        .unwrap();
        write_configuration(private_root.path(), "app1", &snapshot).unwrap();
        // A registry/local latest alias can now refer to a different build.
        // Neither that alias nor its spelling determines the retained image.
        inspect.config.as_mut().unwrap().image = Some("runtime:new-latest-alias".into());
        let recaptured = capture_configuration(
            "app1",
            "life1",
            workspace_root.path().into(),
            &inspect,
            spec,
            None,
        )
        .unwrap();
        assert_eq!(recaptured.params.image_override.as_ref(), Some(&actual));
        let restored = read_configuration(private_root.path(), "app1")
            .unwrap()
            .unwrap()
            .into_params("app1", "life1", workspace_root.path())
            .unwrap();
        assert_eq!(restored.image_override.as_ref(), Some(&actual));
        let mut invalid = captured(workspace_root.path());
        invalid.params.image_override = Some("runtime:latest".into());
        assert!(
            invalid
                .into_params("app1", "life1", workspace_root.path())
                .is_err()
        );
    }

    #[test]
    fn missing_or_invalid_actual_image_never_falls_back_to_tag_or_echoes_private_values() {
        let root = tempfile::tempdir().unwrap();
        let (mut inspect, spec) = fixture(root.path());
        for actual in [
            None,
            Some("runtime:latest".into()),
            Some("sha256:sensitive-image-value".into()),
        ] {
            inspect.image = actual;
            let error = capture_configuration(
                "app1",
                "life1",
                root.path().into(),
                &inspect,
                spec.clone(),
                None,
            )
            .err()
            .unwrap()
            .to_string();
            assert!(error.contains("immutable image identity"));
            for private in [
                "sensitive-image-value",
                "secret=retained",
                "database-secret",
            ] {
                assert!(!error.contains(private));
            }
        }
    }

    #[test]
    fn rejects_foreign_lifecycle_workspace_and_resource_family() {
        let root = tempfile::tempdir().unwrap();
        let (mut inspect, spec) = fixture(root.path());
        assert!(
            capture_configuration(
                "app1",
                "life2",
                root.path().into(),
                &inspect,
                spec.clone(),
                None
            )
            .is_err()
        );
        assert!(
            capture_configuration(
                "app1",
                "life1",
                root.path().join("other"),
                &inspect,
                spec.clone(),
                None,
            )
            .is_err()
        );
        inspect
            .config
            .as_mut()
            .unwrap()
            .labels
            .as_mut()
            .unwrap()
            .insert(
                "service-type".into(),
                ServiceType::UserappBuilder.to_string(),
            );
        assert!(
            capture_configuration("app1", "life1", root.path().into(), &inspect, spec, None)
                .is_err()
        );
        assert!(
            captured(root.path())
                .into_params("app1", "life2", root.path())
                .is_err()
        );
        assert!(
            captured(root.path())
                .into_params("app1", "life1", &root.path().join("other"))
                .is_err()
        );
        assert!(
            captured(root.path())
                .into_params("app2", "life1", root.path())
                .is_err()
        );
    }

    #[test]
    fn explicit_current_lifecycle_binding_restores_adopted_configuration_only_for_its_uid() {
        let root = tempfile::tempdir().unwrap();
        let (inspect, spec) = fixture(root.path());
        let mut binding = shared_types::UserAppResourceBinding {
            app_id: "app1".into(),
            lifecycle_id: "adoptedlife".into(),
            service_type: ServiceType::Userapp,
            physical_uid: "physical1".into(),
            adopted_by_operation: "adoption1".into(),
        };
        assert!(
            capture_configuration(
                "app1",
                "adoptedlife",
                root.path().into(),
                &inspect,
                spec.clone(),
                None
            )
            .is_err()
        );
        let accepted = capture_configuration(
            "app1",
            "adoptedlife",
            root.path().into(),
            &inspect,
            spec.clone(),
            Some(&binding),
        )
        .unwrap();
        assert!(
            accepted
                .into_params("app1", "adoptedlife", root.path())
                .is_ok()
        );
        assert!(
            capture_configuration(
                "app1",
                "adoptedlife",
                root.path().join("other"),
                &inspect,
                spec.clone(),
                Some(&binding)
            )
            .is_err()
        );
        binding.physical_uid = "anotheruid".into();
        assert!(
            capture_configuration(
                "app1",
                "adoptedlife",
                root.path().into(),
                &inspect,
                spec.clone(),
                Some(&binding)
            )
            .is_err()
        );
        binding.physical_uid = "physical1".into();
        binding.service_type = ServiceType::UserappBuilder;
        assert!(
            capture_configuration(
                "app1",
                "adoptedlife",
                root.path().into(),
                &inspect,
                spec.clone(),
                Some(&binding)
            )
            .is_err()
        );
        binding.service_type = ServiceType::Userapp;
        binding.app_id = "anotherapp".into();
        assert!(
            capture_configuration(
                "app1",
                "adoptedlife",
                root.path().into(),
                &inspect,
                spec,
                Some(&binding)
            )
            .is_err()
        );
    }

    #[test]
    fn missing_configuration_does_not_create_storage_and_explicit_save_repairs_damage() {
        let private_root = tempfile::tempdir().unwrap();
        let workspace_root = tempfile::tempdir().unwrap();
        assert!(
            read_configuration(private_root.path(), "app1")
                .unwrap()
                .is_none()
        );
        assert!(!private_root.path().join(CONFIGURATION_DIRECTORY).exists());
        write_configuration(
            private_root.path(),
            "app1",
            &captured(workspace_root.path()),
        )
        .unwrap();
        // Syntactically valid JSON with a credential-looking value exercises a
        // serde type error, whose original error would otherwise echo the value.
        let mut damaged = serde_json::to_value(captured(workspace_root.path())).unwrap();
        damaged["version"] = serde_json::json!("damaged-secret");
        std::fs::write(
            configuration_path(private_root.path(), "app1"),
            serde_json::to_vec(&damaged).unwrap(),
        )
        .unwrap();
        let error = read_configuration(private_root.path(), "app1")
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("damaged-secret"));
        write_configuration(
            private_root.path(),
            "app1",
            &captured(workspace_root.path()),
        )
        .unwrap();
        assert!(
            read_configuration(private_root.path(), "app1")
                .unwrap()
                .is_some()
        );
        assert!(!workspace_root.path().join(CONFIGURATION_DIRECTORY).exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_is_private_and_does_not_follow_user_controlled_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let workspace_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_configuration(root.path(), "app1", &captured(workspace_root.path())).unwrap();
        let directory = root
            .path()
            .join(CONFIGURATION_DIRECTORY)
            .join(PROD_DIRECTORY);
        let path = configuration_path(root.path(), "app1");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let outside_file = outside.path().join("secret");
        std::fs::write(&outside_file, "unmodified").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&outside_file, &path).unwrap();
        assert!(read_configuration(root.path(), "app1").is_err());
        write_configuration(root.path(), "app1", &captured(workspace_root.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "unmodified"
        );
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        symlink(outside.path(), &directory).unwrap();
        assert!(
            write_configuration(root.path(), "app1", &captured(workspace_root.path())).is_err()
        );
    }
}
