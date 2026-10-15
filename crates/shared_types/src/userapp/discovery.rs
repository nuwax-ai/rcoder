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
