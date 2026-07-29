use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub schema_version: u32,
    pub workspace: WorkspaceMeta,
    #[serde(default)]
    pub pingap: PingapSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PingapMode {
    #[default]
    Managed,
    Extend,
    Custom,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PingapSection {
    #[serde(default)]
    pub mode: PingapMode,
    #[serde(default)]
    pub config: Option<String>,
}

impl Default for PingapSection {
    fn default() -> Self {
        Self {
            mode: PingapMode::Managed,
            config: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub project: ProjectMeta,
    pub build: BuildSection,
    pub run: RunSection,
    #[serde(default)]
    pub health: HealthSection,
    #[serde(default)]
    pub proxy: Option<ProxySection>,
    #[serde(default)]
    pub logs: LogsSection,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMeta {
    pub service_id: String,
    pub name: String,
    pub r#type: ProjectType,
    #[serde(default)]
    pub kind: ProjectKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectType {
    Node,
    Java,
    Python,
    Go,
    Rust,
    Static,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectKind {
    #[default]
    Web,
    Worker,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSection {
    pub command: Vec<String>,
    pub artifact: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSection {
    pub command: Vec<String>,
    #[serde(default)]
    pub migrate: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSection {
    #[serde(default = "default_health_path")]
    pub startup_path: String,
    #[serde(default = "default_health_path")]
    pub readiness_path: String,
    #[serde(default = "default_health_path")]
    pub liveness_path: String,
}

impl Default for HealthSection {
    fn default() -> Self {
        Self {
            startup_path: default_health_path(),
            readiness_path: default_health_path(),
            liveness_path: default_health_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySection {
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub upstream_includes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogsSection {
    #[serde(default)]
    pub sources: Vec<LogSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogSource {
    pub id: String,
    pub glob: String,
    pub format: LogFormat,
    #[serde(default)]
    pub multiline_start_pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Jsonl,
    Text,
}

#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub dir: String,
    pub manifest: ProjectManifest,
}

impl DiscoveredProject {
    pub fn name(&self) -> &str {
        &self.manifest.project.name
    }

    pub fn service_id(&self) -> &str {
        &self.manifest.project.service_id
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLock {
    pub schema_version: u32,
    pub release_id: String,
    pub workspace_name: String,
    pub pingap: LockedPingap,
    pub minimum_app_cli_version: String,
    pub runtime_image_digest: String,
    pub services: Vec<LockedService>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPingap {
    pub mode: PingapMode,
    pub config: Option<String>,
    pub version: String,
    pub commit: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedService {
    pub service_id: String,
    pub name: String,
    pub dir: String,
    pub r#type: ProjectType,
    pub kind: ProjectKind,
    pub enabled: bool,
    pub port: u16,
    pub run: RunSection,
    pub health: HealthSection,
    pub proxy: Option<ProxySection>,
    pub logs: Vec<LogSource>,
    pub env: BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

fn default_shutdown_timeout() -> u64 {
    30
}

fn default_health_path() -> String {
    "/health".into()
}
