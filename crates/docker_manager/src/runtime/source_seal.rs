//! Minimal one-shot helper input. No service entrypoint or PG environment is inherited.
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use sha2::{Digest, Sha256};
use shared_types::{RuntimeGenerationHandoff, RuntimeGenerationSourceSeal, UserAppMutationTarget};
use std::collections::BTreeMap;

pub(super) fn invalid(message: &str) -> Error {
    Error::Conflict(message.into())
}
pub(super) fn validate(
    source: &UserAppMutationTarget,
    auth: &RuntimeGenerationHandoff,
) -> Result<()> {
    auth.validate().map_err(Error::ConfigurationError)?;
    source
        .context
        .validate_identity(&auth.app_id)
        .map_err(Error::ConfigurationError)?;
    if source.context.lifecycle_id != auth.lifecycle_id
        || source.context.operation_id != auth.activation.operation_id
        || source.resource.uid != auth.previous_resource_uid
        || source.resource.name != auth.previous_resource_name
    {
        return Err(invalid("Offline source authorization mismatch"));
    }
    Ok(())
}
pub(super) fn name(auth: &RuntimeGenerationHandoff) -> Result<String> {
    let bytes = serde_json::to_vec(auth).map_err(|_| invalid("Invalid offline authorization"))?;
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!("rcoder-seal-{}", &hash[..40]))
}
pub(super) fn environment(
    env: &BTreeMap<String, String>,
    auth: &RuntimeGenerationHandoff,
) -> Result<(String, Vec<String>)> {
    if env.get("PROJECT_ID") != Some(&auth.app_id)
        || env.get(shared_types::APP_DEPLOY_GENERATION_ID) != Some(&auth.previous_generation)
    {
        return Err(invalid("Offline source environment identity mismatch"));
    }
    let workspace = env
        .get("APP_CLI_RUNTIME_WORKSPACE")
        .or_else(|| env.get("APP_CLI_WORKSPACE"))
        .filter(|path| path.starts_with('/') && path.as_str() != "/")
        .ok_or_else(|| invalid("Offline source workspace is not an explicit absolute path"))?
        .clone();
    let mut selected = vec![
        format!("PROJECT_ID={}", auth.app_id),
        format!(
            "{}={}",
            shared_types::APP_DEPLOY_GENERATION_ID,
            auth.previous_generation
        ),
    ];
    let root = env
        .get("APP_CLI_STATE_ROOT")
        .filter(|path| path.starts_with('/') && !path.split('/').any(|part| part == ".."))
        .ok_or_else(|| {
            invalid("Offline source requires its original explicit persistent state root")
        })?;
    selected.push(format!("APP_CLI_STATE_ROOT={root}"));
    Ok((workspace, selected))
}
pub(super) fn command(workspace: &str, auth: &RuntimeGenerationHandoff) -> Result<Vec<String>> {
    let json = serde_json::to_string(auth).map_err(|_| invalid("Invalid offline authorization"))?;
    Ok(vec![
        "sh".into(),
        "-c".into(),
        format!(
            "printf '%s' {} | app-cli seal-source --workspace {}",
            shared_types::pg_utils::pg_shell_quote(&json),
            shared_types::pg_utils::pg_shell_quote(workspace)
        ),
    ])
}
pub(super) fn receipt(
    text: &str,
    auth: &RuntimeGenerationHandoff,
) -> Result<RuntimeGenerationSourceSeal> {
    let seal: RuntimeGenerationSourceSeal = serde_json::from_str(text.trim())
        .map_err(|_| invalid("Offline helper did not return one valid seal receipt"))?;
    if seal.authorization != *auth
        || seal.artifact_release_id.is_empty()
        || seal.source_journal_sha256.len() != 64
        || !seal
            .source_journal_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("Offline helper seal identity or evidence mismatch"));
    }
    Ok(seal)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn authorization() -> RuntimeGenerationHandoff {
        RuntimeGenerationHandoff {
            protocol_version: 1,
            app_id: "sealapp".into(),
            lifecycle_id: "life".into(),
            previous_generation: "old".into(),
            previous_resource_uid: "uid".into(),
            previous_resource_name: "workload".into(),
            activation: shared_types::RuntimeConfigurationActivation {
                operation_id: "operation".into(),
                deployment_generation: "operation".into(),
                config_version: 1,
            },
        }
    }
    #[test]
    fn offline_helper_input_is_stable_and_excludes_database_and_bootstrap_environment() {
        let auth = authorization();
        let env: BTreeMap<_, _> = [
            ("PROJECT_ID", "sealapp"),
            ("APP_DEPLOY_GENERATION_ID", "old"),
            ("APP_CLI_RUNTIME_WORKSPACE", "/data/source"),
            ("APP_CLI_STATE_ROOT", "/data/state"),
            ("POSTGRES_PASSWORD", "private"),
            ("APP_DEPLOY_URL", "https://example.invalid/artifact"),
            ("APP_CLI_MANAGED", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let (workspace, selected) = environment(&env, &auth).unwrap();
        assert_eq!(selected.len(), 3);
        assert!(!selected.iter().any(|v| v.contains("private")
            || v.contains("APP_DEPLOY_URL")
            || v.contains("APP_CLI_MANAGED")));
        let command = command(&workspace, &auth).unwrap();
        assert_eq!(&command[..2], ["sh", "-c"]);
        assert!(command[2].contains("app-cli seal-source --workspace"));
        assert!(!command[2].contains("serve"));
        assert_eq!(name(&auth).unwrap(), name(&auth).unwrap());
        let mut other = auth.clone();
        other.activation.config_version += 1;
        assert_ne!(name(&auth).unwrap(), name(&other).unwrap());
        let mut missing = env;
        missing.remove("APP_CLI_STATE_ROOT");
        assert!(environment(&missing, &auth).is_err());
    }
    #[test]
    fn offline_seal_receipt_rejects_wrong_identity_or_extra_stdout() {
        let auth = authorization();
        let value = RuntimeGenerationSourceSeal {
            authorization: auth.clone(),
            artifact_release_id: "hot".into(),
            source_journal_sha256: "a".repeat(64),
            desired_revision: 2,
        };
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(receipt(&json, &auth).unwrap(), value);
        assert!(receipt(&format!("startup log\n{json}"), &auth).is_err());
        let mut other = auth;
        other.activation.config_version += 1;
        assert!(receipt(&json, &other).is_err());
    }
}

#[cfg(test)]
pub(super) fn test_target(
    specification: serde_json::Value,
) -> container_runtime_api::OfflineSourceSealTarget {
    let context = shared_types::UserAppExecutionContext {
        app_id: "sealapp".into(),
        lifecycle_id: "life".into(),
        operation_id: "operation".into(),
        executor_id: "executor".into(),
        request_fingerprint: "a".repeat(64),
    };
    let source = UserAppMutationTarget {
        context,
        resource: shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::Container,
            name: "source".into(),
            uid: "sourceuid".into(),
            resource_version: None,
        },
    };
    let authorization = RuntimeGenerationHandoff {
        protocol_version: 1,
        app_id: "sealapp".into(),
        lifecycle_id: "life".into(),
        previous_generation: "old".into(),
        previous_resource_uid: "sourceuid".into(),
        previous_resource_name: "source".into(),
        activation: shared_types::RuntimeConfigurationActivation {
            operation_id: "operation".into(),
            deployment_generation: "operation".into(),
            config_version: 1,
        },
    };
    container_runtime_api::OfflineSourceSealTarget {
        source,
        helper_name: "helper".into(),
        helper_uid: "helperuid".into(),
        authorization,
        specification,
    }
}
