use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Intent {
    pub request: RuntimeOperationRequest,
    /// Digest of the original full request, including private configuration.
    pub(super) digest: String,
    pub(super) has_private_config: bool,
    pub(super) address: String,
    #[serde(default)]
    pub(super) project_root: Option<std::path::PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OwnerRecord {
    pub pid: u32,
    pub port: u16,
    pub project_id: String,
    pub owner: OwnerIdentity,
    #[serde(default)]
    pub registration_operation_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OwnerIdentity {
    pub address: String,
    pub runtime_instance_id: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct MigrationLocationRecord {
    pub version: u8,
    /// 归一 workspace origin（记录与当前项目不一致时不使用，防止跨项目误绑）。
    pub origin: String,
    pub receipts_dir: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub owners: HashMap<String, OwnerRecord>,
    pub stops: HashMap<String, crate::service::dev_server::types::ExternalStopRecord>,
    #[serde(default)]
    pub intents: HashMap<String, Intent>,
    #[serde(default)]
    pub(super) completed: HashMap<String, Intent>,
    /// Retired transport registrations, not invented terminal operation results.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) retired: HashMap<String, serde_json::Value>,
    /// DEV-1 §3.3：可续查的本地监督停止（加性字段；不含凭据）。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) local_stops: HashMap<String, crate::service::dev_server::types::LocalStopRecord>,
    /// DEV-R5：迁移回执位置的项目绑定（加性字段；只绑定位置，不伪造
    /// completed——回执本身的成功/未知语义由 app-cli 回执承载）。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) migration_locations: HashMap<String, MigrationLocationRecord>,
}

impl State {
    pub(crate) fn owner_address<'a>(&'a self, project: &str, fallback: &'a str) -> &'a str {
        self.owners
            .get(project)
            .map(|record| record.owner.address.as_str())
            .or_else(|| {
                self.intents
                    .iter()
                    .find(|(slot, _)| slot.starts_with(&format!("{project}|")))
                    .map(|(_, intent)| intent.address.as_str())
            })
            .unwrap_or(fallback)
    }

    pub(crate) fn project_snapshot(&self, project: &str) -> serde_json::Value {
        let intents: std::collections::BTreeMap<_, _> = self
            .intents
            .iter()
            .filter(|(slot, _)| slot.starts_with(&format!("{project}|")))
            .collect();
        serde_json::json!({"owner": self.owners.get(project), "stop": self.stops.get(project), "intents": intents})
    }

    pub(crate) fn needs_owner(&self, project: &str) -> bool {
        self.owners.contains_key(project)
            || self.stops.contains_key(project)
            || self
                .intents
                .keys()
                .chain(self.completed.keys())
                .any(|slot| slot.starts_with(&format!("{project}|")))
    }

    pub(crate) fn has_replaced_registration(&self, project: &str, instance: &str) -> bool {
        self.owners
            .get(project)
            .is_some_and(|owner| owner.owner.runtime_instance_id != instance)
            || self.intents.iter().any(|(slot, intent)| {
                slot.starts_with(&format!("{project}|"))
                    && intent.request.expected_runtime_instance_id != instance
            })
    }
}

pub(super) fn key(project: &str, kind: RuntimeOperationKind) -> String {
    format!("{project}|{kind:?}")
}
pub(super) fn digest(request: &RuntimeOperationRequest) -> Result<String> {
    let bytes = serde_json::to_vec(request).context("encode runtime intent digest")?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
pub(super) fn read(path: &Path) -> Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("external owner state is corrupt; recovery required"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).context("read external owner state; recovery required"),
    }
}

/// This file is a transport cache. The app-cli operation store remains the
/// authority for execution and replay. Call only under the retained file lock.
pub(super) fn read_or_quarantine(path: &Path) -> Result<State> {
    match read(path) {
        Ok(state) => Ok(state),
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            let backup =
                path.with_extension(format!("corrupt-{}.json", uuid::Uuid::new_v4().simple()));
            std::fs::rename(path, &backup)
                .context("preserve damaged external owner registration")?;
            tracing::warn!(backup = %backup.display(), %error, "rebuilding external owner cache from the runtime owner");
            Ok(State::default())
        }
        Err(error) => Err(error),
    }
}
