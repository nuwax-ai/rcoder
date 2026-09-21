//! Checked projections of private Toasty models; the public memory mirror never
//! receives an ORM connection or silently substitutes invalid persistent fields.
use crate::db::models;
use anyhow::{Context, Result, ensure};

pub(in crate::pg) struct ContainerRow {
    pub container_name: String,
    pub container_generation: String,
    pub row_revision: i64,
    pub container_id: Option<String>,
    /// §1.1 持久 workload 身份——schema 已冻结；读取方随契约四解析器
    /// 分层批次接入（对账判"同 workload"），届时移除本豁免。
    #[allow(dead_code)]
    pub workload_uid: Option<String>,
    pub logical_id: String,
    pub container_ip: String,
    pub internal_port: u16,
    pub external_port: u16,
    pub status: String,
    pub service_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub(in crate::pg) struct ProjectRow {
    pub generation: String,
    pub row_revision: i64,
    pub session_identities: std::collections::BTreeMap<String, String>,
    pub project_id: String,
    pub user_id: Option<String>,
    pub pod_id: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    pub container_name: Option<String>,
    pub container_generation: Option<String>,
    pub latest_session: Option<String>,
    pub model_provider: Option<serde_json::Value>,
    pub request_id: Option<String>,
    pub agent_status: Option<serde_json::Value>,
    pub service_type: Option<String>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub(in crate::pg) struct SessionRow {
    pub generation: String,
    pub project_generation: String,
    pub session_id: String,
    pub project_id: String,
}

fn timestamp(value: i64) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_micros(value).context("Invalid persistent project timestamp")
}
fn identity(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty(),
        "Persistent registration identity is empty"
    );
    Ok(())
}
impl TryFrom<models::Container> for ContainerRow {
    type Error = anyhow::Error;
    fn try_from(row: models::Container) -> Result<Self> {
        identity(&row.container_name)?;
        identity(&row.container_generation)?;
        identity(&row.logical_id)?;
        ensure!(row.row_revision >= 1, "Invalid container revision");
        ensure!(
            row.container_id.as_ref().is_none_or(|id| !id.is_empty()),
            "Empty bound container identity"
        );
        let _: shared_types::ServiceType = row.service_type.parse().map_err(anyhow::Error::msg)?;
        timestamp(row.last_activity_at_us)?;
        Ok(Self {
            container_name: row.container_name,
            container_generation: row.container_generation,
            row_revision: row.row_revision,
            container_id: row.container_id,
            workload_uid: row.workload_uid,
            logical_id: row.logical_id,
            container_ip: row.container_ip,
            internal_port: u16::try_from(row.internal_port)
                .context("Invalid container internal port")?,
            external_port: u16::try_from(row.external_port)
                .context("Invalid container external port")?,
            status: row.status,
            service_url: row.service_url,
            created_at: timestamp(row.created_at_us)?,
        })
    }
}
impl ProjectRow {
    pub(in crate::pg) fn from_model(
        row: models::Project,
        sessions: Vec<SessionRow>,
    ) -> Result<Self> {
        identity(&row.project_id)?;
        identity(&row.generation)?;
        ensure!(
            row.payload_version == 1 && row.row_revision >= 1,
            "Unsupported project payload version or revision"
        );
        ensure!(
            row.container_name.is_some() == row.container_generation.is_some(),
            "Incomplete project container identity"
        );
        let service_type = row
            .service_type
            .as_deref()
            .context("Project service type is missing")?;
        let _: shared_types::ServiceType = service_type.parse().map_err(anyhow::Error::msg)?;
        let mut identities = std::collections::BTreeMap::new();
        for session in sessions {
            ensure!(
                session.project_id == row.project_id
                    && session.project_generation == row.generation,
                "Session belongs to another project generation"
            );
            ensure!(
                identities
                    .insert(session.session_id, session.generation)
                    .is_none(),
                "Duplicate persistent session identity"
            );
        }
        ensure!(
            row.latest_session
                .as_ref()
                .is_none_or(|id| identities.contains_key(id)),
            "Project latest session is not owned by its generation"
        );
        // Decode errors deliberately omit the raw private JSON/credential value.
        let model_provider = row
            .model_provider_json
            .as_deref()
            .map(|raw| {
                serde_json::from_str::<shared_types::ModelProviderConfig>(raw)
                    .map_err(|_| anyhow::anyhow!("Invalid persisted model provider configuration"))
                    .and_then(|value| serde_json::to_value(value).map_err(Into::into))
            })
            .transpose()?;
        let agent_status = row
            .agent_status_json
            .as_deref()
            .map(|raw| {
                serde_json::from_str::<shared_types::AgentStatus>(raw)
                    .map_err(|_| anyhow::anyhow!("Invalid persisted agent status"))
                    .and_then(|value| serde_json::to_value(value).map_err(Into::into))
            })
            .transpose()?;
        Ok(Self {
            generation: row.generation,
            row_revision: row.row_revision,
            session_identities: identities,
            project_id: row.project_id,
            user_id: row.user_id,
            pod_id: row.pod_id,
            tenant_id: row.tenant_id,
            space_id: row.space_id,
            isolation_type: row.isolation_type,
            container_name: row.container_name,
            container_generation: row.container_generation,
            latest_session: row.latest_session,
            model_provider,
            request_id: row.request_id,
            agent_status,
            service_type: row.service_type,
            last_activity: timestamp(row.last_activity_at_us)?,
            created_at: timestamp(row.created_at_us)?,
        })
    }
}
impl TryFrom<models::Session> for SessionRow {
    type Error = anyhow::Error;
    fn try_from(row: models::Session) -> Result<Self> {
        identity(&row.session_id)?;
        identity(&row.generation)?;
        identity(&row.project_id)?;
        identity(&row.project_generation)?;
        timestamp(row.created_at_us)?;
        timestamp(row.last_seen_at_us)?;
        Ok(Self {
            generation: row.generation,
            project_generation: row.project_generation,
            session_id: row.session_id,
            project_id: row.project_id,
        })
    }
}

impl ContainerRow {
    pub(in crate::pg) fn persistence_identity(
        &self,
    ) -> shared_types::persistence::ContainerPersistenceIdentity {
        shared_types::persistence::ContainerPersistenceIdentity {
            generation: self.container_generation.clone(),
            revision: self.row_revision,
            physical_uid: self.container_id.clone(),
            predecessor: None,
            predecessor_revision: None,
        }
    }
}
