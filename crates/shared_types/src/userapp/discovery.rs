//! Read-only discovery of an existing managed lifecycle. This is not authority
//! to mutate a resource; subsequent capture must revalidate its physical UID.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserAppDiscoveredIdentity {
    pub app_id: String,
    pub lifecycle_id: String,
    pub dev_uid: Option<String>,
    pub prod_uid: Option<String>,
    pub dev_stopped: bool,
    pub prod_stopped: bool,
    /// Stable PVC identities, excluding resourceVersion so unrelated metadata
    /// updates do not invalidate inventory comparison. Empty for Docker.
    #[serde(default)]
    pub dev_volumes: Vec<crate::AppResourceIdentity>,
    #[serde(default)]
    pub prod_volumes: Vec<crate::AppResourceIdentity>,
}
impl UserAppDiscoveredIdentity {
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_identifier(&self.app_id, "app_id")?;
        crate::validate_identifier(&self.lifecycle_id, "lifecycle_id")?;
        if self.dev_uid.is_none() && self.prod_uid.is_none()
            || self.dev_uid.as_ref().is_some_and(String::is_empty)
            || self.prod_uid.as_ref().is_some_and(String::is_empty)
            || (self.dev_stopped && self.dev_uid.is_none())
            || (self.prod_stopped && self.prod_uid.is_none())
        {
            return Err("Invalid discovered compute identity".into());
        }
        for (owner, volumes) in [
            (&self.dev_uid, &self.dev_volumes),
            (&self.prod_uid, &self.prod_volumes),
        ] {
            let mut names = std::collections::BTreeSet::new();
            for volume in volumes {
                if owner.is_none()
                    || volume.kind != crate::AppResourceKind::PersistentVolumeClaim
                    || volume.name.is_empty()
                    || volume.uid.is_empty()
                    || !names.insert(&volume.name)
                {
                    return Err("Invalid discovered volume identity".into());
                }
            }
        }
        Ok(())
    }
}

/// Captured surviving storage for default-template recovery. This does not
/// assert that a compute instance exists; its UID must be rechecked before use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserAppRetainedStorage {
    pub app_id: String,
    pub scope: crate::UserAppOperationScope,
    pub lifecycle_id: Option<String>,
    pub volumes: Vec<crate::AppResourceIdentity>,
    /// Stable Services may survive controller deletion and preserve its lifecycle.
    #[serde(default)]
    pub services: Vec<crate::AppResourceIdentity>,
}
impl UserAppRetainedStorage {
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_identifier(&self.app_id, "app_id")?;
        match self.scope {
            crate::UserAppOperationScope::Dev | crate::UserAppOperationScope::Prod => {}
            crate::UserAppOperationScope::Application => {
                return Err("Storage recovery requires dev or prod scope".into());
            }
        }
        if let Some(id) = &self.lifecycle_id {
            crate::validate_identifier(id, "lifecycle_id")?;
        }
        if self.volumes.is_empty() {
            return Err("Storage recovery requires surviving volumes".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for volume in &self.volumes {
            if volume.kind != crate::AppResourceKind::PersistentVolumeClaim
                || volume.name.is_empty()
                || volume.uid.is_empty()
                || !names.insert(&volume.name)
            {
                return Err("Invalid retained volume identity".into());
            }
        }
        for service in &self.services {
            if service.kind != crate::AppResourceKind::Service
                || service.name.is_empty()
                || service.uid.is_empty()
                || !names.insert(&service.name)
            {
                return Err("Invalid retained Service identity".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod recovery_command_tests {
    #[test]
    fn historical_deploy_command_keeps_its_serialized_shape_and_traffic_marker_is_explicit() {
        let original =
            serde_json::json!({"type":"deploy","restart":false,"input_digest":"a".repeat(64)});
        let command: crate::UserAppControlCommand =
            serde_json::from_value(original.clone()).unwrap();
        assert!(!command.is_traffic_wake());
        assert_eq!(serde_json::to_value(command).unwrap(), original);
        let recovery = crate::UserAppControlCommand::Deploy {
            restart: false,
            input_digest: "a".repeat(64),
            serving_recovery: true,
        };
        assert!(recovery.is_traffic_wake());
        assert_eq!(
            serde_json::to_value(recovery).unwrap()["serving_recovery"],
            true
        );
    }
}
