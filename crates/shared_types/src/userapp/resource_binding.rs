//! Explicit adoption of an existing physical resource into one application lifecycle.
//! Bindings outlive deletion so an old physical UID cannot be adopted by a new life.
use crate::{ServiceType, UserAppExecutionContext};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

impl<'de> Deserialize<'de> for UserAppResourceBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // R08 升级兼容：存量 PG/SQLite 记录携带已退役的 user_id 字段
        //（用户绑定移除前的持久 JSON）。读取容忍**且仅容忍**该字段；
        // 其余未知字段仍拒绝（不全面关闭严格解析——spec §5）。序列化
        // 恒不写出 user_id（重写后旧字段自然消失）。
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyWire {
            app_id: String,
            #[serde(default)]
            #[allow(dead_code)]
            user_id: Option<String>,
            lifecycle_id: String,
            service_type: ServiceType,
            physical_uid: String,
            adopted_by_operation: String,
        }
        let wire = LegacyWire::deserialize(deserializer)?;
        Ok(Self {
            app_id: wire.app_id,
            lifecycle_id: wire.lifecycle_id,
            service_type: wire.service_type,
            physical_uid: wire.physical_uid,
            adopted_by_operation: wire.adopted_by_operation,
        })
    }
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
    binding: Option<&UserAppResourceBinding>,
) -> Result<bool, String> {
    context.validate_identity(&context.app_id)?;
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

    /// R08：存量持久 JSON（含已退役 user_id 字段）读取兼容且仅容忍该字段。
    #[test]
    fn legacy_binding_with_user_id_deserializes() {
        let legacy = serde_json::json!({
            "app_id": "app-1",
            "user_id": "legacy-owner",
            "lifecycle_id": "life-1",
            "service_type": "user-app-builder",
            "physical_uid": "uid-1",
            "adopted_by_operation": "op-1"
        });
        let binding: UserAppResourceBinding =
            serde_json::from_value(legacy).expect("legacy record with retired user_id decodes");
        assert_eq!(binding.app_id, "app-1");
        assert_eq!(binding.physical_uid, "uid-1");
        // 序列化恒不写出退役字段（重写后旧键自然消失）
        let rewritten = serde_json::to_value(&binding).expect("rewrite");
        assert!(rewritten.get("user_id").is_none());
    }

    /// R08：未知无关字段仍拒绝（严格解析不全面关闭）。
    #[test]
    fn unrelated_unknown_fields_still_rejected() {
        for payload in [
            serde_json::json!({
                "app_id": "a", "lifecycle_id": "l",
                "service_type": "user-app-builder",
                "physical_uid": "u", "adopted_by_operation": "o",
                "mystery": "field"
            }),
            serde_json::json!({
                "app_id": "a", "owner": "x", "lifecycle_id": "l",
                "service_type": "user-app-builder",
                "physical_uid": "u", "adopted_by_operation": "o"
            }),
        ] {
            assert!(
                serde_json::from_value::<UserAppResourceBinding>(payload).is_err(),
                "unknown non-retired fields must stay rejected"
            );
        }
    }
}

#[cfg(test)]
mod legacy_binding_tests {
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
        assert!(!builder_identity_is_bound(&context, &metadata, "uid", None).expect("candidate"));
        assert!(
            builder_identity_is_bound(&context, &metadata, "uid", Some(&binding)).expect("bound")
        );
        let complete_metadata = context.resource_metadata();
        assert!(
            builder_identity_is_bound(
                &context,
                &complete_metadata,
                "replacement-uid",
                Some(&binding)
            )
            .is_err(),
            "native labels must not hide a stale physical receipt"
        );
        metadata.insert("rcoder.io/lifecycle-id".into(), "older-life".into());
        assert!(builder_identity_is_bound(&context, &metadata, "uid", Some(&binding)).is_err());
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
