//! Explicit adoption of an existing physical resource into one application lifecycle.
//! Bindings outlive deletion so an old physical UID cannot be adopted by a new life.
use crate::{ServiceType, UserAppExecutionContext};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAppResourceBinding {
    pub app_id: String,
    pub lifecycle_id: String,
    /// Bound resource family: user-app-builder (UserappBuilder). The shared family
    /// type also represents web-agent-runner (WebAgentRunner), computer-agent-runner
    /// (ComputerAgentRunner), and user-app (Userapp), which this binding rejects.
    pub service_type: ServiceType,
    pub physical_uid: String,
    pub adopted_by_operation: String,
}

impl UserAppResourceBinding {
    pub fn validate(
        &self,
        context: &UserAppExecutionContext,
        physical_uid: &str,
    ) -> Result<(), String> {
        context.validate_identity(&self.app_id)?;
        if self.lifecycle_id != context.lifecycle_id
            || self.service_type != ServiceType::UserappBuilder
            || self.physical_uid.is_empty()
            || self.physical_uid != physical_uid
            || self.adopted_by_operation.is_empty()
        {
            return Err(
                "Physical resource binding does not match the current builder lifecycle".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AdoptBuilderRequest {
    /// Current lifecycle returned by the platform; always explicit for adoption.
    pub lifecycle_id: String,
    /// Stable request identity for exact retries.
    pub request_id: String,
    /// Actual Docker container ID or current Kubernetes Pod UID, never a name.
    pub expected_container_id: String,
}

/// Validate physical ownership before deciding whether an explicit adoption is
/// necessary. A binding permits missing metadata, never conflicting metadata.
pub fn builder_identity_is_bound(
    context: &UserAppExecutionContext,
    metadata: &std::collections::BTreeMap<String, String>,
    physical_uid: &str,
    physical_owner: Option<&str>,
    binding: Option<&UserAppResourceBinding>,
) -> Result<bool, String> {
    context.validate_identity(&context.app_id)?;
    if physical_owner.is_some() {
        return Err("Builder physical owner evidence is missing or conflicting".into());
    }
    for (key, expected) in [
        ("rcoder.io/application-id", context.app_id.as_str()),
        ("rcoder.io/lifecycle-id", context.lifecycle_id.as_str()),
    ] {
        if metadata
            .get(key)
            .is_some_and(|value| !value.is_empty() && value != expected)
        {
            return Err(format!("Builder resource identity conflicts at {key}"));
        }
    }
    // An explicit receipt is authoritative even when native metadata is complete.
    // Never let native labels hide a stale receipt for a different physical UID.
    if let Some(binding) = binding {
        binding.validate(context, physical_uid)?;
    }
    if context.validate_application_metadata(metadata).is_ok() {
        return Ok(true);
    }
    match binding {
        Some(binding) => {
            binding.validate(context, physical_uid)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binding_allows_absent_lifecycle_but_never_conflicting_physical_evidence() {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "control".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let binding = UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "uid".into(),
            adopted_by_operation: "adopt".into(),
        };
        let mut metadata = std::collections::BTreeMap::new();
        assert!(
            !builder_identity_is_bound(&context, &metadata, "uid", Some("owner"), None)
                .expect("candidate")
        );
        assert!(
            builder_identity_is_bound(&context, &metadata, "uid", Some("owner"), Some(&binding))
                .expect("bound")
        );
        assert!(
            builder_identity_is_bound(&context, &metadata, "uid", None, Some(&binding)).is_err()
        );
        let complete_metadata = context.resource_metadata();
        assert!(
            builder_identity_is_bound(
                &context,
                &complete_metadata,
                "replacement-uid",
                Some("owner"),
                Some(&binding)
            )
            .is_err(),
            "native labels must not hide a stale physical receipt"
        );
        metadata.insert("rcoder.io/lifecycle-id".into(), "older-life".into());
        assert!(
            builder_identity_is_bound(&context, &metadata, "uid", Some("owner"), Some(&binding))
                .is_err()
        );
    }

    #[test]
    fn binding_requires_exact_physical_identity_and_lifecycle() {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "control".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let binding = UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "uid".into(),
            adopted_by_operation: "adopt".into(),
        };
        assert!(binding.validate(&context, "uid").is_ok());
        assert!(binding.validate(&context, "replacement").is_err());
        let mut replacement = context.clone();
        replacement.lifecycle_id = "new-life".into();
        assert!(binding.validate(&replacement, "uid").is_err());
        replacement = context;
        replacement.app_id = "other-app".into();
        assert!(binding.validate(&replacement, "uid").is_err());
    }
}
